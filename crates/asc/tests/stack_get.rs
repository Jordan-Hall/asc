//! C26 acceptance: `asc stack get|freeze|unfreeze`, driven through the built binary against an
//! in-process `asc serve` (the real handlers over an in-memory database). The server's change
//! index is filled through the fixture, because the push path that uploads change metadata
//! belongs to the submit unit.
#![cfg(feature = "serve")]

mod server_fixture;
mod support;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Output;

use asc_core::change::ChangeId;
use asc_core::id::ChangesetId;
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

fn ok(sb: &Sandbox, cwd: &Path, args: &[&str]) -> String {
    let out = asc_as(sb, cwd, WRITE, args);
    assert!(
        out.status.success(),
        "asc {args:?} failed: {}",
        text(&out.stderr)
    );
    text(&out.stdout)
}

fn fail(sb: &Sandbox, cwd: &Path, token: &str, args: &[&str]) -> (i32, String) {
    let out = asc_as(sb, cwd, token, args);
    assert!(!out.status.success(), "asc {args:?} should fail");
    (out.status.code().unwrap_or(-1), text(&out.stderr))
}

struct Stack {
    server: Server,
    sb: Sandbox,
    a: PathBuf,
    /// change ids, bottom first
    changes: [String; 3],
    /// git ids, bottom first
    commits: [String; 3],
}

/// A server holding `base` on master and a three-change stack on `feature` (indexed with
/// change metadata), and the client `a` that built it.
fn stack() -> Stack {
    let server = Server::start_with_changes(&["acme/app"]);
    let sb = Sandbox::new();
    let a = sb.dir("a");
    ok(&sb, &a, &["init", "-q"]);
    ok(
        &sb,
        &a,
        &["remote", "add", "origin", &server.url("acme/app")],
    );
    fs::write(a.join("base.txt"), "base\n").unwrap();
    ok(&sb, &a, &["add", "."]);
    ok(&sb, &a, &["commit", "-q", "-m", "base"]);
    ok(&sb, &a, &["push", "origin", "master"]);
    ok(&sb, &a, &["switch", "-q", "-c", "feature"]);
    for n in ["one", "two", "three"] {
        fs::write(a.join(n), n).unwrap();
        ok(&sb, &a, &["add", n]);
        ok(&sb, &a, &["create", "-m", &format!("feat: {n}")]);
    }
    ok(&sb, &a, &["push", "origin", "feature"]);
    let mut changes = Vec::new();
    let mut commits = Vec::new();
    let mut parent: Option<ChangeId> = None;
    for rev in ["HEAD~2", "HEAD~1", "HEAD"] {
        let change = ok(&sb, &a, &["change-id", rev]).trim().to_owned();
        let cs: ChangesetId = ok(&sb, &a, &["rev-parse", "--asc", rev])
            .trim()
            .parse()
            .expect("an asc id");
        let id: ChangeId = change.parse().expect("a change id");
        server.seed_change("acme/app", cs, id, parent);
        parent = Some(id);
        commits.push(ok(&sb, &a, &["rev-parse", rev]).trim().to_owned());
        changes.push(change);
    }
    Stack {
        server,
        sb,
        a,
        changes: changes.try_into().unwrap(),
        commits: commits.try_into().unwrap(),
    }
}

/// A fresh client `b` whose `origin` is the server.
fn fresh(s: &Stack, name: &str) -> PathBuf {
    let b = s.sb.dir(name);
    ok(&s.sb, &b, &["init", "-q"]);
    // `a` committed the `.ascignore` its `init` wrote, so the stack carries it; the one this
    // `init` wrote would be an untracked file in the checkout's way.
    fs::remove_file(b.join(".ascignore")).ok();
    ok(
        &s.sb,
        &b,
        &["remote", "add", "origin", &s.server.url("acme/app")],
    );
    b
}

fn sl(s: &Stack, dir: &Path) -> serde_json::Value {
    let out = ok(&s.sb, dir, &["sl", "--json"]);
    serde_json::from_str(out.trim()).expect("sl json")
}

fn frozen_of(v: &serde_json::Value) -> Vec<(String, bool)> {
    v["stacks"]
        .as_array()
        .into_iter()
        .flatten()
        .flat_map(|s| s["changes"].as_array().into_iter().flatten())
        .map(|c| {
            (
                c["change"].as_str().unwrap().to_owned(),
                c["frozen"].as_bool().unwrap(),
            )
        })
        .collect()
}

#[test]
fn get_fetches_the_stack_frozen_and_checks_the_target_out_detached() {
    let s = stack();
    let b = fresh(&s, "b");
    let out = ok(&s.sb, &b, &["stack", "get", &s.changes[1]]);
    for c in &s.changes {
        assert!(out.contains(c), "{out}");
    }
    // HEAD is the target, detached.
    assert_eq!(ok(&s.sb, &b, &["rev-parse", "HEAD"]).trim(), s.commits[1]);
    assert_eq!(ok(&s.sb, &b, &["branch", "--show-current"]).trim(), "");
    assert_eq!(fs::read_to_string(b.join("two")).unwrap(), "two");
    // Every fetched change is known with its id, and frozen.
    for (i, rev) in [&s.commits[0], &s.commits[1], &s.commits[2]]
        .iter()
        .enumerate()
    {
        assert_eq!(ok(&s.sb, &b, &["change-id", rev]).trim(), s.changes[i]);
    }
    let frozen = frozen_of(&sl(&s, &b));
    assert!(
        s.changes
            .iter()
            .all(|c| frozen.contains(&(c.clone(), true))),
        "{frozen:?}"
    );
}

#[test]
fn downstack_upstack_and_unfrozen_pick_what_is_taken() {
    let s = stack();
    let b = fresh(&s, "down");
    ok(&s.sb, &b, &["stack", "get", "-d", &s.changes[1]]);
    assert_eq!(ok(&s.sb, &b, &["rev-parse", "HEAD"]).trim(), s.commits[1]);
    assert!(
        !ok(&s.sb, &b, &["stack", "get", "-d", &s.changes[1], "--json"]).contains(&s.changes[2])
    );
    assert!(fresh_has(&s, &b, &s.commits[0]));
    assert!(
        !fresh_has(&s, &b, &s.commits[2]),
        "-d must not fetch above the target"
    );

    let c = fresh(&s, "up");
    let out = ok(
        &s.sb,
        &c,
        &["stack", "get", "-u", "--unfrozen", &s.changes[1]],
    );
    assert!(
        out.contains(&s.changes[1]) && out.contains(&s.changes[2]),
        "{out}"
    );
    assert!(!out.contains(&s.changes[0]), "{out}");
    assert!(!out.contains("(frozen)"), "{out}");
    assert!(
        frozen_of(&sl(&s, &c)).iter().all(|(_, f)| !f),
        "--unfrozen freezes nothing"
    );
}

fn fresh_has(s: &Stack, dir: &Path, git: &str) -> bool {
    asc_as(&s.sb, dir, WRITE, &["cat-file", "-e", git])
        .status
        .success()
}

#[test]
fn freeze_and_unfreeze_gate_local_rewrites() {
    let s = stack();
    let a = &s.a;
    let tip = ok(&s.sb, a, &["rev-parse", "HEAD"]).trim().to_owned();
    let out = ok(&s.sb, a, &["stack", "freeze"]);
    assert!(out.starts_with("Froze "), "{out}");
    assert!(
        frozen_of(&sl(&s, a)).contains(&(s.changes[2].clone(), true)),
        "the flag shows in sl"
    );
    // Freezing twice is not an error, and says nothing changed in JSON.
    let again = ok(&s.sb, a, &["stack", "freeze", "--json"]);
    assert!(again.contains("\"changed\":false"), "{again}");

    // A frozen change is not amended, and no ref moves.
    let (code, err) = fail(
        &s.sb,
        a,
        WRITE,
        &["modify", "-m", "feat: three again"],
    );
    assert_eq!(code, 1, "{err}");
    assert!(err.contains("frozen"), "{err}");
    assert_eq!(ok(&s.sb, a, &["rev-parse", "HEAD"]).trim(), tip);

    // A frozen change in the middle is not restacked by an edit below it.
    ok(&s.sb, a, &["stack", "freeze", &s.changes[1]]);
    let (code, err) = fail(
        &s.sb,
        a,
        WRITE,
        &["reword", &s.changes[0], "-m", "feat: uno"],
    );
    assert_eq!(code, 1, "{err}");
    assert!(err.contains("frozen"), "{err}");
    assert_eq!(ok(&s.sb, a, &["rev-parse", "HEAD"]).trim(), tip);

    ok(&s.sb, a, &["stack", "unfreeze"]);
    ok(&s.sb, a, &["stack", "unfreeze", &s.changes[1]]);
    ok(&s.sb, a, &["modify", "-m", "feat: three again"]);
    assert_ne!(ok(&s.sb, a, &["rev-parse", "HEAD"]).trim(), tip);
    assert!(
        frozen_of(&sl(&s, a)).iter().all(|(_, f)| !f),
        "unfreeze clears the flag"
    );
}

#[test]
fn a_change_that_is_not_a_draft_cannot_be_frozen() {
    let s = stack();
    let (code, err) = fail(&s.sb, &s.a, WRITE, &["stack", "freeze", "master"]);
    assert_eq!(code, 1, "{err}");
}

#[test]
fn worktree_opens_a_plain_worktree_beside_the_work_tree_and_leaves_this_one_alone() {
    let s = stack();
    let b = fresh(&s, "b");
    fs::write(b.join("keep.txt"), "mine\n").unwrap();
    let target = b.parent().unwrap().join("review");
    let out = ok(
        &s.sb,
        &b,
        &[
            "stack",
            "get",
            "--worktree",
            target.to_str().unwrap(),
            &s.changes[2],
        ],
    );
    assert!(out.contains("worktree "), "{out}");
    assert_eq!(fs::read_to_string(target.join("three")).unwrap(), "three");
    assert_eq!(
        ok(&s.sb, &target, &["rev-parse", "HEAD"]).trim(),
        s.commits[2]
    );
    assert_eq!(fs::read_to_string(b.join("keep.txt")).unwrap(), "mine\n");
}

#[test]
fn a_worktree_path_outside_the_parent_directory_is_refused_before_any_call() {
    let s = stack();
    let b = fresh(&s, "b");
    let escape = b.join("..").join("..").join("escape");
    s.server.forget_calls();
    for p in [&escape, &PathBuf::from("../../escape2")] {
        let (code, err) = fail(
            &s.sb,
            &b,
            WRITE,
            &[
                "stack",
                "get",
                "--worktree",
                p.to_str().unwrap(),
                &s.changes[0],
            ],
        );
        assert_eq!(code, 128, "{err}");
        assert!(err.contains("parent directory"), "{err}");
    }
    assert_eq!(s.server.count("Stack"), 0, "{:?}", s.server.calls());
    assert!(!fresh_has(&s, &b, &s.commits[0]));
}

#[test]
fn snapshots_are_fetched_and_checked_out_without_freezing() {
    let s = stack();
    fs::write(s.a.join("scratch.txt"), "wip\n").unwrap();
    let shared = ok(&s.sb, &s.a, &["share", "--json"]);
    let v: serde_json::Value = serde_json::from_str(shared.trim()).unwrap();
    let id = v["id"].as_str().unwrap().to_owned();
    let b = fresh(&s, "b");
    ok(&s.sb, &b, &["stack", "get", &id]);
    assert_eq!(fs::read_to_string(b.join("scratch.txt")).unwrap(), "wip\n");
    assert_eq!(
        ok(&s.sb, &b, &["rev-parse", "HEAD"]).trim(),
        id.strip_prefix("asc:").unwrap()
    );
    let (code, err) = fail(
        &s.sb,
        &b,
        WRITE,
        &["stack", "get", &format!("asc:{}", "0".repeat(40))],
    );
    assert_eq!(code, 1, "{err}");
}

#[test]
fn unknown_and_malformed_targets_fail_cleanly() {
    let s = stack();
    let b = fresh(&s, "b");
    let pushed = s.server.count("UpdateRefs");
    let unknown = ChangeId::from_bytes([9; 16]).to_string();
    let (code, err) = fail(&s.sb, &b, WRITE, &["stack", "get", &unknown]);
    assert_ne!(code, 0, "{err}");
    let (code, _) = fail(&s.sb, &b, WRITE, &["stack", "get", "asc:nothex"]);
    assert_ne!(code, 0);
    let (code, _) = fail(&s.sb, &b, WRITE, &["stack", "get"]);
    assert_ne!(code, 0);
    // A reader can get; nothing was written to the server.
    ok(&s.sb, &b, &["stack", "get", &s.changes[0]]);
    let out = asc_as(&s.sb, &b, READ, &["stack", "get", &s.changes[2]]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert_eq!(
        s.server.count("UpdateRefs"),
        pushed,
        "{:?}",
        s.server.calls()
    );
}

#[test]
fn a_plain_git_remote_exits_128() {
    let sb = Sandbox::new();
    let plain = sb.dir("plain.git");
    let made = sb
        .command("git", &plain)
        .args(["init", "-q", "--bare"])
        .output()
        .expect("spawn git");
    assert!(made.status.success(), "{}", text(&made.stderr));
    let a = sb.dir("a");
    ok(&sb, &a, &["init", "-q"]);
    ok(
        &sb,
        &a,
        &["remote", "add", "origin", plain.to_str().unwrap()],
    );
    let (code, err) = fail(
        &sb,
        &a,
        WRITE,
        &["stack", "get", &ChangeId::from_bytes([9; 16]).to_string()],
    );
    assert_eq!(code, 128, "{err}");
    assert!(err.contains("needs an asc server"), "{err}");
}
