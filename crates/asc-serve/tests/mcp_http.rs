//! M12a: the `/mcp` transport. Stateless Streamable HTTP over a stub backend: JSON and SSE
//! replies, 405 for GET and DELETE, Origin (403) and Host (421) checks, header agreement (-32020),
//! body caps, the refused `enabled = true`, the cache role's 404, and the id isolation of
//! stateless clients.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use axum::Router;
use axum::body::Body;
use axum::http::{Request, Response, StatusCode, header};
use asc_mcp::backend::{CallContext, McpBackend, Reply};
use asc_mcp::cursor::KeyRing;
use asc_mcp::errors::ToolError;
use asc_mcp::handler::{Handler, HandlerConfig};
use asc_mcp::schema::Surface;
use asc_serve::config::{CacheConfig, ServeConfig};
use asc_serve::http::Metrics;
use asc_serve::mcp::limits::{MAX_BODY, MAX_COMMIT_BODY};
use asc_serve::mcp::{self, HttpPolicy, McpPrincipal};
use futures::StreamExt;
use serde_json::{Value, json};
use tower::ServiceExt;

const HOST: &str = "asc.example.org";

#[derive(Default)]
struct Stub {
    calls: AtomicUsize,
}

#[async_trait::async_trait]
impl McpBackend for Stub {
    async fn call(&self, tool: &str, _args: &Value, ctx: &CallContext) -> Result<Reply, ToolError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if let Some(p) = &ctx.progress {
            p.report(1.0, Some(2.0), Some("working"));
        }
        let who = ctx.principal.clone().unwrap_or_default();
        Ok(Reply::Messages(Box::pin(futures::stream::iter(vec![Ok(
            json!({ "tool": tool, "principal": who }),
        )]))))
    }
}

fn app() -> (Router, Arc<Stub>) {
    let stub = Arc::new(Stub::default());
    struct Shared(Arc<Stub>);
    #[async_trait::async_trait]
    impl McpBackend for Shared {
        async fn call(
            &self,
            tool: &str,
            args: &Value,
            ctx: &CallContext,
        ) -> Result<Reply, ToolError> {
            self.0.call(tool, args, ctx).await
        }
    }
    let handler = Handler::new(
        Shared(stub.clone()),
        HandlerConfig::new(Surface::Http),
        KeyRing::random().unwrap(),
    );
    let policy = HttpPolicy::new(
        "127.0.0.1:8443".parse().unwrap(),
        Some("https://asc.example.org"),
        &["alias.example.org".to_owned()],
        &["https://app.example.org".to_owned()],
    );
    (mcp::router(handler, policy), stub)
}

fn post(body: &Value) -> axum::http::request::Builder {
    let _ = body;
    Request::builder()
        .method("POST")
        .uri("/mcp")
        .header(header::HOST, HOST)
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::ACCEPT, "application/json, text/event-stream")
}

fn with_principal(mut req: Request<Body>) -> Request<Body> {
    req.extensions_mut().insert(McpPrincipal {
        id: "alice".into(),
        write: false,
    });
    req
}

async fn send(app: &Router, req: Request<Body>) -> Response<Body> {
    app.clone().oneshot(with_principal(req)).await.unwrap()
}

async fn body_json(r: Response<Body>) -> Value {
    let b = axum::body::to_bytes(r.into_body(), usize::MAX)
        .await
        .unwrap();
    serde_json::from_slice(&b).unwrap()
}

fn rpc(id: u64, method: &str, params: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params })
}

fn log_call(id: u64, extra_meta: Option<Value>) -> Value {
    let mut params = json!({ "name": "log", "arguments": { "repo": "acme/widgets" } });
    if let Some(m) = extra_meta {
        params["_meta"] = m;
    }
    rpc(id, "tools/call", params)
}

#[tokio::test]
async fn post_json_reply_and_tool_call() {
    let (app, stub) = app();
    let b = rpc(7, "server/discover", json!({}));
    let r = send(&app, post(&b).body(Body::from(b.to_string())).unwrap()).await;
    assert_eq!(r.status(), StatusCode::OK);
    assert_eq!(r.headers()[header::CONTENT_TYPE], "application/json");
    let v = body_json(r).await;
    assert_eq!(v["id"], 7);
    assert!(v["result"]["supportedVersions"].is_array());

    let b = log_call(8, None);
    let r = send(&app, post(&b).body(Body::from(b.to_string())).unwrap()).await;
    assert_eq!(r.status(), StatusCode::OK);
    let v = body_json(r).await;
    assert_eq!(v["id"], 8, "the client's id is restored");
    assert_eq!(stub.calls.load(Ordering::SeqCst), 1);
    assert!(
        v["result"].to_string().contains("alice"),
        "the principal reaches the backend: {v}"
    );
}

#[tokio::test]
async fn sse_reply_streams_progress_then_result() {
    let (app, _) = app();
    let b = log_call(3, Some(json!({ "progressToken": "p1" })));
    let r = send(&app, post(&b).body(Body::from(b.to_string())).unwrap()).await;
    assert_eq!(r.status(), StatusCode::OK);
    assert_eq!(r.headers()[header::CONTENT_TYPE], "text/event-stream");
    assert_eq!(r.headers()["x-accel-buffering"], "no");
    let mut events = Vec::new();
    let mut s = r.into_body().into_data_stream();
    let mut text = String::new();
    while let Some(c) = s.next().await {
        text.push_str(std::str::from_utf8(&c.unwrap()).unwrap());
    }
    for e in text.split("\n\n").filter(|e| !e.is_empty()) {
        let data = e
            .strip_prefix("event: message\ndata: ")
            .unwrap_or_else(|| panic!("bad event {e:?}"));
        events.push(serde_json::from_str::<Value>(data).unwrap());
    }
    assert!(events.len() >= 2, "{events:?}");
    assert_eq!(events[0]["method"], "notifications/progress");
    let last = events.last().unwrap();
    assert_eq!(last["id"], 3);
    assert!(last.get("result").is_some());
}

#[tokio::test]
async fn notification_is_accepted_without_a_body() {
    let (app, _) = app();
    let b = json!({ "jsonrpc": "2.0", "method": "notifications/initialized" });
    let r = send(&app, post(&b).body(Body::from(b.to_string())).unwrap()).await;
    assert_eq!(r.status(), StatusCode::ACCEPTED);
}

#[tokio::test]
async fn get_and_delete_are_405() {
    let (app, _) = app();
    for m in ["GET", "DELETE", "PUT"] {
        let req = Request::builder()
            .method(m)
            .uri("/mcp")
            .header(header::HOST, HOST)
            .body(Body::empty())
            .unwrap();
        let r = send(&app, req).await;
        assert_eq!(r.status(), StatusCode::METHOD_NOT_ALLOWED, "{m}");
        assert_eq!(r.headers()[header::ALLOW], "POST");
    }
}

#[tokio::test]
async fn origin_is_checked_when_present() {
    let (app, stub) = app();
    let b = log_call(1, None);
    let go = |origin: Option<&str>| {
        let mut r = post(&b);
        if let Some(o) = origin {
            r = r.header(header::ORIGIN, o);
        }
        r.body(Body::from(b.to_string())).unwrap()
    };
    assert_eq!(
        send(&app, go(Some("https://evil.example.net")))
            .await
            .status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        send(&app, go(Some("null"))).await.status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        stub.calls.load(Ordering::SeqCst),
        0,
        "a refused origin never reaches the backend"
    );
    assert_eq!(
        send(&app, go(Some("https://app.example.org")))
            .await
            .status(),
        StatusCode::OK
    );
    assert_eq!(
        send(&app, go(Some("https://asc.example.org")))
            .await
            .status(),
        StatusCode::OK
    );
    assert_eq!(
        send(&app, go(None)).await.status(),
        StatusCode::OK,
        "an absent Origin is accepted"
    );
}

#[tokio::test]
async fn host_outside_the_allowlist_is_421() {
    let (app, stub) = app();
    let b = log_call(1, None);
    for host in ["evil.example.net", "127.0.0.1:9", "asc.example.org.evil.net"] {
        let req = Request::builder()
            .method("POST")
            .uri("/mcp")
            .header(header::HOST, host)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(b.to_string()))
            .unwrap();
        assert_eq!(
            send(&app, req).await.status(),
            StatusCode::MISDIRECTED_REQUEST,
            "{host}"
        );
    }
    let req = Request::builder()
        .method("POST")
        .uri("/mcp")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(b.to_string()))
        .unwrap();
    assert_eq!(
        send(&app, req).await.status(),
        StatusCode::MISDIRECTED_REQUEST,
        "no Host at all"
    );
    assert_eq!(stub.calls.load(Ordering::SeqCst), 0);
    for host in ["alias.example.org", "127.0.0.1:8443", "localhost:8443"] {
        let req = Request::builder()
            .method("POST")
            .uri("/mcp")
            .header(header::HOST, host)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(b.to_string()))
            .unwrap();
        assert_eq!(send(&app, req).await.status(), StatusCode::OK, "{host}");
    }
}

#[tokio::test]
async fn header_mismatch_is_32020() {
    let (app, stub) = app();
    let b = log_call(5, None);
    let cases: [(&str, &str); 4] = [
        ("mcp-method", "tools/list"),
        ("mcp-name", "show"),
        ("mcp-param-repo", "other/repo"),
        ("mcp-protocol-version", "1999-01-01"),
    ];
    for (name, value) in cases {
        let r = send(
            &app,
            post(&b)
                .header(name, value)
                .body(Body::from(b.to_string()))
                .unwrap(),
        )
        .await;
        assert_eq!(r.status(), StatusCode::BAD_REQUEST, "{name}");
        let v = body_json(r).await;
        assert_eq!(v["error"]["code"], -32020, "{name}");
        assert_eq!(v["id"], 5);
    }
    assert_eq!(stub.calls.load(Ordering::SeqCst), 0);
    let r = send(
        &app,
        post(&b)
            .header("mcp-method", "tools/call")
            .header("mcp-name", "log")
            .header("mcp-param-repo", "acme/widgets")
            .header("mcp-protocol-version", "2026-07-28")
            .body(Body::from(b.to_string()))
            .unwrap(),
    )
    .await;
    assert_eq!(r.status(), StatusCode::OK);
}

#[tokio::test]
async fn body_caps() {
    let (app, _) = app();
    let pad = "x".repeat(MAX_BODY + 10);
    let big = json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/call",
        "params": { "name": "log", "arguments": { "repo": "a/b", "pad": pad } } });
    let r = send(&app, post(&big).body(Body::from(big.to_string())).unwrap()).await;
    assert_eq!(r.status(), StatusCode::PAYLOAD_TOO_LARGE);

    // 1 MiB passes the cap (the call itself is judged by the handler, not the transport).
    let ok = json!({ "jsonrpc": "2.0", "id": 1, "method": "ping", "params": { "pad": "x".repeat(MAX_BODY - 200) } });
    assert!(ok.to_string().len() <= MAX_BODY);
    let r = send(&app, post(&ok).body(Body::from(ok.to_string())).unwrap()).await;
    assert_eq!(r.status(), StatusCode::OK);

    // `commit` may use 4 MiB.
    let commit = json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/call",
        "params": { "name": "commit", "arguments": { "repo": "a/b", "pad": pad } } });
    let r = send(
        &app,
        post(&commit).body(Body::from(commit.to_string())).unwrap(),
    )
    .await;
    assert_eq!(r.status(), StatusCode::OK);

    // Past 4 MiB nothing passes, and a declared length is refused before the body is read.
    let huge = json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/call",
        "params": { "name": "commit", "arguments": { "pad": "x".repeat(MAX_COMMIT_BODY) } } });
    let r = send(
        &app,
        post(&huge).body(Body::from(huge.to_string())).unwrap(),
    )
    .await;
    assert_eq!(r.status(), StatusCode::PAYLOAD_TOO_LARGE);
    let r = send(
        &app,
        post(&huge)
            .header(header::CONTENT_LENGTH, (MAX_COMMIT_BODY + 1).to_string())
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(r.status(), StatusCode::PAYLOAD_TOO_LARGE);
}

#[tokio::test]
async fn malformed_requests() {
    let (app, _) = app();
    let b = json!({});
    let r = send(&app, post(&b).body(Body::from("{not json")).unwrap()).await;
    assert_eq!(r.status(), StatusCode::BAD_REQUEST);
    assert_eq!(body_json(r).await["error"]["code"], -32700);
    let r = send(&app, post(&b).body(Body::from("[]")).unwrap()).await;
    assert_eq!(
        r.status(),
        StatusCode::BAD_REQUEST,
        "batches are not accepted"
    );
    let r = send(
        &app,
        Request::builder()
            .method("POST")
            .uri("/mcp")
            .header(header::HOST, HOST)
            .header(header::CONTENT_TYPE, "text/plain")
            .body(Body::from("{}"))
            .unwrap(),
    )
    .await;
    assert_eq!(r.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);
}

#[tokio::test]
async fn no_principal_is_401() {
    let (app, stub) = app();
    let b = log_call(1, None);
    let r = app
        .clone()
        .oneshot(post(&b).body(Body::from(b.to_string())).unwrap())
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(stub.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn clients_sharing_an_id_do_not_see_each_other() {
    let (app, _) = app();
    let b = rpc(1, "ping", json!({}));
    let (a, c) = tokio::join!(
        send(&app, post(&b).body(Body::from(b.to_string())).unwrap()),
        send(&app, post(&b).body(Body::from(b.to_string())).unwrap()),
    );
    assert_eq!(body_json(a).await["id"], 1);
    assert_eq!(body_json(c).await["id"], 1);
    // A cancellation posted by another client cancels nothing.
    let n = json!({ "jsonrpc": "2.0", "method": "notifications/cancelled", "params": { "requestId": 1 } });
    let r = send(&app, post(&n).body(Body::from(n.to_string())).unwrap()).await;
    assert_eq!(r.status(), StatusCode::ACCEPTED);
}

#[tokio::test]
async fn enabled_is_refused_while_no_auth_layer_exists() {
    let base = "store = \"memory:///\"
db = \"sqlite::memory:\"
";
    assert!(ServeConfig::parse(base).is_ok());
    let err = ServeConfig::parse(&format!(
        "{base}[mcp]
enabled = true"
    ))
    .unwrap_err()
    .to_string();
    assert!(err.contains("auth layer"), "{err}");
}

#[tokio::test]
async fn cache_role_answers_404() {
    let cache = asc_serve::cache_role::CacheRole::new(
        "https://upstream.invalid",
        &CacheConfig::default(),
        Arc::new(Metrics::new()),
    )
    .unwrap();
    let router = cache.extensions().http.router();
    let b = log_call(1, None);
    let r = send(&router, post(&b).body(Body::from(b.to_string())).unwrap()).await;
    assert_eq!(r.status(), StatusCode::NOT_FOUND);

    // `register` mounts nothing on the cache role, even if the flag were set.
    let mut cfg = ServeConfig {
        role: asc_serve::config::Role::Cache,
        ..ServeConfig::default()
    };
    cfg.mcp.enabled = true;
    let ext = asc_serve::registry::Extensions::new();
    mcp::register(&ext, &cfg);
    let r = send(
        &ext.http.router(),
        post(&b).body(Body::from(b.to_string())).unwrap(),
    )
    .await;
    assert_eq!(r.status(), StatusCode::NOT_FOUND);
}
