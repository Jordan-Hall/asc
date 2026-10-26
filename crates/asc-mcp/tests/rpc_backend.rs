//! `RpcBackend` against in-process tonic servers (unit M08). The servers are descriptor-driven
//! fakes: they record each request as JSON and answer with scripted messages, so every read tool of
//! SPEC 22.3.2 is checked on the wire without an engine.

use std::convert::Infallible;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::Duration;

use asc_mcp::backend::{CallContext, McpBackend, Reply};
use asc_mcp::catalogue::{self, Needs};
use asc_mcp::cursor::Position;
use asc_mcp::rpc_backend::{DynCodec, RpcBackend};
use asc_mcp::schema::Surface;
use asc_rpc::auth::{BearerToken, Credential};
use asc_rpc::json;
use futures::StreamExt as _;
use futures::stream::BoxStream;
use prost_reflect::{DescriptorPool, DynamicMessage};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;
use tonic::codegen::http;
use tonic::transport::server::TcpIncoming;
use tonic::transport::{Channel, Endpoint, Server};
use tonic::{Request, Response, Status};

type Script = Arc<dyn Fn(&str, &Value) -> Result<Vec<Value>, Status> + Send + Sync>;

#[derive(Clone, Debug)]
struct Seen {
    path: String,
    request: Value,
    authorization: Option<String>,
}

#[derive(Clone)]
struct Fake {
    pool: Arc<DescriptorPool>,
    script: Script,
    seen: Arc<Mutex<Vec<Seen>>>,
    /// Set when a response stream is dropped.
    dropped: Arc<AtomicBool>,
    /// Answer with an endless stream of this message instead of the script.
    endless: Option<Value>,
}

struct DropFlag(Arc<AtomicBool>);
impl Drop for DropFlag {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

impl Fake {
    async fn serve(
        self,
        req: http::Request<tonic::body::Body>,
    ) -> http::Response<tonic::body::Body> {
        let path = req.uri().path().to_owned();
        let method = path
            .strip_prefix('/')
            .and_then(|p| p.split_once('/'))
            .and_then(|(s, m)| {
                self.pool
                    .get_service_by_name(s)?
                    .methods()
                    .find(|x| x.name() == m)
            })
            .expect("known method");
        let (input, output) = (method.input(), method.output());
        let svc = tower::service_fn(move |r: Request<DynamicMessage>| {
            let me = self.clone();
            let path = path.clone();
            let output = output.clone();
            async move {
                let request = json::encode(r.get_ref()).unwrap();
                me.seen.lock().unwrap().push(Seen {
                    path: path.clone(),
                    request: request.clone(),
                    authorization: r
                        .metadata()
                        .get("authorization")
                        .and_then(|v| v.to_str().ok())
                        .map(str::to_owned),
                });
                let stream: BoxStream<'static, Result<DynamicMessage, Status>> = if let Some(msg) =
                    me.endless.clone()
                {
                    let flag = DropFlag(me.dropped.clone());
                    futures::stream::unfold((msg, output, flag), |(msg, output, flag)| async move {
                        tokio::task::yield_now().await;
                        let m = json::decode(&output, &msg).unwrap();
                        Some((Ok(m), (msg, output, flag)))
                    })
                    .boxed()
                } else {
                    let items = (me.script)(&path, &request)?;
                    let msgs: Vec<_> = items
                        .iter()
                        .map(|v| Ok(json::decode(&output, v).unwrap()))
                        .collect();
                    futures::stream::iter(msgs).boxed()
                };
                Ok::<_, Status>(Response::new(stream))
            }
        });
        let mut grpc = tonic::server::Grpc::new(DynCodec::new(input));
        grpc.server_streaming(svc, req).await
    }
}

macro_rules! named {
    ($ty:ident, $name:literal) => {
        #[derive(Clone)]
        struct $ty(Fake);
        impl tower::Service<http::Request<tonic::body::Body>> for $ty {
            type Response = http::Response<tonic::body::Body>;
            type Error = Infallible;
            type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Infallible>> + Send>>;
            fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Infallible>> {
                Poll::Ready(Ok(()))
            }
            fn call(&mut self, req: http::Request<tonic::body::Body>) -> Self::Future {
                let f = self.0.clone();
                Box::pin(async move { Ok(f.serve(req).await) })
            }
        }
        impl tonic::server::NamedService for $ty {
            const NAME: &'static str = $name;
        }
    };
}
named!(RepoSvc, "asc.v1.Repo");
named!(ChangesSvc, "asc.v1.Changes");
named!(ProjectsSvc, "asc.v1.Projects");
named!(WorkspaceSvc, "asc.v1.Workspace");

struct Harness {
    server: Channel,
    ipc: Channel,
    seen: Arc<Mutex<Vec<Seen>>>,
    dropped: Arc<AtomicBool>,
    _tasks: Vec<tokio::task::JoinHandle<()>>,
}

const REPO: &str = "acme/widgets";
const TOKEN: &str = "s3cr3t-token-value";

async fn harness(script: Script, endless: Option<Value>) -> Harness {
    let pool = Arc::new(DescriptorPool::decode(asc_rpc::DESCRIPTOR_SET).unwrap());
    let seen = Arc::new(Mutex::new(Vec::new()));
    let dropped = Arc::new(AtomicBool::new(false));
    let fake = Fake {
        pool,
        script,
        seen: seen.clone(),
        dropped: dropped.clone(),
        endless,
    };
    let mut tasks = Vec::new();
    let mut channels = Vec::new();
    for _ in 0..2 {
        let incoming = TcpIncoming::bind("127.0.0.1:0".parse().unwrap()).unwrap();
        let addr = incoming.local_addr().unwrap();
        let f = fake.clone();
        tasks.push(tokio::spawn(async move {
            let _ = Server::builder()
                .add_service(RepoSvc(f.clone()))
                .add_service(ChangesSvc(f.clone()))
                .add_service(ProjectsSvc(f.clone()))
                .add_service(WorkspaceSvc(f))
                .serve_with_incoming(incoming)
                .await;
        }));
        channels.push(
            Endpoint::from_shared(format!("http://{addr}"))
                .unwrap()
                .connect_lazy(),
        );
    }
    let ipc = channels.pop().unwrap();
    let server = channels.pop().unwrap();
    Harness {
        server,
        ipc,
        seen,
        dropped,
        _tasks: tasks,
    }
}

fn credential() -> Credential {
    Credential::bearer(BearerToken::new(TOKEN).unwrap())
}

fn backend(h: &Harness, surface: Surface) -> RpcBackend {
    RpcBackend::new(surface)
        .with_server(h.server.clone(), REPO, Some(credential()))
        .with_workspace(h.ipc.clone(), "ws1")
}

fn ctx(budget: u64) -> CallContext {
    CallContext {
        principal: None,
        budget_bytes: budget,
        position: None,
        cancel: CancellationToken::new(),
        progress: None,
    }
}

fn empty_script() -> Script {
    Arc::new(|_, _| Ok(vec![json!({})]))
}

async fn collect(reply: Reply) -> Vec<Value> {
    match reply {
        Reply::Messages(s) => s.map(|m| m.unwrap()).collect().await,
        Reply::Verb { .. } => panic!("expected messages"),
    }
}

fn args_for(tool: &str) -> Value {
    match tool {
        "read_file" => json!({ "rev": "main", "path": "src/lib.rs" }),
        _ => json!({}),
    }
}

/// Tools no RPC serves here: `op_log`, `drafts` and `cloud_status` run child verbs; `search` is wired by M15.
const NOT_RPC: [&str; 4] = ["op_log", "search", "drafts", "cloud_status"];

#[tokio::test]
async fn each_read_tool_reaches_its_rpc_with_the_connection_injected() {
    let h = harness(empty_script(), None).await;
    let mut checked = 0;
    for surface in [Surface::Stdio, Surface::Http] {
        let b = backend(&h, surface);
        for def in catalogue::TOOLS
            .iter()
            .filter(|d| d.on(surface) && d.ann.read_only && d.needs == Needs::None)
        {
            if NOT_RPC.contains(&def.name) {
                let e = b
                    .call(def.name, &json!({}), &ctx(1 << 20))
                    .await
                    .unwrap_err();
                assert_eq!(e.code, "NOT_AVAILABLE", "{}", def.name);
                continue;
            }
            h.seen.lock().unwrap().clear();
            let reply = b
                .call(def.name, &args_for(def.name), &ctx(1 << 20))
                .await
                .unwrap_or_else(|e| panic!("{} ({surface:?}): {e}", def.name));
            let msgs = collect(reply).await;
            assert_eq!(msgs.len(), 1, "{} ({surface:?})", def.name);
            let seen = h.seen.lock().unwrap().clone();
            assert_eq!(seen.len(), 1, "{} ({surface:?})", def.name);
            let alt = if surface == Surface::Stdio {
                def.rpc.split(" / ").next().unwrap()
            } else {
                def.rpc.split(" / ").last().unwrap()
            };
            let (service, method) = alt.split_once('.').unwrap_or(("Repo", alt));
            assert_eq!(
                seen[0].path,
                format!("/asc.v1.{service}/{method}"),
                "{}",
                def.name
            );
            let sent = seen[0].request.to_string();
            if service == "Workspace" {
                assert!(
                    seen[0].authorization.is_none(),
                    "{}: the daemon is never sent a token",
                    def.name
                );
            } else {
                assert_eq!(
                    seen[0].authorization.as_deref(),
                    Some(format!("Bearer {TOKEN}").as_str()),
                    "{}",
                    def.name
                );
                assert!(
                    sent.contains(REPO) || !sent.contains("\"repo\""),
                    "{}",
                    def.name
                );
            }
            checked += 1;
        }
    }
    assert!(checked >= 20, "only {checked} tools checked");
}

#[tokio::test]
async fn read_file_joins_rev_and_path_and_status_gets_the_workspace() {
    let h = harness(empty_script(), None).await;
    let b = backend(&h, Surface::Stdio);
    collect(
        b.call(
            "read_file",
            &json!({ "rev": "main", "path": "a b/c.rs", "start_line": 3 }),
            &ctx(1 << 20),
        )
        .await
        .unwrap(),
    )
    .await;
    collect(
        b.call(
            "status",
            &json!({ "untracked": true, "max_files": 10 }),
            &ctx(1 << 20),
        )
        .await
        .unwrap(),
    )
    .await;
    let seen = h.seen.lock().unwrap().clone();
    assert_eq!(seen[0].request["rev_path"], "main:a b/c.rs");
    assert_eq!(seen[0].request["repo"]["name"], REPO);
    assert!(seen[0].request.get("start_line").is_none());
    assert_eq!(seen[1].path, "/asc.v1.Workspace/Status");
    assert_eq!(seen[1].request["ws_id"], "ws1");
    assert_eq!(seen[1].request["untracked"], true);
}

#[tokio::test]
async fn a_working_copy_read_is_not_served_over_rpc() {
    let h = harness(empty_script(), None).await;
    let b = backend(&h, Surface::Stdio);
    let e = b
        .call("read_file", &json!({ "path": "src/lib.rs" }), &ctx(1 << 20))
        .await
        .unwrap_err();
    assert_eq!(e.code, "NOT_AVAILABLE");
    assert!(h.seen.lock().unwrap().is_empty());
}

#[tokio::test]
async fn the_response_is_the_folded_rpc_message_in_the_json_profile() {
    let script: Script = Arc::new(|_, _| {
        Ok(vec![
            json!({ "matches": [{ "path": "a.rs", "line": 1, "text": "fn a()" }] }),
            json!({ "matches": [{ "path": "b.rs", "line": 2, "text": "fn b()" }] }),
        ])
    });
    let h = harness(script, None).await;
    let b = backend(&h, Surface::Http);
    let msgs = collect(
        b.call(
            "grep",
            &json!({ "repo": REPO, "rev": "main", "pattern": "fn" }),
            &ctx(1 << 20),
        )
        .await
        .unwrap(),
    )
    .await;
    assert_eq!(msgs.len(), 2);
    assert_eq!(msgs[0]["matches"][0]["path"], "a.rs");
    assert_eq!(msgs[1]["matches"][0]["text"], "fn b()");
}

#[tokio::test]
async fn a_cursor_position_skips_what_was_already_sent() {
    let script: Script = Arc::new(|_, _| {
        Ok((0..5)
            .map(|i| json!({ "matches": [{ "path": format!("f{i}"), "line": 1 }] }))
            .collect())
    });
    let h = harness(script, None).await;
    let b = backend(&h, Surface::Http);
    let mut c = ctx(1 << 20);
    c.position = Some(Position::Index { index: 3 });
    let msgs = collect(
        b.call("grep", &json!({ "pattern": "x" }), &c)
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(msgs.len(), 2);
    assert_eq!(msgs[0]["matches"][0]["path"], "f3");
}

#[tokio::test]
async fn a_stream_is_dropped_when_the_budget_is_spent() {
    let h = harness(
        empty_script(),
        Some(json!({ "matches": [{ "path": "a.rs", "line": 1, "text": "x".repeat(200) }] })),
    )
    .await;
    let b = backend(&h, Surface::Http);
    let reply = b
        .call("grep", &json!({ "pattern": "x" }), &ctx(1000))
        .await
        .unwrap();
    let msgs = collect(reply).await;
    // The message that crosses the budget is handed over (so the handler can mark `truncated`);
    // nothing after it is read.
    assert!((2..=6).contains(&msgs.len()), "{} messages", msgs.len());
    for _ in 0..100 {
        if h.dropped.load(Ordering::SeqCst) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("the server-side stream was not dropped after the budget was spent");
}

#[tokio::test]
async fn cancelling_ends_the_stream() {
    let h = harness(empty_script(), Some(json!({ "matches": [] }))).await;
    let b = backend(&h, Surface::Http);
    let c = ctx(u64::MAX);
    let cancel = c.cancel.clone();
    let Reply::Messages(mut s) = b
        .call("grep", &json!({ "pattern": "x" }), &c)
        .await
        .unwrap()
    else {
        panic!()
    };
    assert!(s.next().await.is_some());
    cancel.cancel();
    let rest = tokio::time::timeout(Duration::from_secs(5), async {
        while s.next().await.is_some() {}
    })
    .await;
    assert!(rest.is_ok(), "the stream kept running after cancel");
}

#[tokio::test]
async fn worktrees_come_from_the_daemon() {
    let script: Script = Arc::new(|path, _| {
        assert_eq!(path, "/asc.v1.Workspace/Worktrees");
        Ok(vec![
            json!({ "worktrees": [{ "ws_id": "ws1", "path": "/work/a" }, { "ws_id": "ws2", "path": "/work/b" }] }),
        ])
    });
    let h = harness(script, None).await;
    let got = backend(&h, Surface::Stdio).worktrees().await.unwrap();
    assert_eq!(
        got,
        vec![
            std::path::PathBuf::from("/work/a"),
            std::path::PathBuf::from("/work/b")
        ]
    );
    // A server-only backend has no workspace.
    let b = RpcBackend::new(Surface::Http).with_server(h.server.clone(), REPO, None);
    assert!(b.worktrees().await.unwrap().is_empty());
}

// ---- security ----------------------------------------------------------------------------

#[tokio::test]
async fn the_arguments_cannot_name_another_repository_or_workspace() {
    let h = harness(empty_script(), None).await;
    let b = backend(&h, Surface::Http);
    let e = b
        .call(
            "grep",
            &json!({ "repo": "other/secret", "pattern": "x" }),
            &ctx(1 << 20),
        )
        .await
        .unwrap_err();
    assert_eq!(e.code, "PATH_POLICY");
    assert!(
        h.seen.lock().unwrap().is_empty(),
        "nothing is sent for a foreign repository"
    );

    let b = backend(&h, Surface::Stdio);
    collect(
        b.call("status", &json!({ "ws_id": "evil-ws" }), &ctx(1 << 20))
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(h.seen.lock().unwrap()[0].request["ws_id"], "ws1");
}

#[tokio::test]
async fn write_tools_never_reach_the_wire() {
    let h = harness(empty_script(), None).await;
    let b = backend(&h, Surface::Stdio);
    for def in catalogue::TOOLS
        .iter()
        .filter(|d| d.on(Surface::Stdio) && (!d.ann.read_only || d.needs != Needs::None))
    {
        let e = b
            .call(def.name, &json!({}), &ctx(1 << 20))
            .await
            .unwrap_err();
        assert_eq!(e.code, "NOT_AVAILABLE", "{}", def.name);
    }
    assert!(h.seen.lock().unwrap().is_empty());
}

#[tokio::test]
async fn errors_do_not_echo_the_credential_or_control_characters() {
    let script: Script =
        Arc::new(|_, _| Err(Status::unauthenticated(format!("bad token Bearer {TOKEN}"))));
    let h = harness(script, None).await;
    let b = backend(&h, Surface::Http);
    let e = b
        .call("grep", &json!({ "pattern": "x" }), &ctx(1 << 20))
        .await
        .unwrap_err();
    assert!(!format!("{e:?}").contains(TOKEN), "{e:?}");

    let script: Script =
        Arc::new(|_, _| Err(Status::not_found("no such\u{1b}[31m rev\nnext line")));
    let h = harness(script, None).await;
    let e = backend(&h, Surface::Http)
        .call("grep", &json!({ "pattern": "x" }), &ctx(1 << 20))
        .await
        .unwrap_err();
    assert_eq!(e.code, "NOT_FOUND");
    assert!(!e.message.chars().any(char::is_control), "{:?}", e.message);
}

#[tokio::test]
async fn an_unreachable_server_is_not_available() {
    let ch = Endpoint::from_static("http://127.0.0.1:9")
        .connect_timeout(Duration::from_millis(200))
        .connect_lazy();
    let b = RpcBackend::new(Surface::Http).with_server(ch, REPO, None);
    let e = b
        .call("grep", &json!({ "pattern": "x" }), &ctx(1 << 20))
        .await
        .unwrap_err();
    assert_eq!(e.code, "NOT_AVAILABLE");
}

#[tokio::test]
async fn a_missing_connection_is_not_available() {
    let b = RpcBackend::new(Surface::Stdio);
    assert_eq!(
        b.call("status", &json!({}), &ctx(1))
            .await
            .unwrap_err()
            .code,
        "NOT_AVAILABLE"
    );
    assert_eq!(
        b.call("grep", &json!({ "pattern": "x" }), &ctx(1))
            .await
            .unwrap_err()
            .code,
        "NOT_AVAILABLE"
    );
}

#[test]
fn plain_http_to_a_remote_host_is_refused() {
    let r = RpcBackend::new(Surface::Http).connect_server(
        "http://example.com/",
        REPO,
        Some(credential()),
    );
    assert!(r.is_err());
}
