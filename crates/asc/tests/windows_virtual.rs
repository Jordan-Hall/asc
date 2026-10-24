//! U58 acceptance: virtual (ProjFS) workspaces end to end through the built binary.
//! Run with `cargo test -p asc --test windows_virtual -- --ignored` on Windows with ProjFS.
//!
//! TODO(U58-wiring): `asc clone --mode virtual` and `asc worktree add --mode virtual` still
//! refuse ("need the daemon, which is not available yet") and nothing calls
//! `daemon::workspace_factory::register_virtual_backend`, so these tests fail until the CLI
//! (clone.rs, worktree.rs) and the daemon start-up route virtual mode to the ProjFS provider.
//! The `asc serve` source and the `virtual-*.t` compat scenarios need runner support for a
//! virtual mode (tests/compat/src, crates/asc/tests/compat.rs), which this unit does not own.
#![cfg(windows)]

mod support;

use std::path::Path;
use std::process::Output;
use std::time::{Duration, Instant};

use support::{Sandbox, text};

const IDENT: [(&str, &str); 6] = [
    ("GIT_AUTHOR_NAME", "Test"),
    ("GIT_AUTHOR_EMAIL", "test@example.com"),
    ("GIT_AUTHOR_DATE", "1700000000 +0000"),
    ("GIT_COMMITTER_NAME", "Test"),
    ("GIT_COMMITTER_EMAIL", "test@example.com"),
    ("GIT_COMMITTER_DATE", "1700000000 +0000"),
];

fn asc(sb: &Sandbox, cwd: &Path, args: &[&str]) -> Output {
    sb.asc(cwd).envs(IDENT).args(args).output().expect("spawn asc")
}

fn ok(sb: &Sandbox, cwd: &Path, args: &[&str]) -> String {
    let out = asc(sb, cwd, args);
    assert!(out.status.success(), "asc {args:?} failed: {}", text(&out.stderr));
    text(&out.stdout)
}

fn bucket_url(dir: &Path) -> String {
    url::Url::from_directory_path(dir)
        .unwrap()
        .to_string()
        .trim_end_matches('/')
        .to_owned()
}

/// A bucket holding `master` (a.txt, src/b.txt) and `side` (one more file), pushed from a
/// plain source repository; returns the bucket URL.
fn seed(sb: &Sandbox) -> String {
    let src = sb.dir("src-repo");
    ok(sb, &src, &["init", "-q"]);
    std::fs::create_dir_all(src.join("src")).unwrap();
    std::fs::write(src.join("a.txt"), "a\n").unwrap();
    std::fs::write(src.join("src").join("b.txt"), "b\n").unwrap();
    ok(sb, &src, &["add", "."]);
    ok(sb, &src, &["commit", "-q", "-m", "base"]);
    ok(sb, &src, &["switch", "-q", "-c", "side"]);
    std::fs::write(src.join("c.txt"), "c\n").unwrap();
    ok(sb, &src, &["add", "c.txt"]);
    ok(sb, &src, &["commit", "-q", "-m", "side"]);
    ok(sb, &src, &["switch", "-q", "master"]);
    let url = bucket_url(&sb.dir("bucket"));
    ok(sb, &src, &["remote", "add", "origin", &url]);
    ok(sb, &src, &["push", "-q", "origin", "master", "side"]);
    url
}

fn virtual_clone(sb: &Sandbox, url: &str) -> std::path::PathBuf {
    let parent = sb.dir("work");
    ok(sb, &parent, &["clone", "-q", "--mode", "virtual", url, "v"]);
    parent.join("v")
}

/// The total of hydrated downloads the daemon reports, from `asc debug hydrations --json`.
fn downloads(sb: &Sandbox, ws: &Path) -> u64 {
    let out = ok(sb, ws, &["debug", "hydrations", "--json"]);
    let v: serde_json::Value = serde_json::from_str(&out).expect("json");
    v["downloads"].as_u64().expect("downloads counter")
}

#[test]
#[ignore = "needs Windows with ProjFS"]
fn clone_edit_commit_switch_without_downloads() {
    let sb = Sandbox::new();
    let url = seed(&sb);
    let ws = virtual_clone(&sb, &url);
    assert_eq!(std::fs::read_to_string(ws.join("a.txt")).unwrap(), "a\n");

    let t = Instant::now();
    ok(&sb, &ws, &["status", "--porcelain"]);
    let first = t.elapsed();
    let t = Instant::now();
    assert_eq!(ok(&sb, &ws, &["status", "--porcelain"]), "");
    assert!(t.elapsed() < Duration::from_millis(50), "status took {:?} (first {first:?})", t.elapsed());

    std::fs::write(ws.join("a.txt"), "a2\n").unwrap();
    std::fs::write(ws.join("new.txt"), "n\n").unwrap();
    ok(&sb, &ws, &["add", "-A"]);
    ok(&sb, &ws, &["commit", "-q", "-m", "edit"]);
    assert_eq!(ok(&sb, &ws, &["status", "--porcelain"]), "");

    // Switching between branches whose objects are already local downloads nothing.
    let before = downloads(&sb, &ws);
    ok(&sb, &ws, &["switch", "-q", "side"]);
    assert_eq!(std::fs::read_to_string(ws.join("c.txt")).unwrap(), "c\n");
    ok(&sb, &ws, &["switch", "-q", "master"]);
    assert_eq!(downloads(&sb, &ws), before, "switch downloaded objects");
}

#[test]
#[ignore = "needs Windows with ProjFS"]
fn worktree_add_is_fast_and_git_verbs_work_through_the_shim() {
    let sb = Sandbox::new();
    let url = seed(&sb);
    let ws = virtual_clone(&sb, &url);

    let wt = ws.join(".claude").join("worktrees").join("w1");
    let t = Instant::now();
    ok(&sb, &ws, &["worktree", "add", "--mode", "virtual", "-b", "w1", wt.to_str().unwrap()]);
    assert!(t.elapsed() < Duration::from_secs(1), "worktree add took {:?}", t.elapsed());
    assert_eq!(std::fs::read_to_string(wt.join("src").join("b.txt")).unwrap(), "b\n");

    // The git-compatible verbs agree with the plain implementation's output shapes.
    assert_eq!(ok(&sb, &wt, &["status", "--porcelain=v2", "-z"]), "");
    assert!(ok(&sb, &wt, &["log", "--oneline"]).contains("base"));
    std::fs::write(wt.join("a.txt"), "w\n").unwrap();
    assert!(ok(&sb, &wt, &["diff"]).contains("+w"));

    ok(&sb, &ws, &["worktree", "remove", "--force", wt.to_str().unwrap()]);
    assert!(!wt.exists());
}

#[test]
#[ignore = "needs Windows with ProjFS"]
fn convert_to_plain_keeps_edits() {
    let sb = Sandbox::new();
    let url = seed(&sb);
    let ws = virtual_clone(&sb, &url);
    std::fs::write(ws.join("a.txt"), "edited\n").unwrap();
    ok(&sb, &ws, &["worktree", "convert", "--plain"]);
    assert_eq!(std::fs::read_to_string(ws.join("a.txt")).unwrap(), "edited\n");
    assert_eq!(ok(&sb, &ws, &["status", "--porcelain"]), " M a.txt\n");
    assert_eq!(std::fs::read_to_string(ws.join("src").join("b.txt")).unwrap(), "b\n");
}

#[test]
#[ignore = "needs Windows with ProjFS"]
fn tree_without_turbo_json_has_no_index_and_status_matches_git() {
    let sb = Sandbox::new();
    let url = seed(&sb);
    let ws = virtual_clone(&sb, &url);
    // SPEC §9.4: no turbo.json, so asc.turboIndex renders no `index` file.
    let gitdir = ok(&sb, &ws, &["rev-parse", "--git-dir"]);
    let gitdir = ws.join(gitdir.trim());
    assert!(!gitdir.join("index").exists(), "unexpected index in {gitdir:?}");
    std::fs::write(ws.join("a.txt"), "x\n").unwrap();
    let asc_status = ok(&sb, &ws, &["status", "--porcelain=v2"]);
    assert!(asc_status.contains("a.txt"));
}
