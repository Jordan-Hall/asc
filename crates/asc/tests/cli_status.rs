//! U21a acceptance: `status` through the built binary. Each scenario is built twice, once in a
//! asc fixture (engine commits, `state.db` staging) and once in a real git repository with the
//! same files, commit identity and date, so the commit ids agree and the outputs can be compared
//! byte for byte.

mod support;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use asc_core::path::RepoPath;
use asc_state::state::stage::StageRow;
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

/// Real git in `dir`, hermetic and with the fixture's commit identity.
fn git(dir: &Path, home: &Path, args: &[&str]) -> Out {
    run(Command::new("git") // lint:allow-raw-git
        .current_dir(dir)
        .env("HOME", home)
        .env("USERPROFILE", home)
        .env("GIT_CONFIG_GLOBAL", home.join(".gitconfig"))
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_AUTHOR_DATE", "1700000000 +0000")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_DATE", "1700000000 +0000")
        .args(["-c", "core.autocrlf=false", "-c", "core.quotePath=true"])
        .args(args))
}

fn put(root: &Path, rel: &str, content: &str) {
    let path = root.join(rel);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, content).unwrap();
}

fn remove(root: &Path, rel: &str) {
    let _ = fs::remove_file(root.join(rel));
}

/// A state of the repository: committed files, staged changes (`None` = staged delete) and the
/// final working tree edits (`None` = delete).
#[derive(Default)]
struct Scenario {
    head: Vec<(&'static str, &'static str)>,
    staged: Vec<(&'static str, Option<&'static str>)>,
    work: Vec<(&'static str, Option<&'static str>)>,
}

impl Scenario {
    /// The working tree: HEAD's files, then the staged content, then the edits.
    fn write_tree(&self, root: &Path) {
        for (p, c) in &self.head {
            put(root, p, c);
        }
        for (p, c) in &self.staged {
            if let Some(c) = c {
                put(root, p, c);
            }
        }
        self.edit(root);
    }

    fn edit(&self, root: &Path) {
        for (p, c) in &self.work {
            match c {
                Some(c) => put(root, p, c),
                None => remove(root, p),
            }
        }
    }
}

struct Pair {
    f: Fixture,
    git_dir: PathBuf,
    head: asc_core::id::ChangesetId,
}

impl Pair {
    async fn new(s: &Scenario) -> Self {
        let f = Fixture::new().await;
        let head: Vec<(&str, &[u8])> = s.head.iter().map(|(p, c)| (*p, c.as_bytes())).collect();
        let a = f.commit(&[], &head).await;
        f.set_branch("master", a).await;
        f.checkout_branch("master", Some(a)).await;

        // Staged content comes from a side commit whose tree is HEAD's plus the new files.
        let adds: Vec<(&str, &[u8])> = s
            .staged
            .iter()
            .filter_map(|(p, c)| c.map(|c| (*p, c.as_bytes())))
            .collect();
        let mut rows = Vec::new();
        if !adds.is_empty() {
            let side = f.commit(&[a], &adds).await;
            for (p, _) in &adds {
                let leaf = f.file_leaf(side, p).await;
                rows.push(StageRow {
                    path: p.as_bytes().to_vec(),
                    stage: 0,
                    kind: leaf.kind,
                    file: Some(leaf.file),
                    size: leaf.size,
                    git: Some(leaf.git),
                    intent_to_add: false,
                });
            }
        }
        for (p, c) in &s.staged {
            if c.is_none() {
                rows.push(StageRow {
                    path: p.as_bytes().to_vec(),
                    stage: 0,
                    kind: asc_core::object::Kind::File,
                    file: None,
                    size: 0,
                    git: None,
                    intent_to_add: false,
                });
            }
        }
        f.stage(&rows).await;
        s.write_tree(&f.root);

        let git_dir = f.sandbox.dir("gitrepo");
        let home = &f.sandbox.user;
        git(&git_dir, home, &["init", "-q", "-b", "master"]);
        for (p, c) in &s.head {
            put(&git_dir, p, c);
        }
        git(&git_dir, home, &["add", "-A"]);
        let msg = format!("commit of {} files", s.head.len());
        let c = git(&git_dir, home, &["commit", "-q", "-m", &msg]);
        assert_eq!(c.code, 0, "{}", c.stderr);
        for (p, c) in &s.staged {
            match c {
                Some(c) => {
                    put(&git_dir, p, c);
                    git(&git_dir, home, &["add", "--", p]);
                }
                None => {
                    git(&git_dir, home, &["rm", "-q", "--cached", "--", p]);
                }
            }
        }
        s.edit(&git_dir);
        Self {
            f,
            git_dir,
            head: a,
        }
    }

    fn asc(&self, args: &[&str]) -> Out {
        run(self.f.asc().arg("status").args(args))
    }

    fn git(&self, args: &[&str]) -> Out {
        git(
            &self.git_dir,
            &self.f.sandbox.user,
            &[&["status"], args].concat(),
        )
    }

    /// asc's output equals git's for `args`, hints naming `asc` instead of `git`.
    fn same(&self, args: &[&str]) {
        let got = self.asc(args);
        let want = self.git(args);
        assert_eq!(got.code, 0, "asc status {args:?}: {}", got.stderr);
        let want_out = rename_hints(&want.stdout);
        assert_eq!(
            got.stdout, want_out,
            "asc status {args:?}\n--- git:\n{}",
            want.stdout
        );
    }
}

/// The program word of hints: git prints `git`, the `asc` binary prints `asc`.
fn rename_hints(text: &str) -> String {
    text.replace("\"git ", "\"asc ")
}

fn mixed() -> Scenario {
    Scenario {
        head: vec![
            ("a.txt", "one\n"),
            ("b.txt", "two\n"),
            ("c.txt", "three\n"),
            ("d/e.txt", "deep\n"),
            (
                "move-me.txt",
                "same content here\nsecond line\nthird line\n",
            ),
            ("gone.txt", "bye\n"),
        ],
        staged: vec![
            ("a.txt", Some("one staged\n")),
            ("new.txt", Some("brand new\n")),
            ("c.txt", None),
            (
                "moved.txt",
                Some("same content here\nsecond line\nthird line\n"),
            ),
            ("move-me.txt", None),
        ],
        work: vec![
            ("b.txt", Some("two edited\n")),
            ("a.txt", Some("one staged and edited\n")),
            ("gone.txt", None),
            ("d/untracked.txt", Some("u\n")),
            ("loose.txt", Some("loose\n")),
            ("with space.txt", Some("sp\n")),
            ("caf\u{e9}.txt", Some("accent\n")),
        ],
    }
}

#[tokio::test]
async fn long_format_matches_git() {
    let p = Pair::new(&mixed()).await;
    p.same(&[]);
    p.same(&["--long"]);
}

#[tokio::test]
async fn short_and_porcelain_v1_match_git() {
    let p = Pair::new(&mixed()).await;
    for args in [
        &["-s"][..],
        &["-sb"],
        &["--short", "--branch"],
        &["--porcelain"],
        &["--porcelain", "-b"],
        &["--porcelain=v1", "-z"],
        &["-z"],
        &["-sz"],
        &["-s", "--no-renames"],
    ] {
        p.same(args);
    }
}

#[tokio::test]
async fn porcelain_v2_matches_git_including_branch_headers() {
    let p = Pair::new(&mixed()).await;
    for args in [
        &["--porcelain=v2"][..],
        &["--porcelain=v2", "--branch"],
        &["--porcelain=v2", "-z"],
        &["--porcelain=v2", "-b", "-z"],
    ] {
        p.same(args);
    }
}

#[tokio::test]
async fn untracked_modes_and_ignored_match_git() {
    let mut s = mixed();
    s.work.push((".gitignore", Some("*.log\nbuild/\n")));
    s.work.push(("x.log", Some("log\n")));
    s.work.push(("build/out.o", Some("o\n")));
    s.work.push(("newdir/a.txt", Some("a\n")));
    s.work.push(("newdir/sub/b.txt", Some("b\n")));
    let p = Pair::new(&s).await;
    for args in [
        &["-uno"][..],
        &["-unormal"],
        &["-uall"],
        &["-u"],
        &["--untracked-files=no"],
        &["-s", "-uall"],
        &["--ignored"],
        &["--ignored", "-s"],
        &["--porcelain", "--ignored"],
        &["-s", "-uno"],
    ] {
        p.same(args);
    }
}

#[tokio::test]
async fn clean_and_initial_summaries_match_git() {
    let clean = Scenario {
        head: vec![("a.txt", "x\n")],
        ..Scenario::default()
    };
    let p = Pair::new(&clean).await;
    p.same(&[]);
    p.same(&["-s", "-b"]);
    p.same(&["--porcelain=v2", "-b"]);

    let only_untracked = Scenario {
        head: vec![("a.txt", "x\n")],
        work: vec![("u.txt", Some("u\n"))],
        ..Scenario::default()
    };
    let p = Pair::new(&only_untracked).await;
    p.same(&[]);
    p.same(&["-uno"]);

    let staged_only = Scenario {
        head: vec![("a.txt", "x\n")],
        staged: vec![("a.txt", Some("y\n"))],
        ..Scenario::default()
    };
    let p = Pair::new(&staged_only).await;
    p.same(&[]);
    p.same(&["-uno"]);
}

#[tokio::test]
async fn pathspec_limits_and_relative_paths_match_git() {
    let p = Pair::new(&mixed()).await;
    p.same(&["--", "d"]);
    p.same(&["-s", "a.txt", "b.txt"]);
    // From a subdirectory, long and short paths are relative; porcelain stays root-relative.
    for args in [&[][..], &["-s"], &["--porcelain"], &["--porcelain=v2"]] {
        let got = run(p.f.sandbox.asc(&p.f.root.join("d")).arg("status").args(args));
        let want = git(
            &p.git_dir.join("d"),
            &p.f.sandbox.user,
            &[&["status"], args].concat(),
        );
        assert_eq!(got.code, 0, "{}", got.stderr);
        assert_eq!(
            got.stdout,
            rename_hints(&want.stdout),
            "from d/ with {args:?}"
        );
    }
}

#[tokio::test]
async fn unborn_branch_matches_git() {
    let f = Fixture::new().await;
    f.checkout_branch("master", None).await;
    put(&f.root, "u.txt", "u\n");
    let dir = f.sandbox.dir("gitrepo");
    git(&dir, &f.sandbox.user, &["init", "-q", "-b", "master"]);
    put(&dir, "u.txt", "u\n");
    for args in [&[][..], &["-sb"], &["--porcelain=v2", "-b"]] {
        let got = run(f.asc().arg("status").args(args));
        let want = git(&dir, &f.sandbox.user, &[&["status"], args].concat());
        assert_eq!(got.code, 0, "{}", got.stderr);
        assert_eq!(got.stdout, rename_hints(&want.stdout), "{args:?}");
    }
}

/// Points `name` at `cs` (or deletes it) in the fixture's refs, bypassing the chokepoint.
async fn set_ref(f: &Fixture, name: &str, cs: Option<asc_core::id::ChangesetId>) {
    use asc_core::path::RefName;
    use asc_store::refs::{Actor, Expect, RefTxn, RefUpdate, Refs};
    let txn = RefTxn {
        updates: vec![RefUpdate {
            name: RefName::new(name.as_bytes().to_vec()).unwrap(),
            expect: Expect::Any,
            new: cs.map(asc_core::id::RefTarget::Changeset),
            expires_at: None,
        }],
        actor: Actor {
            id: "t".into(),
            via: "test".into(),
        },
        reason: String::new(),
        op_id: uuid::Uuid::now_v7(),
    };
    Refs::update(&f.db.refs(), f.repo, &txn).await.unwrap();
}

#[tokio::test]
async fn upstream_lines_match_git() {
    // Up to date, ahead, behind, diverged and gone, against a remote-tracking ref.
    let f = Fixture::new().await;
    let base = f.commit(&[], &[("f.txt", b"1\n")]).await;
    let mine = f.commit(&[base], &[("f.txt", b"2\n")]).await;
    let theirs = f.commit(&[base], &[("g.txt", b"3\n")]).await;
    let dir = f.sandbox.dir("gitrepo");
    let home = &f.sandbox.user;
    git(&dir, home, &["init", "-q", "-b", "master"]);
    put(&dir, "f.txt", "1\n");
    git(&dir, home, &["add", "-A"]);
    git(&dir, home, &["commit", "-q", "-m", "commit of 1 files"]);
    git(&dir, home, &["branch", "base"]);
    put(&dir, "f.txt", "2\n");
    git(&dir, home, &["commit", "-qam", "commit of 1 files"]);
    git(&dir, home, &["branch", "mine"]);
    git(&dir, home, &["checkout", "-q", "-b", "side", "base"]);
    put(&dir, "g.txt", "3\n");
    git(&dir, home, &["add", "-A"]);
    git(&dir, home, &["commit", "-q", "-m", "commit of 1 files"]);
    git(&dir, home, &["checkout", "-q", "master"]);
    for (cfg, v) in [
        ("remote.origin.url", "x"),
        ("remote.origin.fetch", "+refs/heads/*:refs/remotes/origin/*"),
        ("branch.master.remote", "origin"),
        ("branch.master.merge", "refs/heads/master"),
    ] {
        git(&dir, home, &["config", cfg, v]);
        let out = run(f.asc().args(["config", cfg, v]));
        assert_eq!(out.code, 0, "{}", out.stderr);
    }

    // (label, local tip, remote tip, git local, git remote, f.txt content)
    let cases = [
        ("up to date", mine, Some(mine), "mine", Some("mine"), "2\n"),
        ("ahead", mine, Some(base), "mine", Some("base"), "2\n"),
        ("behind", base, Some(mine), "base", Some("mine"), "1\n"),
        ("diverged", mine, Some(theirs), "mine", Some("side"), "2\n"),
        ("gone", mine, None, "mine", None, "2\n"),
    ];
    for (label, local, remote, git_local, git_remote, content) in cases {
        f.set_branch("master", local).await;
        f.checkout_branch("master", Some(local)).await;
        set_ref(&f, "refs/remotes/origin/master", remote).await;
        put(&f.root, "f.txt", content);
        git(&dir, home, &["reset", "-q", "--hard", git_local]);
        match git_remote {
            Some(r) => git(&dir, home, &["update-ref", "refs/remotes/origin/master", r]),
            None => git(
                &dir,
                home,
                &["update-ref", "-d", "refs/remotes/origin/master"],
            ),
        };
        for args in [
            &[][..],
            &["-sb"],
            &["--porcelain=v2", "-b"],
            &["--no-ahead-behind"],
            &["-sb", "--no-ahead-behind"],
        ] {
            let got = run(f.asc().arg("status").args(args));
            let want = git(&dir, home, &[&["status"], args].concat());
            assert_eq!(got.code, 0, "{label}: {}", got.stderr);
            assert_eq!(got.stdout, rename_hints(&want.stdout), "{label} {args:?}");
        }
    }
}

#[tokio::test]
async fn json_matches_the_golden_shape() {
    let s = Scenario {
        head: vec![("a.txt", "one\n"), ("b.txt", "two\n")],
        staged: vec![("a.txt", Some("one staged\n"))],
        work: vec![("b.txt", Some("edited\n")), ("u.txt", Some("u\n"))],
    };
    let p = Pair::new(&s).await;
    let out = p.asc(&["--json"]);
    assert_eq!(out.code, 0, "{}", out.stderr);
    assert_eq!(out.stdout.lines().count(), 1, "one JSON line");
    let v: serde_json::Value = serde_json::from_str(out.stdout.trim()).unwrap();
    let head_hex = p
        .git(&["--porcelain=v2", "-b"])
        .stdout
        .lines()
        .next()
        .unwrap()
        .trim_start_matches("# branch.oid ")
        .to_owned();
    // The WsStatusResponse fields carry no JsonKind option, so ids and paths are base64.
    assert_eq!(v["head_ref"], "refs/heads/master");
    assert_eq!(v["head"], b64(&unhex(&head_hex)));
    let files = v["files"].as_array().unwrap();
    let find = |path: &str| {
        files
            .iter()
            .find(|f| f["path"] == b64(path.as_bytes()))
            .unwrap_or_else(|| panic!("{path} in {files:?}"))
    };
    assert_eq!(find("a.txt")["xy"], "M.");
    assert_eq!(find("b.txt")["xy"], ".M");
    assert_eq!(find("u.txt")["xy"], "??");
    assert_eq!(find("a.txt")["mode_head"], 0o100644);
    assert_eq!(find("b.txt")["mode_worktree"], 0o100644);
    assert_eq!(files.len(), 3);
    let refused = p.asc(&["--json", "--ignored"]);
    assert_ne!(refused.code, 0);
}

#[tokio::test]
async fn unmerged_paths_match_git() {
    // Conflict stages are staged rows 1 to 3; the fixture builds them from three commits.
    let f = Fixture::new().await;
    let base = f
        .commit(&[], &[("f", b"base\n"), ("g", b"base\n"), ("h", b"base\n")])
        .await;
    let ours = f
        .commit(&[base], &[("f", b"ours\n"), ("k", b"ours\n")])
        .await;
    let theirs = f
        .commit(&[base], &[("f", b"theirs\n"), ("k", b"theirs\n")])
        .await;
    f.set_branch("master", ours).await;
    f.checkout_branch("master", Some(ours)).await;
    let row = |path: &str, stage: u8, leaf: &asc_state::state::stage::Leaf| StageRow {
        path: path.as_bytes().to_vec(),
        stage,
        kind: leaf.kind,
        file: Some(leaf.file),
        size: leaf.size,
        git: Some(leaf.git),
        intent_to_add: false,
    };
    let (b_f, o_f, t_f) = (
        f.file_leaf(base, "f").await,
        f.file_leaf(ours, "f").await,
        f.file_leaf(theirs, "f").await,
    );
    let (o_k, t_k) = (f.file_leaf(ours, "k").await, f.file_leaf(theirs, "k").await);
    let b_g = f.file_leaf(base, "g").await;
    f.stage(&[
        row("f", 1, &b_f),
        row("f", 2, &o_f),
        row("f", 3, &t_f),
        row("k", 2, &o_k),
        row("k", 3, &t_k),
        row("g", 1, &b_g),
        row("g", 2, &b_g),
    ])
    .await;
    for (p, c) in [
        ("f", "<<<<<<<\n"),
        ("g", "base\n"),
        ("h", "base\n"),
        ("k", "<<<<<<<\n"),
    ] {
        put(&f.root, p, c);
    }
    let out = run(f.asc().arg("status"));
    assert_eq!(out.code, 0, "{}", out.stderr);
    for want in [
        "Unmerged paths:",
        "\tboth modified:   f",
        "\tboth added:      k",
        "\tdeleted by them: g",
        "no changes added to commit (use \"asc add\" and/or \"asc commit -a\")",
    ] {
        assert!(out.stdout.contains(want), "{want:?} in\n{}", out.stdout);
    }
    let short = run(f.asc().args(["status", "-s"]));
    assert_eq!(short.stdout, "UU f\nUD g\nAA k\n");
    let v2 = run(f.asc().args(["status", "--porcelain=v2"]));
    let lines: Vec<_> = v2.stdout.lines().collect();
    assert!(
        lines[0].starts_with("u UU N... 100644 100644 100644 100644 "),
        "{lines:?}"
    );
    assert!(
        lines[1].starts_with("u UD N... 100644 100644 000000 100644 "),
        "{lines:?}"
    );
    assert!(
        lines[2].starts_with("u AA N... 000000 100644 100644 100644 "),
        "{lines:?}"
    );
}

#[tokio::test]
async fn optional_locks_gate_the_stat_cache_refresh() {
    use asc_git::index::{EntryFlags, FileMode, IndexEntry, IndexFile, StatTime, write};

    let s = Scenario {
        head: vec![("a.txt", "one\n")],
        ..Scenario::default()
    };
    let p = Pair::new(&s).await;
    let index_path = asc_git::projection::Projection::open(&p.f.root)
        .unwrap()
        .git_dir()
        .join("index");
    // A file old enough not to be racy, with an index entry whose stat is stale.
    let old = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_600_000_000);
    fs::File::options()
        .write(true)
        .open(p.f.root.join("a.txt"))
        .unwrap()
        .set_modified(old)
        .unwrap();
    let leaf = {
        let root = p.f.engine.get_changeset(p.head).await.unwrap().root;
        p.f.engine
            .lookup(root, &RepoPath::new("a.txt").unwrap())
            .await
            .unwrap()
            .unwrap()
    };
    let stale = IndexFile {
        entries: vec![IndexEntry {
            path: RepoPath::new("a.txt").unwrap(),
            mode: FileMode::File,
            oid: leaf.git,
            size: 4,
            ctime: StatTime::default(),
            mtime: StatTime { secs: 1, nsecs: 0 },
            dev: 0,
            ino: 0,
            uid: 0,
            gid: 0,
            flags: EntryFlags::default(),
        }],
        tree: None,
        mtime: Some(old),
    };
    write(&index_path, &stale).unwrap();
    let before = fs::read(&index_path).unwrap();
    let before_mtime = fs::metadata(&index_path).unwrap().modified().unwrap();

    let out = run(p.f.asc().env("GIT_OPTIONAL_LOCKS", "0").arg("status"));
    assert_eq!(out.code, 0, "{}", out.stderr);
    assert!(
        out.stdout.contains("nothing to commit, working tree clean"),
        "{}",
        out.stdout
    );
    assert_eq!(
        fs::read(&index_path).unwrap(),
        before,
        "GIT_OPTIONAL_LOCKS=0 leaves the index"
    );
    assert_eq!(
        fs::metadata(&index_path).unwrap().modified().unwrap(),
        before_mtime
    );

    let out = run(p.f.asc().args(["--no-optional-locks", "status"]));
    assert_eq!(out.code, 0, "{}", out.stderr);
    assert_eq!(
        fs::read(&index_path).unwrap(),
        before,
        "--no-optional-locks leaves the index"
    );

    let out = run(p.f.asc().arg("status"));
    assert_eq!(out.code, 0, "{}", out.stderr);
    assert_ne!(
        fs::read(&index_path).unwrap(),
        before,
        "the stat cache is refreshed"
    );
    let fresh = IndexFile::read(&index_path).unwrap();
    assert_eq!(fresh.entries[0].mtime.secs, 1_600_000_000);
}

#[tokio::test]
async fn shadowed_gitignore_warns_on_stderr() {
    let s = Scenario {
        head: vec![("a.txt", "one\n")],
        work: vec![
            (".gitignore", Some("*.tmp\n")),
            (".ascignore", Some("*.cache\n")),
        ],
        ..Scenario::default()
    };
    let p = Pair::new(&s).await;
    let out = p.asc(&[]);
    assert_eq!(out.code, 0);
    assert!(
        out.stderr
            .contains("warning: asc: .gitignore is not read because .ascignore exists"),
        "{}",
        out.stderr
    );
}

#[tokio::test]
async fn unsupported_forms_are_refused_by_name() {
    let s = Scenario {
        head: vec![("a.txt", "x\n")],
        ..Scenario::default()
    };
    let p = Pair::new(&s).await;
    let out = p.asc(&["--ignored=matching"]);
    assert_ne!(out.code, 0);
    assert!(out.stderr.contains("--ignored=matching"), "{}", out.stderr);
    let out = p.asc(&["--porcelain=v3"]);
    assert_eq!(out.code, 128);
    assert_eq!(out.stderr, "fatal: unsupported porcelain version 'v3'\n");
    let out = p.asc(&["--untracked-files=bogus"]);
    assert_eq!(out.code, 128);
    assert_eq!(out.stderr, "fatal: Invalid untracked files mode 'bogus'\n");
}

fn unhex(hex: &str) -> Vec<u8> {
    (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
        .collect()
}

/// Standard base64 with padding (the JSON profile's encoding of plain `bytes`).
fn b64(data: &[u8]) -> String {
    const A: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in data.chunks(3) {
        let n = chunk
            .iter()
            .enumerate()
            .fold(0u32, |n, (i, &b)| n | u32::from(b) << (16 - 8 * i));
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(A[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
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

/// A tracked path behind a linked directory is outside the work tree (git's
/// has_symlink_leading_path): it reads as deleted and no mode is taken from beyond the link.
#[tokio::test]
async fn tracked_path_behind_a_linked_directory_reads_as_deleted() {
    let f = Fixture::new().await;
    let a = f.commit(&[], &[("d/e.txt", b"deep\n")]).await;
    f.set_branch("master", a).await;
    f.checkout_branch("master", Some(a)).await;
    let outside = f.sandbox.dir("outside");
    fs::write(outside.join("e.txt"), "deep\n").unwrap();
    assert!(
        link_dir(&outside, &f.root.join("d")),
        "cannot make a directory link here"
    );
    let out = run(f.asc().args(["status", "--porcelain=v2"]));
    assert_eq!(out.code, 0, "{}", out.stderr);
    let line = out
        .stdout
        .lines()
        .find(|l| l.ends_with(" d/e.txt"))
        .expect("d/e.txt is listed");
    let fields: Vec<&str> = line.split(' ').collect();
    assert_eq!(fields[1], ".D", "{line}");
    assert_eq!(fields[5], "000000", "no mode from beyond the link: {line}");
    assert_eq!(fs::read(outside.join("e.txt")).unwrap(), b"deep\n");
}
