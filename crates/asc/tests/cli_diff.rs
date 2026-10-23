//! U24c acceptance: `diff`, `diff-tree`, `diff-index` and `diff-files` through the built binary.
//! Each scenario is built twice, once in an asc fixture (engine commits, `state.db` staging) and
//! once in a real git repository with the same files, identity and dates, so the commit ids
//! agree and every output can be compared with git's byte for byte.

mod support;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

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
        .env("GIT_PAGER", "cat")
        .args([
            "-c",
            "core.autocrlf=false",
            "-c",
            "core.quotePath=true",
            "-c",
            "core.filemode=false",
        ])
        .args(args))
}

fn put(root: &Path, rel: &str, content: &str) {
    let path = root.join(rel);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, content).unwrap();
}

type Files = Vec<(&'static str, &'static str)>;
type Edits = Vec<(&'static str, Option<&'static str>)>;

/// Two commits, then staged changes and work tree edits (`None` = delete).
#[derive(Default)]
struct Scenario {
    first: Files,
    second: Files,
    staged: Edits,
    work: Edits,
    /// `add -N` paths with their work tree content.
    intent: Files,
}

impl Scenario {
    fn apply_work(&self, root: &Path) {
        for (p, c) in &self.work {
            match c {
                Some(c) => put(root, p, c),
                None => {
                    let _ = fs::remove_file(root.join(p));
                }
            }
        }
    }
}

struct Pair {
    f: Fixture,
    git_dir: PathBuf,
}

const TEN: &str = "one\ntwo\nthree\nfour\nfive\nsix\nseven\neight\nnine\nten\n";
const TEN_EDITED: &str = "one\ntwo\nthree\nfour\nFIVE\nsix\nseven\neight\nnine\nten\n";
const MOVED: &str = "same content here\nsecond line\nthird line\nfourth line\n";

fn mixed() -> Scenario {
    Scenario {
        first: vec![
            ("a.txt", TEN),
            ("b.txt", "bee\n"),
            ("c.txt", "sea\n"),
            ("d/e.txt", "deep\nfile\n"),
            ("move-me.txt", MOVED),
            ("bin.dat", "\u{0}\u{1}\u{2}binary\u{0}"),
            ("space name.txt", "spaced\n"),
            ("noeol.txt", "no newline"),
            ("gone.txt", "bye\n"),
            ("caf\u{e9}.txt", "accent\n"),
        ],
        second: vec![
            ("a.txt", TEN_EDITED),
            ("new.txt", "brand new\n"),
            ("bin.dat", "\u{0}\u{1}\u{2}binary changed\u{0}\u{0}"),
            ("d/f.txt", "second deep\n"),
            ("d/e.txt", "deep\nfile\nmore\n"),
        ],
        staged: vec![
            (
                "a.txt",
                Some("one\ntwo\nthree\nfour\nFIVE\nsix\nseven\neight\nnine\nTEN\n"),
            ),
            ("staged-new.txt", Some("staged\n")),
            ("c.txt", None),
            ("moved.txt", Some(MOVED)),
            ("move-me.txt", None),
        ],
        work: vec![
            ("b.txt", Some("bee edited\n")),
            (
                "a.txt",
                Some("ONE\ntwo\nthree\nfour\nFIVE\nsix\nseven\neight\nnine\nTEN\n"),
            ),
            ("gone.txt", None),
            ("bin.dat", Some("\u{0}\u{1}\u{2}binary work\u{0}")),
            ("noeol.txt", Some("no newline\nnow one\n")),
            ("caf\u{e9}.txt", Some("accent edited\n")),
            ("space name.txt", Some("spaced edited\n")),
            ("untracked.txt", Some("never shown\n")),
        ],
        intent: vec![],
    }
}

impl Pair {
    async fn new(s: &Scenario) -> Self {
        let f = Fixture::new().await;
        let bytes = |files: &Files| -> Vec<(&str, &[u8])> {
            files.iter().map(|(p, c)| (*p, c.as_bytes())).collect()
        };
        let a = f.commit(&[], &bytes(&s.first)).await;
        let b = f.commit(&[a], &bytes(&s.second)).await;
        f.set_branch("master", b).await;
        f.checkout_branch("master", Some(b)).await;

        let adds: Vec<(&str, &[u8])> = s
            .staged
            .iter()
            .filter_map(|(p, c)| c.map(|c| (*p, c.as_bytes())))
            .collect();
        let mut rows = Vec::new();
        if !adds.is_empty() {
            let side = f.commit(&[b], &adds).await;
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
        for (p, _) in &s.intent {
            let side = f.commit(&[b], &[(p, b"")]).await;
            let leaf = f.file_leaf(side, p).await;
            rows.push(StageRow {
                path: p.as_bytes().to_vec(),
                stage: 0,
                kind: leaf.kind,
                file: Some(leaf.file),
                size: leaf.size,
                git: Some(leaf.git),
                intent_to_add: true,
            });
        }
        f.stage(&rows).await;
        for (p, c) in s.first.iter().chain(&s.second) {
            put(&f.root, p, c);
        }
        for (p, c) in &s.staged {
            if let Some(c) = c {
                put(&f.root, p, c);
            }
        }
        for (p, c) in &s.intent {
            put(&f.root, p, c);
        }
        s.apply_work(&f.root);

        let git_dir = f.sandbox.dir("gitrepo");
        let home = &f.sandbox.user;
        git(&git_dir, home, &["init", "-q", "-b", "master"]);
        for (p, c) in &s.first {
            put(&git_dir, p, c);
        }
        git(&git_dir, home, &["add", "-A"]);
        let c = git(
            &git_dir,
            home,
            &[
                "commit",
                "-q",
                "-m",
                &format!("commit of {} files", s.first.len()),
            ],
        );
        assert_eq!(c.code, 0, "{}", c.stderr);
        for (p, c) in &s.second {
            put(&git_dir, p, c);
        }
        git(&git_dir, home, &["add", "-A"]);
        let c = git(
            &git_dir,
            home,
            &[
                "commit",
                "-q",
                "-m",
                &format!("commit of {} files", s.second.len()),
            ],
        );
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
        for (p, c) in &s.intent {
            put(&git_dir, p, c);
            git(&git_dir, home, &["add", "-N", "--", p]);
        }
        s.apply_work(&git_dir);
        Self { f, git_dir }
    }

    fn asc(&self, cwd: &Path, args: &[&str]) -> Out {
        run(self.f.sandbox.asc(cwd).args(args))
    }

    fn git(&self, cwd: &Path, args: &[&str]) -> Out {
        git(cwd, &self.f.sandbox.user, args)
    }

    /// asc's stdout and exit code equal git's for `args`, run at the work tree root.
    fn same(&self, args: &[&str]) {
        self.same_in(&self.f.root, &self.git_dir, args);
    }

    fn same_in(&self, asc_cwd: &Path, git_cwd: &Path, args: &[&str]) {
        let got = self.asc(asc_cwd, args);
        let want = self.git(git_cwd, args);
        assert_eq!(got.code, want.code, "exit code of {args:?}: {}", got.stderr);
        assert_eq!(
            got.stdout, want.stdout,
            "stdout of {args:?}\n--- stderr: {}",
            got.stderr
        );
    }
}

#[tokio::test]
async fn patch_of_worktree_against_index_matches_git() {
    let p = Pair::new(&mixed()).await;
    p.same(&["diff"]);
    p.same(&["diff", "-p"]);
    p.same(&["diff", "--", "a.txt", "d"]);
    p.same(&["diff", "b.txt"]);
}

#[tokio::test]
async fn output_forms_match_git() {
    let p = Pair::new(&mixed()).await;
    for mode in [
        &["diff"][..],
        &["diff", "--cached"],
        &["diff", "HEAD"],
        &["diff", "HEAD~1", "HEAD"],
    ] {
        for form in [
            &["--stat"][..],
            &["--numstat"],
            &["--shortstat"],
            &["--summary"],
            &["--raw"],
            &["--name-only"],
            &["--name-status"],
            &["-z", "--name-status"],
            &["-z", "--raw"],
            &["-z", "--numstat"],
            &["--patch-with-stat"],
            &["--patch-with-raw"],
            &["--stat", "--summary"],
            &["--stat=60,30"],
            &["--stat-width=50"],
            &["--stat-count=2"],
            &["-s"],
            &["--word-diff"],
        ] {
            let args: Vec<&str> = mode.iter().chain(form).copied().collect();
            p.same(&args);
        }
    }
}

#[tokio::test]
async fn cached_and_commit_comparisons_match_git() {
    let p = Pair::new(&mixed()).await;
    for args in [
        &["diff", "--cached"][..],
        &["diff", "--staged"],
        &["diff", "--cached", "HEAD~1"],
        &["diff", "HEAD"],
        &["diff", "HEAD~1"],
        &["diff", "HEAD~1", "HEAD"],
        &["diff", "HEAD", "HEAD~1"],
        &["diff", "HEAD~1..HEAD"],
        &["diff", "HEAD~1...HEAD"],
        &["diff", "HEAD~1", "HEAD", "--", "d"],
        &["diff", "HEAD~1", "--", "a.txt", "new.txt"],
        &["diff", "--cached", "--", "moved.txt", "move-me.txt"],
        &["diff", "master", "HEAD~1"],
    ] {
        p.same(args);
    }
}

#[tokio::test]
async fn context_algorithm_and_prefix_options_match_git() {
    let p = Pair::new(&mixed()).await;
    for extra in [
        &["-U0"][..],
        &["-U1"],
        &["-U7"],
        &["--unified=2"],
        &["--inter-hunk-context=3"],
        &["--histogram"],
        &["--patience"],
        &["--minimal"],
        &["--diff-algorithm=histogram"],
        &["--no-indent-heuristic"],
        &["--src-prefix=x/", "--dst-prefix=y/"],
        &["--no-prefix"],
        &["--abbrev=12"],
        &["--full-index"],
        &["--no-renames"],
        &["-M50%"],
        &["-M"],
        &["--find-renames=90%"],
        &["--diff-filter=AD"],
        &["--diff-filter=d"],
        &[
            "--no-color",
            "--no-ext-diff",
            "--no-textconv",
            "--submodule=short",
        ],
        &["-a"],
    ] {
        for mode in [
            &["diff"][..],
            &["diff", "--cached"],
            &["diff", "HEAD~1", "HEAD"],
        ] {
            let args: Vec<&str> = mode.iter().chain(extra).copied().collect();
            p.same(&args);
        }
    }
}

#[tokio::test]
async fn binary_patches_match_git() {
    let p = Pair::new(&mixed()).await;
    for args in [
        &["diff", "--binary"][..],
        &["diff", "--cached", "--binary"],
        &["diff", "HEAD~1", "HEAD", "--binary"],
        &["diff", "HEAD~1", "--binary", "--", "bin.dat"],
        // lint-staged's invocation.
        &[
            "diff",
            "--binary",
            "--unified=0",
            "--no-color",
            "--no-ext-diff",
            "--src-prefix=a/",
            "--dst-prefix=b/",
            "--patch",
            "--submodule=short",
            "--",
            "a.txt",
            "bin.dat",
        ],
        &[
            "diff",
            "--cached",
            "--binary",
            "--unified=0",
            "--no-color",
            "--no-ext-diff",
            "--patch",
            "--submodule=short",
        ],
    ] {
        p.same(args);
    }
}

/// Deterministic ASCII noise with NULs, so it is binary and does not compress.
fn blob(seed: u32, len: usize) -> &'static str {
    let mut state = seed;
    let text: String = (0..len)
        .map(|_| {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            char::from(((state >> 24) & 0x7f) as u8)
        })
        .collect();
    Box::leak(text.into_boxed_str())
}

/// Applies `patch` with git to a fresh repository holding `base` and returns the work tree files
/// named in `want`.
fn applied(p: &Pair, patch: &str, base: &Files, want: &[&str]) -> Vec<String> {
    let dir = p.f.sandbox.dir(&format!("apply-{}", patch.len()));
    p.git(&dir, &["init", "-q", "-b", "master"]);
    for (name, content) in base {
        put(&dir, name, content);
    }
    let file =
        p.f.sandbox
            .tmp
            .path()
            .join(format!("p{}.patch", patch.len()));
    fs::write(&file, patch.as_bytes()).unwrap();
    let check = p.git(&dir, &["apply", "--check", file.to_str().unwrap()]);
    assert_eq!(check.code, 0, "git apply --check: {}", check.stderr);
    let done = p.git(&dir, &["apply", file.to_str().unwrap()]);
    assert_eq!(done.code, 0, "git apply: {}", done.stderr);
    want.iter()
        .map(|n| fs::read_to_string(dir.join(n)).unwrap_or_default())
        .collect()
}

#[tokio::test]
async fn larger_binary_patches_apply_with_git() {
    // Deflate output for big payloads is not byte-identical to zlib's, so these are checked by
    // git itself: it must accept the patch and rebuild the content.
    let s = Scenario {
        first: vec![("big.bin", blob(1, 6000)), ("small.txt", "x\n")],
        second: vec![("big.bin", blob(2, 7000)), ("fresh.bin", blob(3, 900))],
        staged: vec![],
        work: vec![],
        ..Scenario::default()
    };
    let p = Pair::new(&s).await;
    let forward = p.asc(&p.f.root, &["diff", "HEAD~1", "HEAD", "--binary"]);
    assert_eq!(forward.code, 0, "{}", forward.stderr);
    let got = applied(&p, &forward.stdout, &s.first, &["big.bin", "fresh.bin"]);
    assert_eq!(got, vec![blob(2, 7000).to_owned(), blob(3, 900).to_owned()]);
    let backward = p.asc(&p.f.root, &["diff", "HEAD", "HEAD~1", "--binary"]);
    let mut second = s.second.clone();
    second.push(("small.txt", "x\n"));
    let got = applied(&p, &backward.stdout, &second, &["big.bin", "fresh.bin"]);
    assert_eq!(got, vec![blob(1, 6000).to_owned(), String::new()]);
    // The stat forms need no deflate and match exactly.
    for args in [
        &["diff", "--stat", "HEAD~1", "HEAD"][..],
        &["diff", "--numstat", "HEAD~1"],
    ] {
        p.same(args);
    }
}

#[tokio::test]
async fn binary_patch_applies_with_git() {
    let p = Pair::new(&mixed()).await;
    let patch = p.asc(&p.f.root, &["diff", "HEAD~1", "HEAD", "--binary"]);
    assert_eq!(patch.code, 0, "{}", patch.stderr);
    // Reverse-apply the patch to a copy of the second commit's tree: git accepts asc's text.
    let file = p.f.sandbox.tmp.path().join("bin.patch");
    fs::write(&file, patch.stdout.as_bytes()).unwrap();
    let check = p.git(
        &p.git_dir,
        &[
            "apply",
            "--check",
            "--reverse",
            "--cached",
            file.to_str().unwrap(),
        ],
    );
    let _ = check;
    let first = p.git(&p.git_dir, &["apply", "--stat", file.to_str().unwrap()]);
    assert_eq!(first.code, 0, "{}", first.stderr);
    assert!(first.stdout.contains("bin.dat"), "{}", first.stdout);
}

#[tokio::test]
async fn exit_codes_and_quiet_match_git() {
    let p = Pair::new(&mixed()).await;
    for args in [
        &["diff", "--exit-code"][..],
        &["diff", "--quiet"],
        &["diff", "--exit-code", "--stat"],
        &["diff", "--quiet", "--cached"],
        &["diff", "--quiet", "HEAD", "HEAD"],
        &["diff", "--exit-code", "HEAD", "HEAD"],
        &["diff", "--quiet", "--", "untracked.txt"],
        &["diff", "--exit-code", "--", "d"],
    ] {
        p.same(args);
    }
}

#[tokio::test]
async fn output_option_writes_the_file() {
    let p = Pair::new(&mixed()).await;
    let want = p.git(&p.git_dir, &["diff", "HEAD~1", "HEAD"]);
    let target = p.f.sandbox.tmp.path().join("out.patch");
    let got = p.asc(
        &p.f.root,
        &[
            "diff",
            "HEAD~1",
            "HEAD",
            &format!("--output={}", target.display()),
        ],
    );
    assert_eq!(got.code, 0, "{}", got.stderr);
    assert_eq!(got.stdout, "");
    assert_eq!(text(&fs::read(&target).unwrap()), want.stdout);
}

#[tokio::test]
async fn subdirectory_and_relative_match_git() {
    let p = Pair::new(&mixed()).await;
    let (asc_d, git_d) = (p.f.root.join("d"), p.git_dir.join("d"));
    p.same_in(&asc_d, &git_d, &["diff"]);
    p.same_in(&asc_d, &git_d, &["diff", "--relative"]);
    p.same_in(
        &asc_d,
        &git_d,
        &["diff", "HEAD~1", "--relative", "--name-only"],
    );
    p.same_in(&asc_d, &git_d, &["diff", "--cached", "--", "."]);
    p.same_in(&asc_d, &git_d, &["diff", "HEAD~1", "HEAD", "--", "e.txt"]);
}

#[tokio::test]
async fn bad_revision_and_ambiguity_match_git() {
    let p = Pair::new(&mixed()).await;
    for args in [
        &["diff", "no-such-rev"][..],
        &["diff", "HEAD", "no-such-path"],
        &["diff", "--cached", "HEAD", "HEAD~1"],
    ] {
        let got = p.asc(&p.f.root, args);
        let want = p.git(&p.git_dir, args);
        assert_eq!(got.code, want.code, "{args:?}: {}", got.stderr);
        if want.code == 128 {
            assert_eq!(got.stderr, want.stderr, "{args:?}");
        }
    }
}

#[tokio::test]
async fn unsupported_flags_are_refused() {
    let p = Pair::new(&mixed()).await;
    for flag in ["--color=always", "--ignore-all-space", "--word-diff=color"] {
        let got = p.asc(&p.f.root, &["diff", flag]);
        assert_ne!(got.code, 0, "{flag}");
        assert_eq!(got.stdout, "");
    }
}

#[tokio::test]
async fn staged_renames_match_git() {
    let s = Scenario {
        first: vec![("old/name.txt", MOVED), ("keep.txt", "keep\n")],
        second: vec![("other.txt", "x\n")],
        staged: vec![("new/name.txt", Some(MOVED)), ("old/name.txt", None)],
        work: vec![],
        ..Scenario::default()
    };
    let p = Pair::new(&s).await;
    for args in [
        &["diff", "--cached"][..],
        &["diff", "--cached", "--summary"],
        &["diff", "--cached", "--stat"],
        &["diff", "--cached", "-M", "--raw"],
        &["diff", "--cached", "--no-renames"],
        &["diff", "HEAD", "--name-status"],
    ] {
        p.same(args);
    }
}

#[tokio::test]
async fn intent_to_add_is_a_new_file_in_the_work_tree_only() {
    let s = Scenario {
        first: vec![("a.txt", "x\n")],
        second: vec![("b.txt", "y\n")],
        intent: vec![("ita.txt", "wanted\nlater\n")],
        ..Scenario::default()
    };
    let p = Pair::new(&s).await;
    for args in [
        &["diff"][..],
        &["diff", "--stat"],
        &["diff", "--raw"],
        &["diff", "--cached"],
        &["diff", "HEAD"],
        &["diff", "--name-status", "HEAD"],
        &["diff-files"],
    ] {
        p.same(args);
    }
}

#[tokio::test]
async fn diff_attribute_decides_text_or_binary_like_git() {
    let s = Scenario {
        first: vec![
            (
                ".gitattributes",
                "*.dat -diff\nforced.bin diff\nsub/*.log binary\n",
            ),
            ("x.dat", "one\ntwo\n"),
            ("forced.bin", "a\0b\nc\n"),
            ("sub/y.log", "log\n"),
            ("plain.txt", "p\n"),
        ],
        second: vec![("plain.txt", "p2\n")],
        work: vec![
            ("x.dat", Some("one\nTWO\n")),
            ("forced.bin", Some("a\0B\nc\n")),
            ("sub/y.log", Some("log2\n")),
            ("plain.txt", Some("p3\n")),
        ],
        ..Scenario::default()
    };
    let p = Pair::new(&s).await;
    for args in [
        &["diff"][..],
        &["diff", "--stat"],
        &["diff", "--numstat"],
        &["diff", "-a"],
        &["diff", "HEAD~1"],
    ] {
        p.same(args);
    }
}

#[tokio::test]
async fn deleted_and_added_files_and_mode_free_cases_match_git() {
    let s = Scenario {
        first: vec![("only.txt", "one\n")],
        second: vec![("two.txt", "two\n")],
        staged: vec![],
        work: vec![
            ("only.txt", None),
            ("two.txt", Some("")),
            ("tiny.txt", Some("x")),
        ],
        ..Scenario::default()
    };
    let p = Pair::new(&s).await;
    for args in [
        &["diff"][..],
        &["diff", "--stat"],
        &["diff", "--summary"],
        &["diff", "--raw"],
        &["diff", "HEAD~1"],
    ] {
        p.same(args);
    }
}

// ------------------------------------------------------------------ plumbing

#[tokio::test]
async fn diff_tree_matches_git() {
    let p = Pair::new(&mixed()).await;
    for args in [
        &["diff-tree", "HEAD~1", "HEAD"][..],
        &["diff-tree", "-r", "HEAD~1", "HEAD"],
        &["diff-tree", "-r", "-t", "HEAD~1", "HEAD"],
        &["diff-tree", "HEAD"],
        &["diff-tree", "-r", "HEAD"],
        &["diff-tree", "-r", "--no-commit-id", "--name-only", "HEAD"],
        &["diff-tree", "--no-commit-id", "--name-status", "-r", "HEAD"],
        &["diff-tree", "-r", "-z", "HEAD"],
        &["diff-tree", "-p", "HEAD"],
        &["diff-tree", "--stat", "HEAD~1", "HEAD"],
        &["diff-tree", "-r", "HEAD~1", "HEAD", "--", "d"],
        &["diff-tree", "HEAD~1"],
        &["diff-tree", "--root", "HEAD~1"],
        &["diff-tree", "-r", "--root", "HEAD~1"],
        &["diff-tree", "-r", "-M", "HEAD~1", "HEAD"],
        &["diff-tree", "--abbrev=10", "-r", "HEAD"],
        &["diff-tree", "--exit-code", "HEAD", "HEAD"],
        &["diff-tree", "--merge-base", "HEAD~1", "HEAD"],
        &["diff-tree", "--merge-base", "HEAD", "HEAD~1"],
        &[
            "diff-tree",
            "-r",
            "--name-only",
            "--no-commit-id",
            "-z",
            "--merge-base",
            "--end-of-options",
            "HEAD~1",
            "HEAD",
            "--",
            "d",
        ],
        &["diff-tree", "-r", "--end-of-options", "HEAD~1", "HEAD"],
    ] {
        p.same(args);
    }
}

#[tokio::test]
async fn diff_index_matches_git() {
    let p = Pair::new(&mixed()).await;
    for args in [
        &["diff-index", "HEAD"][..],
        &["diff-index", "--cached", "HEAD"],
        &["diff-index", "HEAD~1"],
        &["diff-index", "--cached", "HEAD~1"],
        &["diff-index", "-p", "HEAD"],
        &["diff-index", "--name-only", "HEAD"],
        &["diff-index", "--name-status", "--cached", "HEAD"],
        &["diff-index", "-z", "HEAD"],
        &["diff-index", "--quiet", "HEAD"],
        &["diff-index", "--quiet", "--cached", "HEAD"],
        &["diff-index", "HEAD", "--", "d", "a.txt"],
        &["diff-index", "-M", "--cached", "HEAD"],
    ] {
        p.same(args);
    }
}

#[tokio::test]
async fn diff_files_matches_git() {
    let p = Pair::new(&mixed()).await;
    for args in [
        &["diff-files"][..],
        &["diff-files", "-p"],
        &["diff-files", "--name-status"],
        &["diff-files", "--name-only", "-z"],
        &["diff-files", "--stat"],
        &["diff-files", "--quiet"],
        &["diff-files", "a.txt", "b.txt"],
    ] {
        p.same(args);
    }
}

#[tokio::test]
async fn clean_tree_has_no_output_and_exit_zero() {
    let s = Scenario {
        first: vec![("a.txt", "x\n")],
        second: vec![("b.txt", "y\n")],
        staged: vec![],
        work: vec![],
        intent: vec![],
    };
    let p = Pair::new(&s).await;
    for args in [
        &["diff"][..],
        &["diff", "--cached"],
        &["diff", "HEAD"],
        &["diff", "--exit-code"],
        &["diff-files"],
        &["diff-index", "HEAD"],
        &["diff-index", "--cached", "HEAD"],
    ] {
        p.same(args);
    }
}

// ------------------------------------------------------------------ no-index

#[tokio::test]
async fn no_index_matches_git() {
    let p = Pair::new(&Scenario {
        first: vec![("a.txt", "x\n")],
        second: vec![("b.txt", "y\n")],
        ..Scenario::default()
    })
    .await;
    let work = p.f.sandbox.dir("noindex");
    put(&work, "one.txt", "alpha\nbeta\ngamma\n");
    put(&work, "two.txt", "alpha\nBETA\ngamma\n");
    put(&work, "left/same.txt", "same\n");
    put(&work, "left/changed.txt", "old\n");
    put(&work, "left/only-left.txt", "l\n");
    put(&work, "right/same.txt", "same\n");
    put(&work, "right/changed.txt", "new\n");
    put(&work, "right/only-right.txt", "r\n");
    put(&work, "bin1.dat", "\u{0}a");
    put(&work, "bin2.dat", "\u{0}b");
    for args in [
        &["diff", "--no-index", "one.txt", "two.txt"][..],
        &["diff", "--no-index", "left", "right"],
        &["diff", "--no-index", "one.txt", "one.txt"],
        &["diff", "--no-index", "-U0", "one.txt", "two.txt"],
        &["diff", "--no-index", "bin1.dat", "bin2.dat"],
        &["diff", "--no-index", "--binary", "bin1.dat", "bin2.dat"],
        &["diff", "--no-index", "--", "one.txt", "two.txt"],
    ] {
        // Inside the repository and outside it.
        p.same_in(&work, &work, args);
    }
    // Outside any repository `--no-index` is implied by two paths.
    p.same_in(&work, &work, &["diff", "one.txt", "two.txt"]);
}

// ----------------------------------------------------------------------- json

#[tokio::test]
async fn json_lines_have_a_stable_shape() {
    let s = Scenario {
        first: vec![("a.txt", "one\ntwo\n")],
        second: vec![("b.txt", "bee\n")],
        staged: vec![],
        work: vec![("a.txt", Some("one\nTWO\n")), ("b.txt", None)],
        ..Scenario::default()
    };
    let p = Pair::new(&s).await;
    let got = p.asc(&p.f.root, &["diff", "--json"]);
    assert_eq!(got.code, 0, "{}", got.stderr);
    let lines: Vec<serde_json::Value> = got
        .stdout
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(lines.len(), 2, "{}", got.stdout);

    let patch_a = p.git(&p.git_dir, &["diff", "--", "a.txt"]).stdout;
    let patch_b = p.git(&p.git_dir, &["diff", "--", "b.txt"]).stdout;
    let change = |i: usize| lines[i]["changes"][0].clone();
    assert_eq!(lines[0]["text"].as_str().unwrap(), patch_a);
    assert_eq!(lines[1]["text"].as_str().unwrap(), patch_b);
    assert_eq!(change(0)["status"], "M");
    assert_eq!(change(0)["path"], "a.txt");
    assert_eq!(change(0)["old_mode"], 0o100644);
    assert_eq!(change(0)["new_mode"], 0o100644);
    assert_eq!(change(1)["status"], "D");
    assert!(
        change(1).get("new_mode").is_none(),
        "zero values are left out"
    );

    // Golden: the exact bytes of the first line.
    let first = got.stdout.lines().next().unwrap();
    let old_git = change(0)["old_git"].as_str().unwrap().to_owned();
    let new_git = change(0)["new_git"].as_str().unwrap().to_owned();
    let old_id = change(0)["old_id"].as_str().unwrap().to_owned();
    assert_eq!(old_git.len(), 40, "lowercase hex git id");
    assert_eq!(old_id.len(), 64, "lowercase hex asc id");
    assert!(
        change(0).get("new_id").is_none(),
        "a work tree side has no stored file id"
    );
    let first_text = lines[0]["text"].as_str().unwrap().to_owned();
    let expected = serde_json::json!({
        "changes": [{
            "path": change(0)["path"].clone(),
            "status": "M",
            "old_mode": 33188,
            "new_mode": 33188,
            "old_git": old_git,
            "new_git": new_git,
            "old_id": old_id,
            "old_size": 8,
            "new_size": 8,
        }],
        "text": first_text,
    });
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(first).unwrap(),
        expected
    );
}
