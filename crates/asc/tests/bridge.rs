//! P01a acceptance: `asc api --stdio` speaks length-prefixed `BridgeFrame`s (SPEC §24.2): framing,
//! deny-by-default routing, `Hello` with its features, and a relayed server call.
#![cfg(feature = "serve")]

mod server_fixture;
mod support;

use std::io::{Read, Write};
use std::path::Path;
use std::process::{Child, Stdio};

use asc_rpc::server::wire::{
    BridgeFrame, Call, Connectivity, HelloRequest, HelloResponse, VersionRequest, VersionResponse,
    bridge_frame::F,
};
use prost_reflect::prost::Message as _;
use server_fixture::{Server, WRITE};
use support::{Sandbox, text};

const OK: i32 = 0;
const PERMISSION_DENIED: i32 = 7;
const FAILED_PRECONDITION: i32 = 9;
const UNIMPLEMENTED: i32 = 12;

struct Bridge {
    child: Child,
}

impl Bridge {
    fn start(sb: &Sandbox, cwd: &Path, token: Option<&str>) -> Self {
        let mut cmd = sb.asc(cwd);
        cmd.args(["api", "--stdio"])
            .env("ASC_NO_DAEMON", "1")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(t) = token {
            cmd.env("ASC_TOKEN", t);
        }
        let child = cmd.spawn().expect("spawn asc api --stdio");
        // A bridge that stops answering fails the test instead of hanging it.
        let pid = child.id().to_string();
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_secs(45));
            let _ = if cfg!(windows) {
                std::process::Command::new("taskkill")
                    .args(["/F", "/PID", &pid])
                    .output()
            } else {
                std::process::Command::new("kill")
                    .args(["-9", &pid])
                    .output()
            };
        });
        Self { child }
    }

    fn send_raw(&mut self, bytes: &[u8]) {
        let stdin = self.child.stdin.as_mut().unwrap();
        stdin.write_all(bytes).unwrap();
        stdin.flush().unwrap();
    }

    fn send(&mut self, frame: &BridgeFrame) {
        let body = frame.encode_to_vec();
        let mut wire = (body.len() as u32).to_le_bytes().to_vec();
        wire.extend_from_slice(&body);
        self.send_raw(&wire);
    }

    fn call(&mut self, id: u32, method: &str, request: Vec<u8>) {
        self.send(&BridgeFrame {
            id,
            f: Some(F::Call(Call {
                method: method.to_owned(),
                request: request.into(),
                ..Call::default()
            })),
        });
    }

    fn recv(&mut self) -> Option<BridgeFrame> {
        let out = self.child.stdout.as_mut().unwrap();
        let mut len = [0u8; 4];
        out.read_exact(&mut len).ok()?;
        let mut body = vec![0u8; u32::from_le_bytes(len) as usize];
        out.read_exact(&mut body).ok()?;
        Some(BridgeFrame::decode(body.as_slice()).expect("a frame"))
    }

    /// Replies and the closing code of call `id`.
    fn finish(&mut self, id: u32) -> (Vec<Vec<u8>>, i32, String) {
        let mut replies = Vec::new();
        loop {
            let Some(f) = self.recv() else {
                let _ = self.child.kill();
                let mut err = String::new();
                if let Some(mut e) = self.child.stderr.take() {
                    let _ = e.read_to_string(&mut err);
                }
                panic!("the bridge closed before End; stderr: {err}");
            };
            assert_eq!(f.id, id, "frame for another call");
            match f.f {
                Some(F::Reply(b)) => replies.push(b.to_vec()),
                Some(F::End(e)) => return (replies, e.code, e.message),
                other => panic!("unexpected frame {other:?}"),
            }
        }
    }

    fn close(mut self) -> std::process::Output {
        drop(self.child.stdin.take());
        self.child.wait_with_output().expect("wait")
    }
}

fn workspace(sb: &Sandbox) -> std::path::PathBuf {
    let a = sb.dir("a");
    let out = sb.run(&a, &["init", "-q"]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    a
}

#[test]
fn hello_reports_features_and_the_workspace() {
    let sb = Sandbox::new();
    let a = workspace(&sb);
    let mut b = Bridge::start(&sb, &a, None);
    b.call(
        1,
        "asc.v1.Bridge/Hello",
        HelloRequest::default().encode_to_vec(),
    );
    let (replies, code, msg) = b.finish(1);
    assert_eq!(code, OK, "{msg}");
    let hello = HelloResponse::decode(replies[0].as_slice()).unwrap();
    assert!(hello.features.contains(&"plugin.v1".to_owned()));
    assert!(!hello.asc_version.is_empty());
    let root = &hello.roots[0];
    assert!(!root.ws_id.is_empty(), "{root:?}");
    assert_eq!(root.remote_kind, "none");
    assert_eq!(root.server, Connectivity::NotConfigured as i32);
    assert_eq!(root.daemon, Connectivity::NotConfigured as i32);
    assert!(root.branchless);
    let out = b.close();
    assert!(out.status.success(), "{}", text(&out.stderr));
}

#[test]
fn a_plain_directory_has_no_ws_id_and_a_relative_root_is_refused() {
    let sb = Sandbox::new();
    let a = workspace(&sb);
    let plain = sb.dir("plain");
    let mut b = Bridge::start(&sb, &a, None);
    let req = HelloRequest {
        roots: vec![
            plain.to_string_lossy().into_owned(),
            "relative/dir".to_owned(),
        ],
    };
    b.call(1, "asc.v1.Bridge/Hello", req.encode_to_vec());
    let (replies, code, _) = b.finish(1);
    assert_eq!(code, OK);
    let hello = HelloResponse::decode(replies[0].as_slice()).unwrap();
    assert!(hello.roots[0].ws_id.is_empty() && hello.roots[0].error.is_empty());
    assert!(!hello.roots[1].error.is_empty(), "{:?}", hello.roots[1]);
}

#[test]
fn routing_is_deny_by_default() {
    let sb = Sandbox::new();
    let a = workspace(&sb);
    let mut b = Bridge::start(&sb, &a, None);
    for (id, method, want) in [
        (1, "asc.v1.Workspace/Shutdown", PERMISSION_DENIED),
        (2, "grpc.health.v1.Health/Check", PERMISSION_DENIED),
        (3, "asc.v1.Repo/NoSuchMethod", UNIMPLEMENTED),
        (4, "asc.v1.Repo/PutObjects", UNIMPLEMENTED),
        (5, "../etc/passwd", PERMISSION_DENIED),
        (10, "asc.v1.Repo", UNIMPLEMENTED),
        (6, "asc.v1.Bridge/Run", UNIMPLEMENTED),
        (7, "asc.v1.Bridge/Token", UNIMPLEMENTED),
        // The daemon is turned off in this sandbox.
        (8, "asc.v1.Workspace/Ping", FAILED_PRECONDITION),
        // A workspace without a remote has no server.
        (9, "asc.v1.Repo/Version", FAILED_PRECONDITION),
    ] {
        b.call(id, method, Vec::new());
        let (replies, code, msg) = b.finish(id);
        assert_eq!(code, want, "{method}: {msg}");
        assert!(replies.is_empty(), "{method}");
    }
}

#[test]
fn an_oversized_frame_ends_the_bridge_and_stdout_stays_clean() {
    let sb = Sandbox::new();
    let a = workspace(&sb);
    let mut b = Bridge::start(&sb, &a, None);
    // The bridge exits non-zero without reading a body.
    b.send_raw(&(17u32 * 1024 * 1024).to_le_bytes());
    let out = b.close();
    assert!(!out.status.success());
    assert!(out.stdout.is_empty(), "stdout carries frames only");
    assert!(
        text(&out.stderr).contains("16 MiB"),
        "{}",
        text(&out.stderr)
    );
}

#[test]
fn server_calls_are_relayed_and_hello_joins_server_features() {
    let server = Server::start(&["acme/app"]);
    let url = server.url("acme/app");
    let sb = Sandbox::new();
    let a = workspace(&sb);
    let out = sb.run(&a, &["remote", "add", "origin", &url]);
    assert!(out.status.success(), "{}", text(&out.stderr));

    let mut b = Bridge::start(&sb, &a, Some(WRITE));
    b.call(
        1,
        "asc.v1.Repo/Version",
        VersionRequest::default().encode_to_vec(),
    );
    let (replies, code, msg) = b.finish(1);
    assert_eq!(code, OK, "{msg}");
    let v = VersionResponse::decode(replies[0].as_slice()).unwrap();
    assert!(!v.server_version.is_empty());

    b.call(
        2,
        "asc.v1.Bridge/Hello",
        HelloRequest::default().encode_to_vec(),
    );
    let (replies, code, msg) = b.finish(2);
    assert_eq!(code, OK, "{msg}");
    let hello = HelloResponse::decode(replies[0].as_slice()).unwrap();
    assert_eq!(hello.roots[0].remote_kind, "asc", "{:?}", hello.roots[0]);
    assert_eq!(hello.roots[0].repo, "acme/app");
    assert_eq!(hello.roots[0].server, Connectivity::Up as i32);
    assert!(hello.features.contains(&"plugin.v1".to_owned()));
    for f in &v.features {
        assert!(
            hello.features.contains(f),
            "{f} missing from {:?}",
            hello.features
        );
    }
    // The credential never reaches the host's view or stderr.
    assert!(!format!("{hello:?}").contains(WRITE));
    let out = b.close();
    assert!(!text(&out.stderr).contains(WRITE));
}
