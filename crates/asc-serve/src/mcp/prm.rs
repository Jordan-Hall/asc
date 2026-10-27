//! Protected-resource metadata and the scope-filtered `tools/list` (SPEC 22.7.3, 15.6.2; PLAN M13).
//!
//! * [`router`] serves the RFC 9728 document at `/.well-known/oauth-protected-resource` and, by
//!   path insertion, `/.well-known/oauth-protected-resource/mcp`. It is public (it names no repo
//!   and no principal) and is served whether or not `[mcp] enabled` is set. `authorization_servers`
//!   lists the configured `[mcp.oauth]` issuers and is empty when none is configured, so a client
//!   then uses a pre-issued token. Nothing assumes Forgejo.
//! * [`scope_layer`] decides which tools a `tools/list` shows. The authorizer answers only
//!   `(credential, repo, access)`, so the listing shows the write tools only when the request names
//!   a repository in `params.repo` for which the credential holds write. Every other listing is
//!   the read view. A list never grants anything: each `tools/call` is authorised again by the
//!   M29 layer (`auth::layer`).

use std::sync::Arc;

use asc_rpc::auth::{Access, Authorizer};
use axum::Router;
use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::middleware::Next;
use axum::response::Response;
use serde_json::{Value, json};

use super::{HttpPolicy, PATH, limits};
use crate::config::{Role, ServeConfig};
use crate::registry::Extensions;

/// The two metadata paths.
pub const PRM_PATHS: [&str; 2] = [
    "/.well-known/oauth-protected-resource",
    "/.well-known/oauth-protected-resource/mcp",
];
/// The wire scope names: labels only, the grant comes from the authorizer.
pub const SCOPES: [&str; 2] = ["asc:read", "asc:write"];

/// Set in the request extensions by [`scope_layer`] when the credential may write the repository
/// a `tools/list` names. It only widens what the listing shows.
#[derive(Debug, Clone, Copy)]
pub struct ListWrite;

/// What the metadata document says.
#[derive(Clone)]
pub struct Prm {
    public_url: Option<String>,
    issuers: Vec<String>,
    policy: HttpPolicy,
}

impl Prm {
    /// `public_url` is the server's public `https` URL; without it the document names the request
    /// `Host`, which must pass `policy`. `issuers` is `[mcp.oauth] issuer` (empty: no OAuth).
    #[must_use]
    pub fn new(public_url: Option<&str>, issuers: Vec<String>, policy: HttpPolicy) -> Self {
        Self {
            public_url: public_url.map(|u| u.trim_end_matches('/').to_owned()),
            issuers,
            policy,
        }
    }

    /// The canonical `/mcp` URL (the audience of RFC 8707), or `None` for an unknown `Host`.
    fn resource(&self, headers: &HeaderMap) -> Option<String> {
        if let Some(u) = &self.public_url {
            return Some(format!("{u}{PATH}"));
        }
        let host = headers.get(header::HOST)?.to_str().ok()?;
        self.policy
            .host_allowed(host)
            .then(|| format!("https://{}{PATH}", host.to_ascii_lowercase()))
    }

    fn document(&self, resource: &str) -> Value {
        json!({
            "resource": resource,
            "authorization_servers": self.issuers,
            "scopes_supported": SCOPES,
            "bearer_methods_supported": ["header"],
            "resource_name": "asc",
        })
    }
}

/// The absolute URL of the metadata document for the 401 challenge.
#[must_use]
pub fn metadata_url(public_url: &str) -> String {
    format!("{}{}", public_url.trim_end_matches('/'), PRM_PATHS[1])
}

async fn serve(State(prm): State<Arc<Prm>>, req: Request) -> Response {
    let mut r = Response::new(Body::empty());
    let Some(resource) = prm.resource(req.headers()) else {
        *r.status_mut() = StatusCode::MISDIRECTED_REQUEST;
        return r;
    };
    if req.method() == Method::GET {
        *r.body_mut() = Body::from(prm.document(&resource).to_string());
    }
    r.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    r.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("public, max-age=300"),
    );
    r
}

/// The metadata routes. No authentication: the document is public by RFC 9728.
pub fn router(prm: Prm) -> Router {
    let mut r = Router::new();
    for p in PRM_PATHS {
        r = r.route(p, axum::routing::get(serve));
    }
    r.with_state(Arc::new(prm))
}

/// Mounts the metadata on the primary role, enabled or not.
pub fn register(ext: &Extensions, cfg: &ServeConfig, issuers: Vec<String>) {
    if cfg.role != Role::Primary {
        return;
    }
    let policy = HttpPolicy::from_config(cfg);
    ext.http
        .register(router(Prm::new(cfg.public_url.as_deref(), issuers, policy)));
}

#[derive(Clone)]
struct Scope {
    authorizer: Arc<dyn Authorizer>,
}

/// Wraps the `/mcp` stack (outside `auth::layer`, which strips the credential). For a
/// `tools/list` whose `params.repo` the credential may write, it sets [`ListWrite`]; for anything
/// else it changes nothing. A malformed or refused credential is left to the auth layer to answer.
pub fn scope_layer(router: Router, authorizer: Arc<dyn Authorizer>) -> Router {
    router.layer(axum::middleware::from_fn_with_state(
        Scope { authorizer },
        scope,
    ))
}

async fn scope(State(s): State<Scope>, req: Request, next: Next) -> Response {
    if req.method() != Method::POST {
        return next.run(req).await;
    }
    let Ok(Some(credential)) = crate::authn::credential(req.headers()) else {
        return next.run(req).await;
    };
    let (parts, body) = req.into_parts();
    let declared = parts
        .headers
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok());
    let (bytes, parsed) = match limits::read_body(body, declared).await {
        Ok(b) => b,
        Err(limits::BodyError::TooLarge) => return status(StatusCode::PAYLOAD_TOO_LARGE),
        Err(limits::BodyError::Read) => return status(StatusCode::BAD_REQUEST),
    };
    let msg = parsed.or_else(|| serde_json::from_slice::<Value>(&bytes).ok());
    let repo = msg
        .as_ref()
        .filter(|m| m.get("method").and_then(Value::as_str) == Some("tools/list"))
        .and_then(|m| m.pointer("/params/repo"))
        .and_then(Value::as_str)
        .map(str::to_owned);
    let mut req = Request::from_parts(parts, Body::from(bytes));
    if let Some(repo) = repo
        && s.authorizer
            .authorize_credential(Some(&credential), &repo, Access::Write)
            .await
            .is_ok()
    {
        req.extensions_mut().insert(ListWrite);
    }
    next.run(req).await
}

fn status(code: StatusCode) -> Response {
    let mut r = Response::new(Body::empty());
    *r.status_mut() = code;
    r
}
