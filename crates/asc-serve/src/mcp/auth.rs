//! The `/mcp` authenticator on the git [`Authorizer`] (SPEC 22.7.3, 15.6.2; PLAN M29).
//!
//! `/mcp` has no verifier of its own. [`layer`] wraps the endpoint: the U78 edge (a token in the
//! query string is a 400, a bad header a 400 or 401 with the Basic-first challenge) runs first,
//! then every `tools/call` is authorised with exactly one
//! [`Authorizer::authorize_credential`] call for `(credential, repo, read|write)`, the call a git
//! fetch or push makes, so a token valid for git is valid here and shares the authorizer's cache
//! entry (`sha256(token)`, repo, mode). The result is an [`McpPrincipal`] in the request
//! extensions, which is all the endpoint trusts.
//!
//! * A write tool needs `Access::Write`. A principal that may read but not write gets 403
//!   `insufficient_scope`; one that may not read gets the very `NOT_FOUND` a missing repository
//!   gets, so private names do not leak.
//! * The `Authorization` header is removed once it is used, so nothing downstream can forward it
//!   (no passthrough). It is never logged, audited or put in an error.
//! * Every tool call writes one [`AuditRecord`].
//!
//! TODO(M13): `initialize`, `tools/list` and tools without a `repo` have no repository to
//! authorise; they pass with the unbound principal [`UNBOUND`] (which owns nothing), and M13's
//! scope-filtered listing needs a repo-free authenticate on the authorizer.

use std::sync::Arc;
use std::time::Instant;

use axum::Router;
use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{HeaderValue, Method, StatusCode, header};
use axum::middleware::Next;
use axum::response::Response;
use asc_mcp::catalogue;
use asc_mcp::errors::{ToolError, code};
use asc_mcp::schema::Surface as McpSurface;
use asc_rpc::auth::{Access, AuthError, Authorizer};
use serde_json::{Value, json};

use super::McpPrincipal;
use super::audit::{AuditRecord, AuditSink, Mode, Outcome};
use super::limits;
use crate::authn::{self, Surface};

/// The principal id of a message that names no repository to authorise.
pub const UNBOUND: &str = "mcp:unbound";
/// The text of `NOT_FOUND`, the same as `ServeBackend` gives for a missing repository.
const REPO_NOT_FOUND: &str = "repository not found";

/// The authenticator of `/mcp`.
#[derive(Clone)]
pub struct McpAuth {
    authorizer: Arc<dyn Authorizer>,
    audit: Arc<dyn AuditSink>,
    surface: Surface,
}

impl McpAuth {
    /// `resource_metadata` is the absolute URL of the PRM document for the 401 challenge.
    pub fn new(authorizer: Arc<dyn Authorizer>, audit: Arc<dyn AuditSink>, resource_metadata: &str) -> Self {
        Self { authorizer, audit, surface: Surface::Mcp { resource_metadata: resource_metadata.into() } }
    }
}

/// Wraps `router` (the `/mcp` endpoint) in the credential edge and the per-call authorisation.
pub fn layer(router: Router, auth: McpAuth) -> Router {
    let surface = auth.surface.clone();
    authn::guard(router.layer(axum::middleware::from_fn_with_state(auth, authorise)), surface)
}

struct Call {
    tool: String,
    repo: Option<String>,
    mode: Mode,
    id: Value,
}

/// The tool call a message makes: `None` for any other message, `Err` for a call that cannot be
/// authorised because it is malformed.
fn call_of(msg: &Value) -> Option<Result<Call, (&'static str, Call)>> {
    if msg.get("method").and_then(Value::as_str) != Some("tools/call") {
        return None;
    }
    let id = msg.get("id").cloned().unwrap_or(Value::Null);
    let tool = msg.pointer("/params/name").and_then(Value::as_str).unwrap_or("-").to_owned();
    let bad = |m, tool: String, id| Some(Err((m, Call { tool, repo: None, mode: Mode::Read, id })));
    let Some(def) = catalogue::find(&tool, McpSurface::Http) else {
        // An unknown tool is refused by the handler and touches nothing.
        return Some(Ok(Call { tool, repo: None, mode: Mode::Read, id }));
    };
    let mode = if def.ann.read_only { Mode::Read } else { Mode::Write };
    let takes_repo =
        serde_json::from_str::<Value>(def.input_http).is_ok_and(|s| s.pointer("/properties/repo").is_some());
    let repo = match msg.pointer("/params/arguments/repo") {
        Some(Value::String(r)) => Some(r.clone()),
        Some(_) => return bad("repo must be a string", tool, id),
        None if takes_repo => return bad("repo (owner/name) is required", tool, id),
        None => None,
    };
    Some(Ok(Call { tool, repo, mode, id }))
}

fn json_response(status: StatusCode, v: &Value) -> Response {
    let mut r = Response::new(Body::from(v.to_string()));
    *r.status_mut() = status;
    r.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static("application/json"));
    r
}

fn status_only(status: StatusCode) -> Response {
    let mut r = Response::new(Body::empty());
    *r.status_mut() = status;
    r
}

fn invalid_params(id: &Value, message: &str) -> Response {
    json_response(
        StatusCode::BAD_REQUEST,
        &json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32602, "message": message } }),
    )
}

fn not_found(id: &Value) -> Response {
    let result = ToolError::new(code::NOT_FOUND, REPO_NOT_FOUND).to_result();
    json_response(StatusCode::OK, &json!({ "jsonrpc": "2.0", "id": id, "result": result }))
}

fn insufficient_scope(surface: &Surface) -> Response {
    let meta = match surface {
        Surface::Mcp { resource_metadata } => {
            format!(", resource_metadata=\"{}\"", resource_metadata.replace(['"', '\\', '\r', '\n'], ""))
        }
        Surface::Api => String::new(),
    };
    let mut r = json_response(
        StatusCode::FORBIDDEN,
        &json!({ "error": { "code": "insufficient_scope", "message": "write access is required" } }),
    );
    if let Ok(v) = HeaderValue::from_str(&format!("Bearer error=\"insufficient_scope\", scope=\"asc:write\"{meta}")) {
        r.headers_mut().insert(header::WWW_AUTHENTICATE, v);
    }
    r
}

fn access_of(mode: Mode) -> Access {
    match mode {
        Mode::Read => Access::Read,
        Mode::Write => Access::Write,
    }
}

fn record(auth: &McpAuth, principal: Option<String>, c: &Call, outcome: Outcome, started: Instant) {
    auth.audit.record(&AuditRecord {
        principal,
        repo: c.repo.clone(),
        tool: c.tool.clone(),
        mode: c.mode,
        outcome,
        duration: started.elapsed(),
    });
}

async fn authorise(State(auth): State<McpAuth>, req: Request, next: Next) -> Response {
    if req.method() != Method::POST {
        return next.run(req).await;
    }
    let started = Instant::now();
    let (parts, body) = req.into_parts();
    let credential = match authn::credential(&parts.headers) {
        Ok(c) => c,
        Err(AuthError::Malformed) => return status_only(StatusCode::BAD_REQUEST),
        Err(_) => return authn::unauthorized(&auth.surface, &parts.headers),
    };
    let declared = parts
        .headers
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok());
    let (bytes, parsed) = match limits::read_body(body, declared).await {
        Ok(b) => b,
        Err(limits::BodyError::TooLarge) => return status_only(StatusCode::PAYLOAD_TOO_LARGE),
        Err(limits::BodyError::Read) => return status_only(StatusCode::BAD_REQUEST),
    };
    let msg = parsed.or_else(|| serde_json::from_slice::<Value>(&bytes).ok());
    let call = msg.as_ref().and_then(call_of);

    // Every message needs a credential: no credential is a 401, never anonymous access.
    let Some(credential) = credential else {
        let c = match &call {
            Some(Ok(c)) | Some(Err((_, c))) => Some(c),
            None => None,
        };
        if let Some(c) = c {
            record(&auth, None, c, Outcome::Unauthenticated, started);
        }
        return authn::unauthorized(&auth.surface, &parts.headers);
    };

    let mut principal = McpPrincipal { id: UNBOUND.to_owned(), write: false };
    let mut closing: Option<Call> = None;
    match call {
        None => {}
        Some(Err((m, c))) => {
            record(&auth, None, &c, Outcome::BadRequest, started);
            return invalid_params(&c.id, m);
        }
        Some(Ok(c)) if c.repo.is_none() => closing = Some(c),
        Some(Ok(c)) => {
            let repo = c.repo.as_deref().unwrap_or_default();
            match auth.authorizer.authorize_credential(Some(&credential), repo, access_of(c.mode)).await {
                Ok(p) => {
                    principal = McpPrincipal { id: p.id, write: c.mode == Mode::Write };
                    closing = Some(c);
                }
                Err(AuthError::Unauthenticated) => {
                    record(&auth, None, &c, Outcome::Unauthenticated, started);
                    return authn::unauthorized(&auth.surface, &parts.headers);
                }
                Err(AuthError::Malformed) => {
                    record(&auth, None, &c, Outcome::BadRequest, started);
                    return status_only(StatusCode::BAD_REQUEST);
                }
                Err(AuthError::Forbidden) => {
                    // Only "may read, may not write" is a 403; "may not read" looks like a miss.
                    let can_read = c.mode == Mode::Write
                        && auth.authorizer.authorize_credential(Some(&credential), repo, Access::Read).await.is_ok();
                    if can_read {
                        record(&auth, None, &c, Outcome::InsufficientScope, started);
                        return insufficient_scope(&auth.surface);
                    }
                    record(&auth, None, &c, Outcome::NotFound, started);
                    return not_found(&c.id);
                }
            }
        }
    }

    let who = (principal.id != UNBOUND).then(|| principal.id.clone());
    let mut req = Request::from_parts(parts, Body::from(bytes));
    // No passthrough: nothing downstream can see, log or forward the credential.
    req.headers_mut().remove(header::AUTHORIZATION);
    req.extensions_mut().insert(principal);
    let res = next.run(req).await;
    if let Some(c) = closing {
        record(&auth, who, &c, Outcome::Allowed(res.status().as_u16()), started);
    }
    res
}
