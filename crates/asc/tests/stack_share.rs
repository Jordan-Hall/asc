//! C32 acceptance: `asc share` against an in-process `asc serve`. The server is the real handler
//! stack over an in-memory database; the client is the built `asc` binary.
#![cfg(feature = "serve")]

mod server_fixture;
mod support;

use std::path::Path;
use std::process::Output;

use server_fixture::{READ, Server, WRITE};
use support::{Sandbox, text};

const IDENT: [(&str, &str); 6] = [
    ("GIT_AUTHOR_NAME", "Test"),
    ("GIT_AUTHOR_EMAIL", "test@example.com"),
    ("GIT_AUTHOR_DATE", "1700000000 +0000"),
    ("GIT_COMMITTER_NAME", "Test"),
    ("GIT_COMMITTER_EMAIL", "test@example.com"),
    ("GIT_COMMITTER_DATE", "1700000000 +0000"),
];

fn asc_as(sb: &Sandbox, cwd: &Path, token: &str, args: &[&str]) -> Output {
    let mut cmd = sb.asc(cwd);
    cmd.envs(IDENT)
        .env("ASC_TOKEN", token)
        .env("ASC_ALLOW_LOOPBACK_HTTP", "1")
        .stdin(std::process::Stdio::null())
        .args(args);
    cmd.output().expect("spawn asc")
}

fn ok(sb: &Sandbox, cwd: &Path, args: &[&str]) -> Output {
    let out = asc_as(sb, cwd, WRITE, args);
    assert!(
        out.status.success(),
        "asc {args:?} failed: {}",
        text(&out.stderr)
    );
    out
}

/// A client with `origin` at the server and one pushed commit.
fn client(sb: &Sandbox, url: &str) -> std::path::PathBuf {
    let a = sb.dir("a");
    ok(sb, &a, &["init", "-q"]);
    ok(sb, &a, &["remote", "add", "origin", url]);
    std::fs::write(a.join("one.txt"), "one\n").unwrap();
    ok(sb, &a, &["add", "."]);
    ok(sb, &a, &["commit", "-q", "-m", "one"]);
    ok(sb, &a, &["push", "origin", "master"]);
    a
}

fn id_of(stdout: &str) -> String {
    let first = stdout.lines().next().unwrap_or_default();
    first.strip_prefix("asc:").expect("asc:<id> line").to_owned()
}

#[test]
fn a_clean_tree_shares_head_and_roots_it_under_refs_snapshots() {
    let server = Server::start(&["acme/app"]);
    let sb = Sandbox::new();
    let a = client(&sb, &server.url("acme/app"));
    let head = text(&ok(&sb, &a, &["rev-parse", "HEAD"]).stdout)
        .trim()
        .to_owned();

    let st = text(&ok(&sb, &a, &["status", "--porcelain"]).stdout);
    assert_eq!(st, "", "the tree is clean");
    server.forget_calls();
    let out = ok(&sb, &a, &["share"]);
    let stdout = text(&out.stdout);
    assert_eq!(id_of(&stdout), head, "{stdout}");
    let link = stdout.lines().nth(1).expect("a link");
    assert!(
        link.ends_with(&format!("/acme/app/snapshots/{head}")),
        "{link}"
    );
    assert!(server.count("UpdateRefs") >= 1, "{:?}", server.calls());

    let refs = text(&ok(&sb, &a, &["ls-remote", "origin"]).stdout);
    assert!(refs.contains(&format!("refs/snapshots/{head}")), "{refs}");
}

#[test]
fn working_changes_become_a_snapshot_that_leaves_the_tree_alone() {
    let server = Server::start(&["acme/app"]);
    let sb = Sandbox::new();
    let a = client(&sb, &server.url("acme/app"));
    let head = text(&ok(&sb, &a, &["rev-parse", "HEAD"]).stdout)
        .trim()
        .to_owned();
    std::fs::write(a.join("one.txt"), "changed\n").unwrap();
    std::fs::write(a.join("new.txt"), "new\n").unwrap();

    let out = ok(&sb, &a, &["share", "-m", "look at this", "--json"]);
    let v: serde_json::Value = serde_json::from_str(text(&out.stdout).trim()).unwrap();
    assert_eq!(v["version"], 1);
    let id = v["id"]
        .as_str()
        .unwrap()
        .strip_prefix("asc:")
        .unwrap()
        .to_owned();
    assert_ne!(id, head);
    assert_eq!(id.len(), 40);
    assert!(v["expires_at"].as_i64().unwrap() > 1_700_000_000 + 29 * 86_400);

    // The work tree and HEAD are untouched.
    assert_eq!(
        std::fs::read_to_string(a.join("one.txt")).unwrap(),
        "changed\n"
    );
    assert_eq!(std::fs::read_to_string(a.join("new.txt")).unwrap(), "new\n");
    assert_eq!(
        text(&ok(&sb, &a, &["rev-parse", "HEAD"]).stdout).trim(),
        head
    );

    let refs = text(&ok(&sb, &a, &["ls-remote", "origin"]).stdout);
    assert!(refs.contains(&format!("refs/snapshots/{id}")), "{refs}");
}

#[test]
fn a_read_only_token_cannot_share() {
    let server = Server::start(&["acme/app"]);
    let sb = Sandbox::new();
    let a = client(&sb, &server.url("acme/app"));
    let out = asc_as(&sb, &a, READ, &["share"]);
    assert!(!out.status.success());
    let refs = text(&ok(&sb, &a, &["ls-remote", "origin"]).stdout);
    assert!(!refs.contains("refs/snapshots/"), "{refs}");
}

#[test]
fn without_a_remote_share_fails() {
    let sb = Sandbox::new();
    let a = sb.dir("solo");
    ok(&sb, &a, &["init", "-q"]);
    std::fs::write(a.join("f"), "f\n").unwrap();
    ok(&sb, &a, &["add", "f"]);
    ok(&sb, &a, &["commit", "-q", "-m", "f"]);
    let out = asc_as(&sb, &a, WRITE, &["share"]);
    assert!(!out.status.success());
    assert!(
        text(&out.stderr).contains("origin"),
        "{}",
        text(&out.stderr)
    );
}
