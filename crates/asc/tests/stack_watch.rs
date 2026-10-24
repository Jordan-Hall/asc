//! C49 acceptance: `Workspace.Watch` delivers `KIND_STACK` when the local change index records a
//! rewrite or a restack (SPEC 20.6). A real `asc daemon run` serves the stream on a private
//! endpoint (`ASC_DAEMON_ENDPOINT`), so the user's own daemon is never involved.

mod support;

use std::path::Path;
use std::process::Stdio;
use std::time::{Duration, Instant};

use asc_core::id::ChangesetId;
use asc_rpc::ipc::{IpcEndpoint, WorkspaceClient, connect};
use asc_rpc::server::wire;
use support::Fixture;

use asc_core::change::{ChangeId, MetaId, RewriteOp};
use asc_engine::change::{ChangeRow, ChangeRows as _};
use asc_state::changes::StateChanges;

use tonic::transport::Channel;

const A0: &[u8] = b"l1\nl2\nl3\n";
const A_FEAT: &[u8] = b"l1\nl2 feat\nl3\n";
const A_OTHER: &[u8] = b"l1\nl2 other\nl3\n";

struct Daemon {
    child: std::process::Child,
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

struct Rig {
    f: Fixture,
    client: WorkspaceClient<Channel>,
    ws: String,
    _daemon: Daemon,
}

fn write(root: &Path, rel: &str, content: &[u8]) {
    let path = root.join(rel);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, content).unwrap();
}

fn endpoint_name(f: &Fixture) -> String {
    if cfg!(windows) {
        format!(
            "asc-c49-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        )
    } else {
        f.sandbox
            .tmp
            .path()
            .join("ipc")
            .join("d.sock")
            .to_string_lossy()
            .into_owned()
    }
}

/// `master` at c0, `feat` (checked out) at c1 one change ahead, `other` at c2 (conflicts with c1
/// on `a.txt`); the work tree holds c1; a daemon serves it.
async fn rig() -> Rig {
    let f = Fixture::new().await;
    let c0 = f.commit(&[], &[("a.txt", A0), ("b.txt", b"b\n")]).await;
    let c1 = f.commit(&[c0], &[("a.txt", A_FEAT)]).await;
    let c2 = f.commit(&[c0], &[("a.txt", A_OTHER)]).await;
    f.set_branch("master", c0).await;
    f.set_branch("feat", c1).await;
    f.set_branch("other", c2).await;
    f.checkout_branch("feat", Some(c1)).await;
    write(&f.root, "a.txt", A_FEAT);
    write(&f.root, "b.txt", b"b\n");

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
    client
        .mount(wire::MountRequest { ws_id: ws.clone() })
        .await
        .unwrap();
    Rig {
        f,
        client,
        ws,
        _daemon: daemon,
    }
}

fn row(cs: u8, change: u8, op: RewriteOp) -> ChangeRow {
    ChangeRow {
        cs: ChangesetId::of(&[cs]),
        change: ChangeId::from_bytes([change; 16]),
        meta: MetaId::from_bytes([cs; 32]),
        op,
    }
}

async fn put(r: &Rig, cs: u8, change: u8, op: RewriteOp) {
    let rows = StateChanges::new(r.f.db.clone(), r.f.repo);
    rows.insert(&row(cs, change, op), &[]).await.unwrap();
}

async fn open_watch(r: &mut Rig, token: &str) -> tonic::Streaming<wire::WatchEvent> {
    r.client
        .watch(wire::WatchRequest {
            ws_id: r.ws.clone(),
            since_token: token.into(),
        })
        .await
        .unwrap()
        .into_inner()
}

async fn next_stack(s: &mut tonic::Streaming<wire::WatchEvent>) -> wire::WatchEvent {
    let end = Instant::now() + Duration::from_secs(30);
    loop {
        let left = end.saturating_duration_since(Instant::now());
        let e = tokio::time::timeout(left, s.message())
            .await
            .expect("no KIND_STACK event arrived")
            .unwrap()
            .expect("the stream ended");
        if e.kind == wire::watch_event::Kind::Stack as i32 {
            return e;
        }
    }
}

#[tokio::test]
async fn a_rewrite_and_a_restack_are_pushed_but_a_creation_is_not() {
    let mut r = rig().await;
    let mut s = open_watch(&mut r, "").await;
    put(&r, 1, 1, RewriteOp::New).await;
    put(&r, 2, 1, RewriteOp::Amend).await;
    let e = next_stack(&mut s).await;
    assert_eq!(&e.change[..], &[1u8; 16][..]);
    assert_eq!(e.op, "amend", "the creation sent no event");
    assert!(e.token.starts_with("v1."));
    put(&r, 3, 2, RewriteOp::Restack).await;
    let e = next_stack(&mut s).await;
    assert_eq!((&e.change[..], e.op.as_str()), (&[2u8; 16][..], "restack"));
}

#[tokio::test]
async fn a_resumed_stream_gets_the_rewrites_it_missed() {
    let mut r = rig().await;
    let mut s = open_watch(&mut r, "").await;
    put(&r, 1, 1, RewriteOp::Amend).await;
    let first = next_stack(&mut s).await;
    drop(s);
    put(&r, 2, 2, RewriteOp::Rebase).await;
    put(&r, 3, 3, RewriteOp::Squash).await;
    let mut s = open_watch(&mut r, &first.token).await;
    let a = next_stack(&mut s).await;
    let b = next_stack(&mut s).await;
    assert_eq!(
        (&a.change[..], a.op.as_str(), &b.change[..], b.op.as_str()),
        (&[2u8; 16][..], "rebase", &[3u8; 16][..], "squash")
    );
}

#[tokio::test]
async fn a_token_from_before_c49_resumes_from_now() {
    let mut r = rig().await;
    put(&r, 1, 1, RewriteOp::Amend).await;
    let old = "v1.0.0.0.0.0000000000000000.0000000000000000";
    // The stream accepts the old token shape; the earlier rewrite is not replayed.
    let mut s = open_watch(&mut r, old).await;
    put(&r, 2, 2, RewriteOp::Amend).await;
    let e = next_stack(&mut s).await;
    assert_eq!(&e.change[..], &[2u8; 16][..]);
}
