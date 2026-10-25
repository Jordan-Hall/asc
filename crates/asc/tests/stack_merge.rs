//! C28 acceptance: `asc stack merge` against an in-process `asc serve` with the real `Changes`
//! service. With no forge attached the server refuses a queued request (`now = false`), which is
//! the answer shown here; the client is the built `asc` binary.
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

fn client(sb: &Sandbox, url: &str) -> std::path::PathBuf {
    let a = sb.dir("a");
    ok(sb, &a, &["init", "-q"]);
    ok(sb, &a, &["remote", "add", "origin", url]);
    std::fs::write(a.join("one.txt"), "one\n").unwrap();
    ok(sb, &a, &["add", "."]);
    ok(sb, &a, &["commit", "-q", "-m", "one"]);
    ok(sb, &a, &["push", "origin", "master"]);
    std::fs::write(a.join("two.txt"), "two\n").unwrap();
    ok(sb, &a, &["add", "."]);
    ok(sb, &a, &["commit", "-q", "-m", "two"]);
    a
}

#[test]
fn a_change_the_server_does_not_know_is_reported_with_its_reason() {
    // The local draft was never submitted, so the real `Changes` service answers NotFound.
    let server = Server::start_with_changes(&["acme/app"]);
    let sb = Sandbox::new();
    let a = client(&sb, &server.url("acme/app"));
    server.forget_calls();
    let out = asc_as(&sb, &a, WRITE, &["stack", "merge"]);
    assert_eq!(out.status.code(), Some(128), "{}", text(&out.stderr));
    assert_eq!(server.count("RequestMerge"), 1, "{:?}", server.calls());
    assert!(
        text(&out.stderr).contains("unknown change"),
        "{}",
        text(&out.stderr)
    );
}

#[test]
fn json_prints_nothing_on_a_failed_call() {
    let server = Server::start_with_changes(&["acme/app"]);
    let sb = Sandbox::new();
    let a = client(&sb, &server.url("acme/app"));
    let out = asc_as(&sb, &a, WRITE, &["stack", "merge", "--json"]);
    assert!(!out.status.success());
    assert_eq!(text(&out.stdout), "");
}

#[test]
fn a_read_only_token_cannot_request_a_merge() {
    let server = Server::start_with_changes(&["acme/app"]);
    let sb = Sandbox::new();
    let a = client(&sb, &server.url("acme/app"));
    let out = asc_as(&sb, &a, READ, &["stack", "merge", "--now"]);
    assert!(!out.status.success());
    assert_ne!(out.status.code(), Some(0));
    assert!(
        !text(&out.stdout).contains("Landed"),
        "{}",
        text(&out.stdout)
    );
}

#[test]
fn a_server_without_changes_exits_128() {
    let server = Server::start(&["acme/app"]);
    let sb = Sandbox::new();
    let a = client(&sb, &server.url("acme/app"));
    let out = asc_as(&sb, &a, WRITE, &["stack", "merge"]);
    assert_eq!(out.status.code(), Some(128), "{}", text(&out.stderr));
}

#[test]
fn without_a_remote_merge_fails() {
    let sb = Sandbox::new();
    let a = sb.dir("solo");
    ok(&sb, &a, &["init", "-q"]);
    std::fs::write(a.join("f"), "f\n").unwrap();
    ok(&sb, &a, &["add", "f"]);
    ok(&sb, &a, &["commit", "-q", "-m", "f"]);
    let out = asc_as(&sb, &a, WRITE, &["stack", "merge"]);
    assert!(!out.status.success());
}
