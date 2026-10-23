//! U50 acceptance: `asc api <Rpc> [--json <request>|-]` against an in-process `asc serve`. The
//! answers are standard proto3 JSON; `MergeBase`, `Log` and `Diff` round-trip through it.
#![cfg(feature = "serve")]

mod server_fixture;
mod support;

use std::io::Write as _;
use std::path::Path;
use std::process::{Output, Stdio};

use serde_json::{Value, json};
use server_fixture::{Server, WRITE};
use support::{Sandbox, text};

const IDENT: [(&str, &str); 6] = [
    ("GIT_AUTHOR_NAME", "Test"),
    ("GIT_AUTHOR_EMAIL", "test@example.com"),
    ("GIT_AUTHOR_DATE", "1700000000 +0000"),
    ("GIT_COMMITTER_NAME", "Test"),
    ("GIT_COMMITTER_EMAIL", "test@example.com"),
    ("GIT_COMMITTER_DATE", "1700000000 +0000"),
];

fn run(sb: &Sandbox, cwd: &Path, token: Option<&str>, args: &[&str]) -> Output {
    let mut cmd = sb.asc(cwd);
    cmd.envs(IDENT).args(args);
    if let Some(t) = token {
        cmd.env("ASC_TOKEN", t);
    }
    cmd.output().expect("spawn asc")
}

fn ok(sb: &Sandbox, cwd: &Path, args: &[&str]) -> Output {
    let out = run(sb, cwd, Some(WRITE), args);
    assert!(
        out.status.success(),
        "asc {args:?} failed: {}",
        text(&out.stderr)
    );
    out
}

fn commit_file(sb: &Sandbox, repo: &Path, name: &str, body: &str) {
    std::fs::write(repo.join(name), body).unwrap();
    ok(sb, repo, &["add", name]);
    ok(sb, repo, &["commit", "-q", "-m", name]);
}

/// A server with `c1 <- c2` on `master` and `c1 <- b2` on `side`, pushed by a client.
fn fixture() -> (Server, Sandbox, std::path::PathBuf, String) {
    let server = Server::start(&["acme/app"]);
    let url = server.url("acme/app");
    let sb = Sandbox::new();
    let a = sb.dir("a");
    ok(&sb, &a, &["init", "-q"]);
    ok(&sb, &a, &["remote", "add", "origin", &url]);
    commit_file(&sb, &a, "f.txt", "a\nb\nc\n");
    ok(&sb, &a, &["switch", "-q", "-c", "side"]);
    commit_file(&sb, &a, "side.txt", "side\n");
    ok(&sb, &a, &["switch", "-q", "master"]);
    commit_file(&sb, &a, "f.txt", "a\nB\nc\n");
    ok(&sb, &a, &["push", "origin", "master", "side"]);
    (server, sb, a, url)
}

fn api(sb: &Sandbox, cwd: &Path, args: &[&str]) -> Value {
    let out = run(sb, cwd, Some(WRITE), args);
    assert!(out.status.success(), "asc {args:?}: {}", text(&out.stderr));
    serde_json::from_str(&text(&out.stdout)).unwrap_or_else(|e| {
        panic!("not JSON ({e}): {}", text(&out.stdout));
    })
}

fn lines(sb: &Sandbox, cwd: &Path, args: &[&str]) -> Vec<Value> {
    let out = run(sb, cwd, Some(WRITE), args);
    assert!(out.status.success(), "asc {args:?}: {}", text(&out.stderr));
    text(&out.stdout)
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

#[test]
fn merge_base_log_and_diff_round_trip_through_json() {
    let (_server, sb, a, _url) = fixture();

    // Resolve: ids are base64 bytes in proto3 JSON.
    let resolved = api(
        &sb,
        &a,
        &[
            "api",
            "Resolve",
            "--json",
            r#"{"revspecs":["master","side"]}"#,
        ],
    );
    let ids = resolved["ids"].as_array().expect("ids");
    assert_eq!(ids.len(), 2);
    let (master, side) = (ids[0].as_str().unwrap(), ids[1].as_str().unwrap());
    assert_ne!(master, side);

    // MergeBase of the two tips is their common parent.
    let req = json!({"a": master, "b": [side]}).to_string();
    let mb = api(&sb, &a, &["api", "MergeBase", "--json", &req]);
    let bases = mb["bases"].as_array().expect("bases");
    assert_eq!(bases.len(), 1, "{mb}");
    let first_parent = api(
        &sb,
        &a,
        &["api", "Resolve", "--json", r#"{"revspecs":["master~1"]}"#],
    );
    assert_eq!(bases[0], first_parent["ids"][0]);

    // Log is a server stream: one JSON object per line, camelCase keys, 64-bit values as strings.
    let log = lines(
        &sb,
        &a,
        &[
            "api",
            "Repo/Log",
            "--json",
            r#"{"include":["master"],"limit":10}"#,
        ],
    );
    let entries: Vec<&Value> = log
        .iter()
        .flat_map(|m| m["entries"].as_array().into_iter().flatten())
        .collect();
    assert_eq!(entries.len(), 2, "{log:?}");
    assert!(entries[0]["gen"].is_number(), "32-bit values are numbers");
    assert!(
        entries[0]["author"]["time"].is_string(),
        "int64 is a string"
    );

    // Diff between the two branches.
    let diff = lines(
        &sb,
        &a,
        &[
            "api",
            "asc.v1.Repo/Diff",
            "--json",
            r#"{"base":"master","head":"side","format":"DIFF_FORMAT_STATUS"}"#,
        ],
    );
    let changes: Vec<&Value> = diff
        .iter()
        .flat_map(|m| m["changes"].as_array().into_iter().flatten())
        .collect();
    assert!(!changes.is_empty(), "{diff:?}");
    let all = serde_json::to_string(&diff).unwrap();
    assert!(
        all.contains("side.txt") || all.contains("c2lkZS50eHQ"),
        "{all}"
    );
}

#[test]
fn the_request_may_come_from_standard_input_and_names_the_remote() {
    let (server, sb, a, url) = fixture();
    let root = sb.dir("elsewhere");
    let mut cmd = sb.asc(&root);
    cmd.envs(IDENT)
        .env("ASC_TOKEN", WRITE)
        .args(["api", "Resolve", "--remote", &url, "--json", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(br#"{"revspecs":["master"]}"#)
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success(), "{}", text(&out.stderr));
    let v: Value = serde_json::from_str(&text(&out.stdout)).unwrap();
    assert_eq!(v["ids"].as_array().map(Vec::len), Some(1));
    assert!(server.count("Resolve") >= 1);

    // The configured remote name works inside the workspace.
    let v = api(
        &sb,
        &a,
        &[
            "api",
            "Resolve",
            "--remote",
            "origin",
            "--json",
            r#"{"revspecs":["side"]}"#,
        ],
    );
    assert_eq!(v["ids"].as_array().map(Vec::len), Some(1));
}

#[test]
fn bad_requests_and_unknown_rpcs_fail_before_the_network() {
    let (server, sb, a, _url) = fixture();
    server.forget_calls();
    for args in [
        vec!["api", "NoSuchRpc"],
        vec!["api", "Resolve", "--json", r#"{"bogus":1}"#],
        vec!["api", "Resolve", "--json", "{not json"],
        vec!["api", "Log", "--json", r#"{"limit":-1}"#],
        vec!["api", "PutObjects"],
        vec!["api"],
    ] {
        let out = run(&sb, &a, Some(WRITE), &args);
        assert!(!out.status.success(), "{args:?} should fail");
        assert!(
            text(&out.stdout).is_empty(),
            "{args:?}: {}",
            text(&out.stdout)
        );
    }
    assert_eq!(server.count("Resolve"), 0);
    assert_eq!(server.count("Log"), 0);
}

#[test]
fn the_listing_and_the_docs_name_the_public_rpcs() {
    let sb = Sandbox::new();
    let d = sb.dir("d");
    let out = run(&sb, &d, None, &["api", "--list"]);
    assert!(out.status.success());
    let s = text(&out.stdout);
    assert!(s.contains("Repo/MergeBase"), "{s}");
    assert!(s.contains("Repo/Log"), "{s}");
    assert!(s.contains("Projects/Affected"), "{s}");
    assert!(!s.contains("Workspace/"), "{s}");
    let out = run(&sb, &d, None, &["api", "--docs"]);
    assert!(out.status.success());
    let s = text(&out.stdout);
    assert!(s.starts_with("# asc.v1 API"), "{s}");
    assert!(s.contains("### MergeBase"), "{s}");
}

#[test]
fn a_server_refusal_is_reported_without_the_credential() {
    let (_server, sb, a, _url) = fixture();
    let out = run(
        &sb,
        &a,
        Some("wrong-token"),
        &["api", "Resolve", "--json", r#"{"revspecs":["master"]}"#],
    );
    assert!(!out.status.success());
    let err = text(&out.stderr);
    assert!(err.contains("Authentication failed"), "{err}");
    assert!(!err.contains("wrong-token"));
    assert!(text(&out.stdout).is_empty());
}
