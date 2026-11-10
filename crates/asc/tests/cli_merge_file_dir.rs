//! Paths the two sides of a merge both claim, against real git: a file against a directory (the
//! directory keeps the name and the file moves aside to `<path>~<label>`, with git's messages,
//! index stages and work tree, in `merge`, `cherry-pick` and `merge-tree` plain and `-z`), and two
//! new names that differ only in case.

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
            let path = dir.join(name);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, body).unwrap();
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
}

fn base() -> Pair {
    let p = Pair::new();
    p.commit("base.txt", "base\n");
    p
}

impl Pair {
    /// The index, the status and the work-tree files named must agree after a stopped merge.
    fn same_state(&self, files: &[&str]) {
        self.same(&["ls-files", "-s"]);
        let porcelain = |o: Out| {
            o.stdout
                .lines()
                .filter(|l| !l.ends_with(".ascignore"))
                .map(str::to_owned)
                .collect::<Vec<_>>()
        };
        assert_eq!(
            porcelain(self.run_asc(&["status", "--porcelain"])),
            porcelain(self.run_git(&["status", "--porcelain"]))
        );
        for f in files {
            let read = |dir: &Path| std::fs::read_to_string(dir.join(f)).ok();
            assert_eq!(read(&self.asc), read(&self.git), "work-tree file {f}");
        }
        assert!(self.asc.join("a").is_dir() && self.git.join("a").is_dir());
    }
}

#[test]
fn our_file_against_their_directory_moves_ours_aside() {
    let p = base();
    p.branch_with("side", "master", "a/b", "deep\n");
    p.both(&["checkout", "-q", "master"]);
    p.commit("a", "file\n");
    let out = p.same(&["merge", "side"]);
    assert_eq!(out.code, 1);
    assert!(
        out.stdout.contains(
            "CONFLICT (file/directory): directory in the way of a from HEAD; moving it to a~HEAD instead."
        ),
        "{}",
        out.stdout
    );
    p.same_state(&["a~HEAD", "a/b"]);
}

#[test]
fn their_file_against_our_directory_moves_theirs_aside() {
    let p = base();
    p.branch_with("feature/x", "master", "a", "file\n");
    p.both(&["checkout", "-q", "master"]);
    p.commit("a/b", "deep\n");
    let out = p.same(&["merge", "feature/x"]);
    assert_eq!(out.code, 1);
    // The `/` of the label becomes `_` in the new name.
    assert!(
        out.stdout.contains("moving it to a~feature_x instead."),
        "{}",
        out.stdout
    );
    p.same_state(&["a~feature_x", "a/b"]);
}

#[test]
fn a_modified_file_against_a_new_directory_is_also_a_modify_delete() {
    let p = base();
    p.commit("a", "one\n");
    p.both(&["checkout", "-q", "-b", "side"]);
    p.both(&["rm", "-q", "a"]);
    p.commit("a/b", "deep\n");
    p.both(&["checkout", "-q", "master"]);
    p.commit("a", "two\n");
    let out = p.same(&["merge", "side"]);
    assert!(
        out.stdout.contains(
            "CONFLICT (modify/delete): a~HEAD deleted in side and modified in HEAD.  Version HEAD of a~HEAD left in tree."
        ),
        "{}",
        out.stdout
    );
    p.same_state(&["a~HEAD", "a/b"]);
}

#[test]
fn a_cherry_pick_moves_the_file_aside_the_same_way() {
    let p = base();
    p.branch_with("side", "master", "a/b", "deep\n");
    p.both(&["checkout", "-q", "master"]);
    p.commit("a", "file\n");
    let out = p.same(&["cherry-pick", "side"]);
    assert_ne!(out.code, 0);
    p.same_state(&["a~HEAD", "a/b"]);
}

#[test]
fn merge_tree_reports_both_paths_plain_and_with_nul() {
    let p = base();
    p.commit("a", "one\n");
    p.both(&["checkout", "-q", "-b", "side"]);
    p.both(&["rm", "-q", "a"]);
    p.commit("a/b", "deep\n");
    p.both(&["checkout", "-q", "master"]);
    p.commit("a", "two\n");
    let plain = p.same(&["merge-tree", "--write-tree", "master", "side"]);
    assert_eq!(plain.code, 1);
    assert!(
        plain.stdout.contains("moving it to a~master instead."),
        "{}",
        plain.stdout
    );
    let nul = p.same(&["merge-tree", "--write-tree", "-z", "master", "side"]);
    assert!(
        nul.stdout
            .contains("2\0a~master\0a\0CONFLICT (file/directory)\0"),
        "{:?}",
        nul.stdout
    );
    assert!(
        nul.stdout
            .contains("1\0a~master\0CONFLICT (modify/delete)\0"),
        "{:?}",
        nul.stdout
    );
    p.same(&[
        "merge-tree",
        "--write-tree",
        "--name-only",
        "side",
        "master",
    ]);
}

/// Both sides add a name that differs from the other only in case.
fn case_pair(ignorecase: &str) -> Pair {
    let p = base();
    p.both(&["config", "core.ignorecase", ignorecase]);
    p.branch_with("side", "master", "A.txt", "theirs\n");
    p.both(&["checkout", "-q", "master"]);
    p.commit("a.txt", "ours\n");
    p
}

#[test]
fn a_case_pair_merges_like_git_when_ignorecase_is_off() {
    let p = case_pair("false");
    // On a case-sensitive volume the merge is clean with both names. On an insensitive one
    // `a.txt` stands where `A.txt` would go, so git (and asc) refuse it as an untracked file
    // in the way.
    let insensitive = p.asc.join("A.TXT").exists();
    let out = p.same(&["merge", "side"]);
    assert_eq!(out.code, if insensitive { 2 } else { 0 }, "{}", out.stdout);
    p.same(&["ls-tree", "-r", "HEAD"]);
    p.same(&["ls-files", "-s"]);
}

#[test]
fn merge_tree_keeps_both_names_of_a_case_pair() {
    let p = case_pair("true");
    let out = p.same(&["merge-tree", "--write-tree", "master", "side"]);
    assert_eq!(out.code, 0, "{}", out.stdout);
}

// fidelity: U-4
#[test]
fn a_case_pair_on_a_case_insensitive_work_tree_is_refused_and_nothing_moves() {
    let p = case_pair("true");
    let before = p.run_asc(&["rev-parse", "HEAD"]).stdout;
    let out = p.run_asc(&["merge", "side"]);
    assert_eq!(out.code, 128, "{}{}", out.stdout, out.stderr);
    assert!(
        out.stderr
            .contains("'a.txt' and 'A.txt' differ only in case")
            && out.stderr.contains("(git-fidelity U-4)"),
        "{}",
        out.stderr
    );
    assert_eq!(p.run_asc(&["rev-parse", "HEAD"]).stdout, before);
    assert_eq!(
        std::fs::read_to_string(p.asc.join("a.txt")).unwrap(),
        "ours\n"
    );
    let pick = p.run_asc(&["cherry-pick", "side"]);
    assert!(
        pick.stderr.contains("(git-fidelity U-4)"),
        "{}",
        pick.stderr
    );
    assert_eq!(p.run_asc(&["rev-parse", "HEAD"]).stdout, before);
}
