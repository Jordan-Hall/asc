//! U23 acceptance: `branch`, `switch`, `checkout` and `tag` through the built binary. Each
//! scenario builds one history twice, with the fixture for asc and with real git (the same trees,
//! messages, identities and dates, so every git id is equal), runs the same command on both and
//! compares exit code, stdout, stderr and the resulting working tree and status.

mod support;

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use asc_core::id::{ChangesetId, RefTarget};
use asc_core::path::RefName;
use asc_git::projection::Projection;
use asc_state::WorkspaceId;
use asc_state::state::repos;
use asc_state::state::stage::{self, StageRow, StageSet};
use asc_state::state::workspaces::{self, Mode, NewWorkspace};
use asc_store::refs::{Actor, Expect, RefTxn, RefUpdate, Refs};
use support::{Fixture, text};

/// Marks a test script executable (Unix runs a script only with the execute bit; Windows has none).
fn make_executable(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }
    #[cfg(not(unix))]
    let _ = path;
}

const IDENT: [(&str, &str); 6] = [
    ("GIT_AUTHOR_NAME", "Test"),
    ("GIT_AUTHOR_EMAIL", "test@example.com"),
    ("GIT_AUTHOR_DATE", "1700000000 +0000"),
    ("GIT_COMMITTER_NAME", "Test"),
    ("GIT_COMMITTER_EMAIL", "test@example.com"),
    ("GIT_COMMITTER_DATE", "1700000000 +0000"),
];

#[derive(Debug, Clone)]
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

/// asc's output with the program name in hints read as git's, as the compat suite normalises it.
fn norm(s: &str) -> String {
    let mut out = s.to_owned();
    for cmd in [
        "switch", "branch", "checkout", "config", "fetch", "push", "pull", "merge", "rebase",
        "reset", "help", "worktree",
    ] {
        out = out.replace(&format!("asc {cmd}"), &format!("git {cmd}"));
    }
    out.replace("\"asc ", "\"git ").replace("'asc ", "'git ")
}

fn put(root: &Path, rel: &str, content: &str) {
    let path = root.join(rel);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, content).unwrap();
}

fn read(root: &Path, rel: &str) -> String {
    fs::read_to_string(root.join(rel)).unwrap_or_else(|_| "<missing>".into())
}

/// Every file below `dir` except the git metadata.
fn files_of(dir: &Path) -> BTreeMap<String, String> {
    fn walk(base: &Path, dir: &Path, out: &mut BTreeMap<String, String>) {
        for e in fs::read_dir(dir).unwrap() {
            let e = e.unwrap();
            let path = e.path();
            let rel = path
                .strip_prefix(base)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/");
            if rel == ".git" {
                continue;
            }
            if path.is_dir() {
                walk(base, &path, out);
            } else {
                out.insert(rel, fs::read_to_string(&path).unwrap_or_default());
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(dir, dir, &mut out);
    out
}

/// The same history in the fixture (asc) and in a git repository.
struct World {
    f: Fixture,
    git: PathBuf,
    ids: HashMap<String, ChangesetId>,
    shas: HashMap<String, String>,
    files: HashMap<String, BTreeMap<String, String>>,
}

impl World {
    async fn new() -> Self {
        let f = Fixture::new().await;
        let git = f.sandbox.dir("gitrepo");
        let w = Self {
            f,
            git,
            ids: HashMap::new(),
            shas: HashMap::new(),
            files: HashMap::new(),
        };
        assert_eq!(w.g(&["init", "-q", "-b", "master"]).code, 0);
        w
    }

    fn gcmd(&self) -> Command {
        let mut cmd = Command::new("git"); // lint:allow-raw-git
        cmd.current_dir(&self.git)
            .env("HOME", &self.f.sandbox.user)
            .env("USERPROFILE", &self.f.sandbox.user)
            .env("GIT_CONFIG_GLOBAL", self.f.sandbox.user.join(".gitconfig"))
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_EDITOR", ":")
            .env("LC_ALL", "C")
            .args(["-c", "core.autocrlf=false"]);
        for (k, v) in IDENT {
            cmd.env(k, v);
        }
        cmd
    }

    fn g(&self, args: &[&str]) -> Out {
        run(self.gcmd().args(args))
    }

    fn cmd(&self) -> Command {
        let mut cmd = self.f.asc();
        cmd.env("LC_ALL", "C").env("GIT_EDITOR", ":");
        for (k, v) in IDENT {
            cmd.env(k, v);
        }
        cmd
    }

    fn asc(&self, args: &[&str]) -> Out {
        run(self.cmd().args(args))
    }

    /// A commit in both repositories; the ids must agree.
    async fn commit(&mut self, name: &str, parent: Option<&str>, files: &[(&str, &str)]) {
        let parents: Vec<ChangesetId> = parent.map(|p| self.ids[p]).into_iter().collect();
        let owned: Vec<(&str, &[u8])> = files.iter().map(|(p, c)| (*p, c.as_bytes())).collect();
        let id = self.f.commit(&parents, &owned).await;
        let mut content = parent.map(|p| self.files[p].clone()).unwrap_or_default();
        match parent {
            Some(p) => assert_eq!(
                self.g(&["checkout", "-q", "--detach", &self.shas[p].clone()])
                    .code,
                0
            ),
            None => {
                assert_eq!(self.g(&["checkout", "-q", "--orphan", "root"]).code, 0);
                assert_eq!(self.g(&["rm", "-rfq", "--ignore-unmatch", "."]).code, 0);
            }
        }
        let root_commit = parent.is_none();
        for (p, c) in files {
            put(&self.git, p, c);
            content.insert((*p).to_owned(), (*c).to_owned());
            assert_eq!(self.g(&["add", p]).code, 0);
        }
        let msg = format!("commit of {} files", files.len());
        let done = self.g(&["commit", "-q", "-m", &msg]);
        assert_eq!(done.code, 0, "{}", done.stderr);
        let sha = self.g(&["rev-parse", "HEAD"]).stdout.trim().to_owned();
        if root_commit {
            // The scaffolding branch is no part of the scenario.
            assert_eq!(self.g(&["checkout", "-q", "--detach"]).code, 0);
            assert_eq!(self.g(&["branch", "-q", "-D", "root"]).code, 0);
        }
        let ours = self.f.engine.git_commit_of(id).await.unwrap().to_string();
        assert_eq!(
            ours, sha,
            "the fixture and git disagree on the commit {name}"
        );
        self.ids.insert(name.to_owned(), id);
        self.shas.insert(name.to_owned(), sha);
        self.files.insert(name.to_owned(), content);
    }

    async fn branch(&self, name: &str, at: &str) {
        self.f.set_branch(name, self.ids[at]).await;
        assert_eq!(self.g(&["branch", "-f", name, &self.shas[at]]).code, 0);
    }

    /// Sets a ref (outside `refs/heads`) in both repositories.
    async fn set_ref(&self, name: &str, at: &str) {
        let txn = RefTxn {
            updates: vec![RefUpdate {
                name: RefName::new(name.as_bytes().to_vec()).unwrap(),
                expect: Expect::Any,
                new: Some(RefTarget::Changeset(self.ids[at])),
                expires_at: None,
            }],
            actor: Actor {
                id: "fixture".into(),
                via: "test".into(),
            },
            reason: "fixture".into(),
            op_id: uuid::Uuid::now_v7(),
        };
        Refs::update(&self.f.db.refs(), self.f.repo, &txn)
            .await
            .unwrap();
        assert_eq!(self.g(&["update-ref", name, &self.shas[at]]).code, 0);
    }

    /// Checks `branch` out at commit `at` in both, with the files written.
    async fn start(&self, branch: &str, at: &str) {
        self.f.checkout_branch(branch, Some(self.ids[at])).await;
        for (p, c) in &self.files[at] {
            put(&self.f.root, p, c);
        }
        let r = self.g(&["checkout", "-q", "-f", branch]);
        assert_eq!(r.code, 0, "{}", r.stderr);
        // The set-up moves are not part of the scenario: `-` starts without a previous position.
        assert_eq!(
            self.g(&["reflog", "expire", "--expire=now", "--all"]).code,
            0
        );
    }

    /// Runs `args` on both and asserts the same exit code and output.
    fn same(&self, args: &[&str]) -> (Out, Out) {
        let ours = self.asc(args);
        let theirs = self.g(args);
        let ours = Out {
            stdout: norm(&ours.stdout),
            stderr: norm(&ours.stderr),
            ..ours
        };
        assert_eq!(
            (ours.code, ours.stdout.as_str(), ours.stderr.as_str()),
            (theirs.code, theirs.stdout.as_str(), theirs.stderr.as_str()),
            "`{}` differs from git",
            args.join(" ")
        );
        (ours, theirs)
    }

    /// The working tree, `status --short --branch` and HEAD agree with git.
    fn same_state(&self) {
        assert_eq!(
            files_of(&self.f.root),
            files_of(&self.git),
            "working trees differ"
        );
        let (s, g) = (
            self.asc(&["status", "--short", "--branch"]),
            self.g(&["status", "--short", "--branch"]),
        );
        assert_eq!(s.stdout, g.stdout, "status differs");
        let (h, gh) = (
            self.asc(&["rev-parse", "HEAD"]),
            self.g(&["rev-parse", "HEAD"]),
        );
        assert_eq!(h.stdout, gh.stdout, "HEAD differs");
        let (b, gb) = (
            self.asc(&["branch", "--show-current"]),
            self.g(&["branch", "--show-current"]),
        );
        assert_eq!(b.stdout, gb.stdout, "current branch differs");
    }
}

/// `a`: f.txt "one", g.txt "keep"; `b`: f.txt "two", h.txt "new"; master at `b`, topic at `a`,
/// master checked out.
async fn two() -> World {
    let mut w = World::new().await;
    w.commit("a", None, &[("f.txt", "one\n"), ("g.txt", "keep\n")])
        .await;
    w.commit("b", Some("a"), &[("f.txt", "two\n"), ("h.txt", "new\n")])
        .await;
    w.branch("master", "b").await;
    w.branch("topic", "a").await;
    w.start("master", "b").await;
    w
}

mod switching {
    use super::*;

    #[tokio::test]
    async fn switch_to_a_branch_and_back_like_git() {
        let w = two().await;
        w.same(&["switch", "topic"]);
        assert_eq!(read(&w.f.root, "f.txt"), "one\n");
        assert_eq!(read(&w.f.root, "h.txt"), "<missing>");
        w.same_state();
        w.same(&["switch", "topic"]);
        w.same(&["switch", "-"]);
        w.same_state();
        w.same(&["switch", "-"]);
        w.same(&["checkout", "-"]);
        w.same(&["checkout", "master"]);
        w.same_state();
    }

    #[tokio::test]
    async fn dash_without_a_previous_position_is_git_s_error() {
        let w = two().await;
        w.same(&["switch", "-"]);
        w.same(&["switch", "nope"]);
        w.same(&["switch"]);
        w.same(&["switch", "master", "topic"]);
        w.same(&["switch", "HEAD~1"]);
        w.same_state();
    }

    #[tokio::test]
    async fn create_and_reset_branches_like_git() {
        let w = two().await;
        w.same(&["switch", "-c", "n1"]);
        w.same(&["switch", "-c", "n1"]);
        w.same(&["switch", "-c", "n2", "topic"]);
        w.same_state();
        w.same(&["switch", "-C", "n1", "master"]);
        w.same(&["switch", "-C", "n1"]);
        w.same(&["switch", "-c", "bad name"]);
        w.same_state();
        w.same(&["checkout", "-b", "n3"]);
        w.same(&["checkout", "-B", "n3", "topic"]);
        w.same(&["checkout", "-B", "n3"]);
        w.same_state();
        let (asc, git) = (w.asc(&["reflog", "n3"]), w.g(&["reflog", "n3"]));
        assert_eq!(asc.stdout.lines().next(), git.stdout.lines().next());
        w.same(&["branch", "-v"]);
    }

    #[tokio::test]
    async fn a_local_change_in_the_way_is_refused_and_nothing_is_written() {
        let w = two().await;
        put(&w.f.root, "f.txt", "two\nlocal\n");
        put(&w.git, "f.txt", "two\nlocal\n");
        w.same(&["switch", "topic"]);
        w.same(&["checkout", "topic"]);
        assert_eq!(read(&w.f.root, "f.txt"), "two\nlocal\n");
        assert_eq!(
            read(&w.f.root, "h.txt"),
            "new\n",
            "no other path was touched"
        );
        w.same_state();
        // A change at a path the move does not touch is carried across.
        put(&w.f.root, "f.txt", "two\n");
        put(&w.git, "f.txt", "two\n");
        put(&w.f.root, "g.txt", "keep\nedited\n");
        put(&w.git, "g.txt", "keep\nedited\n");
        w.same(&["switch", "topic"]);
        w.same_state();
    }

    #[tokio::test]
    async fn an_untracked_file_in_the_way_is_refused() {
        let w = two().await;
        w.same(&["switch", "topic"]);
        put(&w.f.root, "h.txt", "mine\n");
        put(&w.git, "h.txt", "mine\n");
        w.same(&["switch", "master"]);
        w.same(&["checkout", "master"]);
        assert_eq!(read(&w.f.root, "h.txt"), "mine\n");
        assert_eq!(read(&w.f.root, "f.txt"), "one\n");
        w.same_state();
        // Both kinds at once, and a directory where a file goes.
        put(&w.f.root, "f.txt", "one\nedited\n");
        put(&w.git, "f.txt", "one\nedited\n");
        w.same(&["switch", "master"]);
        w.same_state();
    }

    #[tokio::test]
    async fn a_staged_change_is_refused_when_the_target_differs_and_kept_when_it_does_not() {
        let w = two().await;
        put(&w.f.root, "f.txt", "two\nstaged\n");
        put(&w.git, "f.txt", "two\nstaged\n");
        assert_eq!(w.asc(&["add", "f.txt"]).code, 0);
        assert_eq!(w.g(&["add", "f.txt"]).code, 0);
        w.same(&["switch", "topic"]);
        w.same_state();
        // A staged change at a path both sides share survives the move.
        w.same(&["reset", "-q", "--hard"]);
        w.same_state();
        put(&w.f.root, "g.txt", "keep\nstaged\n");
        put(&w.git, "g.txt", "keep\nstaged\n");
        assert_eq!(w.asc(&["add", "g.txt"]).code, 0);
        assert_eq!(w.g(&["add", "g.txt"]).code, 0);
        w.same(&["switch", "topic"]);
        w.same_state();
        w.same(&["switch", "master"]);
        w.same_state();
    }

    #[tokio::test]
    async fn force_discards_local_changes_and_untracked_blockers() {
        let w = two().await;
        put(&w.f.root, "f.txt", "two\nlocal\n");
        put(&w.git, "f.txt", "two\nlocal\n");
        w.same(&["switch", "-f", "topic"]);
        w.same_state();
        put(&w.f.root, "h.txt", "mine\n");
        put(&w.git, "h.txt", "mine\n");
        w.same(&["checkout", "-f", "master"]);
        w.same_state();
        put(&w.f.root, "g.txt", "keep\nstaged\n");
        put(&w.git, "g.txt", "keep\nstaged\n");
        assert_eq!(w.asc(&["add", "g.txt"]).code, 0);
        assert_eq!(w.g(&["add", "g.txt"]).code, 0);
        w.same(&["switch", "--discard-changes", "topic"]);
        w.same_state();
        w.same(&["switch", "-q", "master"]);
    }

    #[tokio::test]
    async fn detaching_prints_git_s_advice_and_leaving_warns_about_lost_commits() {
        let w = two().await;
        w.same(&["checkout", "HEAD~1"]);
        w.same_state();
        w.same(&["switch", "--detach", "master"]);
        w.same(&["checkout", "--detach", "topic"]);
        w.same(&["switch", "master"]);
        w.same(&["switch", "-q", "--detach", "topic"]);
        w.same_state();
        // A commit made on the detached HEAD that no ref reaches.
        put(&w.f.root, "g.txt", "keep\nmore\n");
        put(&w.git, "g.txt", "keep\nmore\n");
        assert_eq!(
            w.asc(&["commit", "-qam", "detached work"]).code,
            0,
            "asc commit"
        );
        assert_eq!(w.g(&["commit", "-qam", "detached work"]).code, 0);
        let (c, g) = (w.asc(&["switch", "master"]), w.g(&["switch", "master"]));
        assert_eq!(c.code, g.code);
        assert!(
            c.stderr
                .starts_with("Warning: you are leaving 1 commit behind"),
            "{}",
            c.stderr
        );
        assert!(c.stderr.contains("detached work"), "{}", c.stderr);
        assert!(c.stderr.contains("Switched to branch 'master'"));
        assert_eq!(norm(&c.stderr).lines().count(), g.stderr.lines().count());
    }

    #[tokio::test]
    async fn orphan_branches_differ_between_switch_and_checkout() {
        let w = two().await;
        w.same(&["switch", "--orphan", "o1"]);
        w.same_state();
        let w = two().await;
        w.same(&["checkout", "--orphan", "o2"]);
        w.same_state();
        w.same(&["status", "--short"]);
    }

    #[tokio::test]
    async fn switching_is_refused_in_the_middle_of_a_merge_and_with_unmerged_paths() {
        let w = two().await;
        let git_dir = Projection::open(&w.f.root).unwrap().git_dir().to_owned();
        fs::write(git_dir.join("MERGE_HEAD"), format!("{}\n", w.shas["a"])).unwrap();
        let c = w.asc(&["switch", "topic"]);
        assert_eq!(c.code, 128);
        assert_eq!(
            norm(&c.stderr),
            "fatal: cannot switch branch while merging\nConsider \"git merge --quit\" or \"git worktree add\".\n"
        );
        // `checkout` only objects to unmerged entries.
        let leaf = w.f.file_leaf(w.ids["b"], "f.txt").await;
        w.f.stage(&[StageRow {
            path: b"f.txt".to_vec(),
            stage: 2,
            kind: leaf.kind,
            file: Some(leaf.file),
            size: leaf.size,
            git: Some(leaf.git),
            intent_to_add: false,
        }])
        .await;
        let c = w.asc(&["checkout", "topic"]);
        assert_eq!(
            (c.code, c.stderr.as_str()),
            (1, "error: you need to resolve your current index first\n")
        );
        assert_eq!(read(&w.f.root, "f.txt"), "two\n");
        assert!(
            !stage::overlay(&w.f.db, w.f.ws, StageSet::Default)
                .await
                .unwrap()
                .is_empty()
        );
    }
}

mod odds_and_ends {
    use super::*;

    #[tokio::test]
    async fn head_and_non_branch_names_behave_as_in_git() {
        let w = two().await;
        w.same(&["checkout", "HEAD"]);
        w.same(&["checkout", "@"]);
        w.same(&["switch", "HEAD"]);
        w.same(&["switch", "refs/heads/master"]);
        w.same(&["switch", "origin/feat"]);
        w.same(&["tag", "vv", "HEAD~1"]);
        w.same(&["switch", "vv"]);
        w.same(&["checkout", "vv"]);
        w.same_state();
        w.same(&["switch", "master"]);
        w.same(&["checkout", "refs/heads/topic"]);
        w.same_state();
        w.same(&["checkout", "master"]);
        w.same(&["checkout", "-q", "topic"]);
        w.same(&["switch", "-q", "-c", "quiet1"]);
        w.same_state();
    }

    #[tokio::test]
    async fn an_unborn_repository_behaves_as_in_git() {
        let f = Fixture::new().await;
        let w = World {
            git: f.sandbox.dir("gitrepo"),
            f,
            ids: HashMap::new(),
            shas: HashMap::new(),
            files: HashMap::new(),
        };
        assert_eq!(w.g(&["init", "-q", "-b", "master"]).code, 0);
        for args in [
            vec!["switch", "master"],
            vec!["checkout", "master"],
            vec!["branch", "w"],
            vec!["tag", "t1"],
            vec!["tag", "-a", "t2", "-m", "m"],
            vec!["branch"],
            vec!["tag"],
            vec!["switch", "-c", "x"],
            vec!["checkout", "-b", "y"],
            vec!["switch", "--orphan", "z"],
            vec!["branch", "--show-current"],
        ] {
            w.same(&args);
        }
        w.same_state();
    }

    #[tokio::test]
    async fn files_and_directories_swap_places_across_a_switch() {
        // A tracked file that becomes a directory on another branch.
        let mut w2 = World::new().await;
        w2.commit(
            "a",
            None,
            &[(
                "d", "file
",
            )],
        )
        .await;
        w2.commit(
            "b",
            None,
            &[(
                "d/inner", "in
",
            )],
        )
        .await;
        w2.branch("master", "b").await;
        w2.branch("old", "a").await;
        w2.start("master", "b").await;
        w2.same(&["switch", "old"]);
        w2.same_state();
        w2.same(&["switch", "master"]);
        w2.same_state();
    }

    #[tokio::test]
    async fn paths_are_relative_to_the_current_directory() {
        let mut w = World::new().await;
        w.commit(
            "a",
            None,
            &[
                (
                    "d/x.txt", "x1
",
                ),
                (
                    "z.txt", "z1
",
                ),
            ],
        )
        .await;
        w.commit(
            "b",
            Some("a"),
            &[
                (
                    "d/x.txt", "x2
",
                ),
                (
                    "z.txt", "z2
",
                ),
            ],
        )
        .await;
        w.branch("master", "b").await;
        w.branch("old", "a").await;
        w.start("master", "b").await;
        let (c, g) = (
            run(w
                .cmd()
                .current_dir(w.f.root.join("d"))
                .args(["checkout", "old", "--", "x.txt"])),
            run(w
                .gcmd()
                .current_dir(w.git.join("d"))
                .args(["checkout", "old", "--", "x.txt"])),
        );
        assert_eq!((c.code, c.stdout, c.stderr), (g.code, g.stdout, g.stderr));
        w.same_state();
        let (c, g) = (
            run(w
                .cmd()
                .current_dir(w.f.root.join("d"))
                .args(["checkout", "master", "../z.txt"])),
            run(w
                .gcmd()
                .current_dir(w.git.join("d"))
                .args(["checkout", "master", "../z.txt"])),
        );
        assert_eq!((c.code, c.stdout, c.stderr), (g.code, g.stdout, g.stderr));
        w.same_state();
    }
}

mod remotes_and_tracking {
    use super::*;

    async fn tracked() -> World {
        let w = two().await;
        w.set_ref("refs/remotes/origin/feat", "a").await;
        w.set_ref("refs/remotes/origin/master", "a").await;
        for args in [
            vec!["config", "remote.origin.url", "https://example.com/r.git"],
            vec![
                "config",
                "remote.origin.fetch",
                "+refs/heads/*:refs/remotes/origin/*",
            ],
            vec!["config", "branch.master.remote", "origin"],
            vec!["config", "branch.master.merge", "refs/heads/master"],
        ] {
            assert_eq!(w.asc(&args).code, 0, "asc {args:?}");
            assert_eq!(w.g(&args).code, 0, "git {args:?}");
        }
        w
    }

    #[tokio::test]
    async fn a_remote_branch_name_creates_a_tracking_branch() {
        let w = tracked().await;
        w.same(&["checkout", "feat"]);
        w.same_state();
        for key in ["branch.feat.remote", "branch.feat.merge"] {
            assert_eq!(
                w.asc(&["config", key]).stdout,
                w.g(&["config", key]).stdout,
                "{key}"
            );
        }
        w.same(&["switch", "master"]);
        w.same(&["branch", "-D", "feat"]);
        w.same(&["switch", "feat"]);
        w.same_state();
        w.same(&["switch", "--no-guess", "master"]);
        w.same(&["branch", "-D", "feat"]);
        w.same(&["switch", "--no-guess", "feat"]);
        w.same(&["checkout", "--no-guess", "feat"]);
    }

    #[tokio::test]
    async fn switching_reports_how_the_branch_stands_against_its_upstream() {
        let w = tracked().await;
        w.same(&["switch", "master"]);
        w.same(&["switch", "topic"]);
        w.same(&["switch", "master"]);
        w.same(&["checkout"]);
        w.set_ref("refs/remotes/origin/master", "b").await;
        w.same(&["switch", "topic"]);
        w.same(&["switch", "master"]);
        w.same(&["switch", "-q", "master"]);
    }

    #[tokio::test]
    async fn track_and_no_track_on_create() {
        let w = tracked().await;
        w.same(&["switch", "-c", "t1", "origin/feat"]);
        w.same(&["switch", "-c", "t2", "--no-track", "origin/feat"]);
        w.same(&["switch", "-c", "t3", "--track", "master"]);
        w.same(&["switch", "-c", "t4", "-t", "origin/master"]);
        for key in [
            "branch.t1.merge",
            "branch.t2.merge",
            "branch.t3.remote",
            "branch.t4.remote",
        ] {
            assert_eq!(
                w.asc(&["config", key]).stdout,
                w.g(&["config", key]).stdout,
                "{key}"
            );
        }
        w.same(&["branch", "-vv"]);
        w.same(&["branch", "-a"]);
        w.same(&["branch", "-r"]);
    }
}

mod worktrees_and_hooks {
    use super::*;

    /// A second workspace of the same repository that has `branch` checked out.
    async fn hold(w: &World, branch: &str) -> String {
        let repo = repos::get(&w.f.db, w.f.repo)
            .await
            .unwrap()
            .unwrap()
            .repo
            .id;
        let root = w.f.sandbox.dir("other");
        let id = WorkspaceId::new();
        Projection::create(&w.f.home, repo, id, &root).unwrap();
        let path = root.display().to_string();
        workspaces::insert(
            &w.f.db,
            &NewWorkspace {
                id,
                repo: w.f.repo,
                path: path.clone(),
                mount: path.clone(),
                mode: Mode::Plain,
                head: format!("refs/heads/{branch}"),
                base_cs: Some(w.ids["a"]),
                sparse: None,
                ephemeral_until: None,
            },
        )
        .await
        .unwrap();
        path.replace('\\', "/")
    }

    #[tokio::test]
    async fn a_branch_held_by_another_workspace_is_refused_as_git_refuses_it() {
        let w = two().await;
        let path = hold(&w, "topic").await;
        for (args, want) in [
            (
                vec!["switch", "topic"],
                format!("fatal: 'topic' is already used by worktree at '{path}'\n"),
            ),
            (
                vec!["checkout", "topic"],
                format!("fatal: 'topic' is already used by worktree at '{path}'\n"),
            ),
            (
                vec!["switch", "-C", "topic"],
                format!(
                    "fatal: cannot force update the branch 'topic' used by worktree at '{path}'\n"
                ),
            ),
            (
                vec!["branch", "-d", "topic"],
                format!("error: cannot delete branch 'topic' used by worktree at '{path}'\n"),
            ),
            (
                vec!["branch", "-D", "topic"],
                format!("error: cannot delete branch 'topic' used by worktree at '{path}'\n"),
            ),
            (
                vec!["branch", "-f", "topic", "master"],
                format!(
                    "fatal: cannot force update the branch 'topic' used by worktree at '{path}'\n"
                ),
            ),
            (
                vec!["branch", "-m", "topic", "t2"],
                format!(
                    "fatal: cannot force update the branch 'topic' used by worktree at '{path}'\n"
                ),
            ),
        ] {
            let out = w.asc(&args);
            assert_ne!(out.code, 0, "{args:?}");
            assert_eq!(out.stderr, want, "{args:?}");
        }
        // The current branch is in use by this workspace itself.
        let own = w.f.root.display().to_string().replace('\\', "/");
        let out = w.asc(&["branch", "-d", "master"]);
        assert_eq!(
            out.stderr,
            format!("error: cannot delete branch 'master' used by worktree at '{own}'\n")
        );
        // `--ignore-other-worktrees` is git's escape hatch.
        let out = w.asc(&["switch", "--ignore-other-worktrees", "topic"]);
        assert_eq!(out.code, 0, "{}", out.stderr);
    }

    fn install_hook(w: &World, name: &str, body: &str) {
        let path = Projection::open(&w.f.root)
            .unwrap()
            .common_dir()
            .join("hooks")
            .join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        }
    }

    fn log_path(w: &World) -> String {
        w.f.sandbox
            .tmp
            .path()
            .join("hook.log")
            .display()
            .to_string()
            .replace('\\', "/")
    }

    #[tokio::test]
    async fn post_checkout_gets_old_new_and_the_branch_flag_and_its_status_is_the_commands() {
        let w = two().await;
        let log = log_path(&w);
        install_hook(
            &w,
            "post-checkout",
            &format!("echo \"$1 $2 $3\" >> \"{log}\"\nexit 3"),
        );
        let out = w.asc(&["switch", "topic"]);
        assert_eq!(
            out.code, 3,
            "the hook's status is the command's: {}",
            out.stderr
        );
        assert_eq!(
            read(&w.f.root, "f.txt"),
            "one\n",
            "the switch itself happened"
        );
        let out = w.asc(&["checkout", "master", "--", "f.txt"]);
        assert_eq!(out.code, 3);
        let want = format!(
            "{} {} 1\n{} {} 0\n",
            w.shas["b"], w.shas["a"], w.shas["a"], w.shas["a"]
        );
        assert_eq!(fs::read_to_string(log).unwrap().replace("\r\n", "\n"), want);
    }

    #[tokio::test]
    async fn reference_transaction_sees_a_created_branch_and_can_veto_it() {
        let w = two().await;
        let log = log_path(&w);
        install_hook(
            &w,
            "reference-transaction",
            &format!("echo \"$1\" >> \"{log}\"\ncat >> \"{log}\""),
        );
        assert_eq!(w.asc(&["branch", "n1"]).code, 0);
        let line = format!("{} {} refs/heads/n1\n", "0".repeat(40), w.shas["b"]);
        assert_eq!(
            fs::read_to_string(&log).unwrap().replace("\r\n", "\n"),
            format!("prepared\n{line}committed\n{line}")
        );
        install_hook(
            &w,
            "reference-transaction",
            "cat > /dev/null\nif [ \"$1\" = prepared ]; then exit 1; fi",
        );
        let out = w.asc(&["switch", "-c", "n2"]);
        assert_ne!(out.code, 0);
        // The refused move leaves HEAD where it was.
        assert_eq!(w.asc(&["branch", "--show-current"]).stdout, "master\n");
        assert_eq!(
            w.asc(&["rev-parse", "--verify", "-q", "refs/heads/n2"])
                .code,
            1
        );
        assert_eq!(read(&w.f.root, "f.txt"), "two\n");
    }
}

mod paths {
    use super::*;

    #[tokio::test]
    async fn checkout_paths_from_the_index_and_from_a_tree_like_git() {
        let w = two().await;
        put(&w.f.root, "f.txt", "two\nlocal\n");
        put(&w.git, "f.txt", "two\nlocal\n");
        w.same(&["checkout", "f.txt"]);
        w.same(&["checkout", "f.txt"]);
        w.same_state();
        put(&w.f.root, "f.txt", "two\nlocal\n");
        put(&w.git, "f.txt", "two\nlocal\n");
        w.same(&["checkout", "--", "f.txt"]);
        w.same(&["checkout", "topic", "--", "f.txt"]);
        w.same_state();
        w.same(&["checkout", "master", "f.txt", "h.txt"]);
        w.same_state();
        w.same(&["checkout", "HEAD~1", "f.txt", "g.txt"]);
        w.same_state();
        w.same(&["checkout", "HEAD", "--", "f.txt"]);
        w.same_state();
        w.same(&["checkout", "nope.txt"]);
        w.same(&["checkout", "--", "nope.txt"]);
        w.same(&["checkout", "master", "--", "nope.txt"]);
        w.same(&["checkout", "-q", "topic", "g.txt"]);
        w.same(&["checkout", "--"]);
        w.same_state();
    }

    #[tokio::test]
    async fn checkout_paths_in_a_subdirectory_and_with_a_directory() {
        let mut w = World::new().await;
        w.commit(
            "a",
            None,
            &[("d/x.txt", "x1\n"), ("d/y.txt", "y1\n"), ("z.txt", "z\n")],
        )
        .await;
        w.commit("b", Some("a"), &[("d/x.txt", "x2\n"), ("d/y.txt", "y2\n")])
            .await;
        w.branch("master", "b").await;
        w.branch("old", "a").await;
        w.start("master", "b").await;
        w.same(&["checkout", "old", "--", "d"]);
        w.same_state();
        put(&w.f.root, "d/x.txt", "dirty\n");
        put(&w.git, "d/x.txt", "dirty\n");
        w.same(&["checkout", "d"]);
        w.same_state();
    }
}

mod branches {
    use super::*;

    async fn many() -> World {
        let mut w = two().await;
        w.commit("c", Some("b"), &[("i.txt", "more\n")]).await;
        w.branch("feature/x", "c").await;
        w.branch("zed", "a").await;
        w.set_ref("refs/remotes/origin/master", "b").await;
        w.set_ref("refs/remotes/origin/dev", "a").await;
        // Committing moved git's HEAD; put both back on master.
        w.start("master", "b").await;
        w
    }

    #[tokio::test]
    async fn listing_matches_git() {
        let w = many().await;
        for args in [
            vec!["branch"],
            vec!["branch", "--list"],
            vec!["branch", "-v"],
            vec!["branch", "-vv"],
            vec!["branch", "-a"],
            vec!["branch", "-r"],
            vec!["branch", "-av"],
            vec!["branch", "--list", "z*"],
            vec!["branch", "--list", "feature/*", "t*"],
            vec!["branch", "-l", "nomatch"],
            vec!["branch", "--contains", "a"],
            vec!["branch", "--contains"],
            vec!["branch", "--no-contains", "b"],
            vec!["branch", "--merged", "master"],
            vec!["branch", "--no-merged", "master"],
            vec!["branch", "--merged"],
            vec!["branch", "--points-at", "master"],
            vec!["branch", "--sort=-refname"],
            vec!["branch", "--sort=objectname", "-v"],
            vec![
                "branch",
                "--format=%(refname:short) %(objectname:short) %(HEAD)",
            ],
            vec!["branch", "-a", "--format=%(refname)"],
            vec!["branch", "-v", "--abbrev=10"],
            vec!["branch", "-v", "--no-abbrev"],
            vec!["branch", "--show-current"],
            vec!["branch", "-i", "--list", "ZED"],
            vec!["branch", "--no-color"],
        ] {
            w.same(&args);
        }
    }

    #[tokio::test]
    async fn colour_matches_git() {
        let w = many().await;
        w.same(&["branch", "--color=always"]);
        w.same(&["branch", "--color=always", "-a"]);
    }

    #[tokio::test]
    async fn listing_when_detached_shows_the_head_line() {
        let w = many().await;
        w.same(&["switch", "-q", "--detach", "a"]);
        w.same(&["branch"]);
        w.same(&["branch", "-v"]);
        w.same(&["branch", "--show-current"]);
    }

    #[tokio::test]
    async fn create_delete_rename_and_copy_match_git() {
        let w = many().await;
        for args in [
            vec!["branch", "x1"],
            vec!["branch", "x1"],
            vec!["branch", "x2", "topic"],
            vec!["branch", "bad..name"],
            vec!["branch", "x3", "nope"],
            vec!["branch", "-f", "x2", "master"],
            vec!["branch", "-d", "x1"],
            vec!["branch", "-d", "nope"],
            vec!["branch", "-d", "x2", "nope", "zed"],
            vec!["branch", "-D", "feature/x"],
            vec!["branch", "-d"],
            vec!["branch", "-m", "topic", "t2"],
            vec!["branch", "-m", "t2", "master"],
            vec!["branch", "-m", "nope", "z"],
            vec!["branch", "-M", "t2", "topic"],
            vec!["branch", "-m", "master", "main2"],
            vec!["branch", "-m", "main2", "master"],
            vec!["branch", "-m"],
            vec!["branch", "-c", "master", "cp1"],
            vec!["branch", "-c", "master", "cp1"],
            vec!["branch", "-C", "zed", "cp1"],
            vec!["branch", "-c", "nope", "cp2"],
            vec!["branch"],
        ] {
            w.same(&args);
        }
        w.same_state();
        // The current branch is in use by this workspace; git names its work tree.
        let own = w.f.root.display().to_string().replace('\\', "/");
        for args in [
            vec!["branch", "-d", "master"],
            vec!["branch", "-D", "master"],
        ] {
            let out = w.asc(&args);
            assert_eq!(
                (out.code, out.stderr),
                (
                    1,
                    format!(
                        "error: cannot delete branch 'master' used by worktree at '{own}'
"
                    )
                )
            );
        }
        let out = w.asc(&["branch", "-f", "master", "topic"]);
        assert_eq!(
            (out.code, out.stderr),
            (
                128,
                format!(
                    "fatal: cannot force update the branch 'master' used by worktree at '{own}'
"
                )
            )
        );
    }

    #[tokio::test]
    async fn deleting_checks_that_the_branch_is_merged() {
        let w = many().await;
        // `feature/x` is ahead of master; `zed` is behind it.
        w.same(&["branch", "-d", "feature/x"]);
        w.same(&["branch", "-d", "zed"]);
        w.same(&["branch", "-d", "-r", "origin/dev"]);
        w.same(&["branch", "-d", "-r", "origin/nope"]);
        w.same(&["branch", "-a"]);
    }

    #[tokio::test]
    async fn deleting_with_an_upstream_warns_like_git() {
        let w = many().await;
        for args in [
            vec![
                "config",
                "remote.origin.fetch",
                "+refs/heads/*:refs/remotes/origin/*",
            ],
            vec!["config", "branch.zed.remote", "origin"],
            vec!["config", "branch.zed.merge", "refs/heads/dev"],
        ] {
            assert_eq!(w.asc(&args).code, 0);
            assert_eq!(w.g(&args).code, 0);
        }
        w.same(&["branch", "-d", "zed"]);
        w.same(&["config", "--get", "branch.zed.remote"]);
    }

    #[tokio::test]
    async fn renaming_the_current_branch_moves_head_and_the_branch_settings() {
        let w = many().await;
        for args in [
            vec!["config", "branch.master.remote", "origin"],
            vec!["config", "branch.master.merge", "refs/heads/master"],
        ] {
            assert_eq!(w.asc(&args).code, 0);
            assert_eq!(w.g(&args).code, 0);
        }
        w.same(&["branch", "-m", "trunk"]);
        w.same_state();
        w.same(&["config", "--get", "branch.trunk.remote"]);
        w.same(&["config", "--get", "branch.master.remote"]);
        w.same(&["branch", "-c", "trunk", "dup"]);
        w.same(&["config", "--get", "branch.dup.merge"]);
        let (c, g) = (w.asc(&["reflog", "trunk"]), w.g(&["reflog", "trunk"]));
        assert_eq!(c.stdout.lines().count() > 0, g.stdout.lines().count() > 0);
    }

    #[tokio::test]
    async fn an_unborn_branch_can_be_renamed() {
        let f = Fixture::new().await;
        let w = World {
            git: f.sandbox.dir("gitrepo"),
            f,
            ids: HashMap::new(),
            shas: HashMap::new(),
            files: HashMap::new(),
        };
        assert_eq!(w.g(&["init", "-q", "-b", "master"]).code, 0);
        w.same(&["branch", "-m", "trunk"]);
        w.same(&["branch", "--show-current"]);
        w.same(&["branch"]);
        let out = w.asc(&["branch", "-d", "trunk"]);
        let own = w.f.root.display().to_string().replace('\\', "/");
        assert_eq!(
            out.stderr,
            format!(
                "error: cannot delete branch 'trunk' used by worktree at '{own}'
"
            )
        );
    }

    #[tokio::test]
    async fn upstream_settings_match_git() {
        let w = many().await;
        let fetch = [
            "config",
            "remote.origin.fetch",
            "+refs/heads/*:refs/remotes/origin/*",
        ];
        assert_eq!(w.asc(&fetch).code, 0);
        assert_eq!(w.g(&fetch).code, 0);
        for args in [
            vec!["branch", "-u", "master"],
            vec!["branch", "-u", "origin/dev"],
            vec!["branch", "-u", "zed", "topic"],
            vec!["branch", "--set-upstream-to=origin/master", "topic"],
            vec!["branch", "-u", "nope"],
            vec!["branch", "-u", "origin/dev", "nobranch"],
            vec!["branch", "-vv"],
            vec!["branch", "--unset-upstream", "topic"],
            vec!["branch", "--unset-upstream", "topic"],
            vec!["branch", "--unset-upstream"],
            vec!["branch", "-vv"],
        ] {
            w.same(&args);
        }
        for key in [
            "branch.master.remote",
            "branch.master.merge",
            "branch.topic.remote",
        ] {
            assert_eq!(
                w.asc(&["config", key]).stdout,
                w.g(&["config", key]).stdout,
                "{key}"
            );
        }
    }
}

mod tags {
    use super::*;

    async fn tagged() -> World {
        let w = two().await;
        // Distinct tagger dates keep the listing order meaningful.
        w
    }

    #[tokio::test]
    async fn lightweight_and_annotated_tags_match_git() {
        let w = tagged().await;
        for args in [
            vec!["tag", "v1"],
            vec!["tag", "v1"],
            vec!["tag", "-a", "v2", "-m", "release two"],
            vec!["tag", "-a", "-m", "multi\nline\n\nbody", "v10", "HEAD~1"],
            vec!["tag", "v1.5", "HEAD~1"],
            vec!["tag", "-m", "implied annotation", "v8"],
            vec!["tag", "-a", "v9", "-m", ""],
            vec!["tag", "bad..name"],
            vec!["tag", "v11", "nope"],
            vec!["tag", "-a", "v12"],
            vec!["tag"],
            vec!["tag", "-l"],
            vec!["tag", "-l", "v1*"],
            vec!["tag", "--list", "v2"],
            vec!["tag", "-l", "nomatch"],
            vec!["tag", "-n"],
            vec!["tag", "-n3", "-l", "v1*"],
            vec!["tag", "-n", "v10"],
            vec!["tag", "--sort=-refname"],
            vec!["tag", "--sort=version:refname"],
            vec![
                "tag",
                "--format=%(refname:short) %(objecttype) %(objectname:short)",
            ],
            vec!["tag", "--contains", "HEAD~1"],
            vec!["tag", "--points-at", "HEAD"],
            vec!["tag", "--merged", "HEAD~1"],
            vec!["tag", "--no-merged", "HEAD~1"],
            vec!["tag", "-i", "-l", "V1*"],
        ] {
            w.same(&args);
        }
        // The tag objects themselves are the same bytes.
        for name in ["v2", "v10", "v8", "v9"] {
            let (c, g) = (
                w.asc(&["cat-file", "-p", name]),
                w.g(&["cat-file", "-p", name]),
            );
            assert_eq!(c.stdout, g.stdout, "cat-file -p {name}");
            let (c, g) = (w.asc(&["rev-parse", name]), w.g(&["rev-parse", name]));
            assert_eq!(c.stdout, g.stdout, "rev-parse {name}");
        }
    }

    #[tokio::test]
    async fn delete_and_force_match_git() {
        let w = tagged().await;
        for args in [
            vec!["tag", "v1"],
            vec!["tag", "-a", "v2", "-m", "two"],
            vec!["tag", "v3", "HEAD~1"],
            vec!["tag", "-d", "v1", "v2", "nope"],
            vec!["tag", "-d"],
            vec!["tag", "-f", "v3"],
            vec!["tag", "-f", "v3"],
            vec!["tag", "-f", "v3", "HEAD~1"],
            vec!["tag", "-fa", "-m", "now annotated", "v3"],
            vec!["tag", "-d", "v3"],
            vec!["tag"],
        ] {
            w.same(&args);
        }
    }

    #[tokio::test]
    async fn the_message_can_come_from_a_file_or_the_editor() {
        let w = tagged().await;
        let file = w.f.sandbox.tmp.path().join("msg.txt");
        fs::write(&file, "from a file\n\nsecond paragraph\n").unwrap();
        let path = file.display().to_string();
        w.same(&["tag", "-F", &path, "vf"]);
        let (c, g) = (
            w.asc(&["cat-file", "-p", "vf"]),
            w.g(&["cat-file", "-p", "vf"]),
        );
        assert_eq!(c.stdout, g.stdout);
        // An editor that writes a message (comments are stripped).
        let script = w.f.sandbox.tmp.path().join("editor.sh");
        fs::write(
            &script,
            "#!/bin/sh\nprintf 'edited message\\n# a comment\\n' > \"$1\"\n",
        )
        .unwrap();
        let editor = script.display().to_string().replace('\\', "/");
        make_executable(&script);
        let mut cmd = w.cmd();
        cmd.env("GIT_EDITOR", &editor).args(["tag", "-a", "ve"]);
        assert_eq!(run(&mut cmd).code, 0);
        let mut cmd = w.gcmd();
        cmd.env("GIT_EDITOR", &editor).args(["tag", "-a", "ve"]);
        assert_eq!(run(&mut cmd).code, 0);
        let (c, g) = (
            w.asc(&["cat-file", "-p", "ve"]),
            w.g(&["cat-file", "-p", "ve"]),
        );
        assert_eq!(c.stdout, g.stdout);
        // No message at all.
        let mut cmd = w.cmd();
        cmd.env("GIT_EDITOR", ":").args(["tag", "-a", "vn"]);
        let out = run(&mut cmd);
        assert_eq!(
            (out.code, out.stderr.as_str()),
            (128, "fatal: no tag message?\n")
        );
    }

    #[tokio::test]
    async fn signed_tags_use_the_signing_program_and_insist_on_its_status() {
        let w = tagged().await;
        let script = w.f.sandbox.tmp.path().join("fakegpg.sh");
        let body = "#!/bin/sh\ncat > /dev/null\nif [ -n \"$FAKEGPG_FAIL\" ]; then exit 1; fi\n\
                    echo '[GNUPG:] SIG_CREATED D 1 8 00 1700000000 ABCDEF' >&2\n\
                    printf -- '-----BEGIN PGP SIGNATURE-----\\n\\nZmFrZQ==\\n-----END PGP SIGNATURE-----\\n'\n";
        fs::write(&script, body).unwrap();
        make_executable(&script);
        let program = script.display().to_string().replace('\\', "/");
        for args in [
            vec!["config", "gpg.program", &program],
            vec!["config", "user.signingkey", "KEY1"],
        ] {
            assert_eq!(w.asc(&args).code, 0);
            assert_eq!(w.g(&args).code, 0);
        }
        for args in [
            vec!["tag", "-s", "-m", "signed", "s1"],
            vec![
                "tag",
                "-u",
                "OTHER",
                "-m",
                "signed by other",
                "s2",
                "HEAD~1",
            ],
            vec!["tag", "-l", "-n1", "s*"],
        ] {
            w.same(&args);
        }
        for name in ["s1", "s2"] {
            let (c, g) = (
                w.asc(&["cat-file", "-p", name]),
                w.g(&["cat-file", "-p", name]),
            );
            assert_eq!(c.stdout, g.stdout, "cat-file -p {name}");
            assert!(c.stdout.contains("BEGIN PGP SIGNATURE"));
            let (c, g) = (w.asc(&["rev-parse", name]), w.g(&["rev-parse", name]));
            assert_eq!(c.stdout, g.stdout, "rev-parse {name}");
        }
        // A program that fails, or that does not report a created signature, makes no tag.
        let mut cmd = w.cmd();
        cmd.env("FAKEGPG_FAIL", "1")
            .args(["tag", "-s", "-m", "x", "s3"]);
        let mut gcmd = w.gcmd();
        gcmd.env("FAKEGPG_FAIL", "1")
            .args(["tag", "-s", "-m", "x", "s3"]);
        let (c, g) = (run(&mut cmd), run(&mut gcmd));
        assert_eq!((c.code, g.code), (128, 128), "{} / {}", c.stderr, g.stderr);
        assert!(c.stderr.contains("unable to sign the tag"), "{}", c.stderr);
        assert_eq!(w.asc(&["tag", "-l", "s3"]).stdout, "");
        fs::write(&script, "#!/bin/sh\ncat > /dev/null\necho sig\n").unwrap();
        let (c, g) = (
            w.asc(&["tag", "-s", "-m", "x", "s4"]),
            w.g(&["tag", "-s", "-m", "x", "s4"]),
        );
        assert_eq!((c.code, g.code), (128, 128), "{} / {}", c.stderr, g.stderr);
        assert_eq!(w.asc(&["tag", "-l", "s4"]).stdout, "");
    }

    #[tokio::test]
    async fn tag_changes_run_the_reference_transaction_hook() {
        let w = tagged().await;
        let log =
            w.f.sandbox
                .tmp
                .path()
                .join("ref.log")
                .display()
                .to_string()
                .replace('\\', "/");
        let path = Projection::open(&w.f.root)
            .unwrap()
            .common_dir()
            .join("hooks")
            .join("reference-transaction");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(
            &path,
            format!("#!/bin/sh\necho \"$1\" >> \"{log}\"\ncat >> \"{log}\"\n"),
        )
        .unwrap();
        make_executable(&path);
        assert_eq!(w.asc(&["tag", "light"]).code, 0);
        let line = format!("{} {} refs/tags/light\n", "0".repeat(40), w.shas["b"]);
        assert_eq!(
            fs::read_to_string(&log).unwrap().replace("\r\n", "\n"),
            format!("prepared\n{line}committed\n{line}")
        );
    }
}
