//! U73 acceptance: the plugin RPCs of the daemon (`Status`, `Diff`, `Log`, `Blame`, `Affected`,
//! `Stage`, `Watch`) against the matching `--json` CLI output on a fixture workspace, and
//! `asc watch --since` resuming without loss. A real `asc daemon run` serves the RPCs on a private
//! endpoint (`ASC_DAEMON_ENDPOINT`), so the user's own daemon is never involved.

mod support;

use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::{Child, Command, Output, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use asc_core::id::ChangesetId;
use asc_core::id::RefTarget;
use asc_core::path::RefName;
use asc_rpc::ipc::{IpcEndpoint, WorkspaceClient, connect};
use asc_rpc::server::wire;
use asc_store::refs::{Actor, Expect, RefTxn, RefUpdate, Refs};
use prost_reflect::{DescriptorPool, DynamicMessage};
use serde_json::Value;
use support::{Fixture, text};
use tonic::transport::Channel;

const A: &str = r#"{"name":"a","version":"1.0.0"}"#;
const B: &str = r#"{"name":"b","version":"1.0.0"}"#;
const ROOT: &str = r#"{"name":"root","private":true,"workspaces":["pkgs/*"]}"#;

const BASE_FILES: &[(&str, &[u8])] = &[
    ("package.json", ROOT.as_bytes()),
    ("pkgs/a/package.json", A.as_bytes()),
    ("pkgs/a/src/index.js", b"1\n"),
    ("pkgs/b/package.json", B.as_bytes()),
    ("pkgs/b/src/index.js", b"1\n"),
    ("README.md", b"hi\n"),
];

fn pool() -> DescriptorPool {
    DescriptorPool::decode(asc_rpc::DESCRIPTOR_SET).unwrap()
}

/// A wire message as the asc JSON profile prints it.
macro_rules! js {
    ($name:literal, $msg:expr) => {{
        let mut d = DynamicMessage::new(pool().get_message_by_name($name).unwrap());
        d.transcode_from(&$msg).unwrap();
        asc_rpc::json::encode(&d).unwrap()
    }};
}

fn lines(out: &[u8]) -> Vec<Value> {
    text(out).lines().filter(|l| !l.trim().is_empty()).map(|l| serde_json::from_str(l).unwrap()).collect()
}

/// A daemon on a private endpoint; killed on drop.
struct Daemon {
    child: Child,
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

struct Rig {
    f: Fixture,
    ep: String,
    client: WorkspaceClient<Channel>,
    ws: String,
    c0: ChangesetId,
    _daemon: Daemon,
}

async fn set_ref(f: &Fixture, name: &str, cs: ChangesetId) {
    let txn = RefTxn {
        updates: vec![RefUpdate {
            name: RefName::new(name.as_bytes().to_vec()).unwrap(),
            expect: Expect::Any,
            new: Some(RefTarget::Changeset(cs)),
            expires_at: None,
        }],
        actor: Actor { id: "fixture".into(), via: "test".into() },
        reason: "fixture".into(),
        op_id: uuid::Uuid::now_v7(),
    };
    Refs::update(&f.db.refs(), f.repo, &txn).await.unwrap();
}

fn write(root: &Path, rel: &str, content: &[u8]) {
    let path = root.join(rel);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, content).unwrap();
}

/// `asc args…` in the workspace: no daemon of its own, a fixed identity.
fn asc_command(f: &Fixture, ep: &str, args: &[&str]) -> Command {
    let mut c = f.asc();
    c.args(args)
        .env("ASC_NO_DAEMON", "1")
        .env("ASC_DAEMON_ENDPOINT", ep)
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.com");
    c
}

fn asc(f: &Fixture, ep: &str, args: &[&str]) -> Output {
    asc_command(f, ep, args).output().expect("spawn asc")
}

fn ok(f: &Fixture, ep: &str, args: &[&str]) -> String {
    let out = asc(f, ep, args);
    assert!(out.status.success(), "{args:?}: {}", text(&out.stderr));
    text(&out.stdout)
}

fn endpoint_name(f: &Fixture) -> String {
    if cfg!(windows) {
        format!("asc-u73-{}-{}", std::process::id(), std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos())
    } else {
        f.sandbox.tmp.path().join("ipc").join("d.sock").to_string_lossy().into_owned()
    }
}

/// The workspace of two Node packages on `feat`, one commit ahead of `master`, with its files on
/// disk, and a daemon that serves it.
async fn rig() -> Rig {
    let f = Fixture::new().await;
    let c0 = f.commit(&[], BASE_FILES).await;
    let c1 = f.commit(&[c0], &[("pkgs/a/src/index.js", b"2\n")]).await;
    f.set_branch("master", c0).await;
    set_ref(&f, "refs/remotes/origin/master", c0).await;
    f.set_branch("feat", c1).await;
    f.checkout_branch("feat", Some(c1)).await;
    for (path, content) in BASE_FILES {
        write(&f.root, path, content);
    }
    write(&f.root, "pkgs/a/src/index.js", b"2\n");

    let ep = endpoint_name(&f);
    let child = f
        .asc()
        .args(["daemon", "run"])
        .env("ASC_DAEMON_ENDPOINT", &ep)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn the daemon");
    let daemon = Daemon { child };
    let mut client = WorkspaceClient::new(connect(&IpcEndpoint::from_raw(ep.clone())));
    let mut up = false;
    for _ in 0..200 {
        if client.ping(wire::PingRequest {}).await.is_ok() {
            up = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(up, "the daemon did not come up");
    let ws = f.ws.to_string();
    client.mount(wire::MountRequest { ws_id: ws.clone() }).await.unwrap();
    let rig = Rig { f, ep, client, ws, c0, _daemon: daemon };
    // The fixture is clean before anything changes.
    let status = ok(&rig.f, &rig.ep, &["status", "--json", "--untracked-files=all"]);
    assert_eq!(lines(status.as_bytes())[0]["files"].as_array().map_or(0, Vec::len), 0, "{status}");
    rig
}

async fn collect<T>(mut s: tonic::Streaming<T>) -> Vec<T> {
    let mut v = Vec::new();
    while let Some(m) = tokio::time::timeout(Duration::from_secs(60), s.message()).await.expect("stream stalled").unwrap() {
        v.push(m);
    }
    v
}

fn kind(e: &wire::WatchEvent) -> wire::watch_event::Kind {
    wire::watch_event::Kind::try_from(e.kind).unwrap()
}

/// Reads events until `want` matches one, and returns all read.
async fn until(
    s: &mut tonic::Streaming<wire::WatchEvent>,
    secs: u64,
    mut want: impl FnMut(&wire::WatchEvent) -> bool,
) -> Vec<wire::WatchEvent> {
    let end = Instant::now() + Duration::from_secs(secs);
    let mut seen = Vec::new();
    loop {
        let left = end.saturating_duration_since(Instant::now());
        match tokio::time::timeout(left, s.message()).await {
            Ok(Ok(Some(e))) => {
                let hit = want(&e);
                seen.push(e);
                if hit {
                    return seen;
                }
            }
            other => panic!("the wanted event did not arrive ({other:?}); saw {:?}", seen.iter().map(|e| (kind(e), e.r#ref.clone())).collect::<Vec<_>>()),
        }
    }
}

// ------------------------------------------------------------------------------ RPC == CLI

#[tokio::test]
async fn status_equals_the_json_cli() {
    let mut r = rig().await;
    write(&r.f.root, "pkgs/a/src/index.js", b"3\n");
    write(&r.f.root, "new.txt", b"new\n");
    let mut got = r
        .client
        .status(wire::WsStatusRequest { ws_id: r.ws.clone(), untracked: true, ..Default::default() })
        .await
        .unwrap()
        .into_inner();
    assert!(got.token.starts_with("v1."), "{}", got.token);
    got.token.clear();
    let cli = lines(ok(&r.f, &r.ep, &["status", "--json", "--untracked-files=all", "--"]).as_bytes());
    assert_eq!(cli.len(), 1);
    assert_eq!(js!("asc.v1.WsStatusResponse", got), cli[0]);
    let files = cli[0]["files"].as_array().unwrap();
    assert!(files.iter().any(|f| f["xy"] == ".M") && files.iter().any(|f| f["xy"] == "??"), "{files:?}");

    // A pathspec limits the answer the same way, and untracked files can be left out.
    let mut one = r
        .client
        .status(wire::WsStatusRequest { ws_id: r.ws.clone(), paths: vec!["pkgs/a/src/index.js".into()], ..Default::default() })
        .await
        .unwrap()
        .into_inner();
    one.token.clear();
    let cli = lines(ok(&r.f, &r.ep, &["status", "--json", "--untracked-files=no", "--", "pkgs/a/src/index.js"]).as_bytes());
    assert_eq!(js!("asc.v1.WsStatusResponse", one), cli[0]);
    assert_eq!(one.files.len(), 1);
}

#[tokio::test]
async fn diff_equals_the_json_cli() {
    let mut r = rig().await;
    write(&r.f.root, "pkgs/a/src/index.js", b"3\n");
    let patch = wire::DiffFormat::Patch as i32;

    let got = collect(
        r.client
            .diff(wire::WsDiffRequest { ws_id: r.ws.clone(), format: patch, ..Default::default() })
            .await
            .unwrap()
            .into_inner(),
    )
    .await;
    let cli = lines(ok(&r.f, &r.ep, &["diff", "--json", "-p", "--"]).as_bytes());
    assert!(!cli.is_empty());
    assert_eq!(got.iter().map(|m| js!("asc.v1.DiffResponse", *m)).collect::<Vec<_>>(), cli);

    let got = collect(
        r.client
            .diff(wire::WsDiffRequest {
                ws_id: r.ws.clone(),
                from: "master".into(),
                to: "feat".into(),
                format: patch,
                paths: vec!["pkgs/a/src/index.js".into()],
                ..Default::default()
            })
            .await
            .unwrap()
            .into_inner(),
    )
    .await;
    let cli = lines(ok(&r.f, &r.ep, &["diff", "--json", "-p", "master", "feat", "--", "pkgs/a/src/index.js"]).as_bytes());
    assert!(!cli.is_empty());
    assert_eq!(got.iter().map(|m| js!("asc.v1.DiffResponse", *m)).collect::<Vec<_>>(), cli);

    // The staged side: a revision against the index.
    ok(&r.f, &r.ep, &["add", "pkgs/a/src/index.js"]);
    let got = collect(
        r.client
            .diff(wire::WsDiffRequest { ws_id: r.ws.clone(), from: "HEAD".into(), to: "index".into(), format: patch, ..Default::default() })
            .await
            .unwrap()
            .into_inner(),
    )
    .await;
    let cli = lines(ok(&r.f, &r.ep, &["diff", "--json", "-p", "--cached", "HEAD", "--"]).as_bytes());
    assert!(!cli.is_empty());
    assert_eq!(got.iter().map(|m| js!("asc.v1.DiffResponse", *m)).collect::<Vec<_>>(), cli);
}

#[tokio::test]
async fn log_equals_the_json_cli() {
    let mut r = rig().await;
    let query = wire::LogRequest { include: vec!["feat".into()], limit: 10, ..Default::default() };
    let got = collect(r.client.log(wire::WsLogRequest { ws_id: r.ws.clone(), query: Some(query) }).await.unwrap().into_inner()).await;
    let cli = lines(ok(&r.f, &r.ep, &["log", "--json", "-n10", "feat", "--"]).as_bytes());
    assert!(!cli.is_empty());
    assert_eq!(got.iter().map(|m| js!("asc.v1.LogResponse", *m)).collect::<Vec<_>>(), cli);
    assert_eq!(got.iter().map(|m| m.entries.len()).sum::<usize>(), 2);

    // Exclusion and a path limit.
    let query = wire::LogRequest {
        include: vec!["feat".into()],
        exclude: vec!["master".into()],
        paths: vec!["pkgs/a".into()],
        limit: 10,
        ..Default::default()
    };
    let got = collect(r.client.log(wire::WsLogRequest { ws_id: r.ws.clone(), query: Some(query) }).await.unwrap().into_inner()).await;
    let cli = lines(ok(&r.f, &r.ep, &["log", "--json", "-n10", "feat", "^master", "--", "pkgs/a"]).as_bytes());
    assert_eq!(got.iter().map(|m| js!("asc.v1.LogResponse", *m)).collect::<Vec<_>>(), cli);
    assert_eq!(got.iter().map(|m| m.entries.len()).sum::<usize>(), 1);
}

#[tokio::test]
async fn blame_equals_the_json_cli_including_uncommitted_lines() {
    let mut r = rig().await;
    write(&r.f.root, "pkgs/a/src/index.js", b"2\nuncommitted\n");
    let req = wire::WsBlameRequest { ws_id: r.ws.clone(), path: "pkgs/a/src/index.js".into(), ..Default::default() };
    let got = collect(r.client.blame(req).await.unwrap().into_inner()).await;
    let cli = lines(ok(&r.f, &r.ep, &["blame", "--json", "--", "pkgs/a/src/index.js"]).as_bytes());
    assert!(!cli.is_empty());
    assert_eq!(got.iter().map(|m| js!("asc.v1.BlameResponse", *m)).collect::<Vec<_>>(), cli);
    // The last line has no commit.
    let last = got.iter().flat_map(|m| m.hunks.iter()).last().unwrap();
    assert!(last.origin.is_empty());

    // A line range and a revision.
    let req = wire::WsBlameRequest {
        ws_id: r.ws.clone(),
        rev: "master".into(),
        path: "pkgs/a/src/index.js".into(),
        start_line: 1,
        end_line: 1,
        ..Default::default()
    };
    let got = collect(r.client.blame(req).await.unwrap().into_inner()).await;
    let cli = lines(ok(&r.f, &r.ep, &["blame", "--json", "-L1,1", "master", "--", "pkgs/a/src/index.js"]).as_bytes());
    assert_eq!(got.iter().map(|m| js!("asc.v1.BlameResponse", *m)).collect::<Vec<_>>(), cli);
}

#[tokio::test]
async fn affected_equals_the_json_cli() {
    let mut r = rig().await;
    let query = wire::AffectedRequest { base: "refs/remotes/origin/master".into(), ..Default::default() };
    let got = r
        .client
        .affected(wire::WsAffectedRequest { ws_id: r.ws.clone(), query: Some(query), uncommitted: false })
        .await
        .unwrap()
        .into_inner()
        .result
        .unwrap();
    let cli = lines(ok(&r.f, &r.ep, &["affected", "--json", "--base", "refs/remotes/origin/master"]).as_bytes());
    let doc = &cli[0];
    let want: Vec<&str> = doc["projects"].as_array().unwrap().iter().map(|p| p["id"].as_str().unwrap()).collect();
    assert_eq!(want, ["node:a"]);
    let have: Vec<&str> = got.projects.iter().map(|p| p.project.as_ref().unwrap().id.as_str()).collect();
    assert_eq!(have, want);
    assert!(got.projects[0].direct);
    assert_eq!(got.projects[0].reasons, ["file:pkgs/a/src/index.js"]);
    assert_eq!(got.all, doc["all"].as_bool().unwrap());
    assert_eq!(hex(&got.head), doc["head"].as_str().unwrap());
    assert_eq!(hex(&got.merge_base), doc["mergeBase"].as_str().unwrap());

    // Uncommitted edits count when asked.
    write(&r.f.root, "pkgs/b/src/index.js", b"edited\n");
    let query = wire::AffectedRequest { base: "feat".into(), ..Default::default() };
    let got = r
        .client
        .affected(wire::WsAffectedRequest { ws_id: r.ws.clone(), query: Some(query), uncommitted: true })
        .await
        .unwrap()
        .into_inner()
        .result
        .unwrap();
    let cli = lines(ok(&r.f, &r.ep, &["affected", "--json", "--base", "feat", "--uncommitted"]).as_bytes());
    let want: Vec<&str> = cli[0]["projects"].as_array().unwrap().iter().map(|p| p["id"].as_str().unwrap()).collect();
    assert_eq!(want, ["node:b"]);
    assert_eq!(got.projects.iter().map(|p| p.project.as_ref().unwrap().id.clone()).collect::<Vec<_>>(), want);
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

#[tokio::test]
async fn stage_adds_removes_and_applies_patches() {
    let mut r = rig().await;
    write(&r.f.root, "new.txt", b"new\n");
    write(&r.f.root, "pkgs/a/src/index.js", b"3\n");

    let mut got = r
        .client
        .stage(wire::WsStageRequest { ws_id: r.ws.clone(), add: vec!["new.txt".into()], ..Default::default() })
        .await
        .unwrap()
        .into_inner()
        .status
        .unwrap();
    got.token.clear();
    let cli = lines(ok(&r.f, &r.ep, &["status", "--json", "--untracked-files=all", "--"]).as_bytes());
    assert_eq!(js!("asc.v1.WsStatusResponse", got), cli[0]);
    let xy = |name: &str| got.files.iter().find(|f| &f.path[..] == name.as_bytes()).map(|f| f.xy.clone());
    assert_eq!(xy("new.txt").as_deref(), Some("A."));
    assert_eq!(xy("pkgs/a/src/index.js").as_deref(), Some(".M"));

    // Unstage it again.
    let mut got = r
        .client
        .stage(wire::WsStageRequest { ws_id: r.ws.clone(), remove: vec!["new.txt".into()], ..Default::default() })
        .await
        .unwrap()
        .into_inner()
        .status
        .unwrap();
    got.token.clear();
    let cli = lines(ok(&r.f, &r.ep, &["status", "--json", "--untracked-files=all", "--"]).as_bytes());
    assert_eq!(js!("asc.v1.WsStatusResponse", got), cli[0]);
    assert_eq!(got.files.iter().find(|f| &f.path[..] == b"new.txt").map(|f| f.xy.clone()).as_deref(), Some("??"));

    // A patch stages a hunk (`git apply --cached`).
    let patch = "diff --git a/pkgs/a/src/index.js b/pkgs/a/src/index.js\n--- a/pkgs/a/src/index.js\n+++ b/pkgs/a/src/index.js\n@@ -1 +1 @@\n-2\n+3\n";
    let got = r
        .client
        .stage(wire::WsStageRequest { ws_id: r.ws.clone(), patch: patch.into(), ..Default::default() })
        .await
        .unwrap()
        .into_inner()
        .status
        .unwrap();
    assert_eq!(got.files.iter().find(|f| &f.path[..] == b"pkgs/a/src/index.js").map(|f| f.xy.clone()).as_deref(), Some("M."));

    // A patch that does not apply is refused and stages nothing.
    let bad = "diff --git a/README.md b/README.md\n--- a/README.md\n+++ b/README.md\n@@ -1 +1 @@\n-nothing like this\n+x\n";
    let e = r
        .client
        .stage(wire::WsStageRequest { ws_id: r.ws.clone(), patch: bad.into(), ..Default::default() })
        .await
        .unwrap_err();
    assert_eq!(e.code(), tonic::Code::FailedPrecondition, "{e}");
    let _ = r.c0;
}

#[tokio::test]
async fn requests_are_authorised_and_bounded() {
    let mut r = rig().await;
    let ws = r.ws.clone();
    let code = |e: tonic::Status| e.code();

    // Only a mounted workspace is served.
    let other = support::Fixture::new().await;
    let e = r.client.status(wire::WsStatusRequest { ws_id: other.ws.to_string(), ..Default::default() }).await.unwrap_err();
    assert_eq!(code(e), tonic::Code::FailedPrecondition);
    let e = r.client.status(wire::WsStatusRequest { ws_id: "../../x".into(), ..Default::default() }).await.unwrap_err();
    assert_eq!(code(e), tonic::Code::InvalidArgument);

    // A revision is never an option, and a path stays inside the work tree.
    let bad = wire::WsDiffRequest { ws_id: ws.clone(), from: "--output=evil".into(), to: "feat".into(), ..Default::default() };
    assert_eq!(code(r.client.diff(bad).await.unwrap_err()), tonic::Code::InvalidArgument);
    let query = wire::LogRequest { include: vec!["--all".into()], ..Default::default() };
    assert_eq!(code(r.client.log(wire::WsLogRequest { ws_id: ws.clone(), query: Some(query) }).await.unwrap_err()), tonic::Code::InvalidArgument);
    let req = wire::WsStatusRequest { ws_id: ws.clone(), paths: vec!["../outside".into()], ..Default::default() };
    assert_eq!(code(r.client.status(req).await.unwrap_err()), tonic::Code::InvalidArgument);
    let req = wire::WsStageRequest { ws_id: ws.clone(), add: vec!["/etc/passwd".into()], ..Default::default() };
    assert_eq!(code(r.client.stage(req).await.unwrap_err()), tonic::Code::InvalidArgument);
    let req = wire::WatchRequest { ws_id: ws.clone(), since_token: "not-a-token".into() };
    assert_eq!(code(r.client.watch(req).await.unwrap_err()), tonic::Code::InvalidArgument);

    // A bad revision is the CLI's error, not a crash.
    let query = wire::LogRequest { include: vec!["no-such-rev".into()], ..Default::default() };
    let bad = wire::WsLogRequest { ws_id: ws.clone(), query: Some(query) };
    let e = match r.client.log(bad).await {
        Ok(s) => collect_err(s.into_inner()).await,
        Err(e) => e,
    };
    assert_eq!(e.code(), tonic::Code::FailedPrecondition, "{e}");

    // Files named like options stay literal.
    write(&r.f.root, "-rf", b"x\n");
    let req = wire::WsStatusRequest { ws_id: ws, untracked: true, paths: vec!["-rf".into()], ..Default::default() };
    let got = r.client.status(req).await.unwrap().into_inner();
    assert_eq!(got.files.len(), 1);
    assert_eq!(&got.files[0].path[..], b"-rf");
}

async fn collect_err<T>(mut s: tonic::Streaming<T>) -> tonic::Status {
    loop {
        match s.message().await {
            Ok(Some(_)) => {}
            Ok(None) => panic!("the stream ended without an error"),
            Err(e) => return e,
        }
    }
}

// ----------------------------------------------------------------------------------- Watch

#[tokio::test]
async fn watch_reports_an_edit_a_commit_and_a_switch() {
    use wire::watch_event::Kind;
    let mut r = rig().await;
    let mut s = r.client.watch(wire::WatchRequest { ws_id: r.ws.clone(), since_token: String::new() }).await.unwrap().into_inner();

    // An edit.
    write(&r.f.root, "pkgs/a/src/index.js", b"3\n");
    let seen = until(&mut s, 30, |e| kind(e) == Kind::Paths).await;
    let paths = &seen.last().unwrap().paths;
    assert!(paths.iter().any(|p| &p[..] == b"pkgs/a/src/index.js"), "{paths:?}");
    assert!(seen.last().unwrap().token.starts_with("v1."));

    // Staging, then a commit that moves the branch.
    ok(&r.f, &r.ep, &["add", "pkgs/a/src/index.js"]);
    until(&mut s, 30, |e| kind(e) == Kind::Stage).await;
    ok(&r.f, &r.ep, &["commit", "-m", "edit a"]);
    let seen = until(&mut s, 30, |e| kind(e) == Kind::Head).await;
    let head = seen.last().unwrap();
    assert_eq!(head.r#ref, "refs/heads/feat");
    assert_eq!(head.old.len(), 20);
    assert_eq!(head.new.len(), 20);
    assert_ne!(head.old, head.new);
    let tip = ok(&r.f, &r.ep, &["rev-parse", "HEAD"]);
    assert_eq!(hex(&head.new), tip.trim());

    // Another branch moves: a ref event, not a HEAD one.
    ok(&r.f, &r.ep, &["branch", "other"]);
    let seen = until(&mut s, 30, |e| kind(e) == Kind::Ref).await;
    assert_eq!(seen.last().unwrap().r#ref, "refs/heads/other");

    // A switch.
    ok(&r.f, &r.ep, &["switch", "other"]);
    let seen = until(&mut s, 30, |e| kind(e) == Kind::SwitchDone).await;
    assert!(seen.iter().any(|e| kind(e) == Kind::Head && e.r#ref == "refs/heads/other"), "{:?}", seen.iter().map(kind).collect::<Vec<_>>());
}

struct Watcher {
    child: Child,
    rx: mpsc::Receiver<String>,
}

impl Drop for Watcher {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Watcher {
    fn start(r: &Rig, since: Option<&str>) -> Self {
        let mut args = vec!["watch", "--json"];
        if let Some(t) = since {
            args.extend(["--since", t]);
        }
        let mut c = asc_command(&r.f, &r.ep, &args);
        c.env_remove("ASC_NO_DAEMON").stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null());
        let mut child = c.spawn().unwrap();
        let out = child.stdout.take().unwrap();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(out).lines().map_while(Result::ok) {
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        Self { child, rx }
    }

    /// Events until one matches `kind` (by its profile name).
    fn until(&self, kind: &str, secs: u64) -> Vec<Value> {
        let end = Instant::now() + Duration::from_secs(secs);
        let mut seen = Vec::new();
        loop {
            let left = end.saturating_duration_since(Instant::now());
            let line = self.rx.recv_timeout(left).unwrap_or_else(|_| panic!("no {kind} event; saw {seen:?}"));
            let v: Value = serde_json::from_str(&line).unwrap();
            let hit = v["kind"] == kind;
            seen.push(v);
            if hit {
                return seen;
            }
        }
    }
}

#[tokio::test]
async fn asc_watch_since_resumes_without_loss() {
    let r = rig().await;
    write(&r.f.root, "new.txt", b"new\n");

    // The position before anything happens, as `Status` reports it.
    let mut r = r;
    let start = r.client.status(wire::WsStatusRequest { ws_id: r.ws.clone(), ..Default::default() }).await.unwrap().into_inner().token;

    // First run: see the staging, keep its token.
    ok(&r.f, &r.ep, &["add", "new.txt"]);
    let w = Watcher::start(&r, Some(&start));
    let seen = w.until("stage", 30);
    let token = seen.last().unwrap()["token"].as_str().unwrap().to_owned();
    assert!(token.starts_with("v1."), "{token}");
    drop(w);

    // Nobody watches while the commit happens.
    ok(&r.f, &r.ep, &["commit", "-m", "add new"]);

    // The second run replays what the first did not see: the branch move and the cleared staging,
    // and none of the staging it already saw.
    let w = Watcher::start(&r, Some(&token));
    let mut all = w.until("head", 30);
    all.extend(w.until("stage", 30));
    let kinds: Vec<&str> = all.iter().map(|e| e["kind"].as_str().unwrap()).collect();
    assert_eq!(kinds.iter().filter(|k| **k == "head").count(), 1, "{kinds:?}");
    let head = all.iter().find(|e| e["kind"] == "head").unwrap();
    assert_eq!(head["ref"], "refs/heads/feat");
    // Edits made while nobody watched cannot be replayed; the stream says so.
    assert!(kinds.contains(&"resync"), "{kinds:?}");
    // Every event carries a token that parses and a later one resumes after it.
    assert!(all.iter().all(|e| e["token"].as_str().is_some_and(|t| t.starts_with("v1."))));
}
