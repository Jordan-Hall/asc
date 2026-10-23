//! U21b acceptance: `add`, `rm` and `mv`, driven through the built binary against real git
//! (via `RealGit`) on identical files. The asc side builds its fixture directly through the
//! engine and `state.db` (PLAN "Acceptance before U25"); no sibling command is used.

mod support;

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use asc_core::object::Kind;
use asc_git::index::{FileMode, IndexFile};
use asc_git::realgit::RealGit;
use asc_state::state::stage::{self, StageSet};
use asc_state::state::workspaces;
use asc_store::key::ObjKey;
use asc_store::objects::Objects;
use futures::TryStreamExt;
use support::{Fixture, text};

struct Out {
    code: i32,
    stdout: String,
    stderr: String,
}

fn run(cmd: &mut Command) -> Out {
    let out = cmd.output().expect("spawn");
    Out {
        code: out.status.code().unwrap_or(-1),
        stdout: text(&out.stdout),
        stderr: text(&out.stderr),
    }
}

/// An asc workspace and a real git repository holding the same files.
struct Rig {
    f: Fixture,
    git: PathBuf,
    git_bin: PathBuf,
}

impl Rig {
    async fn new() -> Self {
        let f = Fixture::new().await;
        let git = f.sandbox.dir("gitwork");
        let git_bin = RealGit::resolve(None, None)
            .expect("real git")
            .path()
            .to_owned();
        let rig = Self { f, git, git_bin };
        for args in [
            &["init", "-q"][..],
            &["config", "core.autocrlf", "false"],
            &["config", "user.name", "T"],
            &["config", "user.email", "t@example.com"],
        ] {
            assert_eq!(rig.git(args).code, 0);
        }
        rig
    }

    fn git(&self, args: &[&str]) -> Out {
        run(self.f.sandbox.command(&self.git_bin, &self.git).args(args))
    }

    fn asc(&self, args: &[&str]) -> Out {
        run(self.f.asc().args(args))
    }

    /// Writes `rel` in both work trees.
    fn write(&self, rel: &str, bytes: &[u8]) {
        for root in [&self.f.root, &self.git] {
            let p = root.join(rel);
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(p, bytes).unwrap();
        }
    }

    /// Deletes `rel` from both work trees.
    fn delete(&self, rel: &str) {
        for root in [&self.f.root, &self.git] {
            fs::remove_file(root.join(rel)).unwrap();
        }
    }

    /// Commits `files` in both: the same HEAD tree, nothing staged.
    async fn seed(&self, files: &[(&str, &[u8])]) {
        for (rel, bytes) in files {
            self.write(rel, bytes);
        }
        let c = self.f.commit(&[], files).await;
        self.f.set_branch("master", c).await;
        self.f.checkout_branch("master", Some(c)).await;
        assert_eq!(self.git(&["add", "-A"]).code, 0);
        assert_eq!(self.git(&["commit", "-qm", "seed"]).code, 0);
    }

    /// `ls-files -s` of the staged tree, as asc has it: HEAD overlaid with the `stage` rows.
    async fn asc_index(&self) -> Vec<String> {
        let ws = workspaces::get(&self.f.db, self.f.ws)
            .await
            .unwrap()
            .unwrap();
        let mut entries: BTreeMap<(Vec<u8>, u8), String> = BTreeMap::new();
        if let Some(cs) = ws.ws.base_cs {
            let root = self.f.engine.get_changeset(cs).await.unwrap().root;
            let mut walk = self.f.engine.walk(root, None);
            while let Some((path, e)) = walk.try_next().await.unwrap() {
                entries.insert(
                    (path.as_bytes().to_vec(), 0),
                    line(e.kind, &e.git.to_string(), 0),
                );
            }
        }
        for r in stage::overlay(&self.f.db, self.f.ws, StageSet::Default)
            .await
            .unwrap()
        {
            if r.stage == 0 {
                entries.remove(&(r.path.clone(), 0));
            }
            if let Some(git) = r.git.filter(|_| r.file.is_some()) {
                entries.insert((r.path, r.stage), line(r.kind, &git.to_string(), r.stage));
            }
        }
        entries
            .into_iter()
            .map(|((path, _), l)| format!("{l}\t{}", String::from_utf8_lossy(&path)))
            .collect()
    }

    fn git_index(&self) -> Vec<String> {
        let out = self.git(&["ls-files", "-s"]);
        assert_eq!(out.code, 0, "{}", out.stderr);
        out.stdout.lines().map(str::to_owned).collect()
    }

    async fn same(&self, what: &str) {
        assert_eq!(self.asc_index().await, self.git_index(), "{what}");
    }
}

fn line(kind: Kind, oid: &str, stage: u8) -> String {
    let mode = match kind {
        Kind::Exec => "100755",
        Kind::Symlink => "120000",
        Kind::Gitlink => "160000",
        _ => "100644",
    };
    format!("{mode} {oid} {stage}")
}

const EMPTY_BLOB: &str = "e69de29bb2d1d6434b8b29ae775ad8c2e48c5391";

#[tokio::test]
async fn add_dot_stages_what_git_stages() {
    let r = Rig::new().await;
    r.write("a.txt", b"alpha\n");
    r.write("dir/b.txt", b"beta\n");
    r.write("dir/sub/c.bin", &[0, 1, 2, 255, 254]);
    r.write("empty", b"");
    r.write(".gitignore", b"*.log\nbuild/\n!keep.log\n");
    r.write("x.log", b"log\n");
    r.write("keep.log", b"kept\n");
    r.write("build/out.o", b"obj");
    r.write("dir/.gitignore", b"b.txt\n");
    let (c, g) = (r.asc(&["add", "."]), r.git(&["add", "."]));
    assert_eq!(
        (c.code, c.stderr.as_str()),
        (g.code, g.stderr.as_str()),
        "{}",
        c.stderr
    );
    assert_eq!(c.code, 0);
    r.same("add .").await;
    let index = r.git_index().join("\n");
    assert!(
        !index.contains("x.log") && !index.contains("build/") && index.contains("keep.log"),
        "{index}"
    );
    assert!(!r.f.root.join(".git").is_dir(), "the gitfile is not staged");
}

#[tokio::test]
async fn crlf_files_get_gits_blob_under_text_attributes() {
    let r = Rig::new().await;
    r.write(
        ".gitattributes",
        b"*.txt text=auto\n*.sh text eol=lf\n*.dat binary\n*.crlf eol=crlf\n*.keep -text\n",
    );
    r.write("auto.txt", b"one\r\ntwo\r\n");
    r.write("lone.txt", b"one\r\ntwo\rthree\r\n");
    r.write("zero.txt", b"a\0\r\nb\r\n");
    r.write("x.sh", b"echo\r\n");
    r.write("raw.dat", b"a\r\nb\r\n");
    r.write("eol.crlf", b"a\r\nb\r\n");
    r.write("off.keep", b"a\r\nb\r\n");
    r.write("sub/deep.txt", b"p\r\nq\r\n");
    let c = r.asc(&["add", "."]);
    assert_eq!(c.code, 0, "{}", c.stderr);
    assert_eq!(r.git(&["add", "."]).code, 0);
    r.same("normalised blobs").await;
}

#[tokio::test]
async fn update_and_all_follow_git() {
    let r = Rig::new().await;
    for (name, body) in [("a", "1\n"), ("b", "2\n"), ("c", "3\n"), ("d/e", "4\n")] {
        r.write(name, body.as_bytes());
    }
    assert_eq!(r.asc(&["add", "a", "b", "c", "d"]).code, 0);
    assert_eq!(r.git(&["add", "a", "b", "c", "d"]).code, 0);
    r.same("explicit adds").await;
    r.write("b", b"2 changed\n");
    r.delete("c");
    r.write("new", b"n\n");
    assert_eq!(r.asc(&["add", "-u"]).code, 0);
    assert_eq!(r.git(&["add", "-u"]).code, 0);
    r.same("add -u: no new file, changes and deletions staged")
        .await;
    assert!(!r.asc_index().await.join("\n").contains("\tnew"));
    r.write("d/e", b"4 changed\n");
    r.write("d/f", b"5\n");
    assert_eq!(r.asc(&["add", "-A"]).code, 0);
    assert_eq!(r.git(&["add", "-A"]).code, 0);
    r.same("add -A").await;
    r.delete("a");
    assert_eq!(
        r.asc(&["add", "a"]).code,
        0,
        "a deleted tracked path is staged by name"
    );
    assert_eq!(r.git(&["add", "a"]).code, 0);
    r.same("add of a deleted path").await;
}

#[tokio::test]
async fn staging_over_a_head_keeps_rows_minimal() {
    let r = Rig::new().await;
    r.seed(&[
        ("keep.txt", b"keep\n"),
        ("mod.txt", b"one\n"),
        ("gone.txt", b"bye\n"),
        ("d/x", b"x\n"),
    ])
    .await;
    r.write("mod.txt", b"two\n");
    r.delete("gone.txt");
    r.write("new.txt", b"new\n");
    r.write("d/x", b"x changed\n");
    assert_eq!(r.asc(&["add", "-u", "d"]).code, 0);
    assert_eq!(r.git(&["add", "-u", "d"]).code, 0);
    r.same("add -u with a directory").await;
    assert_eq!(r.asc(&["add", "-u"]).code, 0);
    assert_eq!(r.git(&["add", "-u"]).code, 0);
    r.same("add -u").await;
    let rows = stage::overlay(&r.f.db, r.f.ws, StageSet::Default)
        .await
        .unwrap();
    let paths: Vec<_> = rows
        .iter()
        .map(|x| String::from_utf8_lossy(&x.path).into_owned())
        .collect();
    assert_eq!(
        paths,
        ["d/x", "gone.txt", "mod.txt"],
        "one row per difference from HEAD"
    );
    // Restoring the HEAD content and adding it drops the row.
    r.write("mod.txt", b"one\n");
    assert_eq!(r.asc(&["add", "mod.txt"]).code, 0);
    assert_eq!(r.git(&["add", "mod.txt"]).code, 0);
    r.same("back to HEAD").await;
    let rows = stage::overlay(&r.f.db, r.f.ws, StageSet::Default)
        .await
        .unwrap();
    assert!(rows.iter().all(|x| x.path != b"mod.txt"), "{rows:?}");
}

#[tokio::test]
async fn intent_to_add_records_an_empty_entry() {
    let r = Rig::new().await;
    r.write("a.txt", b"content\n");
    r.write("b.txt", b"other\n");
    assert_eq!(r.asc(&["add", "-N", "a.txt"]).code, 0);
    assert_eq!(r.git(&["add", "-N", "a.txt"]).code, 0);
    r.same("add -N").await;
    assert!(r.git_index()[0].contains(EMPTY_BLOB));
    let rows = stage::overlay(&r.f.db, r.f.ws, StageSet::Default)
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert!(rows[0].intent_to_add, "{rows:?}");
    // A later plain add stages the content.
    assert_eq!(r.asc(&["add", "a.txt"]).code, 0);
    assert_eq!(r.git(&["add", "a.txt"]).code, 0);
    r.same("add after -N").await;
    assert!(
        !stage::overlay(&r.f.db, r.f.ws, StageSet::Default)
            .await
            .unwrap()[0]
            .intent_to_add
    );
}

#[tokio::test]
async fn ignored_paths_need_force_and_dry_run_changes_nothing() {
    let r = Rig::new().await;
    r.write(".gitignore", b"*.log\nbuild/\n");
    r.write("x.log", b"log\n");
    r.write("a.txt", b"a\n");
    r.write("build/o", b"o");
    let (c, g) = (
        r.asc(&["add", "x.log", "a.txt"]),
        r.git(&["add", "x.log", "a.txt"]),
    );
    assert_eq!((c.code, &c.stderr), (g.code, &g.stderr));
    assert_eq!(c.code, 1);
    r.same("the rest is still added").await;
    let (c, g) = (r.asc(&["add", "build"]), r.git(&["add", "build"]));
    assert_eq!((c.code, &c.stderr), (g.code, &g.stderr));
    r.same("an ignored directory").await;
    let (c, g) = (
        r.asc(&["add", "-n", "-f", "x.log"]),
        r.git(&["add", "-n", "-f", "x.log"]),
    );
    assert_eq!((c.code, &c.stdout), (g.code, &g.stdout));
    assert_eq!(c.stdout, "add 'x.log'\n");
    r.same("-n stages nothing").await;
    assert_eq!(r.asc(&["add", "-f", "x.log", "build"]).code, 0);
    assert_eq!(r.git(&["add", "-f", "x.log", "build"]).code, 0);
    r.same("-f").await;
}

#[tokio::test]
async fn nothing_specified_and_unmatched_pathspecs_report_like_git() {
    let r = Rig::new().await;
    r.write("a.txt", b"a\n");
    for args in [
        &["add"][..],
        &["add", "nope"],
        &["add", "a.txt", "*.zzz"],
        &["add", "-A", "-u"],
        &["add", ""],
    ] {
        let (c, g) = (r.asc(args), r.git(args));
        assert_eq!(
            (c.code, &c.stdout, &c.stderr),
            (g.code, &g.stdout, &g.stderr),
            "{args:?}"
        );
    }
    r.same("a failing pathspec stages nothing").await;
}

#[tokio::test]
async fn unsupported_attributes_fail_and_lfs_is_stored_natively() {
    let r = Rig::new().await;
    r.write(
        ".gitattributes",
        b"*.id ident\n*.enc working-tree-encoding=UTF-16\n*.flt filter=foo\n*.bin filter=lfs\n",
    );
    for (name, attr) in [
        ("a.id", "ident"),
        ("a.enc", "working-tree-encoding"),
        ("a.flt", "filter=foo"),
    ] {
        r.write(name, b"x\n");
        let out = r.asc(&["add", name]);
        assert_eq!(out.code, 128, "{name}");
        assert_eq!(
            out.stderr,
            format!("fatal: asc: attribute '{attr}' is not supported\n")
        );
    }
    assert!(
        r.asc_index().await.is_empty(),
        "a refused path stages nothing"
    );
    let big = vec![7u8; 10 * 1024 * 1024];
    r.write("big.bin", &big);
    let out = r.asc(&["add", "big.bin"]);
    assert_eq!(out.code, 0, "{}", out.stderr);
    assert_eq!(
        out.stderr,
        "warning: asc: ignoring filter=lfs for big.bin (large files are native)\n"
    );
    let rows = stage::overlay(&r.f.db, r.f.ws, StageSet::Default)
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].size, big.len() as u64);
    let file = rows[0].file.unwrap();
    assert!(
        r.f.objects
            .get(&ObjKey::ChunkList(file))
            .await
            .unwrap()
            .is_some(),
        "a chunked FileId"
    );
    assert!(
        r.f.objects
            .get(&ObjKey::File(file))
            .await
            .unwrap()
            .is_none()
    );
    let git = r.git(&["hash-object", "big.bin"]);
    assert_eq!(
        rows[0].git.unwrap().to_string(),
        git.stdout.trim(),
        "the blob is the plain content"
    );
}

fn entries(path: &Path) -> Vec<(String, String, u8, bool)> {
    IndexFile::read(path)
        .unwrap()
        .entries
        .iter()
        .map(|e| {
            let mode = match e.mode {
                FileMode::File => "file",
                FileMode::Exec => "exec",
                FileMode::Symlink => "link",
                FileMode::Gitlink => "gitlink",
            };
            (
                String::from_utf8_lossy(e.path.as_bytes()).into_owned(),
                format!("{mode} {}", e.oid),
                e.flags.stage as u8,
                e.flags.intent_to_add,
            )
        })
        .collect()
}

#[tokio::test]
async fn git_index_file_names_a_temporary_stage_set() {
    let r = Rig::new().await;
    r.write("a", b"a\n");
    r.write("d/b", b"b\n");
    let tmp = r.f.sandbox.dir("tmpindex");
    let (asc_index, git_index) = (tmp.join("asc-x"), tmp.join("git-x"));
    let c = run(r.f.asc().args(["add", "a"]).env("GIT_INDEX_FILE", &asc_index));
    assert_eq!(c.code, 0, "{}", c.stderr);
    let g = run(r
        .f
        .sandbox
        .command(&r.git_bin, &r.git)
        .args(["add", "a"])
        .env("GIT_INDEX_FILE", &git_index));
    assert_eq!(g.code, 0);
    assert_eq!(entries(&asc_index), entries(&git_index));
    assert!(
        r.asc_index().await.is_empty(),
        "the default staging is unchanged"
    );
    assert!(!r.f.root.join(".git").is_dir());
    // The set is registered: a second add through the same file adds to it.
    for (bin_asc, args) in [(true, ["add", "d"]), (false, ["add", "d"])] {
        let out = if bin_asc {
            run(r.f.asc().args(args).env("GIT_INDEX_FILE", &asc_index))
        } else {
            run(r
                .f
                .sandbox
                .command(&r.git_bin, &r.git)
                .args(args)
                .env("GIT_INDEX_FILE", &git_index))
        };
        assert_eq!(out.code, 0, "{}", out.stderr);
    }
    assert_eq!(entries(&asc_index), entries(&git_index));
    assert_eq!(entries(&asc_index).len(), 2);
    assert!(r.asc_index().await.is_empty());
}

#[tokio::test]
async fn git_index_file_imports_an_existing_file() {
    let r = Rig::new().await;
    r.write("a", b"a\n");
    r.write("b", b"b\n");
    assert_eq!(r.asc(&["add", "a", "b"]).code, 0);
    // A file git wrote, holding blobs asc knows from its git map.
    let tmp = r.f.sandbox.dir("imp");
    let file = tmp.join("x");
    let g = run(r
        .f
        .sandbox
        .command(&r.git_bin, &r.git)
        .args(["add", "a"])
        .env("GIT_INDEX_FILE", &file));
    assert_eq!(g.code, 0);
    r.write("c", b"c\n");
    let c = run(r.f.asc().args(["add", "c"]).env("GIT_INDEX_FILE", &file));
    assert_eq!(c.code, 0, "{}", c.stderr);
    let g2 = run(r
        .f
        .sandbox
        .command(&r.git_bin, &r.git)
        .args(["add", "c"])
        .env("GIT_INDEX_FILE", &file));
    assert_eq!(g2.code, 0);
    let names: Vec<_> = entries(&file).into_iter().map(|e| e.0).collect();
    assert_eq!(names, ["a", "c"], "the imported entries plus the new one");
}

fn same_run(r: &Rig, args: &[&str]) -> Out {
    let (c, g) = (r.asc(args), r.git(args));
    assert_eq!(
        (c.code, &c.stdout, &c.stderr),
        (g.code, &g.stdout, &g.stderr),
        "{args:?}"
    );
    c
}

fn same_files(r: &Rig, rels: &[&str]) {
    for rel in rels {
        assert_eq!(
            r.f.root.join(rel).exists(),
            r.git.join(rel).exists(),
            "{rel}"
        );
    }
}

#[tokio::test]
async fn rm_follows_git() {
    let r = Rig::new().await;
    r.seed(&[
        ("a.txt", b"a\n"),
        ("b.txt", b"b\n"),
        ("dir/c.txt", b"c\n"),
        ("dir/d.txt", b"d\n"),
        ("e.txt", b"e\n"),
        ("f.txt", b"f\n"),
        ("g.txt", b"g\n"),
        ("h.txt", b"h\n"),
    ])
    .await;
    same_run(&r, &["rm", "--cached", "a.txt"]);
    r.same("--cached").await;
    same_files(&r, &["a.txt"]);
    same_run(&r, &["rm", "b.txt"]);
    r.same("rm").await;
    same_files(&r, &["b.txt"]);
    same_run(&r, &["rm", "dir"]);
    same_run(&r, &["rm", "-n", "-r", "dir"]);
    r.same("-n").await;
    same_files(&r, &["dir/c.txt"]);
    same_run(&r, &["rm", "-rq", "dir"]);
    r.same("rm -r").await;
    same_files(&r, &["dir/c.txt", "dir"]);

    r.write("e.txt", b"e changed\n");
    same_run(&r, &["rm", "e.txt"]);
    r.write("f.txt", b"f staged\n");
    assert_eq!(r.asc(&["add", "f.txt"]).code, 0);
    assert_eq!(r.git(&["add", "f.txt"]).code, 0);
    same_run(&r, &["rm", "f.txt", "e.txt"]);
    r.write("f.txt", b"f edited again\n");
    same_run(&r, &["rm", "f.txt"]);
    same_run(&r, &["rm", "--cached", "f.txt"]);
    assert_eq!(r.asc(&["add", "g.txt"]).code, 0);
    r.write("g.txt", b"g staged\n");
    assert_eq!(r.asc(&["add", "g.txt"]).code, 0);
    assert_eq!(r.git(&["add", "g.txt"]).code, 0);
    same_run(&r, &["rm", "g.txt"]);
    same_run(&r, &["rm", "--cached", "g.txt"]);
    r.same("refusals changed nothing").await;
    same_run(&r, &["rm", "-f", "e.txt", "g.txt"]);
    r.same("rm -f").await;
    same_files(&r, &["e.txt", "g.txt"]);

    same_run(&r, &["rm", "nope"]);
    same_run(&r, &["rm", "--ignore-unmatch", "nope"]);
    same_run(&r, &["rm"]);
    // A removal of a path added but never committed leaves no row.
    r.write("new.txt", b"n\n");
    assert_eq!(r.asc(&["add", "new.txt"]).code, 0);
    assert_eq!(r.git(&["add", "new.txt"]).code, 0);
    same_run(&r, &["rm", "--cached", "new.txt"]);
    r.same("unstaging an added file").await;
    let rows = stage::overlay(&r.f.db, r.f.ws, StageSet::Default)
        .await
        .unwrap();
    assert!(rows.iter().all(|x| x.path != b"new.txt"), "{rows:?}");
}

#[tokio::test]
async fn mv_follows_git_and_records_renames() {
    let r = Rig::new().await;
    r.seed(&[
        ("a.txt", b"a\n"),
        ("d/x", b"x\n"),
        ("d/y", b"y\n"),
        ("e/z", b"z\n"),
    ])
    .await;
    same_run(&r, &["mv", "a.txt", "b.txt"]);
    r.same("file rename").await;
    same_files(&r, &["a.txt", "b.txt"]);
    let pending = asc_state::state::ws_ops::pending_renames(&r.f.db, r.f.ws)
        .await
        .unwrap();
    assert_eq!(pending, [(b"a.txt".to_vec(), b"b.txt".to_vec())]);

    same_run(&r, &["mv", "d", "f"]);
    r.same("directory rename").await;
    same_files(&r, &["d/x", "f/x", "f/y"]);
    same_run(&r, &["mv", "-v", "b.txt", "e"]);
    r.same("into a directory").await;
    same_run(&r, &["mv", "-n", "f/x", "g.txt"]);
    r.same("-n").await;
    same_files(&r, &["f/x", "g.txt"]);
    let mut pending = asc_state::state::ws_ops::pending_renames(&r.f.db, r.f.ws)
        .await
        .unwrap();
    pending.sort();
    let want = [("a.txt", "e/b.txt"), ("d/x", "f/x"), ("d/y", "f/y")]
        .map(|(a, b)| (a.as_bytes().to_vec(), b.as_bytes().to_vec()));
    assert_eq!(pending, want, "a chain of renames folds into one");

    r.write("untracked.txt", b"u\n");
    for args in [
        &["mv", "nope", "z"][..],
        &["mv", "untracked.txt", "z"],
        &["mv", "f/y", "e/z"],
        &["mv", "f", "f/inner"],
        &["mv", "f/y", "nodir/y"],
        &["mv", "f/x", "f/y", "e/b.txt"],
    ] {
        same_run(&r, args);
    }
    same_run(&r, &["mv", "-f", "f/y", "e/z"]);
    r.same("-f overwrites").await;
    same_files(&r, &["f/y", "e/z"]);
}

#[tokio::test]
async fn paths_are_relative_to_the_current_directory() {
    let r = Rig::new().await;
    r.write("top.txt", b"t\n");
    r.write("dir/a.txt", b"a\n");
    r.write("dir/sub/b.txt", b"b\n");
    r.write("other/c.txt", b"c\n");
    let in_dir = |bin_asc: bool, args: &[&str]| {
        if bin_asc {
            run(r.f.sandbox.asc(&r.f.root.join("dir")).args(args))
        } else {
            run(r
                .f
                .sandbox
                .command(&r.git_bin, &r.git.join("dir"))
                .args(args))
        }
    };
    for args in [
        &["add", "sub"][..],
        &["add", "../top.txt"],
        &["add", "a.txt", "../other"],
    ] {
        let (c, g) = (in_dir(true, args), in_dir(false, args));
        assert_eq!((c.code, &c.stderr), (g.code, &g.stderr), "{args:?}");
        r.same(&format!("{args:?}")).await;
    }
    let (c, g) = (
        in_dir(true, &["add", "../../outside"]),
        in_dir(false, &["add", "../../outside"]),
    );
    assert_eq!(c.code, g.code);
    assert!(
        c.stderr.contains("is outside repository at"),
        "{}",
        c.stderr
    );
    r.write("dir/late.txt", b"l\n");
    r.write("top2.txt", b"t2\n");
    assert_eq!(in_dir(true, &["add", "-A"]).code, 0);
    assert_eq!(in_dir(false, &["add", "-A"]).code, 0);
    r.same("add -A without paths covers the whole tree").await;
}

#[tokio::test]
async fn ascignore_wins_and_shadowing_is_reported_once() {
    let r = Rig::new().await;
    fs::write(r.f.root.join(".ascignore"), b"*.tmp\n").unwrap();
    fs::write(r.f.root.join(".gitignore"), b"*.log\n").unwrap();
    fs::write(r.f.root.join("a.txt"), b"a\n").unwrap();
    fs::write(r.f.root.join("x.log"), b"l\n").unwrap();
    fs::write(r.f.root.join("y.tmp"), b"t\n").unwrap();
    fs::create_dir_all(r.f.root.join(".asc/info")).unwrap();
    fs::write(r.f.root.join(".asc/info/exclude"), b"z\n").unwrap();
    fs::write(r.f.root.join(".asc/redirections"), b"cache\n").unwrap();
    fs::create_dir_all(r.f.root.join("cache")).unwrap();
    fs::write(r.f.root.join("cache/big"), b"c\n").unwrap();
    let out = r.asc(&["add", "."]);
    assert_eq!(out.code, 0, "{}", out.stderr);
    assert_eq!(
        out.stderr,
        "warning: asc: .gitignore is not read because .ascignore exists (rename or merge it: asc migrate ignore)\n"
    );
    let staged: Vec<_> = r
        .asc_index()
        .await
        .into_iter()
        .map(|l| l.split('\t').nth(1).unwrap().to_owned())
        .collect();
    assert_eq!(
        staged,
        [
            ".asc/redirections",
            ".ascignore",
            ".gitignore",
            "a.txt",
            "x.log"
        ]
    );
}

// ---------------------------------------------------------------- symlinked ancestors
//
// git's has_symlink_leading_path: a tracked path under a symlinked (or, on Windows, junctioned)
// ancestor is outside the work tree, so it is never read, written or deleted.

/// Makes `link` a directory symlink (Unix) or a junction (Windows, where a symlink needs a
/// privilege) to `target`; false when the OS refuses.
fn link_dir(target: &Path, link: &Path) -> bool {
    #[cfg(unix)]
    let made = std::os::unix::fs::symlink(target, link).is_ok();
    #[cfg(windows)]
    let made = Command::new("cmd")
        .args(["/C", "mklink", "/J"])
        .arg(link)
        .arg(target)
        .output()
        .is_ok_and(|o| o.status.success());
    #[cfg(not(any(unix, windows)))]
    let made = false;
    made
}

/// A repository tracking `files`, with the first directory of `files[0]` swapped for a link to
/// an outside directory holding the same files with `outside` content.
async fn linked_rig(files: &[(&str, &[u8])], outside_body: &[u8]) -> (Rig, PathBuf) {
    let r = Rig::new().await;
    r.seed(files).await;
    let dir = files[0].0.split('/').next().unwrap();
    let out_dir = r.f.sandbox.dir("outside");
    for (rel, _) in files {
        let Some(inner) = rel.strip_prefix(&format!("{dir}/")) else {
            continue;
        };
        let p = out_dir.join(inner);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, outside_body).unwrap();
    }
    fs::remove_dir_all(r.f.root.join(dir)).unwrap();
    assert!(
        link_dir(&out_dir, &r.f.root.join(dir)),
        "cannot make a directory link here"
    );
    (r, out_dir)
}

#[tokio::test]
async fn rm_does_not_delete_through_a_linked_directory() {
    let (r, outside) = linked_rig(&[("a/b", b"same\n"), ("keep", b"k\n")], b"same\n").await;
    let out = r.asc(&["rm", "a/b"]);
    assert_eq!(out.code, 0, "{}", out.stderr);
    assert_eq!(
        fs::read(outside.join("b")).unwrap(),
        b"same\n",
        "the outside file survives"
    );
    assert!(
        fs::symlink_metadata(r.f.root.join("a")).is_ok(),
        "the link itself is not removed"
    );
}

#[tokio::test]
async fn rm_cleanup_stops_at_a_linked_directory() {
    let (r, outside) = linked_rig(&[("a/e/x", b"x\n")], b"x\n").await;
    let out = r.asc(&["rm", "a/e/x"]);
    assert_eq!(out.code, 0, "{}", out.stderr);
    assert!(outside.join("e/x").is_file(), "the outside file survives");
    assert!(
        outside.join("e").is_dir(),
        "the outside directory is not removed"
    );
    assert!(outside.is_dir());
    assert!(
        fs::symlink_metadata(r.f.root.join("a")).is_ok(),
        "the link itself is not removed"
    );
}

#[tokio::test]
async fn add_does_not_read_through_a_linked_directory() {
    let (r, outside) = linked_rig(&[("a/b", b"orig\n"), ("keep", b"k\n")], b"SECRET\n").await;
    // The blob id that staging the outside content would give.
    fs::write(r.git.join("probe"), b"SECRET\n").unwrap();
    let secret = r.git(&["hash-object", "probe"]).stdout.trim().to_owned();
    assert_eq!(secret.len(), 40);
    for args in [
        &["add", "a/b"][..],
        &["add", "-u"],
        &["add", "-A"],
        &["add", "."],
    ] {
        let out = r.asc(args);
        assert_eq!(out.code, 0, "{args:?}: {}", out.stderr);
        let index = r.asc_index().await.join("\n");
        assert!(
            !index.contains(&secret),
            "{args:?} staged outside content:\n{index}"
        );
    }
    assert_eq!(fs::read(outside.join("b")).unwrap(), b"SECRET\n");
}
