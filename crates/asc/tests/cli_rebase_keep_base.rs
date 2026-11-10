//! `asc rebase --keep-base`, against real git: the new base is the one merge base of the upstream
//! and the branch rebased (`<upstream>...<branch>`), with a branch argument from another branch or
//! a detached HEAD, and the same refusals.

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

    fn edit(&self, name: &str, body: &str, message: &str) {
        self.put(name, body);
        self.both(&["add", name]);
        self.both(&["commit", "-q", "-m", message]);
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
}

/// master: base, m1. topic (from base): t1, t2. HEAD is master.
fn forked() -> Pair {
    let p = Pair::new();
    p.edit("f", "base\n", "base");
    p.both(&["switch", "-q", "-c", "topic"]);
    p.edit("t1", "1\n", "t1");
    p.edit("t2", "2\n", "t2");
    p.both(&["switch", "-q", "master"]);
    p.edit("m1", "m\n", "m1");
    p
}

#[test]
fn keep_base_with_a_branch_keeps_that_branch_on_its_own_base() {
    let p = forked();
    let before = p.run_asc(&["rev-parse", "topic"]).stdout;
    let out = p.same(&["rebase", "--keep-base", "master", "topic"]);
    assert!(
        out.stdout.contains("Current branch topic is up to date."),
        "{}",
        out.stdout
    );
    assert_eq!(p.run_asc(&["rev-parse", "topic"]).stdout, before);
    p.same(&["symbolic-ref", "HEAD"]);
    p.same(&["log", "--format=%H %P %s", "topic"]);
}

#[test]
fn keep_base_forced_with_a_branch_replays_it_on_the_same_base() {
    let p = forked();
    p.same(&["rebase", "--keep-base", "-f", "master", "topic"]);
    p.same(&["log", "--format=%P %s", "topic"]);
    p.same(&["symbolic-ref", "HEAD"]);
}

#[test]
fn keep_base_from_a_detached_head_with_a_branch_uses_the_branch() {
    let p = forked();
    p.both(&["switch", "-q", "--detach", "master"]);
    p.same(&["rebase", "--keep-base", "-f", "master", "topic"]);
    p.same(&["log", "--format=%P %s", "topic"]);
    p.same(&["symbolic-ref", "HEAD"]);
}

#[test]
fn keep_base_refusals_match_git() {
    let p = forked();
    p.same_failure(&[
        "rebase",
        "--keep-base",
        "--onto",
        "master",
        "master",
        "topic",
    ]);
    p.same_failure(&["rebase", "--keep-base", "nope", "topic"]);
    p.both(&["switch", "-q", "--orphan", "lone"]);
    p.edit("l", "l\n", "lone");
    p.same_failure(&["rebase", "--keep-base", "master"]);
}
