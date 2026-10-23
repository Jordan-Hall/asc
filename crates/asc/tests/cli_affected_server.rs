//! G10 acceptance: `asc affected` on the server path (SPEC §12A.9 "Where it runs"): a workspace
//! with a server remote asks the server when both revisions are pushed and the local engine
//! otherwise; `--repo` always asks the server and needs no workspace. The answer is identical to
//! the local path for the same history. The server is the real handler stack in process.
#![cfg(feature = "serve")]

mod server_fixture;
mod support;

use std::path::Path;
use std::process::Output;

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

const A: &str = r#"{"name":"a","version":"1.0.0"}"#;
const B: &str = r#"{"name":"b","version":"1.0.0"}"#;
const ROOT: &str = r#"{"name":"root","private":true,"workspaces":["pkgs/*"]}"#;

fn run(sb: &Sandbox, cwd: &Path, envs: &[(&str, &str)], args: &[&str]) -> Output {
    let mut cmd = sb.asc(cwd);
    cmd.envs(IDENT)
        .env("ASC_ALLOW_LOOPBACK_HTTP", "1")
        .env("ASC_TOKEN", WRITE)
        .envs(envs.iter().copied())
        .args(args);
    cmd.output().expect("spawn asc")
}

fn ok(sb: &Sandbox, cwd: &Path, args: &[&str]) -> String {
    let out = run(sb, cwd, &[], args);
    assert!(out.status.success(), "{args:?}: {}", text(&out.stderr));
    text(&out.stdout)
}

fn write(repo: &Path, files: &[(&str, &str)]) {
    for (name, body) in files {
        let p = repo.join(name);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, body).unwrap();
    }
}

fn commit(sb: &Sandbox, repo: &Path, msg: &str, files: &[(&str, &str)]) {
    write(repo, files);
    ok(sb, repo, &["add", "."]);
    ok(sb, repo, &["commit", "-q", "-m", msg]);
}

/// `master` holds two Node packages; `feat` edits a source file of `a`. Both are committed in
/// `repo`; the remote (when `url` is given) gets `master` and, with `push_feat`, `feat`.
fn history(sb: &Sandbox, name: &str, url: Option<&str>, push_feat: bool) -> std::path::PathBuf {
    let repo = sb.dir(name);
    ok(sb, &repo, &["init", "-q"]);
    if let Some(url) = url {
        ok(sb, &repo, &["remote", "add", "origin", url]);
    }
    commit(
        sb,
        &repo,
        "base",
        &[
            ("package.json", ROOT),
            ("pkgs/a/package.json", A),
            ("pkgs/a/src/index.js", "1"),
            ("pkgs/b/package.json", B),
            ("pkgs/b/src/index.js", "1"),
        ],
    );
    ok(sb, &repo, &["branch", "-M", "master"]);
    if url.is_some() {
        ok(sb, &repo, &["push", "origin", "master"]);
    }
    ok(sb, &repo, &["checkout", "-q", "-b", "feat"]);
    commit(sb, &repo, "edit a", &[("pkgs/a/src/index.js", "2")]);
    if url.is_some() && push_feat {
        ok(sb, &repo, &["push", "origin", "feat"]);
    }
    repo
}

const ARGS: [&str; 6] = ["affected", "--base", "master", "--head", "feat", "--json"];

#[test]
fn pushed_revisions_are_answered_by_the_server_and_equal_the_local_answer() {
    let server = Server::start(&["acme/app"]);
    let sb = Sandbox::new();
    let client = history(&sb, "client", Some(&server.url("acme/app")), true);
    let local_sb = Sandbox::new();
    let local = history(&local_sb, "local", None, false);

    server.forget_calls();
    let served = ok(&sb, &client, &ARGS);
    assert_eq!(server.count("Affected"), 1, "{:?}", server.calls());
    let expected = ok(&local_sb, &local, &ARGS);
    assert_eq!(served, expected);
    let v: serde_json::Value = serde_json::from_str(&served).unwrap();
    assert_eq!(v["projects"].as_array().unwrap().len(), 1, "{served}");

    // The other formats go through the same report.
    let gha = ok(
        &sb,
        &client,
        &[
            "affected", "--base", "master", "--head", "feat", "--format", "gha",
        ],
    );
    let gha_local = ok(
        &local_sb,
        &local,
        &[
            "affected", "--base", "master", "--head", "feat", "--format", "gha",
        ],
    );
    assert_eq!(gha, gha_local);
}

#[test]
fn an_unpushed_head_is_answered_locally() {
    let server = Server::start(&["acme/app"]);
    let sb = Sandbox::new();
    let client = history(&sb, "client", Some(&server.url("acme/app")), false);
    server.forget_calls();
    let out = ok(&sb, &client, &ARGS);
    assert_eq!(server.count("Affected"), 0, "{:?}", server.calls());
    assert!(out.contains("pkgs/a") || out.contains("\"a\""), "{out}");
}

#[test]
fn uncommitted_always_runs_locally() {
    let server = Server::start(&["acme/app"]);
    let sb = Sandbox::new();
    let client = history(&sb, "client", Some(&server.url("acme/app")), true);
    server.forget_calls();
    ok(
        &sb,
        &client,
        &[
            "affected",
            "--base",
            "master",
            "--head",
            "feat",
            "--uncommitted",
        ],
    );
    assert_eq!(server.count("Affected"), 0, "{:?}", server.calls());
}

#[test]
fn repo_needs_no_workspace() {
    let server = Server::start(&["acme/app"]);
    let sb = Sandbox::new();
    let client = history(&sb, "client", Some(&server.url("acme/app")), true);
    let via_ws = ok(&sb, &client, &ARGS);

    let empty = Sandbox::new();
    let cwd = empty.tmp.path().to_owned();
    let args = [
        "affected", "--repo", "acme/app", "--base", "master", "--head", "feat", "--json",
    ];
    server.forget_calls();
    let by_name = run(
        &empty,
        &cwd,
        &[("ASC_SERVER", &server.base().replacen("http", "asc+http", 1))],
        &args,
    );
    assert!(by_name.status.success(), "{}", text(&by_name.stderr));
    assert_eq!(text(&by_name.stdout), via_ws);
    assert_eq!(server.count("Affected"), 1, "{:?}", server.calls());

    // A full URL needs no configured server.
    let url = server.url("acme/app");
    let by_url = run(
        &empty,
        &cwd,
        &[],
        &[
            "affected", "--repo", &url, "--base", "master", "--head", "feat", "--json",
        ],
    );
    assert!(by_url.status.success(), "{}", text(&by_url.stderr));
    assert_eq!(text(&by_url.stdout), via_ws);
}

#[test]
fn repo_errors_are_usage_or_revision_errors() {
    let server = Server::start(&["acme/app"]);
    let sb = Sandbox::new();
    let _client = history(&sb, "client", Some(&server.url("acme/app")), true);
    let empty = Sandbox::new();
    let cwd = empty.tmp.path().to_owned();
    let url = server.url("acme/app");

    let bad = run(
        &empty,
        &cwd,
        &[],
        &["affected", "--repo", "not a repo", "--json"],
    );
    assert_eq!(bad.status.code(), Some(129), "{}", text(&bad.stderr));

    let missing = run(
        &empty,
        &cwd,
        &[],
        &[
            "affected", "--repo", &url, "--base", "nope", "--head", "feat",
        ],
    );
    assert_eq!(
        missing.status.code(),
        Some(128),
        "{}",
        text(&missing.stderr)
    );

    let dirty = run(
        &empty,
        &cwd,
        &[],
        &["affected", "--repo", &url, "--uncommitted"],
    );
    assert!(!dirty.status.success());
}
