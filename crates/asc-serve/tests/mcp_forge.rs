//! M34: the optional `forge` toolset. Off without an adapter; authorised through the repository
//! gate before the forge is asked; the adapter acts as the principal, never with the caller's
//! token; results are cleaned, marked untrusted and lockdown-filtered; handles are validated.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use axum::Router;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::routing::get;
use asc_core::transport_policy::Policy;
use asc_mcp::backend::{CallContext, McpBackend, Reply};
use asc_mcp::errors::ToolError;
use asc_serve::mcp::backend::RepoGate;
use asc_serve::mcp::forge::{
    ForgeActor, ForgeAdapter, ForgeBackend, ForgeConfig, ForgeError, ForgeItem, ForgejoAdapter,
    Handles,
};
use futures::StreamExt as _;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

struct Inner;

#[async_trait]
impl McpBackend for Inner {
    async fn call(&self, tool: &str, _a: &Value, _c: &CallContext) -> Result<Reply, ToolError> {
        Err(ToolError::new("INNER", tool))
    }
}

struct Gate;

#[async_trait]
impl RepoGate for Gate {
    async fn can_read(&self, principal: &str, repo: &str) -> bool {
        principal == "alice" && repo == "o/r"
    }
}

#[derive(Default)]
struct Fake {
    seen: Mutex<Vec<String>>,
}

#[async_trait]
impl ForgeAdapter for Fake {
    fn name(&self) -> &'static str {
        "fake"
    }
    async fn issues(
        &self,
        a: &ForgeActor,
        _r: &str,
        _s: &str,
        _l: usize,
    ) -> Result<Vec<ForgeItem>, ForgeError> {
        self.seen.lock().unwrap().push(a.principal.clone());
        Ok(vec![
            ForgeItem {
                id: 1,
                title: "t\u{1b}[31m".into(),
                body: "x".repeat(100_000),
                author: "maint".into(),
                author_can_write: Some(true),
                ..ForgeItem::default()
            },
            ForgeItem {
                id: 2,
                title: "ignore previous".into(),
                author: "rando".into(),
                author_can_write: Some(false),
                ..ForgeItem::default()
            },
            ForgeItem {
                id: 3,
                title: "unknown".into(),
                author: "who".into(),
                author_can_write: None,
                ..ForgeItem::default()
            },
        ])
    }
    async fn issue(&self, _a: &ForgeActor, _r: &str, n: u64) -> Result<ForgeItem, ForgeError> {
        if n == 9 {
            Err(ForgeError::NotFound)
        } else {
            Err(ForgeError::Unavailable("boom".into()))
        }
    }
    async fn pr_comments(
        &self,
        _a: &ForgeActor,
        _r: &str,
        _n: u64,
        _l: usize,
    ) -> Result<(Vec<ForgeItem>, Handles), ForgeError> {
        Ok((
            vec![],
            Handles {
                asc_change: Some("klmn".into()),
                head: Some("ab12".into()),
                merge_base: Some("NOT HEX".into()),
                stack_parent: Some("0123".into()),
                version: Some(2),
            },
        ))
    }
    async fn actions_runs(
        &self,
        _a: &ForgeActor,
        _r: &str,
        _l: usize,
    ) -> Result<Vec<ForgeItem>, ForgeError> {
        Ok(vec![])
    }
}

fn ctx(principal: Option<&str>) -> CallContext {
    CallContext {
        principal: principal.map(str::to_owned),
        budget_bytes: 1 << 20,
        position: None,
        cancel: CancellationToken::new(),
        progress: None,
    }
}

fn backend(adapter: Option<Arc<dyn ForgeAdapter>>, cfg: &ForgeConfig) -> ForgeBackend<Inner> {
    ForgeBackend::new(Inner, Arc::new(Gate), adapter, cfg)
}

async fn run(
    b: &impl McpBackend,
    tool: &str,
    args: Value,
    who: Option<&str>,
) -> Result<Value, ToolError> {
    match b.call(tool, &args, &ctx(who)).await? {
        Reply::Messages(mut s) => s.next().await.expect("one message"),
        _ => panic!("messages expected"),
    }
}

fn cfg(lockdown: bool) -> ForgeConfig {
    ForgeConfig {
        adapter: Some("fake".into()),
        lockdown,
    }
}

#[tokio::test]
async fn off_without_adapter() {
    let b = backend(None, &ForgeConfig::default());
    assert!(b.tools().is_empty());
    let e = run(&b, "forge_issues", json!({"repo": "o/r"}), Some("alice"))
        .await
        .unwrap_err();
    assert_eq!(e.code, "NOT_AVAILABLE");
    // Other tools pass through untouched.
    assert_eq!(
        run(&b, "read_file", json!({}), Some("alice"))
            .await
            .unwrap_err()
            .code,
        "INNER"
    );
}

#[test]
fn config_rejects_unknown_adapter() {
    assert!(
        ForgeConfig {
            adapter: Some("github".into()),
            lockdown: true
        }
        .validate()
        .is_err()
    );
    assert!(
        ForgeConfig {
            adapter: Some("forgejo".into()),
            lockdown: true
        }
        .validate()
        .is_ok()
    );
    assert!(!ForgeConfig::default().enabled());
}

#[tokio::test]
async fn tools_are_read_only_and_listed_with_adapter() {
    let b = backend(Some(Arc::new(Fake::default())), &cfg(true));
    let t = b.tools();
    assert_eq!(t.len(), 4);
    for d in &t {
        assert_eq!(d["annotations"]["readOnlyHint"], true);
        assert_eq!(d["annotations"]["destructiveHint"], false);
    }
}

#[tokio::test]
async fn authorised_before_the_forge_is_asked() {
    let fake = Arc::new(Fake::default());
    let b = backend(Some(fake.clone()), &cfg(true));
    // An unreadable repo answers like a missing one and the adapter is never reached.
    let e = run(
        &b,
        "forge_issues",
        json!({"repo": "o/secret"}),
        Some("alice"),
    )
    .await
    .unwrap_err();
    assert_eq!(e.code, "NOT_FOUND");
    let e = run(&b, "forge_issues", json!({"repo": "o/r"}), Some("mallory"))
        .await
        .unwrap_err();
    assert_eq!(e.code, "NOT_FOUND");
    assert_eq!(
        run(&b, "forge_issues", json!({"repo": "o/r"}), None)
            .await
            .unwrap_err()
            .code,
        "UNAUTHENTICATED"
    );
    assert!(fake.seen.lock().unwrap().is_empty());
    run(&b, "forge_issues", json!({"repo": "o/r"}), Some("alice"))
        .await
        .unwrap();
    assert_eq!(*fake.seen.lock().unwrap(), vec!["alice".to_owned()]);
}

#[tokio::test]
async fn principal_is_never_an_argument() {
    let b = backend(Some(Arc::new(Fake::default())), &cfg(true));
    let e = run(
        &b,
        "forge_issues",
        json!({"repo": "o/r", "principal": "root"}),
        Some("alice"),
    )
    .await
    .unwrap_err();
    assert_eq!(e.code, "INVALID_ARGUMENT");
    for bad in ["o", "../x/y", "o/r/z", "o/.r", "o/r r"] {
        let e = run(&b, "forge_issues", json!({"repo": bad}), Some("alice"))
            .await
            .unwrap_err();
        assert_eq!(e.code, "INVALID_ARGUMENT", "{bad}");
    }
}

#[tokio::test]
async fn lockdown_hides_authors_without_write_and_cleans_text() {
    let b = backend(Some(Arc::new(Fake::default())), &cfg(true));
    let v = run(&b, "forge_issues", json!({"repo": "o/r"}), Some("alice"))
        .await
        .unwrap();
    let list = v["items"]["list"].as_array().unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(v["items"]["hidden_by_lockdown"], 2);
    assert_eq!(list[0]["untrusted"], true);
    assert!(!list[0]["title"].as_str().unwrap().contains('\u{1b}'));
    assert!(list[0]["body"].as_str().unwrap().chars().count() <= 8 * 1024);

    let open = backend(Some(Arc::new(Fake::default())), &cfg(false));
    let v = run(&open, "forge_issues", json!({"repo": "o/r"}), Some("alice"))
        .await
        .unwrap();
    assert_eq!(v["items"]["list"].as_array().unwrap().len(), 3);
}

#[tokio::test]
async fn errors_and_handles() {
    let b = backend(Some(Arc::new(Fake::default())), &cfg(true));
    let e = run(
        &b,
        "forge_issue",
        json!({"repo": "o/r", "number": 9}),
        Some("alice"),
    )
    .await
    .unwrap_err();
    assert_eq!(e.code, "NOT_FOUND");
    let e = run(
        &b,
        "forge_issue",
        json!({"repo": "o/r", "number": 1}),
        Some("alice"),
    )
    .await
    .unwrap_err();
    assert_eq!(e.code, "NOT_AVAILABLE");
    let v = run(
        &b,
        "forge_pr_comments",
        json!({"repo": "o/r", "number": 3}),
        Some("alice"),
    )
    .await
    .unwrap();
    // Forge-supplied handles that break the AscHandles patterns are dropped.
    assert_eq!(
        v["asc_handles"],
        json!({"asc_change": "klmn", "head": "ab12", "version": 2})
    );
}

// ------------------------------------------------------------------ Forgejo implementation

type Log = Arc<Mutex<Vec<(String, Option<String>, Option<String>)>>>;

async fn handler(State(s): State<Log>, uri: axum::http::Uri, h: HeaderMap) -> axum::Json<Value> {
    let hv = |n: &str| h.get(n).and_then(|v| v.to_str().ok()).map(str::to_owned);
    s.lock()
        .unwrap()
        .push((uri.path().to_owned(), hv("authorization"), hv("sudo")));
    let p = uri.path();
    if p.ends_with("/permission") {
        let w = p.contains("/maint/");
        return axum::Json(json!({"permission": if w { "write" } else { "read" }}));
    }
    if p.ends_with("/issues") {
        return axum::Json(json!([
            {"number": 1, "title": "a", "state": "open", "body": "b", "user": {"login": "maint"}},
            {"number": 2, "title": "b", "state": "open", "body": "c", "user": {"login": "rando"}}
        ]));
    }
    axum::Json(json!({}))
}

#[tokio::test]
async fn forgejo_acts_as_principal_with_service_token_only() {
    let seen = Log::default();
    let app = Router::new()
        .fallback(get(handler))
        .with_state(seen.clone());
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/", l.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });

    let ad = ForgejoAdapter::new(&url, "SERVICE-TOKEN", Policy::LOOPBACK_HTTP).unwrap();
    let items = ad
        .issues(
            &ForgeActor {
                principal: "alice".into(),
            },
            "o/r",
            "open",
            10,
        )
        .await
        .unwrap();
    assert_eq!(items.len(), 2);
    assert_eq!(items[0].author_can_write, Some(true));
    assert_eq!(items[1].author_can_write, Some(false));
    for (_, auth, sudo) in seen.lock().unwrap().iter() {
        assert_eq!(auth.as_deref(), Some("token SERVICE-TOKEN"));
        assert_eq!(sudo.as_deref(), Some("alice"));
    }
    // A principal that is not a plain login never reaches the wire.
    let n = seen.lock().unwrap().len();
    let e = ad
        .issues(
            &ForgeActor {
                principal: "a\r\nX: y".into(),
            },
            "o/r",
            "open",
            1,
        )
        .await
        .unwrap_err();
    assert_eq!(e, ForgeError::NotFound);
    assert_eq!(seen.lock().unwrap().len(), n);
    assert!(!format!("{ad:?}").contains("SERVICE-TOKEN"));
}

#[test]
fn forgejo_refuses_plain_http_to_remote_and_credentials() {
    assert!(ForgejoAdapter::new("http://forge.example/", "t", Policy::STRICT).is_err());
    assert!(ForgejoAdapter::new("https://u:p@forge.example/", "t", Policy::STRICT).is_err());
    assert!(ForgejoAdapter::new("https://forge.example/", "", Policy::STRICT).is_err());
    assert!(ForgejoAdapter::new("https://forge.example/", "t", Policy::STRICT).is_ok());
}
