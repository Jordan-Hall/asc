//! M29: the `/mcp` authenticator on the git `Authorizer` (SPEC 22.7.3, 15.6.2). A token valid for
//! git is valid for MCP through one authorizer call, a read-only token cannot call a write tool,
//! Basic and Bearer give one principal, an unreadable repository looks like a missing one, a
//! query-string token is a 400, the credential never leaks, and every call is audited.
//!
//! The same suite runs over the static token table and over a counting authorizer that fronts it.
//! TODO(K19): the same tests against `fake_forgejo` (`PublicApiAuthz`, `InternalAuthz`) once that
//! fake is shared outside `tests/fake_forgejo.rs`.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use asc_rpc::auth::{
    Access, AuthError, Authorizer, BearerToken, Credential, Principal, StaticTokens,
};
use asc_serve::mcp::McpPrincipal;
use asc_serve::mcp::audit::{MemoryAudit, Mode, Outcome};
use asc_serve::mcp::auth::{McpAuth, layer};
use async_trait::async_trait;
use axum::Router;
use axum::body::Body;
use axum::extract::Request as AxRequest;
use axum::http::{Request, Response, StatusCode, header};
use base64::Engine as _;
use serde_json::{Value, json};
use tower::ServiceExt;

const READ_TOKEN: &str = "tok-read-0123456789";
const WRITE_TOKEN: &str = "tok-write-0123456789";
const HOST: &str = "asc.example.org";
const PRM: &str = "https://asc.example.org/.well-known/oauth-protected-resource/mcp";

/// Counts every authorizer call and refuses `acme/hidden` to everyone, as a forge does for a
/// repository the principal cannot see.
struct Counting {
    inner: StaticTokens,
    seen: Mutex<Vec<(String, String, Access)>>,
    calls: AtomicUsize,
}

impl Counting {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            inner: StaticTokens::new()
                .with_token(READ_TOKEN, "reader", Access::Read)
                .with_token(WRITE_TOKEN, "writer", Access::Write),
            seen: Mutex::new(Vec::new()),
            calls: AtomicUsize::new(0),
        })
    }
}

#[async_trait]
impl Authorizer for Counting {
    async fn authorize(
        &self,
        token: Option<&BearerToken>,
        repo: &str,
        access: Access,
    ) -> Result<Principal, AuthError> {
        if repo == "acme/hidden" {
            return Err(AuthError::Forbidden);
        }
        self.inner.authorize(token, repo, access).await
    }

    async fn authorize_credential(
        &self,
        credential: Option<&Credential>,
        repo: &str,
        access: Access,
    ) -> Result<Principal, AuthError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if let Some(c) = credential {
            self.seen.lock().unwrap().push((
                c.secret().expose().to_owned(),
                repo.to_owned(),
                access,
            ));
        }
        self.authorize(credential.map(Credential::secret), repo, access)
            .await
    }

    async fn authenticate(&self, credential: Option<&Credential>) -> Result<Principal, AuthError> {
        self.inner.authenticate(credential).await
    }
}

/// An authorizer that does not implement `authenticate`: the trait default must refuse.
struct NoAuthenticate(StaticTokens);

#[async_trait]
impl Authorizer for NoAuthenticate {
    async fn authorize(
        &self,
        token: Option<&BearerToken>,
        repo: &str,
        access: Access,
    ) -> Result<Principal, AuthError> {
        self.0.authorize(token, repo, access).await
    }
}

/// What the endpoint behind the layer saw.
#[derive(Default)]
struct Seen {
    hits: AtomicUsize,
    last: Mutex<Option<(Option<McpPrincipal>, bool, String)>>,
}

fn app(authz: Arc<dyn Authorizer>) -> (Router, Arc<MemoryAudit>, Arc<Seen>) {
    let seen = Arc::new(Seen::default());
    let s2 = Arc::clone(&seen);
    let inner = Router::new().route(
        "/mcp",
        axum::routing::post(move |req: AxRequest| {
            let s = Arc::clone(&s2);
            async move {
                s.hits.fetch_add(1, Ordering::SeqCst);
                let has_auth = req.headers().contains_key(header::AUTHORIZATION);
                let p = req.extensions().get::<McpPrincipal>().cloned();
                let body = axum::body::to_bytes(req.into_body(), usize::MAX)
                    .await
                    .unwrap();
                *s.last.lock().unwrap() =
                    Some((p, has_auth, String::from_utf8_lossy(&body).into_owned()));
                axum::Json(json!({ "jsonrpc": "2.0", "id": 1, "result": {} }))
            }
        }),
    );
    let audit = Arc::new(MemoryAudit::default());
    let app = layer(inner, McpAuth::new(authz, audit.clone(), PRM));
    (app, audit, seen)
}

fn call(tool: &str, repo: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": { "name": tool, "arguments": { "repo": repo } } })
}

fn bearer(t: &str) -> String {
    format!("Bearer {t}")
}

fn basic(user: &str, t: &str) -> String {
    format!(
        "Basic {}",
        base64::engine::general_purpose::STANDARD.encode(format!("{user}:{t}"))
    )
}

async fn send(app: &Router, auth: Option<&str>, uri: &str, body: &Value) -> Response<Body> {
    let mut b = Request::builder()
        .method("POST")
        .uri(uri)
        .header(header::HOST, HOST)
        .header(header::CONTENT_TYPE, "application/json");
    if let Some(a) = auth {
        b = b.header(header::AUTHORIZATION, a);
    }
    app.clone()
        .oneshot(b.body(Body::from(body.to_string())).unwrap())
        .await
        .unwrap()
}

async fn text(r: Response<Body>) -> String {
    String::from_utf8_lossy(
        &axum::body::to_bytes(r.into_body(), usize::MAX)
            .await
            .unwrap(),
    )
    .into_owned()
}

#[tokio::test]
async fn a_git_token_reads_through_one_authorizer_call_with_the_git_arguments() {
    let authz = Counting::new();
    // What a git fetch does: one call for (credential, repo, read).
    let git = Credential::basic("x-access-token", READ_TOKEN).unwrap();
    authz
        .authorize_credential(Some(&git), "acme/widgets", Access::Read)
        .await
        .unwrap();
    let (app, audit, seen) = app(authz.clone());
    let r = send(
        &app,
        Some(&bearer(READ_TOKEN)),
        "/mcp",
        &call("log", "acme/widgets"),
    )
    .await;
    assert_eq!(r.status(), StatusCode::OK);
    let seen_calls = authz.seen.lock().unwrap().clone();
    assert_eq!(seen_calls.len(), 2, "one call per tool call");
    assert_eq!(
        seen_calls[0], seen_calls[1],
        "the same (secret, repo, mode) as git, so one cache entry"
    );
    let (p, has_auth, _) = seen.last.lock().unwrap().clone().unwrap();
    assert_eq!(p.unwrap().id, "reader");
    assert!(!has_auth, "the credential is not passed on");
    let rec = audit.records();
    assert_eq!(rec.len(), 1);
    assert_eq!(rec[0].principal.as_deref(), Some("reader"));
    assert_eq!(
        (rec[0].repo.as_deref(), rec[0].tool.as_str(), rec[0].mode),
        (Some("acme/widgets"), "log", Mode::Read)
    );
    assert_eq!(rec[0].outcome, Outcome::Allowed(200));
}

#[tokio::test]
async fn a_read_only_token_cannot_call_a_write_tool() {
    let (app, audit, seen) = app(Counting::new());
    let r = send(
        &app,
        Some(&bearer(READ_TOKEN)),
        "/mcp",
        &call("submit", "acme/widgets"),
    )
    .await;
    assert_eq!(r.status(), StatusCode::FORBIDDEN);
    let www = r.headers()[header::WWW_AUTHENTICATE]
        .to_str()
        .unwrap()
        .to_owned();
    assert!(
        www.contains("insufficient_scope") && www.contains("asc:write"),
        "{www}"
    );
    assert_eq!(
        seen.hits.load(Ordering::SeqCst),
        0,
        "the endpoint never ran"
    );
    assert_eq!(audit.records()[0].outcome, Outcome::InsufficientScope);
    assert_eq!(audit.records()[0].mode, Mode::Write);

    // A write token passes, and is marked as holding write.
    let r = send(
        &app,
        Some(&bearer(WRITE_TOKEN)),
        "/mcp",
        &call("submit", "acme/widgets"),
    )
    .await;
    assert_eq!(r.status(), StatusCode::OK);
    let (p, _, _) = seen.last.lock().unwrap().clone().unwrap();
    let p = p.unwrap();
    assert_eq!((p.id.as_str(), p.write), ("writer", true));
}

#[tokio::test]
async fn basic_and_bearer_give_one_principal() {
    let (app, _, seen) = app(Counting::new());
    let mut ids = Vec::new();
    for h in [
        bearer(WRITE_TOKEN),
        basic("alice", WRITE_TOKEN),
        basic("x-access-token", WRITE_TOKEN),
        basic("", WRITE_TOKEN),
    ] {
        let r = send(&app, Some(&h), "/mcp", &call("log", "acme/widgets")).await;
        assert_eq!(r.status(), StatusCode::OK);
        ids.push(seen.last.lock().unwrap().clone().unwrap().0.unwrap().id);
    }
    assert!(ids.iter().all(|i| i == "writer"), "{ids:?}");
}

#[tokio::test]
async fn an_unreadable_repo_answers_like_a_missing_one() {
    // `acme/hidden` is refused by the authorizer; a write tool on it must look the same, even to
    // a token that could write elsewhere, and must not be a 403.
    let (app, audit, seen) = app(Counting::new());
    let hidden = send(
        &app,
        Some(&bearer(WRITE_TOKEN)),
        "/mcp",
        &call("log", "acme/hidden"),
    )
    .await;
    assert_eq!(hidden.status(), StatusCode::OK);
    let hidden = text(hidden).await;
    let hidden_write = send(
        &app,
        Some(&bearer(WRITE_TOKEN)),
        "/mcp",
        &call("submit", "acme/hidden"),
    )
    .await;
    assert_eq!(
        hidden_write.status(),
        StatusCode::OK,
        "no 403 that would reveal the repository"
    );
    assert_eq!(text(hidden_write).await, hidden);
    assert!(
        hidden.contains("NOT_FOUND") && hidden.contains("repository not found"),
        "{hidden}"
    );
    assert_eq!(seen.hits.load(Ordering::SeqCst), 0);
    assert!(
        audit
            .records()
            .iter()
            .all(|r| r.outcome == Outcome::NotFound)
    );
}

#[tokio::test]
async fn a_query_string_token_is_a_400_and_a_missing_or_bad_one_a_401() {
    let (app, _, seen) = app(Counting::new());
    for q in ["token", "access_token", "api_key"] {
        let r = send(
            &app,
            Some(&bearer(READ_TOKEN)),
            &format!("/mcp?{q}={READ_TOKEN}"),
            &call("log", "acme/widgets"),
        )
        .await;
        assert_eq!(r.status(), StatusCode::BAD_REQUEST, "{q}");
    }
    let r = send(&app, None, "/mcp", &call("log", "acme/widgets")).await;
    assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
    let all: Vec<_> = r
        .headers()
        .get_all(header::WWW_AUTHENTICATE)
        .iter()
        .map(|v| v.to_str().unwrap().to_owned())
        .collect();
    assert!(
        all[0].starts_with("Basic realm=\"asc asc.example.org\""),
        "{all:?}"
    );
    assert!(
        all.iter()
            .any(|v| v.contains("resource_metadata") && v.contains("asc:read")),
        "{all:?}"
    );
    let r = send(
        &app,
        Some(&bearer("not-a-token-0123456")),
        "/mcp",
        &call("log", "acme/widgets"),
    )
    .await;
    assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(seen.hits.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn the_token_is_in_no_response_audit_record_or_downstream_header() {
    let (app, audit, seen) = app(Counting::new());
    let cases = [
        (Some(bearer(READ_TOKEN)), call("log", "acme/widgets")),
        (Some(bearer(READ_TOKEN)), call("submit", "acme/widgets")),
        (Some(bearer(READ_TOKEN)), call("log", "acme/hidden")),
        (
            Some(bearer("wrong-token-0123456789")),
            call("log", "acme/widgets"),
        ),
        (
            Some(bearer(READ_TOKEN)),
            json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": { "name": "log", "arguments": {} } }),
        ),
    ];
    for (h, body) in &cases {
        let r = send(&app, h.as_deref(), "/mcp", body).await;
        let shown = format!("{:?}", r.headers());
        assert!(!shown.contains(READ_TOKEN), "{shown}");
        assert!(!text(r).await.contains(READ_TOKEN));
    }
    for rec in audit.records() {
        let shown = format!("{rec:?}");
        assert!(
            !shown.contains(READ_TOKEN) && !shown.contains("wrong-token"),
            "{shown}"
        );
    }
    assert!(
        seen.last
            .lock()
            .unwrap()
            .as_ref()
            .is_some_and(|(_, has_auth, body)| !has_auth && !body.contains(READ_TOKEN))
    );
}

#[tokio::test]
async fn every_call_writes_one_audit_record_with_its_outcome() {
    let (app, audit, _) = app(Counting::new());
    send(
        &app,
        Some(&bearer(READ_TOKEN)),
        "/mcp",
        &call("log", "acme/widgets"),
    )
    .await;
    send(
        &app,
        Some(&bearer(READ_TOKEN)),
        "/mcp",
        &call("submit", "acme/widgets"),
    )
    .await;
    send(&app, None, "/mcp", &call("log", "acme/widgets")).await;
    send(&app, Some(&bearer(READ_TOKEN)), "/mcp", &json!({ "jsonrpc": "2.0", "id": 4, "method": "tools/call", "params": { "name": "log", "arguments": {} } })).await;
    let out: Vec<Outcome> = audit.records().iter().map(|r| r.outcome).collect();
    assert_eq!(
        out,
        [
            Outcome::Allowed(200),
            Outcome::InsufficientScope,
            Outcome::Unauthenticated,
            Outcome::BadRequest
        ]
    );
}

fn init() -> Value {
    json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {} })
}

fn list() -> Value {
    json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list" })
}

#[tokio::test]
async fn messages_without_a_repo_act_as_the_authenticated_principal() {
    let (app, audit, seen) = app(Counting::new());
    assert_eq!(
        send(&app, None, "/mcp", &list()).await.status(),
        StatusCode::UNAUTHORIZED
    );
    for (msg, token, who) in [
        (init(), READ_TOKEN, "reader"),
        (list(), WRITE_TOKEN, "writer"),
    ] {
        assert_eq!(
            send(&app, Some(&bearer(token)), "/mcp", &msg)
                .await
                .status(),
            StatusCode::OK
        );
        let (p, _, _) = seen.last.lock().unwrap().clone().unwrap();
        let p = p.unwrap();
        assert_eq!((p.id.as_str(), p.write), (who, false));
    }
    let r = send(&app, Some(&basic("alice", READ_TOKEN)), "/mcp", &list()).await;
    assert_eq!(r.status(), StatusCode::OK);
    assert!(audit.records().is_empty(), "only tool calls are audited");
}

#[tokio::test]
async fn a_bogus_token_on_a_repo_free_message_is_a_401() {
    let (app, _, seen) = app(Counting::new());
    for msg in [init(), list()] {
        for h in [
            bearer("bogus-token-0123456789"),
            basic("alice", "bogus-token-0123456789"),
        ] {
            let r = send(&app, Some(&h), "/mcp", &msg).await;
            assert_eq!(r.status(), StatusCode::UNAUTHORIZED, "{msg}");
            assert!(r.headers().get_all(header::WWW_AUTHENTICATE).iter().count() >= 1);
        }
        assert_eq!(
            send(&app, None, "/mcp", &msg).await.status(),
            StatusCode::UNAUTHORIZED
        );
    }
    assert_eq!(
        seen.hits.load(Ordering::SeqCst),
        0,
        "the endpoint never ran"
    );
}

#[tokio::test]
async fn a_repo_free_tool_call_is_authenticated_and_audited_as_the_real_principal() {
    let (app, audit, seen) = app(Counting::new());
    let whoami = json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": { "name": "whoami", "arguments": {} } });
    let bogus = send(
        &app,
        Some(&bearer("bogus-token-0123456789")),
        "/mcp",
        &whoami,
    )
    .await;
    assert_eq!(bogus.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(seen.hits.load(Ordering::SeqCst), 0);
    let ok = send(&app, Some(&bearer(READ_TOKEN)), "/mcp", &whoami).await;
    assert_eq!(ok.status(), StatusCode::OK);
    let rec = audit.records();
    assert_eq!(rec.len(), 2, "{rec:?}");
    assert_eq!(
        (rec[0].principal.as_deref(), rec[0].outcome),
        (None, Outcome::Unauthenticated)
    );
    assert_eq!(
        (rec[1].principal.as_deref(), rec[1].outcome),
        (Some("reader"), Outcome::Allowed(200))
    );
}

#[tokio::test]
async fn an_authorizer_without_authenticate_refuses_repo_free_messages() {
    let authz: Arc<dyn Authorizer> = Arc::new(NoAuthenticate(StaticTokens::new().with_token(
        READ_TOKEN,
        "reader",
        Access::Read,
    )));
    let (app, _, seen) = app(authz);
    for msg in [init(), list()] {
        let r = send(&app, Some(&bearer(READ_TOKEN)), "/mcp", &msg).await;
        assert_eq!(r.status(), StatusCode::UNAUTHORIZED, "fail closed: {msg}");
    }
    // A repository call still works: it is decided by `authorize`.
    assert_eq!(
        send(
            &app,
            Some(&bearer(READ_TOKEN)),
            "/mcp",
            &call("log", "acme/widgets")
        )
        .await
        .status(),
        StatusCode::OK
    );
    assert_eq!(seen.hits.load(Ordering::SeqCst), 1);
}
