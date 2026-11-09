//! `asc signing default` against a stand-in `Signing` server on loopback: it reads and sets the account's
//! sign-by-default switch, a set carries an operation id, the value is kept in the global config as
//! `asc.signByDefault`, a local `commit.gpgsign` is reported as winning, and a bad value never leaves the machine.

mod support;

use std::path::Path;
use std::process::Output;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use asc_rpc::server::wire;
use asc_rpc::services::signing_server::{Signing, SigningServer};
use support::{Sandbox, text};
use tonic::{Request, Response, Status};

const TOKEN: &str = "secret-token-value";

#[derive(Default)]
struct State {
    on: bool,
    updated: i64,
    gets: usize,
    sets: Vec<(bool, Option<String>)>,
}

#[derive(Clone, Default)]
struct Fake(Arc<Mutex<State>>);

fn op_of<T>(r: &Request<T>) -> Option<String> {
    r.metadata()
        .get("x-asc-op-id")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned)
}

#[tonic::async_trait]
impl Signing for Fake {
    async fn list_keys(
        &self,
        _: Request<wire::ListSigningKeysRequest>,
    ) -> Result<Response<wire::ListSigningKeysResponse>, Status> {
        Err(Status::unimplemented("not in this test"))
    }
    async fn add_key(
        &self,
        _: Request<wire::AddSigningKeyRequest>,
    ) -> Result<Response<wire::AddSigningKeyResponse>, Status> {
        Err(Status::unimplemented("not in this test"))
    }
    async fn revoke_key(
        &self,
        _: Request<wire::RevokeSigningKeyRequest>,
    ) -> Result<Response<wire::RevokeSigningKeyResponse>, Status> {
        Err(Status::unimplemented("not in this test"))
    }
    async fn submit_signature(
        &self,
        _: Request<wire::SubmitSignatureRequest>,
    ) -> Result<Response<wire::SubmitSignatureResponse>, Status> {
        Err(Status::unimplemented("not in this test"))
    }
    async fn verify(
        &self,
        _: Request<wire::VerifySignaturesRequest>,
    ) -> Result<Response<wire::VerifySignaturesResponse>, Status> {
        Err(Status::unimplemented("not in this test"))
    }
    async fn get_server_key(
        &self,
        _: Request<wire::GetServerKeyRequest>,
    ) -> Result<Response<wire::GetServerKeyResponse>, Status> {
        Err(Status::unimplemented("not in this test"))
    }
    async fn get_signing_defaults(
        &self,
        r: Request<wire::GetSigningDefaultsRequest>,
    ) -> Result<Response<wire::GetSigningDefaultsResponse>, Status> {
        assert!(
            r.get_ref().user.is_empty(),
            "the CLI asks for its own account"
        );
        let mut s = self.0.lock().unwrap();
        s.gets += 1;
        Ok(Response::new(wire::GetSigningDefaultsResponse {
            sign_by_default: s.on,
            updated: s.updated,
        }))
    }
    async fn set_signing_defaults(
        &self,
        r: Request<wire::SetSigningDefaultsRequest>,
    ) -> Result<Response<wire::SetSigningDefaultsResponse>, Status> {
        let op = op_of(&r);
        let mut s = self.0.lock().unwrap();
        s.on = r.get_ref().sign_by_default;
        s.updated = 1_794_000_000;
        let on = s.on;
        s.sets.push((on, op));
        Ok(Response::new(wire::SetSigningDefaultsResponse {
            sign_by_default: s.on,
            updated: s.updated,
        }))
    }
}

struct Server {
    addr: std::net::SocketAddr,
    fake: Fake,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    thread: Option<JoinHandle<()>>,
}

impl Server {
    fn start() -> Self {
        let fake = Fake::default();
        let svc = fake.clone();
        let (addr_tx, addr_rx) = std::sync::mpsc::channel();
        let (stop, stop_rx) = tokio::sync::oneshot::channel::<()>();
        let thread = std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(1)
                .enable_all()
                .build()
                .expect("runtime");
            rt.block_on(async move {
                let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                    .await
                    .expect("bind");
                addr_tx.send(listener.local_addr().unwrap()).unwrap();
                let incoming = futures::stream::unfold(listener, |l| async move {
                    let r = l.accept().await.map(|(s, _)| s);
                    Some((r, l))
                });
                let _ = tonic::transport::Server::builder()
                    .add_service(SigningServer::new(svc))
                    .serve_with_incoming_shutdown(incoming, async {
                        let _ = stop_rx.await;
                    })
                    .await;
            });
        });
        Self {
            addr: addr_rx.recv().unwrap(),
            fake,
            stop: Some(stop),
            thread: Some(thread),
        }
    }

    fn url(&self) -> String {
        format!("asc+http://{}/acme/app", self.addr)
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        if let Some(s) = self.stop.take() {
            let _ = s.send(());
        }
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn run(sb: &Sandbox, cwd: &Path, server: &Server, pre: &[&str], args: &[&str]) -> Output {
    let mut cmd = sb.asc(cwd);
    cmd.env("ASC_TOKEN", TOKEN).env("ASC_NO_DAEMON", "1");
    cmd.args(pre).args(args).args(["--remote", &server.url()]);
    cmd.output().expect("spawn asc")
}

fn ok(out: &Output) -> String {
    let (so, se) = (text(&out.stdout), text(&out.stderr));
    assert_eq!(out.status.code(), Some(0), "stderr: {se}");
    assert!(
        !format!("{so}{se}").contains(TOKEN),
        "the token was printed"
    );
    so
}

fn global(sb: &Sandbox) -> String {
    // Keys are case-insensitive; the writer keeps them lower case.
    std::fs::read_to_string(sb.user.join(".gitconfig"))
        .unwrap()
        .to_ascii_lowercase()
}

#[test]
fn signing_default_reads_sets_and_keeps_the_account_switch() {
    let server = Server::start();
    let sb = Sandbox::new();
    let cwd = sb.dir("w");

    let so = ok(&run(&sb, &cwd, &server, &[], &["signing", "default"]));
    assert_eq!(so.trim(), "sign by default: off");
    assert!(
        global(&sb).contains("signbydefault = false"),
        "{}",
        global(&sb)
    );

    let so = ok(&run(&sb, &cwd, &server, &[], &["signing", "default", "on"]));
    assert_eq!(so.trim(), "sign by default: on");
    assert!(
        global(&sb).contains("signbydefault = true"),
        "{}",
        global(&sb)
    );
    {
        let s = server.fake.0.lock().unwrap();
        assert_eq!(s.sets.len(), 1);
        assert!(s.sets[0].0);
        assert!(
            s.sets[0].1.as_deref().is_some_and(|o| !o.is_empty()),
            "a set carries an operation id"
        );
    }

    let so = ok(&run(
        &sb,
        &cwd,
        &server,
        &[],
        &["signing", "default", "--json"],
    ));
    let v: serde_json::Value = serde_json::from_str(&so).unwrap();
    assert_eq!(v["sign_by_default"], true, "{so}");

    // A local commit.gpgsign that disagrees is named as the winner.
    let so = ok(&run(
        &sb,
        &cwd,
        &server,
        &["-c", "commit.gpgsign=false"],
        &["signing", "default"],
    ));
    assert!(so.contains("commit.gpgsign is false here and wins"), "{so}");

    let so = ok(&run(
        &sb,
        &cwd,
        &server,
        &[],
        &["signing", "default", "off"],
    ));
    assert_eq!(so.trim(), "sign by default: off");
    assert!(global(&sb).contains("signbydefault = false"));
    assert_eq!(server.fake.0.lock().unwrap().gets, 3);
}

#[test]
fn a_bad_signing_value_never_leaves_the_machine() {
    let server = Server::start();
    let sb = Sandbox::new();
    let cwd = sb.dir("w");
    for bad in ["maybe", "ON ", "1x"] {
        let out = run(&sb, &cwd, &server, &[], &["signing", "default", bad]);
        assert_ne!(out.status.code(), Some(0));
        assert!(
            text(&out.stderr).contains("on or off"),
            "{}",
            text(&out.stderr)
        );
    }
    let s = server.fake.0.lock().unwrap();
    assert_eq!((s.gets, s.sets.len()), (0, 0));
    assert!(!global(&sb).contains("signbydefault"));
}
