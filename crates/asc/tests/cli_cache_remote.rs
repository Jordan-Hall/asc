//! `asc cache push` and `asc cache warm` against the in-process server's turbo store: an entry
//! goes up as one artefact, its key is listed under the revision, and a store that lost it gets it
//! back from the revision index.
#![cfg(feature = "serve")]

mod server_fixture;
mod support;

use std::path::Path;
use std::process::Output;

use asc_buildcache::{Action, ActionKey, Origin, Store};
use serde_json::Value;
use server_fixture::{Server, WRITE};
use support::{Sandbox, asc_binary, text};

const IDENT: [(&str, &str); 6] = [
    ("GIT_AUTHOR_NAME", "Test"),
    ("GIT_AUTHOR_EMAIL", "test@example.com"),
    ("GIT_AUTHOR_DATE", "1700000000 +0000"),
    ("GIT_COMMITTER_NAME", "Test"),
    ("GIT_COMMITTER_EMAIL", "test@example.com"),
    ("GIT_COMMITTER_DATE", "1700000000 +0000"),
];

fn run(sb: &Sandbox, cwd: &Path, args: &[&str]) -> Output {
    sb.asc(cwd)
        .envs(IDENT)
        .env("ASC_ALLOW_LOOPBACK_HTTP", "1")
        .env("ASC_TOKEN", WRITE)
        .env_remove("CI")
        .args(args)
        .output()
        .expect("spawn asc")
}

fn ok(sb: &Sandbox, cwd: &Path, args: &[&str]) -> String {
    let out = run(sb, cwd, args);
    assert!(out.status.success(), "{args:?}: {}", text(&out.stderr));
    text(&out.stdout)
}

fn json(sb: &Sandbox, cwd: &Path, args: &[&str]) -> Value {
    serde_json::from_str(&ok(sb, cwd, args)).unwrap()
}

/// A work tree with one commit and the server as `origin`.
fn repo(sb: &Sandbox, server: &Server) -> std::path::PathBuf {
    let repo = sb.dir("app");
    ok(sb, &repo, &["init", "-q"]);
    ok(
        sb,
        &repo,
        &["remote", "add", "origin", &server.url("acme/app")],
    );
    std::fs::write(repo.join("main.rs"), "fn main() {}\n").unwrap();
    ok(sb, &repo, &["add", "."]);
    ok(sb, &repo, &["commit", "-q", "-m", "base"]);
    repo
}

fn seed(store: &Store) -> ActionKey {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("target")).unwrap();
    std::fs::write(dir.path().join("target/app.rlib"), vec![7u8; 100_000]).unwrap();
    let key = ActionKey::builder("rustc").part("crate", b"app").finish();
    let mut a = Action::new("rustc", "app", Origin::default());
    a.outputs = store.put_outputs(dir.path(), &["target".into()]).unwrap();
    store.put_action(&key, &a).unwrap();
    store.queue_push(&key).unwrap();
    key
}

#[test]
fn push_lists_the_revision_and_warm_fetches_it_back() {
    if asc_binary().is_none() {
        return;
    }
    let server = Server::start(&["acme/app"]);
    let sb = Sandbox::new();
    let repo = repo(&sb, &server);
    let store = Store::open(sb.asc_home.join("buildcache")).unwrap();
    let key = seed(&store);
    let head = ok(&sb, &repo, &["rev-parse", "HEAD"]).trim().to_owned();

    let pushed = json(&sb, &repo, &["cache", "push", "--json"]);
    assert_eq!(pushed["uploaded"], 1, "{pushed}");
    assert_eq!(pushed["present"], 0);
    // The index is under the changeset id; the git spelling of HEAD resolves to it below.
    let rev = pushed["rev"].as_str().unwrap();
    assert!(
        rev.len() == 64 && rev.bytes().all(|b| b.is_ascii_hexdigit()),
        "{rev}"
    );
    assert!(store.queued().unwrap().is_empty(), "the outbox drained");
    // A key the server holds is not sent again.
    store.queue_push(&key).unwrap();
    let again = json(&sb, &repo, &["cache", "push", "--json"]);
    assert_eq!(
        (again["uploaded"].as_u64(), again["present"].as_u64()),
        (Some(0), Some(1))
    );

    let before = store.get_action(&key).unwrap().unwrap();
    ok(&sb, &repo, &["cache", "clear"]);
    assert!(store.get_action(&key).unwrap().is_none());
    let warmed = json(&sb, &repo, &["cache", "warm", "--json"]);
    assert_eq!(warmed["keys"], 1, "{warmed}");
    assert_eq!(warmed["fetched"], 1, "{warmed}");
    assert_eq!(store.get_action(&key).unwrap().unwrap(), before);
    let out = sb.dir("restored");
    assert!(store.restore(&before, &out).unwrap());
    assert_eq!(
        std::fs::read(out.join("target/app.rlib")).unwrap(),
        vec![7u8; 100_000]
    );
    let twice = json(&sb, &repo, &["cache", "warm", &head, "--json"]);
    assert_eq!(
        (twice["local"].as_u64(), twice["fetched"].as_u64()),
        (Some(1), Some(0))
    );

    // An unknown revision is refused; one with no index lists nothing.
    let bad = run(&sb, &repo, &["cache", "warm", "nope"]);
    assert!(!bad.status.success());
    assert!(
        text(&bad.stderr).contains("unknown revision"),
        "{}",
        text(&bad.stderr)
    );
}

#[test]
fn the_server_tier_can_be_switched_off() {
    if asc_binary().is_none() {
        return;
    }
    let server = Server::start(&["acme/app"]);
    let sb = Sandbox::new();
    let repo = repo(&sb, &server);
    ok(&sb, &repo, &["config", "asc.buildcache.remote", "off"]);
    for cmd in ["push", "warm"] {
        let out = run(&sb, &repo, &["cache", cmd]);
        assert!(!out.status.success(), "{cmd}");
        assert!(
            text(&out.stderr).contains("server tier is off"),
            "{}",
            text(&out.stderr)
        );
    }
    ok(
        &sb,
        &repo,
        &["config", "asc.buildcache.remote", "sometimes"],
    );
    let out = run(&sb, &repo, &["cache", "push"]);
    assert!(
        text(&out.stderr).contains("asc.buildcache.remote"),
        "{}",
        text(&out.stderr)
    );
}
