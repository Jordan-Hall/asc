//! `Workspace.Inputs` answers what `asc inputs --json` prints: a real `asc daemon run` serves the
//! RPC on a private endpoint (`ASC_DAEMON_ENDPOINT`), so the user's own daemon is never involved.

mod support;

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use asc_core::id::FileHasher;
use asc_rpc::ipc::{IpcEndpoint, WorkspaceClient, connect};
use asc_rpc::server::wire;
use bytes::Bytes;
use support::{Fixture, text};
use tonic::Code;
use wire::ws_input_entry::Kind;

struct Daemon(std::process::Child);

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn write(root: &Path, rel: &str, content: &[u8]) {
    let path = root.join(rel);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, content).unwrap();
}

fn endpoint_name(f: &Fixture) -> String {
    if cfg!(windows) {
        format!(
            "asc-inputs-{}-{}",
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

fn hash(bytes: &[u8]) -> Vec<u8> {
    let mut h = FileHasher::new();
    h.update(bytes);
    h.finalize().as_bytes().to_vec()
}

#[tokio::test]
async fn inputs_match_the_cli() {
    let f = Fixture::new().await;
    let files: &[(&str, &[u8])] = &[
        ("app/a.rs", b"a\n"),
        ("app/b.rs", b"b\n"),
        ("lib/c.rs", b"c\n"),
    ];
    let cs = f.commit(&[], files).await;
    f.set_branch("master", cs).await;
    f.checkout_branch("master", Some(cs)).await;
    for (p, c) in files {
        write(&f.root, p, c);
    }
    write(&f.root, "app/a.rs", b"changed\n");

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
    let _daemon = Daemon(child);
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
    let req = |paths: &[&str], allow_missing: bool, digest_only: bool| wire::WsInputsRequest {
        ws_id: ws.clone(),
        paths: paths
            .iter()
            .map(|p| Bytes::from(p.as_bytes().to_vec()))
            .collect(),
        allow_missing,
        digest_only,
    };

    let got = client
        .inputs(req(&[], false, false))
        .await
        .unwrap()
        .into_inner();
    let rows: Vec<_> = got
        .entries
        .iter()
        .map(|e| (e.kind, String::from_utf8_lossy(&e.path).into_owned()))
        .collect();
    assert_eq!(
        rows,
        [
            (Kind::File as i32, "app/a.rs".to_owned()),
            (Kind::File as i32, "app/b.rs".to_owned()),
            (Kind::Tree as i32, "lib".to_owned()),
        ]
    );
    assert_eq!(got.entries[0].id.as_ref(), hash(b"changed\n"));
    assert_eq!(got.entries[1].id.as_ref(), hash(b"b\n"));

    let cli = f
        .asc()
        .args(["inputs", "--digest"])
        .env("ASC_NO_DAEMON", "1")
        .output()
        .unwrap();
    assert!(cli.status.success(), "{}", text(&cli.stderr));
    let hex: String = got.digest.iter().map(|b| format!("{b:02x}")).collect();
    assert_eq!(text(&cli.stdout).trim(), hex, "the RPC and the CLI agree");

    let only = client
        .inputs(req(&["lib"], false, true))
        .await
        .unwrap()
        .into_inner();
    assert!(only.entries.is_empty());
    assert_eq!(only.digest.len(), 32);

    let missing = client
        .inputs(req(&["nope"], false, false))
        .await
        .unwrap_err();
    assert_eq!(missing.code(), Code::FailedPrecondition, "{missing:?}");
    let allowed = client
        .inputs(req(&["nope"], true, false))
        .await
        .unwrap()
        .into_inner();
    assert!(allowed.entries.is_empty());

    let unmounted = client
        .inputs(wire::WsInputsRequest {
            ws_id: "00000000-0000-0000-0000-000000000000".into(),
            ..Default::default()
        })
        .await
        .unwrap_err();
    assert_ne!(unmounted.code(), Code::Ok);
}
