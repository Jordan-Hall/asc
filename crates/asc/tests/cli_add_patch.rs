//! U21c acceptance: `add -p`, driven through the built binary against real git (via `RealGit`)
//! on identical files and identical answers. The staged blobs must equal git's, and so must
//! what the dialogue prints, bar the letters of the prompt (they differ between git versions).
//! The asc side builds its fixture directly through the engine and `state.db`; no sibling
//! command is used.

mod support;

use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use asc_core::object::Kind;
use asc_git::index::{FileMode, IndexFile};
use asc_git::realgit::RealGit;
use asc_state::state::stage::{self, StageSet};
use asc_state::state::workspaces;
use futures::TryStreamExt;
use support::{Fixture, text};

struct Out {
    code: i32,
    stdout: String,
    stderr: String,
}

/// Runs `cmd` with `input` on its standard input.
fn run_with(cmd: &mut Command, input: &str) -> Out {
    let mut child = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn");
    let mut stdin = child.stdin.take().unwrap();
    let _ = stdin.write_all(input.as_bytes());
    drop(stdin);
    let out = child.wait_with_output().unwrap();
    Out { code: out.status.code().unwrap_or(-1), stdout: text(&out.stdout), stderr: text(&out.stderr) }
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
        let git_bin = RealGit::resolve(None, None).expect("real git").path().to_owned();
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

    fn git_cmd(&self, cwd: &Path) -> Command {
        self.f.sandbox.command(&self.git_bin, cwd)
    }

    fn git(&self, args: &[&str]) -> Out {
        run_with(self.git_cmd(&self.git).args(args), "")
    }

    fn asc(&self, args: &[&str]) -> Out {
        run_with(self.f.asc().args(args), "")
    }

    fn write(&self, rel: &str, bytes: &[u8]) {
        for root in [&self.f.root, &self.git] {
            let p = root.join(rel);
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(p, bytes).unwrap();
        }
    }

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

    async fn asc_index(&self) -> Vec<String> {
        let ws = workspaces::get(&self.f.db, self.f.ws).await.unwrap().unwrap();
        let mut entries: BTreeMap<(Vec<u8>, u8), String> = BTreeMap::new();
        if let Some(cs) = ws.ws.base_cs {
            let root = self.f.engine.get_changeset(cs).await.unwrap().root;
            let mut walk = self.f.engine.walk(root, None);
            while let Some((path, e)) = walk.try_next().await.unwrap() {
                entries.insert((path.as_bytes().to_vec(), 0), line(e.kind, &e.git.to_string()));
            }
        }
        for r in stage::overlay(&self.f.db, self.f.ws, StageSet::Default).await.unwrap() {
            if r.stage == 0 {
                entries.remove(&(r.path.clone(), 0));
            }
            if let Some(git) = r.git.filter(|_| r.file.is_some()) {
                entries.insert((r.path, r.stage), line(r.kind, &git.to_string()));
            }
        }
        entries.into_iter().map(|((path, _), l)| format!("{l}\t{}", String::from_utf8_lossy(&path))).collect()
    }

    fn git_index(&self) -> Vec<String> {
        let out = self.git(&["ls-files", "-s"]);
        assert_eq!(out.code, 0, "{}", out.stderr);
        out.stdout.lines().map(|l| l.replace(" 0\t", "\t")).collect()
    }

    /// `add -p args…` fed `answers`, on both sides, with `env` set on both.
    fn patch(&self, args: &[&str], answers: &str, env: &[(&str, &str)]) -> (Out, Out) {
        self.patch_in(&self.f.root, &self.git, args, answers, env)
    }

    fn patch_in(&self, asc_cwd: &Path, git_cwd: &Path, args: &[&str], answers: &str, env: &[(&str, &str)]) -> (Out, Out) {
        let mut c = self.f.sandbox.asc(asc_cwd);
        c.arg("add").arg("-p").args(args);
        let mut g = self.git_cmd(git_cwd);
        g.arg("add").arg("-p").args(args);
        for (k, v) in env {
            c.env(k, v);
            g.env(k, v);
        }
        (run_with(&mut c, answers), run_with(&mut g, answers))
    }

    /// Both sides agree on the exit code, the dialogue and the staged tree.
    async fn agree(&self, what: &str, (c, g): &(Out, Out)) {
        assert_eq!(c.code, g.code, "{what}: exit code\n{}\n{}", c.stderr, g.stderr);
        assert_eq!(norm(&c.stdout), norm(&g.stdout), "{what}: dialogue");
        assert_eq!(self.asc_index().await, self.git_index(), "{what}: staged tree");
    }
}

fn line(kind: Kind, oid: &str) -> String {
    let mode = match kind {
        Kind::Exec => "100755",
        Kind::Symlink => "120000",
        Kind::Gitlink => "160000",
        _ => "100644",
    };
    format!("{mode} {oid}")
}

/// The dialogue without the letters each prompt offers.
fn norm(s: &str) -> String {
    let mut out = String::new();
    let mut rest = s;
    while let Some(at) = rest.find(" [y,n,q,a,d") {
        out.push_str(&rest[..at]);
        let tail = &rest[at..];
        let end = tail.find("]? ").map_or(tail.len(), |e| e + 1);
        out.push_str(" [..]");
        rest = &tail[end..];
    }
    out.push_str(rest);
    out
}

fn numbered(n: usize, change: &[usize]) -> Vec<u8> {
    (1..=n)
        .map(|i| if change.contains(&i) { format!("new{i}\n") } else { format!("{i}\n") })
        .collect::<String>()
        .into_bytes()
}

/// A script the editor runs on the hunk file: `sh script file`.
fn editor(r: &Rig, name: &str, body: &str) -> String {
    let dir = r.f.sandbox.dir("editors");
    let path = dir.join(name);
    fs::write(&path, body).unwrap();
    format!("sh \"{}\"", path.display().to_string().replace('\\', "/"))
}

#[tokio::test]
async fn staged_blobs_match_git_for_the_same_answers() {
    let scripts = [
        "y\nn\ny\nn\n",
        "n\na\n",
        "y\nd\n",
        "J\nd\ny\n",
        "J\nJ\na\n",
        "y\ny\nq\n",
        "K\ny\nj\nn\nk\ny\n",
        "g3\ny\nq\n",
        "g\n2\nn\ng 4\ny\nq\n",
        "/new35\ny\nq\n",
        "/\nnew20\ny\nq\n",
        "y\n",
        "n\nn\nn\nn\n",
        "?\nz\nyes\n\nq\n",
        "p\nP\nq\n",
        "Y\nN\nA\n",
        "y\nK\nK\nK\ny\nq\n",
    ];
    for answers in scripts {
        let r = Rig::new().await;
        r.seed(&[("f.txt", &numbered(60, &[]))]).await;
        r.write("f.txt", &numbered(60, &[5, 20, 35, 50]));
        let got = r.patch(&[], answers, &[]);
        r.agree(&format!("answers {answers:?}"), &got).await;
    }
}

#[tokio::test]
async fn split_and_edit_stage_what_git_stages() {
    let cases: [(&str, &str); 6] = [
        ("s\ny\nn\n", ""),
        ("s\nn\ny\n", ""),
        ("s\nj\ny\nq\n", ""),
        ("e\nn\n", "sed -i 's/^+new4$/+EDITED/' \"$1\"\n"),
        ("s\ne\ny\n", "sed -i 's/^+new8$/+EDITED\\n+MORE/' \"$1\"\n"),
        // The edit's context is not in the file: asked again, then discarded.
        ("e\nn\ny\n", "sed -i 's/^ 3$/ three/' \"$1\"\n"),
    ];
    for (answers, script) in cases {
        let r = Rig::new().await;
        r.seed(&[("f.txt", &numbered(14, &[]))]).await;
        r.write("f.txt", &numbered(14, &[4, 8]));
        let ed = editor(&r, "ed.sh", script);
        let got = r.patch(&[], answers, &[("GIT_EDITOR", ed.as_str())]);
        r.agree(&format!("answers {answers:?}, editor {script:?}"), &got).await;
    }
    // Every line removed: the edit is abandoned and the hunk stays undecided.
    let r = Rig::new().await;
    r.seed(&[("f.txt", &numbered(14, &[]))]).await;
    r.write("f.txt", &numbered(14, &[4, 8]));
    let ed = editor(&r, "empty.sh", ": > \"$1\"\n");
    let got = r.patch(&[], "e\ny\nq\n", &[("GIT_EDITOR", ed.as_str())]);
    r.agree("abandoned edit", &got).await;
}

#[tokio::test]
async fn added_deleted_and_binary_files_follow_git() {
    let r = Rig::new().await;
    r.seed(&[
        ("a.txt", &numbered(10, &[])),
        ("del.txt", b"gone\nsoon\n"),
        ("empty.txt", b""),
        ("bin.dat", &[0, 1, 2, 3]),
        ("keep.txt", b"keep\n"),
    ])
    .await;
    r.write("a.txt", &numbered(10, &[5]));
    r.delete("del.txt");
    r.delete("empty.txt");
    r.write("bin.dat", &[0, 9, 9, 9]);
    r.write("new.txt", b"fresh\nlines\n");
    r.write("zero.txt", b"");
    for args in [&["add", "-N", "new.txt", "zero.txt"][..]] {
        assert_eq!(r.asc(args).code, 0);
        assert_eq!(r.git(args).code, 0);
    }
    let got = r.patch(&[], "y\ny\ny\ny\n", &[]);
    r.agree("every file offered", &got).await;
    assert!(got.0.stdout.contains("Stage deletion") && got.0.stdout.contains("Stage addition"));
    assert!(!got.0.stdout.contains("bin.dat"), "binary files are skipped");
}

#[tokio::test]
async fn messages_when_nothing_is_offered() {
    let r = Rig::new().await;
    r.seed(&[("a.txt", b"a\n"), ("bin.dat", &[0, 1])]).await;
    let got = r.patch(&[], "y\n", &[]);
    r.agree("nothing changed", &got).await;
    assert_eq!(got.0.stdout, "No changes.\n");
    r.write("bin.dat", &[0, 2]);
    let got = r.patch(&[], "y\n", &[]);
    r.agree("only a binary file changed", &got).await;
    assert_eq!(got.0.stdout, "Only binary files changed.\n");
    r.write("a.txt", b"b\n");
    let got = r.patch(&["nothing.txt"], "y\n", &[]);
    r.agree("pathspec without a change", &got).await;
    let got = r.patch(&["a.txt"], "n\n", &[]);
    r.agree("declined", &got).await;
}

#[tokio::test]
async fn dry_run_is_refused_like_git() {
    let r = Rig::new().await;
    r.seed(&[("a.txt", b"a\n")]).await;
    let (c, g) = r.patch(&["-n"], "", &[]);
    assert_eq!((c.code, &c.stderr), (g.code, &g.stderr));
    assert_eq!(c.code, 128);
}

#[tokio::test]
async fn crlf_work_tree_files_diff_and_stage_in_their_normalised_form() {
    let r = Rig::new().await;
    r.write(".gitattributes", b"*.txt text=auto\n");
    r.seed(&[("a.txt", &numbered(30, &[])), (".gitattributes", b"*.txt text=auto\n")]).await;
    // Same lines but CRLF: no difference at all after normalisation.
    let crlf = |change: &[usize]| String::from_utf8(numbered(30, change)).unwrap().replace('\n', "\r\n").into_bytes();
    r.write("a.txt", &crlf(&[]));
    let got = r.patch(&[], "y\n", &[]);
    r.agree("line endings alone", &got).await;
    assert_eq!(got.0.stdout, "No changes.\n");
    r.write("a.txt", &crlf(&[3, 25]));
    for answers in ["y\nn\n", "n\ny\n", "a\n"] {
        let r2 = Rig::new().await;
        r2.write(".gitattributes", b"*.txt text=auto\n");
        r2.seed(&[("a.txt", &numbered(30, &[])), (".gitattributes", b"*.txt text=auto\n")]).await;
        r2.write("a.txt", &crlf(&[3, 25]));
        let got = r2.patch(&[], answers, &[]);
        r2.agree(&format!("CRLF, answers {answers:?}"), &got).await;
    }
}

#[tokio::test]
async fn paths_are_chosen_relative_to_the_current_directory() {
    let r = Rig::new().await;
    r.seed(&[("top.txt", b"t\n"), ("dir/a.txt", b"a\n"), ("dir/sub/b.txt", b"b\n"), ("other/c.txt", b"c\n")]).await;
    for p in ["top.txt", "dir/a.txt", "dir/sub/b.txt", "other/c.txt"] {
        r.write(p, b"changed\n");
    }
    let (asc_dir, git_dir) = (r.f.root.join("dir"), r.git.join("dir"));
    // Without a path git offers the whole repository, whatever the directory.
    let got = r.patch_in(&asc_dir, &git_dir, &[], "n\nn\nn\nn\n", &[]);
    r.agree("no path", &got).await;
    assert!(got.0.stdout.contains("dir/a.txt") && got.0.stdout.contains("top.txt"), "{}", got.0.stdout);
    let got = r.patch_in(&asc_dir, &git_dir, &["."], "y\nn\n", &[]);
    r.agree("the current directory", &got).await;
    assert!(!got.0.stdout.contains("top.txt"), "{}", got.0.stdout);
    let got = r.patch_in(&asc_dir, &git_dir, &["../top.txt", "sub"], "y\ny\n", &[]);
    r.agree("paths", &got).await;
}

#[tokio::test]
async fn git_index_file_names_a_temporary_stage_set() {
    let r = Rig::new().await;
    r.seed(&[("a.txt", &numbered(30, &[])), ("b.txt", b"b\n")]).await;
    let tmp = r.f.sandbox.dir("tmpindex");
    let (asc_file, git_file) = (tmp.join("asc-x"), tmp.join("git-x"));
    let c = run_with(r.f.asc().args(["add", "a.txt", "b.txt"]).env("GIT_INDEX_FILE", &asc_file), "");
    assert_eq!(c.code, 0, "{}", c.stderr);
    let g = run_with(r.git_cmd(&r.git).args(["add", "a.txt", "b.txt"]).env("GIT_INDEX_FILE", &git_file), "");
    assert_eq!(g.code, 0);
    r.write("a.txt", &numbered(30, &[3, 25]));
    let mut c = r.f.asc();
    c.args(["add", "-p"]).env("GIT_INDEX_FILE", &asc_file);
    let mut g = r.git_cmd(&r.git);
    g.args(["add", "-p"]).env("GIT_INDEX_FILE", &git_file);
    let (c, g) = (run_with(&mut c, "n\ny\n"), run_with(&mut g, "n\ny\n"));
    assert_eq!(c.code, g.code, "{}", c.stderr);
    assert_eq!(norm(&c.stdout), norm(&g.stdout));
    assert_eq!(entries(&asc_file), entries(&git_file));
    assert_eq!(r.asc_index().await, r.git_index(), "the default staging is unchanged");
}

fn entries(path: &Path) -> Vec<(String, String)> {
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
            (String::from_utf8_lossy(e.path.as_bytes()).into_owned(), format!("{mode} {}", e.oid))
        })
        .collect()
}

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

#[tokio::test]
async fn patch_mode_does_not_read_through_a_linked_directory() {
    let r = Rig::new().await;
    r.seed(&[("a/b", b"orig\n"), ("keep", b"k\n")]).await;
    let outside = r.f.sandbox.dir("outside");
    fs::write(outside.join("b"), b"SECRET\n").unwrap();
    fs::remove_dir_all(r.f.root.join("a")).unwrap();
    assert!(link_dir(&outside, &r.f.root.join("a")), "cannot make a directory link here");
    fs::write(r.git.join("probe"), b"SECRET\n").unwrap();
    let secret = r.git(&["hash-object", "probe"]).stdout.trim().to_owned();
    let out = run_with(r.f.asc().args(["add", "-p"]), "y\ny\n");
    assert_eq!(out.code, 0, "{}", out.stderr);
    assert!(!out.stdout.contains("SECRET"), "outside content was shown:\n{}", out.stdout);
    let index = r.asc_index().await.join("\n");
    assert!(!index.contains(&secret), "outside content was staged:\n{index}");
    assert_eq!(fs::read(outside.join("b")).unwrap(), b"SECRET\n");
}

#[tokio::test]
async fn a_file_over_the_inline_limit_is_not_offered() {
    let r = Rig::new().await;
    r.seed(&[("small.txt", b"s\n")]).await;
    r.write("small.txt", b"t\n");
    let big = vec![b'x'; 4 * 1024 * 1024 + 1];
    fs::write(r.f.root.join("big.txt"), &big).unwrap();
    assert_eq!(r.asc(&["add", "-N", "big.txt"]).code, 0);
    let out = run_with(r.f.asc().args(["add", "-p"]), "y\ny\n");
    assert_eq!(out.code, 0, "{}", out.stderr);
    assert!(out.stdout.contains("small.txt") && !out.stdout.contains("big.txt"), "{}", out.stdout);
}

#[tokio::test]
async fn a_file_of_too_many_lines_is_not_offered() {
    let r = Rig::new().await;
    r.seed(&[("many.txt", b"x\n")]).await;
    r.write("many.txt", &vec![b'\n'; (1 << 18) + 1]);
    let out = run_with(r.f.asc().args(["add", "-p"]), "y\n");
    assert_eq!(out.code, 0, "{}", out.stderr);
    assert_eq!(out.stdout, "No changes.\n");
    assert!(out.stderr.contains("many.txt") && out.stderr.contains("lines"), "{}", out.stderr);
}

#[cfg(unix)]
#[tokio::test]
async fn a_mode_change_is_its_own_hunk() {
    use std::os::unix::fs::PermissionsExt;
    for answers in ["y\ny\n", "y\nn\n", "n\ny\n", "n\nn\n"] {
        let r = Rig::new().await;
        r.seed(&[("run.sh", &numbered(20, &[]))]).await;
        r.write("run.sh", &numbered(20, &[10]));
        for root in [&r.f.root, &r.git] {
            fs::set_permissions(root.join("run.sh"), fs::Permissions::from_mode(0o755)).unwrap();
        }
        let got = r.patch(&[], answers, &[]);
        r.agree(&format!("answers {answers:?}"), &got).await;
    }
    // A mode change alone.
    let r = Rig::new().await;
    r.seed(&[("run.sh", b"echo\n")]).await;
    for root in [&r.f.root, &r.git] {
        fs::set_permissions(root.join("run.sh"), fs::Permissions::from_mode(0o755)).unwrap();
    }
    let got = r.patch(&[], "y\n", &[]);
    r.agree("mode only", &got).await;
    assert!(got.0.stdout.contains("Stage mode change"), "{}", got.0.stdout);
}

#[tokio::test]
async fn the_hunk_list_pages_like_git() {
    let r = Rig::new().await;
    r.seed(&[("f.txt", &numbered(400, &[]))]).await;
    let changed: Vec<usize> = (5..400).step_by(10).collect();
    r.write("f.txt", &numbered(400, &changed));
    let jumps = "J\n".repeat(12);
    for answers in [
        format!("{jumps}g\n\n\n\n15\ny\nq\n"),
        "g\n\nx\nK\ng\n40\nn\n/new255\ny\nq\n".to_owned(),
        "d\n".to_owned(),
    ] {
        let got = r.patch(&[], &answers, &[]);
        let (c, g) = (&got.0, &got.1);
        assert_eq!(c.code, g.code, "{answers:?}");
        assert_eq!(norm(&c.stdout), norm(&g.stdout), "{answers:?}");
        let r2 = &r;
        assert_eq!(r2.asc_index().await, r2.git_index(), "{answers:?}");
    }
}
