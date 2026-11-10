//! `asc cherry-pick`, `asc revert` and `asc rebase` with no commit under HEAD (an unborn branch) and
//! with root commits, against real git: the same commands in a repository made by `asc` and one
//! made by git, with fixed identities and dates, so the output, the exit code and the commits made
//! must agree.

mod support;

use std::path::{Path, PathBuf};
use std::process::Command;

use support::{Sandbox, text};

const IDENT: [(&str, &str); 6] = [
    ("GIT_AUTHOR_NAME", "Test"),
    ("GIT_AUTHOR_EMAIL", "test@example.com"),
    ("GIT_AUTHOR_DATE", "1700000000 +0000"),
    ("GIT_COMMITTER_NAME", "Test"),
    ("GIT_COMMITTER_EMAIL", "test@example.com"),
    ("GIT_COMMITTER_DATE", "1700000000 +0000"),
];

struct Out {
    code: i32,
    stdout: String,
    stderr: String,
}

struct Pair {
    sb: Sandbox,
    asc: PathBuf,
    git: PathBuf,
}

impl Pair {
    fn new() -> Self {
        let sb = Sandbox::new();
        let asc = sb.dir("asc-repo");
        let git = sb.dir("git-repo");
        let p = Self { sb, asc, git };
        p.both(&["init", "-q"]);
        p
    }

    fn git_cmd(&self, dir: &Path) -> Command {
        let mut c = asc_git::realgit::pinned_command(); // lint:allow-raw-git
        for (key, _) in std::env::vars_os() {
            if key
                .to_string_lossy()
                .to_ascii_uppercase()
                .starts_with("GIT_")
            {
                c.env_remove(&key);
            }
        }
        c.current_dir(dir)
            .env("HOME", &self.sb.user)
            .env("USERPROFILE", &self.sb.user)
            .env("GIT_CONFIG_GLOBAL", self.sb.user.join(".gitconfig"))
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_PAGER", "cat")
            .env("TZ", "UTC")
            .env("LC_ALL", "C")
            .env("NO_COLOR", "1")
            .args([
                "-c",
                "core.autocrlf=false",
                "-c",
                "init.defaultBranch=master",
            ])
            .envs(IDENT);
        c
    }

    fn run_asc(&self, args: &[&str]) -> Out {
        let o = self
            .sb
            .asc(&self.asc)
            .envs(IDENT)
            .args(args)
            .output()
            .expect("spawn asc");
        Out {
            code: o.status.code().unwrap_or(-1),
            stdout: text(&o.stdout),
            stderr: text(&o.stderr),
        }
    }

    fn run_git(&self, args: &[&str]) -> Out {
        let o = self
            .git_cmd(&self.git)
            .args(args)
            .output()
            .expect("spawn git");
        Out {
            code: o.status.code().unwrap_or(-1),
            stdout: text(&o.stdout),
            stderr: text(&o.stderr),
        }
    }

    /// The same command in both; both must succeed.
    fn both(&self, args: &[&str]) {
        let (a, g) = (self.run_asc(args), self.run_git(args));
        assert_eq!(a.code, 0, "asc {args:?}: {}", a.stderr);
        assert_eq!(g.code, 0, "git {args:?}: {}", g.stderr);
    }

    /// The same command in both; exit code and stdout must be the same.
    fn same(&self, args: &[&str]) -> Out {
        let (a, g) = (self.run_asc(args), self.run_git(args));
        assert_eq!(
            (a.code, &a.stdout),
            (g.code, &g.stdout),
            "{args:?}\nasc stderr: {}\ngit stderr: {}",
            a.stderr,
            g.stderr
        );
        a
    }

    fn put(&self, name: &str, body: &str) {
        for dir in [&self.asc, &self.git] {
            std::fs::write(dir.join(name), body).unwrap();
        }
    }

    fn head(&self) -> (String, String) {
        let (a, g) = (
            self.run_asc(&["rev-parse", "HEAD"]),
            self.run_git(&["rev-parse", "HEAD"]),
        );
        (a.stdout.trim().to_owned(), g.stdout.trim().to_owned())
    }

    fn same_head(&self) {
        let (a, g) = self.head();
        assert_eq!(a, g, "the merge commits differ");
        self.same(&["cat-file", "-p", "HEAD"]);
        self.same(&["ls-tree", "-r", "HEAD"]);
    }

    /// The same failing command in both: the exit code and the first line of stderr agree.
    fn same_failure(&self, args: &[&str]) -> Out {
        let (a, g) = (self.run_asc(args), self.run_git(args));
        assert_ne!(g.code, 0, "git {args:?} succeeded");
        let first = |s: &str| s.lines().next().unwrap_or_default().to_owned();
        assert_eq!(
            (a.code, first(&a.stderr)),
            (g.code, first(&g.stderr)),
            "{args:?}\nasc stderr: {}\ngit stderr: {}",
            a.stderr,
            g.stderr
        );
        a
    }

    /// HEAD names no commit in either repository.
    fn unborn(&self) {
        for out in [
            self.run_asc(&["rev-parse", "--verify", "-q", "HEAD"]),
            self.run_git(&["rev-parse", "--verify", "-q", "HEAD"]),
        ] {
            assert_ne!(out.code, 0, "HEAD is {}", out.stdout);
        }
    }

    /// A new branch with no commits and an empty working tree.
    fn orphan(&self, branch: &str) {
        self.both(&["switch", "-q", "--orphan", branch]);
    }

    fn edit(&self, name: &str, body: &str, message: &str) {
        self.put(name, body);
        self.both(&["add", name]);
        self.both(&["commit", "-q", "-m", message]);
    }
}

/// master: `f` = 1, then 2, then 3, then `g` added.
fn history() -> Pair {
    let p = Pair::new();
    p.edit("f", "1\n", "one");
    p.edit("f", "2\n", "two");
    p.edit("f", "3\n", "three");
    p.edit("g", "g\n", "gee");
    p
}

#[test]
fn a_cherry_pick_onto_an_unborn_branch_makes_a_root_commit() {
    let p = history();
    p.orphan("un");
    p.same(&["cherry-pick", "master~3"]);
    p.same_head();
    p.same(&["log", "--format=%P %s"]);
}

#[test]
fn several_cherry_picks_onto_an_unborn_branch_chain_from_a_root_commit() {
    let p = history();
    p.orphan("un");
    p.same(&["cherry-pick", "master~3", "master"]);
    p.same_head();
    p.same(&["log", "--format=%P %s"]);
}

#[test]
fn a_cherry_pick_without_commit_onto_an_unborn_branch_stages_only() {
    let p = history();
    p.orphan("un");
    p.same(&["cherry-pick", "-n", "master~3"]);
    p.unborn();
    p.same(&["ls-files", "-s"]);
}

#[test]
fn several_reverts_on_an_unborn_branch_are_refused() {
    let p = history();
    p.orphan("un");
    let out = p.same_failure(&["revert", "master", "master~1"]);
    assert!(
        out.stderr.contains("can't revert as initial commit"),
        "{}",
        out.stderr
    );
    p.unborn();
}

/// The same conflicting cherry-pick in both: the same exit code and the same index.
fn conflicting_pick(p: &Pair, args: &[&str]) {
    let a = p.run_asc(args);
    let g = p.run_git(args);
    assert_eq!(
        a.code, g.code,
        "asc: {}
git: {}",
        a.stderr, g.stderr
    );
    assert_ne!(g.code, 0, "{args:?} conflicts");
    p.same(&["ls-files", "-s"]);
}

#[test]
fn a_single_abort_while_unborn_is_refused_as_git_does() {
    let p = history();
    p.orphan("un");
    conflicting_pick(&p, &["cherry-pick", "master~1"]);
    let out = p.same_failure(&["cherry-pick", "--abort"]);
    assert!(
        out.stderr.contains("fatal: cherry-pick failed"),
        "{}",
        out.stderr
    );
    p.unborn();
}

// git's own abort of a sequence started on an unborn branch fails to read the empty record it
// wrote, or moves the branch to the commit it was picking; asc refuses it and changes nothing.
// fidelity: U-3
#[test]
fn an_abort_of_a_sequence_started_unborn_is_refused() {
    let p = history();
    p.orphan("un");
    // `one` applies as a root commit; `three` changes `f` from 2, which is not there.
    conflicting_pick(&p, &["cherry-pick", "master~3", "master~1"]);
    let head = p.run_asc(&["rev-parse", "HEAD"]).stdout;
    let index = p.run_asc(&["ls-files", "-s"]).stdout;
    let out = p.run_asc(&["cherry-pick", "--abort"]);
    assert_eq!(out.code, 128, "{}", out.stderr);
    assert!(
        out.stderr.contains("(git-fidelity U-3)") && out.stderr.contains("--quit"),
        "{}",
        out.stderr
    );
    assert_eq!(p.run_asc(&["rev-parse", "HEAD"]).stdout, head);
    assert_eq!(p.run_asc(&["ls-files", "-s"]).stdout, index);
    let quit = p.run_asc(&["cherry-pick", "--quit"]);
    assert_eq!(quit.code, 0, "{}", quit.stderr);
    assert_eq!(p.run_asc(&["rev-parse", "HEAD"]).stdout, head);
}

#[test]
fn a_rebase_onto_unrelated_history_replays_the_root_commit() {
    let p = history();
    p.orphan("side");
    p.edit("s", "s\n", "s1");
    p.edit("t", "t\n", "s2");
    p.same(&["rebase", "master"]);
    p.same_head();
    p.same(&["log", "--format=%P %s"]);
}

#[test]
fn a_rebase_on_an_unborn_branch_fails_as_git_does() {
    let p = history();
    p.orphan("un");
    // git prints the missing-upstream advice on stdout.
    let out = p.same(&["rebase"]);
    assert_eq!(out.code, 1);
    let out = p.same_failure(&["rebase", "master"]);
    assert!(
        out.stderr.contains("Could not resolve HEAD to a commit"),
        "{}",
        out.stderr
    );
    p.same_failure(&["rebase", "nope"]);
    p.unborn();
}
