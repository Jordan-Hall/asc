//! U21d acceptance: `reset` and `restore` through the built binary. The repositories are built
//! with the fixture (no sibling command exists yet), so staged state is read back from
//! `state.db` and files from the working tree; stdout is compared with real git where the
//! scenario can be replayed in a git repository.

mod support;

use std::fs;
use std::path::Path;
use std::process::Command;

use asc_core::id::{ChangesetId, RefTarget};
use asc_core::path::RefName;
use asc_state::state::stage::{self, StageRow, StageSet};
use asc_store::refs::Refs;
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

fn asc(f: &Fixture, args: &[&str]) -> Out {
    run(f.asc().args(args))
}

fn put(root: &Path, rel: &str, content: &str) {
    let path = root.join(rel);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, content).unwrap();
}

fn read(root: &Path, rel: &str) -> String {
    fs::read_to_string(root.join(rel)).unwrap()
}

/// `a`: f.txt "one", g.txt "keep"; `b`: f.txt "two", h.txt "new". The branch and the working
/// tree are at `b`.
async fn base() -> (Fixture, ChangesetId, ChangesetId) {
    let f = Fixture::new().await;
    let a = f.commit(&[], &[("f.txt", b"one\n"), ("g.txt", b"keep\n")]).await;
    let b = f.commit(&[a], &[("f.txt", b"two\n"), ("h.txt", b"new\n")]).await;
    f.set_branch("master", b).await;
    f.checkout_branch("master", Some(b)).await;
    put(&f.root, "f.txt", "two\n");
    put(&f.root, "g.txt", "keep\n");
    put(&f.root, "h.txt", "new\n");
    (f, a, b)
}

async fn branch(f: &Fixture) -> ChangesetId {
    let name = RefName::new(b"refs/heads/master".to_vec()).unwrap();
    let entry = f.db.refs().get(f.repo, &name).await.unwrap().expect("master");
    match entry.target {
        RefTarget::Changeset(c) => c,
        RefTarget::Tag(_) => panic!("master is a tag"),
    }
}

async fn staged(f: &Fixture) -> Vec<StageRow> {
    stage::overlay(&f.db, f.ws, StageSet::Default).await.unwrap()
}

/// Real git in a directory, with hermetic configuration.
fn git(dir: &Path, home: &Path, args: &[&str]) -> Out {
    run(Command::new("git") // lint:allow-raw-git
        .current_dir(dir)
        .env("HOME", home)
        .env("USERPROFILE", home)
        .env("GIT_CONFIG_GLOBAL", home.join(".gitconfig"))
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "T")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_NAME", "T")
        .env("GIT_COMMITTER_EMAIL", "t@example.com")
        .args(["-c", "core.autocrlf=false"])
        .args(args))
}

/// The same history as [`base`] in a git repository, with the working tree at `b`.
fn git_base(f: &Fixture) -> std::path::PathBuf {
    let dir = f.sandbox.dir("gitrepo");
    let home = &f.sandbox.user;
    git(&dir, home, &["init", "-q", "-b", "master"]);
    put(&dir, "f.txt", "one\n");
    put(&dir, "g.txt", "keep\n");
    git(&dir, home, &["add", "."]);
    git(&dir, home, &["commit", "-q", "-m", "commit of 2 files"]);
    put(&dir, "f.txt", "two\n");
    put(&dir, "h.txt", "new\n");
    git(&dir, home, &["add", "."]);
    git(&dir, home, &["commit", "-q", "-m", "commit of 2 files"]);
    dir
}

#[tokio::test]
async fn soft_moves_the_branch_and_keeps_the_commit_staged() {
    let (f, a, b) = base().await;
    let out = asc(&f, &["reset", "--soft", "HEAD~1"]);
    assert_eq!((out.code, out.stdout.as_str()), (0, ""), "{}", out.stderr);
    assert_eq!(branch(&f).await, a);

    // The effective index still equals `b`: its changes show as staged rows over `a`.
    let rows = staged(&f).await;
    let paths: Vec<_> = rows.iter().map(|r| r.path.clone()).collect();
    assert_eq!(paths, [b"f.txt".to_vec(), b"h.txt".to_vec()]);
    let at_b = f.file_leaf(b, "f.txt").await;
    assert_eq!(rows[0].file, Some(at_b.file));
    assert_eq!(rows[0].git, Some(at_b.git));

    let orig = fs::read_to_string(
        asc_git::projection::Projection::open(&f.root).unwrap().git_dir().join("ORIG_HEAD"),
    )
    .unwrap();
    assert_eq!(orig.trim(), f.engine.git_commit_of(b).await.unwrap().to_string());
    assert_eq!(read(&f.root, "f.txt"), "two\n", "the working tree is untouched");
}

#[tokio::test]
async fn mixed_resets_the_index_and_reports_what_git_reports() {
    let (f, a, _) = base().await;
    let out = asc(&f, &["reset", "HEAD~1"]);
    assert_eq!(out.code, 0, "{}", out.stderr);
    assert_eq!(branch(&f).await, a);
    assert!(staged(&f).await.is_empty(), "the index equals the new HEAD");
    assert_eq!(read(&f.root, "f.txt"), "two\n", "the working tree is untouched");

    let dir = git_base(&f);
    let want = git(&dir, &f.sandbox.user, &["reset", "HEAD~1"]);
    assert_eq!(out.stdout, want.stdout);
    assert_eq!(out.stdout, "Unstaged changes after reset:\nM\tf.txt\n");
}

#[tokio::test]
async fn hard_with_no_head_change_restores_modified_deleted_and_staged_files() {
    let (f, a, _) = base().await;
    put(&f.root, "f.txt", "scribble\n");
    fs::remove_file(f.root.join("h.txt")).unwrap();
    put(&f.root, "added.txt", "staged only\n");
    let leaf = f.file_leaf(a, "f.txt").await;
    f.stage(&[StageRow::from_leaf(b"added.txt".to_vec(), Some(&leaf))]).await;

    let out = asc(&f, &["reset", "--hard"]);
    assert_eq!(out.code, 0, "{}", out.stderr);
    assert!(out.stdout.starts_with("HEAD is now at "), "{}", out.stdout);
    assert!(out.stdout.ends_with(" commit of 2 files\n"), "{}", out.stdout);
    assert_eq!(read(&f.root, "f.txt"), "two\n");
    assert_eq!(read(&f.root, "h.txt"), "new\n");
    assert!(!f.root.join("added.txt").exists(), "a staged-only file is removed");
    assert!(staged(&f).await.is_empty());

    // Same scenario in git.
    let dir = git_base(&f);
    let home = &f.sandbox.user;
    put(&dir, "f.txt", "scribble\n");
    fs::remove_file(dir.join("h.txt")).unwrap();
    put(&dir, "added.txt", "staged only\n");
    git(&dir, home, &["add", "added.txt"]);
    let want = git(&dir, home, &["reset", "--hard"]);
    for file in ["f.txt", "g.txt", "h.txt"] {
        assert_eq!(read(&f.root, file), read(&dir, file), "{file}");
    }
    assert_eq!(f.root.join("added.txt").exists(), dir.join("added.txt").exists());
    let tail = |s: &str| s.split_once(' ').map(|_| s.split(' ').skip(5).collect::<Vec<_>>().join(" "));
    assert_eq!(tail(&out.stdout), tail(&want.stdout), "subject after the abbreviated id");
}

#[tokio::test]
async fn hard_to_an_earlier_commit_rewrites_the_tree() {
    let (f, a, _) = base().await;
    let out = asc(&f, &["reset", "--hard", "HEAD~1"]);
    assert_eq!(out.code, 0, "{}", out.stderr);
    assert_eq!(branch(&f).await, a);
    assert_eq!(read(&f.root, "f.txt"), "one\n");
    assert_eq!(read(&f.root, "g.txt"), "keep\n");
    assert!(!f.root.join("h.txt").exists());
    assert!(staged(&f).await.is_empty());
}

#[tokio::test]
async fn path_reset_unstages_only_those_paths() {
    let (f, a, b) = base().await;
    f.set_branch("master", a).await;
    f.checkout_branch("master", Some(a)).await;
    let (lf, lh) = (f.file_leaf(b, "f.txt").await, f.file_leaf(b, "h.txt").await);
    f.stage(&[
        StageRow::from_leaf(b"f.txt".to_vec(), Some(&lf)),
        StageRow::from_leaf(b"h.txt".to_vec(), Some(&lh)),
    ])
    .await;
    let out = asc(&f, &["reset", "HEAD", "--", "f.txt"]);
    assert_eq!(out.code, 0, "{}", out.stderr);
    assert_eq!(branch(&f).await, a, "no ref moves");
    let rows = staged(&f).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].path, b"h.txt");
    // Without `--`, a path is recognised when it is not a revision.
    assert_eq!(asc(&f, &["reset", "h.txt"]).code, 0);
    assert!(staged(&f).await.is_empty());
}

#[tokio::test]
async fn restore_worktree_takes_the_index_version() {
    let (f, _, _) = base().await;
    put(&f.root, "f.txt", "scribble\n");
    put(&f.root, "h.txt", "also scribbled\n");
    fs::remove_file(f.root.join("g.txt")).unwrap();
    let out = asc(&f, &["restore", "f.txt", "g.txt"]);
    assert_eq!((out.code, out.stdout.as_str()), (0, ""), "{}", out.stderr);
    assert_eq!(read(&f.root, "f.txt"), "two\n");
    assert_eq!(read(&f.root, "g.txt"), "keep\n");
    assert_eq!(read(&f.root, "h.txt"), "also scribbled\n", "unnamed paths are left alone");
    assert_eq!(asc(&f, &["restore", "."]).code, 0);
    assert_eq!(read(&f.root, "h.txt"), "new\n");
}

#[tokio::test]
async fn restore_staged_returns_the_index_to_head() {
    let (f, a, b) = base().await;
    f.set_branch("master", a).await;
    f.checkout_branch("master", Some(a)).await;
    let leaf = f.file_leaf(b, "f.txt").await;
    f.stage(&[StageRow::from_leaf(b"f.txt".to_vec(), Some(&leaf))]).await;
    let out = asc(&f, &["restore", "--staged", "f.txt"]);
    assert_eq!(out.code, 0, "{}", out.stderr);
    assert!(staged(&f).await.is_empty());
    assert_eq!(read(&f.root, "f.txt"), "two\n", "--staged leaves the working tree alone");
}

#[tokio::test]
async fn restore_source_rewrites_and_removes_files() {
    let (f, _, _) = base().await;
    let out = asc(&f, &["restore", "--source=HEAD~1", "f.txt", "h.txt"]);
    assert_eq!(out.code, 0, "{}", out.stderr);
    assert_eq!(read(&f.root, "f.txt"), "one\n");
    assert!(!f.root.join("h.txt").exists(), "absent in the source: removed");

    let out = asc(&f, &["restore", "-S", "-W", "--source=HEAD~1", "g.txt"]);
    assert_eq!(out.code, 0, "{}", out.stderr);
}

#[tokio::test]
async fn restore_resolves_paths_from_the_current_directory() {
    let f = Fixture::new().await;
    let a = f.commit(&[], &[("sub/a.txt", b"in sub\n"), ("a.txt", b"root\n")]).await;
    f.set_branch("master", a).await;
    f.checkout_branch("master", Some(a)).await;
    put(&f.root, "sub/a.txt", "x\n");
    put(&f.root, "a.txt", "y\n");
    let sub = f.root.join("sub");
    let out = run(f.sandbox.asc(&sub).args(["restore", "a.txt"]));
    assert_eq!(out.code, 0, "{}", out.stderr);
    assert_eq!(read(&f.root, "sub/a.txt"), "in sub\n");
    assert_eq!(read(&f.root, "a.txt"), "y\n");
    let out = run(f.sandbox.asc(&sub).args(["restore", "../a.txt"]));
    assert_eq!(out.code, 0, "{}", out.stderr);
    assert_eq!(read(&f.root, "a.txt"), "root\n");
}

#[tokio::test]
async fn errors_match_git() {
    let (f, _, _) = base().await;
    let out = asc(&f, &["restore"]);
    assert_eq!(out.code, 128);
    assert!(out.stderr.contains("fatal: you must specify path(s) to restore"), "{}", out.stderr);

    let out = asc(&f, &["restore", "nosuch"]);
    assert_eq!(out.code, 1);
    assert!(
        out.stderr.contains("error: pathspec 'nosuch' did not match any file(s) known to git"),
        "{}",
        out.stderr
    );

    let out = asc(&f, &["reset", "--soft", "--", "f.txt"]);
    assert_eq!(out.code, 128);
    assert!(out.stderr.contains("fatal: Cannot do soft reset with paths."), "{}", out.stderr);

    let out = asc(&f, &["reset", "--hard", "HEAD", "--", "f.txt"]);
    assert_eq!(out.code, 128);
    assert!(out.stderr.contains("fatal: Cannot do hard reset with paths."), "{}", out.stderr);

    let out = asc(&f, &["reset", "nosuch"]);
    assert_eq!(out.code, 128);
    assert!(out.stderr.contains("fatal: ambiguous argument 'nosuch'"), "{}", out.stderr);

    // Flags outside the native table are refused before anything runs.
    assert_eq!(asc(&f, &["reset", "--keep"]).code, 129);
    assert_eq!(asc(&f, &["restore", "--patch", "f.txt"]).code, 129);
}
