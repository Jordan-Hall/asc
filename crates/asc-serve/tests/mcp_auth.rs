//! M13: `/mcp` auth at the edge (SPEC 22.7.3, 15.6.2). A request without a token gets 401 with the
//! Basic challenge and a Bearer challenge carrying `resource_metadata`; too little access gets 403
//! `insufficient_scope`; the protected-resource document is public at both well-known paths with
//! an empty `authorization_servers` (no `[mcp.oauth]`); a `tools/list` shows no write tool to a
//! read-only principal; a stale list never grants a call.
//!
//! Runs over the static token table. TODO(K19): the same suite against `fake_forgejo`
//! (`PublicApiAuthz`, `InternalAuthz`) once that fake is shared outside `tests/fake_forgejo.rs`.

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, Response, StatusCode, header};
use asc_mcp::backend::{CallContext, McpBackend, Reply};
use asc_mcp::cursor::KeyRing;
use asc_mcp::errors::ToolError;
use asc_mcp::handler::{Handler, HandlerConfig};
use asc_mcp::schema::Surface;
use asc_rpc::auth::{Access, Authorizer, StaticTokens};
use asc_serve::config::ServeConfig;
use asc_serve::mcp::audit::MemoryAudit;
use asc_serve::mcp::auth::{McpAuth, layer};
use asc_serve::mcp::prm::{self, Prm, metadata_url, scope_layer};
use asc_serve::mcp::{self, HttpPolicy};
use serde_json::{Value, json};
use tower::ServiceExt;

const READ_TOKEN: &str = "tok-read-0123456789";
const WRITE_TOKEN: &str = "tok-write-0123456789";
const HOST: &str = "asc.example.org";
const PUBLIC: &str = "https://asc.example.org";

struct Stub;

#[async_trait::async_trait]
impl McpBackend for Stub {
    async fn call(
        &self,
        tool: &str,
        _args: &Value,
        _ctx: &CallContext,
    ) -> Result<Reply, ToolError> {
        Ok(Reply::Messages(Box::pin(futures::stream::iter(vec![Ok(
            json!({ "tool": tool }),
        )]))))
    }
}

fn policy() -> HttpPolicy {
    HttpPolicy::new("127.0.0.1:8443".parse().unwrap(), Some(PUBLIC), &[], &[])
}

fn authz() -> Arc<dyn Authorizer> {
    Arc::new(
        StaticTokens::new()
            .with_token(READ_TOKEN, "reader", Access::Read)
            .with_token(WRITE_TOKEN, "writer", Access::Write),
    )
}

/// The endpoint behind M29's layer and M13's scope layer, plus the public metadata routes.
fn app() -> Router {
    let handler = Handler::new(
        Stub,
        HandlerConfig::new(Surface::Http),
        KeyRing::random().unwrap(),
    );
    let a = authz();
    let mcp = scope_layer(
        layer(
            mcp::router(handler, policy()),
            McpAuth::new(
                a.clone(),
                Arc::new(MemoryAudit::default()),
                &metadata_url(PUBLIC),
            ),
        ),
        a,
    );
    mcp.merge(prm::router(Prm::new(Some(PUBLIC), Vec::new(), policy())))
}

fn rpc(method: &str, params: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": params })
}

async fn post(app: &Router, token: Option<&str>, body: &Value) -> Response<Body> {
    let mut b = Request::builder()
        .method("POST")
        .uri("/mcp")
        .header(header::HOST, HOST)
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::ACCEPT, "application/json, text/event-stream");
    if let Some(t) = token {
        b = b.header(header::AUTHORIZATION, format!("Bearer {t}"));
    }
    app.clone()
        .oneshot(b.body(Body::from(body.to_string())).unwrap())
        .await
        .unwrap()
}

async fn get(app: &Router, uri: &str, host: &str) -> Response<Body> {
    let req = Request::builder()
        .uri(uri)
        .header(header::HOST, host)
        .body(Body::empty())
        .unwrap();
    app.clone().oneshot(req).await.unwrap()
}

async fn json_of(r: Response<Body>) -> Value {
    serde_json::from_slice(
        &axum::body::to_bytes(r.into_body(), usize::MAX)
            .await
            .unwrap(),
    )
    .unwrap()
}

fn challenges(r: &Response<Body>) -> Vec<String> {
    r.headers()
        .get_all(header::WWW_AUTHENTICATE)
        .iter()
        .map(|v| v.to_str().unwrap().to_owned())
        .collect()
}

async fn tool_names(r: Response<Body>) -> Vec<String> {
    assert_eq!(r.status(), StatusCode::OK);
    let v = json_of(r).await;
    v["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap().to_owned())
        .collect()
}

fn call(tool: &str) -> Value {
    rpc(
        "tools/call",
        json!({ "name": tool, "arguments": { "repo": "acme/widgets", "changes": ["c1"] } }),
    )
}

#[tokio::test]
async fn no_token_is_401_with_basic_first_and_a_bearer_challenge_with_resource_metadata() {
    let app = app();
    for body in [
        call("log"),
        rpc("tools/list", json!({})),
        rpc("server/discover", json!({})),
    ] {
        let r = post(&app, None, &body).await;
        assert_eq!(r.status(), StatusCode::UNAUTHORIZED, "{body}");
        let www = challenges(&r);
        assert!(
            www[0].starts_with("Basic realm=\"asc asc.example.org\""),
            "{www:?}"
        );
        let want = format!("resource_metadata=\"{}\"", metadata_url(PUBLIC));
        assert!(
            www.iter().any(|c| c.starts_with("Bearer")
                && c.contains(&want)
                && c.contains("scope=\"asc:read\"")),
            "{www:?}"
        );
    }
    // A wrong token is the same 401 for a repo-free message.
    let r = post(&app, Some("nope"), &call("log")).await;
    assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn too_little_access_is_403_insufficient_scope() {
    let app = app();
    let r = post(&app, Some(READ_TOKEN), &call("submit")).await;
    assert_eq!(r.status(), StatusCode::FORBIDDEN);
    let www = challenges(&r).join(" | ");
    assert!(
        www.contains("insufficient_scope") && www.contains("asc:write"),
        "{www}"
    );
    let r = post(&app, Some(WRITE_TOKEN), &call("submit")).await;
    assert_eq!(r.status(), StatusCode::OK);
}

#[tokio::test]
async fn the_metadata_document_is_public_at_both_paths_with_no_issuers() {
    let app = app();
    for path in prm::PRM_PATHS {
        let r = get(&app, path, HOST).await;
        assert_eq!(r.status(), StatusCode::OK, "{path}");
        assert_eq!(r.headers()[header::CONTENT_TYPE], "application/json");
        assert!(!r.headers().contains_key(header::WWW_AUTHENTICATE));
        let v = json_of(r).await;
        assert_eq!(v["resource"], "https://asc.example.org/mcp");
        assert_eq!(v["authorization_servers"], json!([]));
        assert_eq!(v["scopes_supported"], json!(["asc:read", "asc:write"]));
        let text = v.to_string();
        assert!(
            !text.contains("orgejo") && !text.contains("token"),
            "{text}"
        );
    }
}

#[tokio::test]
async fn configured_issuers_are_listed_and_an_unknown_host_is_refused() {
    let prm_app = prm::router(Prm::new(
        None,
        vec!["https://idp.example.com".into()],
        policy(),
    ));
    let r = get(&prm_app, prm::PRM_PATHS[0], HOST).await;
    let v = json_of(r).await;
    assert_eq!(
        v["authorization_servers"],
        json!(["https://idp.example.com"])
    );
    assert_eq!(v["resource"], "https://asc.example.org/mcp");
    // Without `public_url` the document never echoes a Host the server does not own.
    let r = get(&prm_app, prm::PRM_PATHS[1], "evil.example.net").await;
    assert_eq!(r.status(), StatusCode::MISDIRECTED_REQUEST);
}

#[tokio::test]
async fn the_metadata_is_mounted_when_mcp_is_disabled_on_the_primary_only() {
    let ext = asc_serve::registry::Extensions::new();
    let mut cfg = ServeConfig {
        public_url: Some(PUBLIC.into()),
        ..ServeConfig::default()
    };
    assert!(!cfg.mcp.enabled);
    mcp::register(&ext, &cfg);
    let r = get(&ext.http.router(), prm::PRM_PATHS[1], HOST).await;
    assert_eq!(r.status(), StatusCode::OK);

    let ext = asc_serve::registry::Extensions::new();
    cfg.role = asc_serve::config::Role::Cache;
    mcp::register(&ext, &cfg);
    let r = get(&ext.http.router(), prm::PRM_PATHS[1], HOST).await;
    assert_eq!(r.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn a_read_only_principals_tools_list_has_no_write_tool() {
    let app = app();
    let write_tools = ["submit", "commit", "rebase", "restack"];
    // Read token, with and without a repo hint: the read view.
    for params in [json!({}), json!({ "repo": "acme/widgets" })] {
        let names =
            tool_names(post(&app, Some(READ_TOKEN), &rpc("tools/list", params)).await).await;
        assert!(names.contains(&"log".to_owned()), "{names:?}");
        assert!(
            write_tools
                .iter()
                .all(|w| !names.contains(&(*w).to_owned())),
            "{names:?}"
        );
    }
    // A write token that names no repo gets the read view too: the list never guesses a grant.
    let names =
        tool_names(post(&app, Some(WRITE_TOKEN), &rpc("tools/list", json!({}))).await).await;
    assert!(!names.contains(&"submit".to_owned()), "{names:?}");
    // A write token naming a repo it may write sees the write tools.
    let names = tool_names(
        post(
            &app,
            Some(WRITE_TOKEN),
            &rpc("tools/list", json!({ "repo": "acme/widgets" })),
        )
        .await,
    )
    .await;
    assert!(names.contains(&"submit".to_owned()), "{names:?}");
}

#[tokio::test]
async fn a_stale_list_never_grants_a_call() {
    let app = app();
    // The writer's list shows `submit` ...
    let names = tool_names(
        post(
            &app,
            Some(WRITE_TOKEN),
            &rpc("tools/list", json!({ "repo": "acme/widgets" })),
        )
        .await,
    )
    .await;
    assert!(names.contains(&"submit".to_owned()));
    // ... a read-only token that replays the call still gets 403: the call is authorised again,
    // and the list's write view does not carry over to a call.
    let r = post(&app, Some(READ_TOKEN), &call("submit")).await;
    assert_eq!(r.status(), StatusCode::FORBIDDEN);
    // A forged repo hint on a list does not widen a read token's list or its calls.
    let r = post(
        &app,
        Some(READ_TOKEN),
        &rpc("tools/list", json!({ "repo": "acme/widgets" })),
    )
    .await;
    assert!(!tool_names(r).await.contains(&"submit".to_owned()));
    // A `tools/call` carrying `params.repo` gets no write view either.
    let sneaky = rpc(
        "tools/call",
        json!({ "name": "submit", "repo": "acme/widgets", "arguments": { "repo": "acme/widgets", "changes": ["c"] } }),
    );
    let r = post(&app, Some(READ_TOKEN), &sneaky).await;
    assert_eq!(r.status(), StatusCode::FORBIDDEN);
}
