//! `asc merge <commit> <commit>…` (git's octopus strategy) against real git: the same commands in a
//! repository made by `asc` and one made by git, with fixed identities and dates, so the output,
//! the exit code and the merge commit itself must agree.

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

    fn commit(&self, name: &str, body: &str) {
        self.put(name, body);
        self.both(&["add", name]);
        self.both(&["commit", "-q", "-m", &format!("add {name}")]);
    }

    /// A new branch at `base` carrying one more file.
    fn branch_with(&self, branch: &str, base: &str, name: &str, body: &str) {
        self.both(&["checkout", "-q", "-b", branch, base]);
        self.commit(name, body);
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
}

const BIG: &str = "one\ntwo\nthree\nfour\nfive\nsix\nseven\neight\nnine\nten\n";

fn base() -> Pair {
    let p = Pair::new();
    p.commit("base.txt", "base\n");
    p.commit("big.txt", BIG);
    p
}

#[test]
fn an_octopus_of_disjoint_branches_is_one_commit_with_every_head_as_parent() {
    let p = base();
    p.branch_with("b1", "master", "b1.txt", "one\n");
    p.branch_with("b2", "master", "b2.txt", "two\n");
    p.branch_with("b3", "master", "b3.txt", "three\n");
    p.both(&["checkout", "-q", "master"]);
    p.commit("m.txt", "main\n");
    let out = p.same(&["merge", "b1", "b2", "b3"]);
    assert!(out.stdout.contains("Merge made by the 'octopus' strategy."));
    p.same_head();
    p.same(&["log", "--format=%P", "-1"]);
    // Everything is merged: asking again changes nothing.
    p.same(&["merge", "b1", "b2"]);
}

#[test]
fn a_head_already_merged_is_skipped_and_a_first_fast_forward_moves_the_result() {
    let p = base();
    p.branch_with("b1", "master", "b1.txt", "one\n");
    p.branch_with("b2", "master", "b2.txt", "two\n");
    p.both(&["checkout", "-q", "master"]);
    p.both(&["branch", "same", "master"]);
    // HEAD is an ancestor of b1: the octopus fast-forwards to it, then merges b2.
    let out = p.same(&["merge", "b1", "b2"]);
    assert!(
        out.stdout.contains("Fast-forwarding to: b1"),
        "{}",
        out.stdout
    );
    p.same_head();
    p.same(&["log", "--format=%P", "-1"]);
    // `same` is an ancestor of HEAD, so it is no head of the merge: what is left is an ordinary
    // merge of b3, as git does it.
    p.branch_with("b3", "b1", "b3.txt", "three\n");
    p.both(&["checkout", "-q", "b2"]);
    let out = p.same(&["merge", "same", "b3"]);
    assert!(
        out.stdout.contains("Merge made by the 'ort' strategy."),
        "{}",
        out.stdout
    );
    p.same_head();
}

#[test]
fn changes_to_one_file_in_different_places_take_the_automatic_merge() {
    let p = base();
    let edit = |line: &str, to: &str| BIG.replace(line, to);
    p.both(&["checkout", "-q", "-b", "b1", "master"]);
    p.put("big.txt", &edit("one\n", "ONE\n"));
    p.both(&["commit", "-q", "-a", "-m", "edit one"]);
    p.both(&["checkout", "-q", "-b", "b2", "master"]);
    p.put("big.txt", &edit("ten\n", "TEN\n"));
    p.both(&["commit", "-q", "-a", "-m", "edit ten"]);
    p.branch_with("b3", "master", "b3.txt", "three\n");
    p.both(&["checkout", "-q", "master"]);
    p.commit("m.txt", "main\n");
    let out = p.same(&["merge", "b1", "b2", "b3"]);
    assert!(
        out.stdout
            .contains("Simple merge did not work, trying automatic merge."),
        "{}",
        out.stdout
    );
    p.same_head();
}

#[test]
fn a_conflict_in_a_head_that_is_not_the_last_stops_the_octopus() {
    let p = base();
    let edit = |to: &str| BIG.replace("five\n", to);
    p.both(&["checkout", "-q", "-b", "b1", "master"]);
    p.put("big.txt", &edit("FIVE\n"));
    p.both(&["commit", "-q", "-a", "-m", "five a"]);
    p.both(&["checkout", "-q", "-b", "b2", "master"]);
    p.put("big.txt", &edit("5\n"));
    p.both(&["commit", "-q", "-a", "-m", "five b"]);
    p.branch_with("b3", "master", "b3.txt", "three\n");
    p.both(&["checkout", "-q", "master"]);
    p.commit("m.txt", "main\n");
    let before = p.head();
    let out = p.same(&["merge", "b1", "b2", "b3"]);
    assert_eq!(out.code, 2, "{}", out.stdout);
    assert!(out.stdout.contains("Should not be doing an octopus."));
    // asc leaves HEAD where it was.
    assert_eq!(p.head().0, before.0);
}

#[test]
fn a_conflict_in_the_last_head_leaves_a_merge_to_finish() {
    let p = base();
    let edit = |to: &str| BIG.replace("five\n", to);
    p.branch_with("b3", "master", "b3.txt", "three\n");
    p.both(&["checkout", "-q", "-b", "b1", "master"]);
    p.put("big.txt", &edit("FIVE\n"));
    p.both(&["commit", "-q", "-a", "-m", "five a"]);
    p.both(&["checkout", "-q", "-b", "b2", "master"]);
    p.put("big.txt", &edit("5\n"));
    p.both(&["commit", "-q", "-a", "-m", "five b"]);
    p.both(&["checkout", "-q", "master"]);
    p.commit("m.txt", "main\n");
    let a = p.run_asc(&["merge", "b3", "b1", "b2"]);
    let g = p.run_git(&["merge", "b3", "b1", "b2"]);
    assert_eq!(
        a.code, g.code,
        "asc: {} {}\ngit: {} {}",
        a.stdout, a.stderr, g.stdout, g.stderr
    );
    assert_eq!(a.code, 1);
    assert!(
        a.stdout
            .contains("Automatic merge failed; fix conflicts and then commit the result.")
    );
    // Resolve and conclude the same way in both.
    p.put("big.txt", &edit("FIVE-5\n"));
    p.both(&["add", "big.txt"]);
    p.both(&["commit", "-q", "-m", "Merge octopus"]);
    p.same(&["log", "--format=%P", "-1"]);
    p.same(&["ls-tree", "-r", "HEAD"]);
}

#[test]
fn a_squash_of_several_commits_stages_them_and_writes_the_squash_message() {
    let p = base();
    p.branch_with("b1", "master", "b1.txt", "one\n");
    p.branch_with("b2", "master", "b2.txt", "two\n");
    p.both(&["checkout", "-q", "master"]);
    p.commit("m.txt", "main\n");
    let out = p.same(&["merge", "--squash", "b1", "b2"]);
    assert!(
        out.stdout.contains("Squash commit -- not updating HEAD"),
        "{}",
        out.stdout
    );
    // (`asc init` leaves an untracked .ascignore that git has no counterpart of.)
    let staged = |o: Out| {
        o.stdout
            .lines()
            .filter(|l| !l.ends_with(".ascignore"))
            .collect::<Vec<_>>()
            .join("\n")
    };
    assert_eq!(
        staged(p.run_asc(&["status", "--porcelain"])),
        staged(p.run_git(&["status", "--porcelain"]))
    );
    let squash_msg = |dir: &Path, o: Out| {
        let git_dir = dir.join(o.stdout.trim());
        std::fs::read_to_string(git_dir.join("SQUASH_MSG")).unwrap()
    };
    let a = squash_msg(&p.asc, p.run_asc(&["rev-parse", "--absolute-git-dir"]));
    let g = squash_msg(&p.git, p.run_git(&["rev-parse", "--absolute-git-dir"]));
    // With equal commit dates the order of the listing is a tie git breaks by argument order and
    // asc by its own walk; the same commits are listed either way.
    let sorted = |t: &str| {
        let mut l: Vec<String> = t.lines().map(str::to_owned).collect();
        l.sort();
        l
    };
    assert_eq!(sorted(&a), sorted(&g));
}
