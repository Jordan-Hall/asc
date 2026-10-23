//! The server credential edge (SPEC §15.6.2; PLAN U78): one parser and one challenge for every
//! HTTP surface of `asc serve`.
//!
//! Every route reads its `Authorization` header with [`credential`] (the parser the gRPC
//! interceptor uses, `asc_rpc::auth::parse_authorization`) and makes one
//! [`Authorizer::authorize_credential`] call. [`guard`] wraps a router: it refuses a malformed
//! header (400) or an unusable one (401) before the handler runs, refuses a token in the query
//! string (400), and adds the `WWW-Authenticate` challenge to every 401 the handler returns, so
//! no route has to remember it. A 401 carries `Basic realm="asc <host>", charset="UTF-8"` first,
//! because git and every credential helper retry only after a Basic challenge, then `Bearer`;
//! `/mcp` adds its `resource_metadata` challenge (SPEC §22.7.3).
//!
//! [`guard_admin`] is the same edge for the admin listener, whose token is an operator secret
//! outside the `Authorizer`. [`git_router`] answers the git smart-HTTP paths so that a real git
//! client pointed at `asc serve` meets the challenge and then a plain 404.
//!
//! Nothing here logs or returns a credential.

use std::sync::Arc;

use axum::Router;
use axum::extract::{Path, Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::any;
use asc_rpc::auth::{Access, AuthError, Authorizer, Credential, parse_authorization};
use sha2::{Digest, Sha256};

/// Query parameters that must never carry a credential (SPEC §15.6.2): such a request is a 400.
const QUERY_TOKEN_KEYS: &[&str] = &[
    "token",
    "access_token",
    "auth_token",
    "private_token",
    "api_key",
    "apikey",
    "password",
    "secret",
];

/// Which challenges a surface sends after the Basic one.
#[derive(Clone, Debug)]
pub enum Surface {
    /// A route that takes Basic or Bearer: `Bearer realm="asc <host>"` follows Basic.
    Api,
    /// `/mcp`: also `Bearer resource_metadata="<url>", scope="asc:read"` (SPEC §22.7.3).
    Mcp { resource_metadata: Arc<str> },
}

/// The host to name in the realm: the request's `Host` header when it is plain host characters,
/// else nothing (the realm is then just `asc`).
fn host_of(headers: &HeaderMap) -> Option<&str> {
    let host = headers.get(header::HOST)?.to_str().ok()?;
    let ok = !host.is_empty()
        && host.len() <= 255
        && host
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b':' | b'[' | b']'));
    ok.then_some(host)
}

/// The `WWW-Authenticate` values of `surface`, Basic first.
pub fn challenges(surface: &Surface, headers: &HeaderMap) -> Vec<HeaderValue> {
    let realm = match host_of(headers) {
        Some(h) => format!("asc {h}"),
        None => "asc".to_owned(),
    };
    let mut values = vec![
        format!("Basic realm=\"{realm}\", charset=\"UTF-8\""),
        format!("Bearer realm=\"{realm}\""),
    ];
    if let Surface::Mcp { resource_metadata } = surface {
        values.push(format!(
            "Bearer resource_metadata=\"{}\", scope=\"asc:read\"",
            resource_metadata.replace(['"', '\\', '\r', '\n'], "")
        ));
    }
    values
        .iter()
        .filter_map(|v| HeaderValue::from_str(v).ok())
        .collect()
}

/// A 401 with the challenges of `surface`.
pub fn unauthorized(surface: &Surface, headers: &HeaderMap) -> Response {
    let mut res = (
        StatusCode::UNAUTHORIZED,
        axum::Json(serde_json::json!({ "error": { "message": "unauthenticated" } })),
    )
        .into_response();
    for c in challenges(surface, headers) {
        res.headers_mut().append(header::WWW_AUTHENTICATE, c);
    }
    res
}

/// The credential of a request, parsed by the one shared parser: `None` is anonymous.
pub fn credential(headers: &HeaderMap) -> Result<Option<Credential>, AuthError> {
    parse_authorization(
        headers
            .get_all(header::AUTHORIZATION)
            .iter()
            .map(HeaderValue::as_bytes),
    )
}

/// Whether `query` names a credential parameter.
fn query_carries_token(query: &str) -> bool {
    query.split('&').any(|pair| {
        let key = pair.split_once('=').map_or(pair, |(k, _)| k);
        QUERY_TOKEN_KEYS
            .iter()
            .any(|bad| key.eq_ignore_ascii_case(bad))
    })
}

fn bad_request(message: &'static str) -> Response {
    (
        StatusCode::BAD_REQUEST,
        axum::Json(serde_json::json!({ "error": { "message": message } })),
    )
        .into_response()
}

async fn edge(State(surface): State<Surface>, req: Request, next: Next) -> Response {
    if req.uri().query().is_some_and(query_carries_token) {
        return bad_request("credentials do not belong in the query string");
    }
    match credential(req.headers()) {
        Ok(_) => {}
        Err(AuthError::Malformed) => return bad_request("malformed credentials"),
        Err(_) => return unauthorized(&surface, req.headers()),
    }
    let headers = req.headers().clone();
    let mut res = next.run(req).await;
    if res.status() == StatusCode::UNAUTHORIZED
        && !res.headers().contains_key(header::WWW_AUTHENTICATE)
    {
        for c in challenges(&surface, &headers) {
            res.headers_mut().append(header::WWW_AUTHENTICATE, c);
        }
    }
    res
}

/// Wraps every route of `router` in the credential edge of `surface`.
pub fn guard(router: Router, surface: Surface) -> Router {
    router.layer(axum::middleware::from_fn_with_state(surface, edge))
}

/// Compares two secrets without an early exit and without leaking the length: both are hashed to
/// equal-length digests first.
pub fn secret_eq(a: &str, b: &str) -> bool {
    let (x, y) = (Sha256::digest(a), Sha256::digest(b));
    let diff = x.iter().zip(y.iter()).fold(0u8, |d, (p, q)| d | (p ^ q));
    std::hint::black_box(diff) == 0
}

async fn admin_edge(State(token): State<Option<Arc<str>>>, req: Request, next: Next) -> Response {
    if let Some(expected) = token {
        // Bearer, `token` or Basic (the password, or the user name when the password is empty, is
        // the token). Any failure, malformed or not, is the same 401.
        let given = credential(req.headers()).ok().flatten();
        let ok = given
            .as_ref()
            .is_some_and(|c| secret_eq(c.secret().expose(), &expected));
        if !ok {
            return unauthorized(&Surface::Api, req.headers());
        }
    }
    next.run(req).await
}

/// Gates `router`, the admin listener's routes: with a `token`, every route requires it as Bearer
/// or Basic, compared in constant time. Without one the listener is loopback-only (SPEC §8.1).
pub fn guard_admin(router: Router, token: Option<Arc<str>>) -> Router {
    router.layer(axum::middleware::from_fn_with_state(token, admin_edge))
}

// ------------------------------------------------------------------------------ git hosts

const GIT_ELSEWHERE: &str = "the git protocol is served by the forge; this host serves asc's API\n";

/// `/<owner>/<repo>[.git]/info/refs`, `git-upload-pack` and `git-receive-pack` (SPEC §15.6.2,
/// "Git protocol hosts"): anonymous gets the challenge, so a credential helper is asked; once
/// authenticated, a 404 that says where git is served. The same 404 answers a repository the
/// principal cannot read and one that does not exist.
pub fn git_router(authorizer: Arc<dyn Authorizer>) -> Router {
    async fn git_stub(
        State(authorizer): State<Arc<dyn Authorizer>>,
        Path((owner, repo)): Path<(String, String)>,
        headers: HeaderMap,
    ) -> Response {
        let Ok(Some(credential)) = credential(&headers) else {
            return unauthorized(&Surface::Api, &headers);
        };
        let repo = repo.strip_suffix(".git").unwrap_or(&repo);
        match authorizer
            .authorize_credential(Some(&credential), &format!("{owner}/{repo}"), Access::Read)
            .await
        {
            Err(AuthError::Unauthenticated) => unauthorized(&Surface::Api, &headers),
            _ => (StatusCode::NOT_FOUND, GIT_ELSEWHERE).into_response(),
        }
    }
    let routes = Router::new()
        .route("/{owner}/{repo}/info/refs", any(git_stub))
        .route("/{owner}/{repo}/git-upload-pack", any(git_stub))
        .route("/{owner}/{repo}/git-receive-pack", any(git_stub))
        .with_state(authorizer);
    guard(routes, Surface::Api)
}
