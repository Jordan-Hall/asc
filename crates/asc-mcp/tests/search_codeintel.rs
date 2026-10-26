//! I21: the `search` tool over `RpcBackend` against an in-process `CodeIntel` server. The server is a
//! descriptor-driven fake: it records each request as JSON and answers per RPC, so the mode mapping,
//! the injected connection (repository, credential, default scope) and the reply contract (`scope`,
//! `coverage`, `est_tokens`) are checked on the wire. The real handler is exercised by
//! `asc-serve/tests/mcp_search.rs`.

use std::convert::Infallible;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

use asc_mcp::backend::{CallContext, McpBackend, Reply};
use asc_mcp::config::{McpConfig, parse_toolsets};
use asc_mcp::cursor::KeyRing;
use asc_mcp::handler::{Caller, Handler, HandlerConfig};
use asc_mcp::rpc_backend::{DynCodec, RpcBackend};
use asc_mcp::schema::Surface;
use asc_rpc::auth::{BearerToken, Credential};
use asc_rpc::json;
use futures::StreamExt as _;
use prost_reflect::{DescriptorPool, DynamicMessage};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;
use tonic::codegen::http;
use tonic::transport::server::TcpIncoming;
use tonic::transport::{Channel, Endpoint, Server};
use tonic::{Request, Response, Status};

const REPO: &str = "acme/widgets";
const TOKEN: &str = "s3cr3t-token-value";
/// A change id: 16 bytes in the 32-letter alphabet `k`..`z`.
const CHANGE: &str = "kmkmkmkmkmkmkmkmkmkmkmkmkmkmkmkm";

#[derive(Clone, Debug)]
struct Seen {
    path: String,
    request: Value,
    authorization: Option<String>,
}

#[derive(Clone)]
struct Fake {
    pool: Arc<DescriptorPool>,
    seen: Arc<Mutex<Vec<Seen>>>,
}

fn answer(path: &str) -> Value {
    // `scope.coverage` is what the real service reports; `est_tokens` is its own estimate.
    let scope = json!({ "changeset": "ab12", "root": "cd34", "generation": 3, "coverage": 0.5 });
    match path {
        "/asc.v1.CodeIntel/Search" => json!({
            "scope": scope, "est_tokens": 17,
            "files": [{ "path": "src/lib.rs", "matches": [{ "line": 3, "text": "fn main() {}" }] }],
        }),
        "/asc.v1.CodeIntel/Symbols" => json!({
            "scope": scope, "est_tokens": 9,
            "symbols": [{ "name": "main", "kind": "function", "is_def": true, "path": "src/lib.rs", "line": 3, "confidence": "import_match" }],
        }),
        "/asc.v1.CodeIntel/Semantic" => json!({
            "scope": scope, "est_tokens": 21,
            "chunks": [{ "path": "src/lib.rs", "start_line": 1, "end_line": 4, "score": 0.9, "text": "fn main() {}" }],
        }),
        other => panic!("unexpected RPC {other}"),
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
                me.seen.lock().unwrap().push(Seen {
                    path: path.clone(),
                    request: json::encode(r.get_ref()).unwrap(),
                    authorization: r
                        .metadata()
                        .get("authorization")
                        .and_then(|v| v.to_str().ok())
                        .map(str::to_owned),
                });
                let m = json::decode(&output, &answer(&path)).unwrap();
                let stream = futures::stream::iter(vec![Ok::<_, Status>(m)]).boxed();
                Ok::<_, Status>(Response::new(stream))
            }
        });
        let mut grpc = tonic::server::Grpc::new(DynCodec::new(input));
        grpc.server_streaming(svc, req).await
    }
}

#[derive(Clone)]
struct CodeIntelSvc(Fake);
impl tower::Service<http::Request<tonic::body::Body>> for CodeIntelSvc {
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
impl tonic::server::NamedService for CodeIntelSvc {
    const NAME: &'static str = "asc.v1.CodeIntel";
}

struct Harness {
    channel: Channel,
    seen: Arc<Mutex<Vec<Seen>>>,
    _task: tokio::task::JoinHandle<()>,
}

async fn harness() -> Harness {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let fake = Fake {
        pool: Arc::new(DescriptorPool::decode(asc_rpc::DESCRIPTOR_SET).unwrap()),
        seen: seen.clone(),
    };
    let incoming = TcpIncoming::bind("127.0.0.1:0".parse().unwrap()).unwrap();
    let addr = incoming.local_addr().unwrap();
    let task = tokio::spawn(async move {
        let _ = Server::builder()
            .add_service(CodeIntelSvc(fake))
            .serve_with_incoming(incoming)
            .await;
    });
    let channel = Endpoint::from_shared(format!("http://{addr}"))
        .unwrap()
        .connect_lazy();
    Harness {
        channel,
        seen,
        _task: task,
    }
}

fn backend(h: &Harness, surface: Surface) -> RpcBackend {
    RpcBackend::new(surface).with_server(
        h.channel.clone(),
        REPO,
        Some(Credential::bearer(BearerToken::new(TOKEN).unwrap())),
    )
}

fn ctx() -> CallContext {
    CallContext {
        principal: None,
        budget_bytes: 1 << 20,
        position: None,
        cancel: CancellationToken::new(),
        progress: None,
    }
}

async fn run(b: &RpcBackend, args: Value) -> Vec<Value> {
    match McpBackend::call(b, "search", &args, &ctx()).await.unwrap() {
        Reply::Messages(s) => s.map(|m| m.unwrap()).collect().await,
        Reply::Verb { .. } => panic!("a verb reply"),
    }
}

fn only_seen(h: &Harness) -> Seen {
    let mut seen = h.seen.lock().unwrap();
    assert_eq!(seen.len(), 1, "{seen:?}");
    seen.pop().unwrap()
}

#[tokio::test]
async fn each_mode_reaches_its_rpc_with_the_connection_injected() {
    let h = harness().await;
    let cases = [
        (json!({"query": "fn main", "mode": "text"}), "Search"),
        (json!({"query": "fn +main", "mode": "regex"}), "Search"),
        (
            json!({"query": "main", "mode": "symbol", "op": "refs"}),
            "Symbols",
        ),
        (
            json!({"query": "where does it start", "mode": "semantic", "k": 4, "rerank": true}),
            "Semantic",
        ),
    ];
    for surface in [Surface::Stdio, Surface::Http] {
        let b = backend(&h, surface);
        for (args, rpc) in &cases {
            let msgs = run(&b, args.clone()).await;
            assert_eq!(msgs.len(), 1, "{args}");
            let m = &msgs[0];
            // The reply contract: `scope`, `coverage` and `est_tokens` on every mode.
            assert_eq!(m["scope"]["changeset"], "ab12", "{args}");
            assert_eq!(m["coverage"], 0.5, "{args}");
            assert!(m["est_tokens"].as_u64().unwrap() > 0, "{args}");

            let seen = only_seen(&h);
            assert_eq!(seen.path, format!("/asc.v1.CodeIntel/{rpc}"), "{args}");
            assert_eq!(
                seen.authorization.as_deref(),
                Some(format!("Bearer {TOKEN}").as_str())
            );
            // The connection decides the repository; no scope means the default branch.
            let scope = &seen.request["scope"];
            assert_eq!(scope["repo"]["name"], REPO, "{args}");
            assert_eq!(scope["rev"], "HEAD", "{args}");
            match *rpc {
                "Search" => {
                    assert_eq!(seen.request["pattern"], args["query"]);
                    // The profile leaves out the default (`literal`).
                    let mode = (args["mode"] == "regex").then_some("regex");
                    assert_eq!(seen.request["mode"].as_str(), mode, "{args}");
                    assert_eq!(seen.request["context_lines"], 2);
                    assert_eq!(seen.request["budget"]["max_results"], 50);
                    assert_eq!(m["files"][0]["path"], "src/lib.rs");
                }
                "Symbols" => {
                    assert_eq!(seen.request["name"], "main");
                    assert_eq!(seen.request["op"], "refs");
                    assert_eq!(m["symbols"][0]["confidence"], "import_match");
                }
                _ => {
                    assert_eq!(seen.request["query"], args["query"]);
                    assert_eq!(seen.request["k"], 4);
                    assert_eq!(seen.request["rerank"], true);
                    assert_eq!(m["chunks"][0]["start_line"], 1);
                }
            }
        }
    }
}

#[tokio::test]
async fn an_explicit_scope_reaches_the_wire() {
    let h = harness().await;
    let b = backend(&h, Surface::Http);
    run(
        &b,
        json!({"query": "x", "mode": "text", "rev": "v1.2", "paths": ["src", "docs/a b"], "include_excluded": true}),
    )
    .await;
    let s = only_seen(&h).request["scope"].clone();
    assert_eq!(s["rev"], "v1.2");
    assert_eq!(s["paths"], json!(["src", "docs/a b"]));
    assert_eq!(s["include_excluded"], true);

    run(
        &b,
        json!({"query": "x", "mode": "text", "stack": CHANGE, "diff_only": true}),
    )
    .await;
    let s = only_seen(&h).request["scope"].clone();
    assert_eq!(s["stack_top"], CHANGE);
    assert_eq!(s["stack_diff_only"], true);
    assert!(s.get("rev").is_none(), "{s}");

    run(&b, json!({"query": "x", "mode": "text", "change": CHANGE})).await;
    let s = only_seen(&h).request["scope"].clone();
    assert_eq!(s["change"], CHANGE);
}

#[tokio::test]
async fn the_repo_argument_cannot_redirect_the_call() {
    let h = harness().await;
    let b = backend(&h, Surface::Http);
    let e = McpBackend::call(
        &b,
        "search",
        &json!({"query": "x", "mode": "text", "repo": "other/repo"}),
        &ctx(),
    )
    .await
    .unwrap_err();
    assert_eq!(e.code, "PATH_POLICY");
    assert!(h.seen.lock().unwrap().is_empty());
    // The connection's own name is accepted.
    run(&b, json!({"query": "x", "mode": "text", "repo": REPO})).await;
    assert_eq!(only_seen(&h).request["scope"]["repo"]["name"], REPO);
}

#[tokio::test]
async fn bad_arguments_never_reach_the_server() {
    let h = harness().await;
    let b = backend(&h, Surface::Stdio);
    for args in [
        json!({"query": "x", "mode": "text", "op": "def"}),
        json!({"query": "x", "mode": "fuzzy"}),
        json!({"query": "x", "mode": "text", "paths": ["../etc"]}),
        json!({"query": "x", "mode": "text", "rev": "a", "change": CHANGE}),
    ] {
        McpBackend::call(&b, "search", &args, &ctx())
            .await
            .unwrap_err();
    }
    assert!(h.seen.lock().unwrap().is_empty());
}

#[tokio::test]
async fn without_a_server_the_tool_is_not_available() {
    let b = RpcBackend::new(Surface::Stdio);
    let e = McpBackend::call(&b, "search", &json!({"query": "x", "mode": "text"}), &ctx())
        .await
        .unwrap_err();
    assert_eq!(e.code, "NOT_AVAILABLE");
}

fn handler(b: RpcBackend, surface: Surface, features: &[&str]) -> Handler<RpcBackend> {
    let mut hc = HandlerConfig::new(surface);
    hc.config = McpConfig {
        toolsets: parse_toolsets("core,search"),
        ..McpConfig::default()
    };
    hc.features = features.iter().map(|f| (*f).to_owned()).collect();
    Handler::new(b, hc, KeyRing::from_keys(&[[7; 32]]).unwrap())
}

async fn rpc(h: &Handler<RpcBackend>, method: &str, params: Value) -> Value {
    let (tx, _rx) = tokio::sync::mpsc::channel(8);
    let msg = json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": params });
    h.handle_message(msg, &Caller::default(), &tx)
        .await
        .expect("a response")
}

fn has_search(resp: &Value) -> bool {
    resp["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .any(|t| t["name"] == "search")
}

#[tokio::test]
async fn the_handler_lists_and_serves_search_only_with_search_v1() {
    let h = harness().await;
    for surface in [Surface::Stdio, Surface::Http] {
        let without = handler(backend(&h, surface), surface, &[]);
        assert!(!has_search(&rpc(&without, "tools/list", json!({})).await));
        let denied = rpc(
            &without,
            "tools/call",
            json!({"name": "search", "arguments": {"query": "x", "mode": "text"}}),
        )
        .await;
        assert!(denied["error"].is_object(), "{denied}");

        let with = handler(backend(&h, surface), surface, &["search.v1"]);
        let list = rpc(&with, "tools/list", json!({})).await;
        assert!(has_search(&list), "{list}");
        let mut args = json!({"query": "fn main", "mode": "text"});
        if surface == Surface::Http {
            args["repo"] = REPO.into();
        }
        let resp = rpc(
            &with,
            "tools/call",
            json!({"name": "search", "arguments": args}),
        )
        .await;
        let result = &resp["result"];
        assert_ne!(result["isError"], true, "{resp}");
        let text = result.to_string();
        assert!(
            text.contains("src/lib.rs") && text.contains("coverage"),
            "{text}"
        );
        h.seen.lock().unwrap().clear();
    }
}
