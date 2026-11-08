//! Link mode (`asc.worktree.link = symlink`) through the built binary: extra plain worktrees share
//! one read-only copy of each unchanged file, reads and status see the files, a save replaces the
//! link, `asc gc` prunes the copies no link names, and bad settings are refused.
//!
//! A machine without symbolic link rights (Windows without Developer Mode) checks only that the
//! checkout falls back to plain copies.

mod support;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Output;

use support::{Sandbox, text};

const IDENT: [(&str, &str); 6] = [
    ("GIT_AUTHOR_NAME", "Test"),
    ("GIT_AUTHOR_EMAIL", "test@example.com"),
    ("GIT_AUTHOR_DATE", "1700000000 +0000"),
    ("GIT_COMMITTER_NAME", "Test"),
    ("GIT_COMMITTER_EMAIL", "test@example.com"),
    ("GIT_COMMITTER_DATE", "1700000000 +0000"),
];

/// `asc args…` with the daemon off, so every worktree is plain.
fn asc(sb: &Sandbox, cwd: &Path, args: &[&str]) -> Output {
    sb.asc(cwd)
        .envs(IDENT)
        .env("ASC_NO_DAEMON", "1")
        .args(args)
        .output()
        .expect("spawn asc")
}

fn ok(sb: &Sandbox, cwd: &Path, args: &[&str]) -> String {
    let out = asc(sb, cwd, args);
    assert!(
        out.status.success(),
        "asc {args:?} failed: {}",
        text(&out.stderr)
    );
    text(&out.stdout)
}

fn bucket_url(dir: &Path) -> String {
    url::Url::from_directory_path(dir)
        .unwrap()
        .to_string()
        .trim_end_matches('/')
        .to_owned()
}

/// A plain clone of a bucket holding `master` (a.txt, src/b.txt).
fn clone(sb: &Sandbox) -> PathBuf {
    let src = sb.dir("src-repo");
    ok(sb, &src, &["init", "-q"]);
    fs::create_dir_all(src.join("src")).unwrap();
    fs::write(src.join("a.txt"), "a\n").unwrap();
    fs::write(src.join("src").join("b.txt"), "b\n").unwrap();
    ok(sb, &src, &["add", "."]);
    ok(sb, &src, &["commit", "-q", "-m", "base"]);
    let url = bucket_url(&sb.dir("bucket"));
    ok(sb, &src, &["remote", "add", "origin", &url]);
    ok(sb, &src, &["push", "-q", "origin", "master"]);
    let parent = sb.dir("work");
    ok(sb, &parent, &["clone", "-q", &url, "p"]);
    parent.join("p")
}

fn add_worktree(sb: &Sandbox, ws: &Path, name: &str) -> PathBuf {
    let wt = sb.tmp.path().join("wts").join(name);
    ok(
        sb,
        ws,
        &["worktree", "add", "-q", "-b", name, wt.to_str().unwrap()],
    );
    wt
}

fn area(sb: &Sandbox) -> PathBuf {
    sb.asc_home.join("shared-blobs")
}

#[test]
fn linked_worktrees_share_one_copy_and_gc_prunes_unlinked_ones() {
    let sb = Sandbox::new();
    let ws = clone(&sb);
    ok(&sb, &ws, &["config", "asc.worktree.link", "symlink"]);
    let w1 = add_worktree(&sb, &ws, "w1");
    let a1 = w1.join("a.txt");
    assert_eq!(fs::read_to_string(&a1).unwrap(), "a\n");
    if !fs::symlink_metadata(&a1).unwrap().file_type().is_symlink() {
        // No symbolic link rights here: the checkout fell back to plain copies.
        assert_eq!(ok(&sb, &w1, &["status", "--porcelain"]), "");
        return;
    }
    let target = fs::read_link(&a1).unwrap();
    assert!(target.starts_with(area(&sb)), "{}", target.display());
    assert!(fs::metadata(&target).unwrap().permissions().readonly());
    assert_eq!(ok(&sb, &w1, &["status", "--porcelain"]), "");

    let w2 = add_worktree(&sb, &ws, "w2");
    assert_eq!(fs::read_link(w2.join("a.txt")).unwrap(), target);
    assert_eq!(
        fs::read_to_string(w2.join("src").join("b.txt")).unwrap(),
        "b\n"
    );

    // An editor that saves by replacing the file replaces the link and leaves the copy alone.
    fs::remove_file(&a1).unwrap();
    fs::write(&a1, "a1\n").unwrap();
    assert_eq!(ok(&sb, &w1, &["status", "--porcelain"]), " M a.txt\n");
    assert_eq!(fs::read_to_string(w2.join("a.txt")).unwrap(), "a\n");
    ok(&sb, &w1, &["add", "a.txt"]);
    ok(&sb, &w1, &["commit", "-q", "-m", "edit"]);
    assert_eq!(ok(&sb, &w1, &["status", "--porcelain"]), "");
    assert!(ok(&sb, &w1, &["diff", "HEAD~1", "--", "a.txt"]).contains("+a1"));

    // Only w2 links the old a.txt; once it is gone, gc removes that copy and keeps src/b.txt.
    ok(
        &sb,
        &ws,
        &["worktree", "remove", "--force", w2.to_str().unwrap()],
    );
    ok(&sb, &ws, &["config", "asc.gc.sharedGrace", "0s"]);
    let dry = ok(&sb, &ws, &["gc", "--dry-run"]);
    assert!(
        dry.contains("shared: would remove 1 unlinked files"),
        "{dry}"
    );
    assert!(target.exists());
    let done = ok(&sb, &ws, &["gc"]);
    assert!(done.contains("shared: removed 1 unlinked files"), "{done}");
    assert!(!target.exists());
    assert_eq!(
        fs::read_to_string(w1.join("src").join("b.txt")).unwrap(),
        "b\n"
    );
    assert_eq!(ok(&sb, &w1, &["status", "--porcelain"]), "");
}

#[test]
fn gc_keeps_young_copies() {
    let sb = Sandbox::new();
    let ws = clone(&sb);
    ok(&sb, &ws, &["config", "asc.worktree.link", "symlink"]);
    let w1 = add_worktree(&sb, &ws, "w1");
    if !fs::symlink_metadata(w1.join("a.txt"))
        .unwrap()
        .file_type()
        .is_symlink()
    {
        return;
    }
    ok(
        &sb,
        &ws,
        &["worktree", "remove", "--force", w1.to_str().unwrap()],
    );
    // The default grace (an hour) keeps copies a checkout may not have linked yet.
    let done = ok(&sb, &ws, &["gc"]);
    assert!(done.contains("shared: removed 0 unlinked files"), "{done}");
}

#[test]
fn the_default_copy_mode_makes_no_links() {
    let sb = Sandbox::new();
    let ws = clone(&sb);
    let w1 = add_worktree(&sb, &ws, "w1");
    assert!(
        !fs::symlink_metadata(w1.join("a.txt"))
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert!(!area(&sb).exists());
}

#[test]
fn bad_settings_are_refused() {
    let sb = Sandbox::new();
    let ws = clone(&sb);
    ok(&sb, &ws, &["config", "asc.worktree.link", "hardlink"]);
    let wt = sb.tmp.path().join("wts").join("bad");
    let out = asc(
        &sb,
        &ws,
        &["worktree", "add", "-q", "-b", "bad", wt.to_str().unwrap()],
    );
    assert!(!out.status.success());
    assert!(
        text(&out.stderr).contains("invalid asc.worktree.link 'hardlink'; use copy or symlink"),
        "{}",
        text(&out.stderr)
    );
    ok(&sb, &ws, &["config", "asc.worktree.link", "copy"]);

    ok(&sb, &ws, &["config", "asc.worktree.mode", "lazy"]);
    let out = asc(
        &sb,
        &ws,
        &["worktree", "add", "-q", "-b", "bad2", wt.to_str().unwrap()],
    );
    assert!(!out.status.success());
    assert!(
        text(&out.stderr).contains("invalid asc.worktree.mode 'lazy'; use auto, virtual or plain"),
        "{}",
        text(&out.stderr)
    );
    ok(&sb, &ws, &["config", "--unset", "asc.worktree.mode"]);

    ok(&sb, &ws, &["config", "asc.gc.sharedGrace", "soon"]);
    fs::create_dir_all(area(&sb)).unwrap();
    let out = asc(&sb, &ws, &["gc"]);
    assert!(!out.status.success());
    assert!(
        text(&out.stderr).contains("invalid asc.gc.sharedGrace 'soon'"),
        "{}",
        text(&out.stderr)
    );
}
