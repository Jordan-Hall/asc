//! `read-tree` with several trees, against real git: the union without `-m`, the two-way merge
//! (`-m <old> <new>`), the three-way merge (`-m <base> <ours> <theirs>`) with `--aggressive` and
//! `--trivial`, `--prefix` with several trees, the refusals (an entry the merge would overwrite,
//! a work-tree file that is not up to date, `-i` skipping that check) and the argument errors.

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
}

impl Pair {
    /// The same command in both; exit code, stdout and stderr must be the same.
    fn same_all(&self, args: &[&str]) -> Out {
        let (a, g) = (self.run_asc(args), self.run_git(args));
        assert_eq!(
            (a.code, &a.stdout, &a.stderr),
            (g.code, &g.stdout, &g.stderr),
            "{args:?}"
        );
        a
    }

    fn index(&self) -> Out {
        self.same(&["ls-files", "-s"])
    }
}

/// `base` with a, b, c, e and f; `ours` changes a and c, adds d and deletes e; `theirs` changes
/// b and c, adds d differently and deletes f. `ours` is checked out.
fn three() -> Pair {
    let p = Pair::new();
    for (name, body) in [
        ("a", "a\n"),
        ("b", "b\n"),
        ("c", "c\n"),
        ("e", "e\n"),
        ("f", "f\n"),
    ] {
        p.put(name, body);
    }
    p.both(&["add", "a", "b", "c", "e", "f"]);
    p.both(&["commit", "-q", "-m", "base"]);
    p.both(&["branch", "base"]);
    p.both(&["checkout", "-q", "-b", "theirs"]);
    p.put("b", "b2\n");
    p.put("c", "c-theirs\n");
    p.put("d", "d-theirs\n");
    p.both(&["rm", "-q", "f"]);
    p.both(&["add", "b", "c", "d"]);
    p.both(&["commit", "-q", "-m", "theirs"]);
    p.both(&["checkout", "-q", "-b", "ours", "base"]);
    p.put("a", "a2\n");
    p.put("c", "c-ours\n");
    p.put("d", "d-ours\n");
    p.both(&["rm", "-q", "e"]);
    p.both(&["add", "a", "c", "d"]);
    p.both(&["commit", "-q", "-m", "ours"]);
    p
}

#[test]
fn a_three_way_merge_collapses_the_trivial_paths_and_stages_the_rest() {
    let p = three();
    let out = p.same_all(&["read-tree", "-m", "base", "ours", "theirs"]);
    assert_eq!(out.code, 0);
    let index = p.index().stdout;
    assert!(
        index.contains(" 1\tc\n") && index.contains(" 2\td\n"),
        "{index}"
    );
    assert!(
        index.contains(" 1\tf\n") && index.contains(" 0\tb\n"),
        "{index}"
    );
}

#[test]
fn aggressive_resolves_deletions() {
    let p = three();
    p.same_all(&["read-tree", "-m", "--aggressive", "base", "ours", "theirs"]);
    let index = p.index().stdout;
    assert!(
        !index.contains("\te\n") && !index.contains("\tf\n"),
        "{index}"
    );
}

#[test]
fn trivial_refuses_a_merge_that_needs_file_merging() {
    let p = three();
    let out = p.same_all(&["read-tree", "-m", "--trivial", "base", "ours", "theirs"]);
    assert_eq!(out.code, 128);
    assert!(out.stderr.contains("Merge requires file-level merging"));
    p.index();
}

#[test]
fn a_three_way_merge_refuses_an_index_that_differs_from_ours() {
    let p = three();
    p.put("b", "staged\n");
    p.both(&["add", "b"]);
    let out = p.same_all(&["read-tree", "-m", "base", "ours", "theirs"]);
    assert_eq!(out.code, 128);
    p.index();
}

#[test]
fn a_dirty_file_stops_a_merge_unless_minus_i() {
    let p = three();
    p.put("c", "dirty\n");
    let out = p.same_all(&["read-tree", "-m", "base", "ours", "theirs"]);
    assert_eq!(out.code, 128);
    assert!(
        out.stderr.contains("Entry 'c' not uptodate"),
        "{}",
        out.stderr
    );
    p.index();
    p.same_all(&["read-tree", "-m", "-i", "base", "ours", "theirs"]);
    p.index();
}

#[test]
fn a_file_against_a_directory_in_a_three_way_merge() {
    let p = Pair::new();
    p.commit("base.txt", "base\n");
    p.both(&["branch", "base"]);
    p.both(&["checkout", "-q", "-b", "theirs"]);
    p.commit("x/y", "deep\n");
    p.both(&["checkout", "-q", "-b", "ours", "base"]);
    p.commit("x", "file\n");
    p.same_all(&["read-tree", "-m", "base", "ours", "theirs"]);
    p.index();
}

#[test]
fn a_two_way_merge_carries_staged_changes_forward() {
    let p = three();
    p.both(&["checkout", "-q", "base"]);
    // A staged change at a path the move leaves alone stays; one at a path it changes refuses.
    p.put("e", "staged\n");
    p.both(&["add", "e"]);
    p.same_all(&["read-tree", "-m", "base", "theirs"]);
    p.index();
    p.both(&["read-tree", "base"]);
    p.put("b", "staged\n");
    p.both(&["add", "b"]);
    let out = p.same_all(&["read-tree", "-m", "base", "theirs"]);
    assert_eq!(out.code, 128);
    assert!(
        out.stderr
            .contains("Entry 'b' would be overwritten by merge")
    );
    p.index();
}

#[test]
fn a_two_way_merge_checks_the_files_it_replaces() {
    let p = three();
    p.both(&["checkout", "-q", "base"]);
    p.put("b", "dirty\n");
    let out = p.same_all(&["read-tree", "-m", "base", "theirs"]);
    assert_eq!(out.code, 128);
    p.same_all(&["read-tree", "-m", "-i", "base", "theirs"]);
    p.index();
}

#[test]
fn several_trees_without_m_read_their_union() {
    let p = three();
    p.same_all(&["read-tree", "ours", "theirs"]);
    p.index();
    p.same_all(&["read-tree", "theirs", "base", "ours"]);
    p.index();
}

#[test]
fn a_prefix_with_several_trees_merges_against_the_whole_index() {
    let p = three();
    let out = p.same_all(&["read-tree", "--prefix=p/", "base", "base"]);
    assert_eq!(out.code, 0);
    p.index();
    let out = p.same_all(&["read-tree", "--prefix=p/", "base", "theirs"]);
    assert_eq!(out.code, 128);
    p.same_all(&["read-tree", "--prefix=p/", "base", "ours", "theirs"]);
    p.index();
}

#[test]
fn the_argument_errors_match_git() {
    let p = three();
    for args in [
        &["read-tree", "-m"][..],
        &["read-tree", "-m", "--prefix=p/", "base", "ours"],
        &["read-tree", "-m", "--empty"],
        &[
            "read-tree",
            "-m",
            "base",
            "base",
            "base",
            "base",
            "base",
            "base",
            "base",
            "base",
            "base",
        ],
        &["read-tree", "-i", "base", "ours"],
        &["read-tree", "--empty", "base", "ours"],
    ] {
        let out = p.same_all(args);
        assert_eq!(out.code, 128, "{args:?}");
    }
    p.same_all(&["read-tree", "-m", "base", "ours", "theirs"]);
    let out = p.same_all(&["read-tree", "-m", "base", "ours", "theirs"]);
    assert!(out.stderr.contains("resolve your current index first"));
}
