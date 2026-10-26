//! The `asc serve` `/mcp` transport (SPEC 22.2, 22.5 "HTTP transport", 22.7.2; PLAN M12a).
//!
//! Stateless Streamable HTTP 2026-07-28: POST only (GET and DELETE answer 405), one JSON-RPC
//! message per request, no session. A reply is `application/json`, or `text/event-stream` when
//! the client accepts it and the call reports progress. Before a body is read the endpoint checks
//! `Host` (421, DNS rebinding) and `Origin` (403); an absent `Origin` is accepted, because the
//! endpoint takes tokens in `Authorization` only and never cookies. After the body is read the
//! `MCP-Protocol-Version`, `Mcp-Method`, `Mcp-Name` and `Mcp-Param-Repo` headers must agree with
//! it (-32020). Bodies are capped at 1 MiB (4 MiB for `commit`).
//!
//! The mount is off by default and refused while no auth layer exists (`config::MCP_AUTH_LAYER`).
//! The endpoint never authenticates itself: the auth layer (M13) puts an [`McpPrincipal`] in the
//! request extensions, and a request without one is refused with 401.

pub mod audit;
pub mod auth;
pub mod backend;
pub mod forge;
pub mod land;
pub mod limits;
pub mod oauth;
pub mod prm;
pub mod write;

use std::collections::HashSet;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use axum::Router;
use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, Method, Request, Response, StatusCode, header};
use asc_mcp::backend::McpBackend;
use asc_mcp::handler::{Caller, Handler, Notifier, SUPPORTED_VERSIONS};
use futures::future::BoxFuture;
use serde_json::{Value, json};
use tokio::sync::mpsc;

use crate::config::{Role, ServeConfig};
use crate::registry::Extensions;

/// JSON-RPC code for a header that disagrees with the body.
pub const HEADER_MISMATCH: i32 = -32020;
/// The mount path.
pub const PATH: &str = "/mcp";

/// Set in the request extensions by the auth layer (M13): who is calling.
// TODO(M13): the authorizer layer constructs this from the verified token.
#[derive(Debug, Clone)]
pub struct McpPrincipal {
    /// The authenticated principal id.
    pub id: String,
    /// The token holds the `asc:write` scope.
    pub write: bool,
}

// ------------------------------------------------------------------ Host and Origin policy

/// The `Host` and `Origin` allowlists (SPEC 22.5).
#[derive(Debug, Clone, Default)]
pub struct HttpPolicy {
    hosts: HashSet<String>,
    origins: HashSet<String>,
}

fn split_host_port(authority: &str) -> (String, Option<String>) {
    let a = authority.trim().to_ascii_lowercase();
    if let Some(rest) = a.strip_prefix('[') {
        if let Some((h, tail)) = rest.split_once(']') {
            let port = tail
                .strip_prefix(':')
                .filter(|p| !p.is_empty())
                .map(str::to_owned);
            return (format!("[{h}]"), port);
        }
        return (a, None);
    }
    match a.rsplit_once(':') {
        Some((h, p)) if !h.contains(':') => {
            (h.trim_end_matches('.').to_owned(), Some(p.to_owned()))
        }
        _ => (a.trim_end_matches('.').to_owned(), None),
    }
}

/// `scheme://host[:port]`, lowercase, default port dropped. `None` when it is not an origin.
fn normalize_origin(o: &str) -> Option<String> {
    let o = o.trim().to_ascii_lowercase();
    let (scheme, rest) = o.split_once("://")?;
    if scheme != "http" && scheme != "https" {
        return None;
    }
    let rest = rest.trim_end_matches('/');
    if rest.is_empty() || rest.contains(['/', '?', '#', '@']) {
        return None;
    }
    let (h, p) = split_host_port(rest);
    let default = if scheme == "https" { "443" } else { "80" };
    Some(match p {
        Some(p) if p != default => format!("{scheme}://{h}:{p}"),
        _ => format!("{scheme}://{h}"),
    })
}

impl HttpPolicy {
    /// The server's own names: `public_url`, the listen address, plus the configured lists.
    #[must_use]
    pub fn new(
        listen: SocketAddr,
        public_url: Option<&str>,
        allowed_hosts: &[String],
        allowed_origins: &[String],
    ) -> Self {
        let mut p = Self::default();
        if let Some(u) = public_url
            && let Ok(url) = url::Url::parse(u)
            && let Some(h) = url.host_str()
        {
            let h = h.to_ascii_lowercase();
            p.hosts.insert(h.clone());
            let mut origin = format!("{}://{h}", url.scheme());
            if let Some(port) = url.port() {
                p.hosts.insert(format!("{h}:{port}"));
                origin.push_str(&format!(":{port}"));
            }
            if let Some(o) = normalize_origin(&origin) {
                p.origins.insert(o);
            }
        }
        if !listen.ip().is_unspecified() {
            let ip = match listen.ip() {
                std::net::IpAddr::V4(a) => a.to_string(),
                std::net::IpAddr::V6(a) => format!("[{a}]"),
            };
            p.add_listen_name(&ip, listen.port());
        }
        if listen.ip().is_loopback() {
            p.add_listen_name("localhost", listen.port());
        }
        for h in allowed_hosts {
            p.hosts.insert(h.trim().to_ascii_lowercase());
        }
        for o in allowed_origins {
            if let Some(n) = normalize_origin(o) {
                p.origins.insert(n);
            }
        }
        p
    }

    fn add_listen_name(&mut self, host: &str, port: u16) {
        self.hosts.insert(format!("{host}:{port}"));
        for scheme in ["http", "https"] {
            if let Some(o) = normalize_origin(&format!("{scheme}://{host}:{port}")) {
                self.origins.insert(o);
            }
        }
    }

    /// Whether a `Host` value (or URI authority) names this server.
    #[must_use]
    pub fn host_allowed(&self, value: &str) -> bool {
        let v = value.trim().to_ascii_lowercase();
        if v.is_empty() {
            return false;
        }
        if self.hosts.contains(&v) {
            return true;
        }
        // A bare entry allows every port of that name; an entry with a port is exact.
        let (h, _) = split_host_port(&v);
        self.hosts.contains(&h)
    }

    /// Whether an `Origin` value is the server's own or listed.
    #[must_use]
    pub fn origin_allowed(&self, value: &str) -> bool {
        normalize_origin(value).is_some_and(|o| self.origins.contains(&o))
    }
}

// ------------------------------------------------------------------ header and body agreement

fn header_str<'a>(h: &'a HeaderMap, name: &str) -> Result<Option<&'a str>, ()> {
    match h.get(name) {
        None => Ok(None),
        Some(v) => v.to_str().map(Some).map_err(|_| ()),
    }
}

/// Checks the `MCP-Protocol-Version`, `Mcp-Method`, `Mcp-Name` and `Mcp-Param-Repo` headers
/// against the message. A header that is present must match; the error names the header.
///
/// # Errors
/// The message for the -32020 reply.
pub fn check_headers(h: &HeaderMap, body: &Value) -> Result<(), String> {
    let bad = |name: &str| Err(format!("header {name} does not match the request body"));
    match header_str(h, "mcp-protocol-version") {
        Err(()) => return bad("MCP-Protocol-Version"),
        Ok(Some(v)) => {
            if !SUPPORTED_VERSIONS.contains(&v) {
                return bad("MCP-Protocol-Version");
            }
            for ptr in ["/params/protocolVersion", "/params/_meta/protocolVersion"] {
                if body
                    .pointer(ptr)
                    .and_then(Value::as_str)
                    .is_some_and(|b| b != v)
                {
                    return bad("MCP-Protocol-Version");
                }
            }
        }
        Ok(None) => {}
    }
    let method = body.get("method").and_then(Value::as_str);
    match header_str(h, "mcp-method") {
        Err(()) => return bad("Mcp-Method"),
        Ok(Some(v)) if Some(v) != method => return bad("Mcp-Method"),
        Ok(_) => {}
    }
    match header_str(h, "mcp-name") {
        Err(()) => return bad("Mcp-Name"),
        Ok(Some(v)) => {
            let expect = match method {
                Some("tools/call" | "prompts/get") => body.pointer("/params/name"),
                Some("resources/read") => body.pointer("/params/uri"),
                _ => None,
            };
            if expect.and_then(Value::as_str) != Some(v) {
                return bad("Mcp-Name");
            }
        }
        Ok(None) => {}
    }
    match header_str(h, "mcp-param-repo") {
        Err(()) => return bad("Mcp-Param-Repo"),
        Ok(Some(v))
            if body
                .pointer("/params/arguments/repo")
                .and_then(Value::as_str)
                != Some(v) =>
        {
            return bad("Mcp-Param-Repo");
        }
        Ok(_) => {}
    }
    Ok(())
}

// ------------------------------------------------------------------ the endpoint

trait Service: Send + Sync + 'static {
    fn call(&self, msg: Value, caller: Caller, out: Notifier) -> BoxFuture<'static, Option<Value>>;
}

impl<B: McpBackend> Service for Handler<B> {
    fn call(&self, msg: Value, caller: Caller, out: Notifier) -> BoxFuture<'static, Option<Value>> {
        let h = self.clone();
        Box::pin(async move { h.handle_message(msg, &caller, &out).await })
    }
}

struct McpState {
    svc: Arc<dyn Service>,
    policy: HttpPolicy,
    next_id: AtomicU64,
}

/// The `/mcp` router over `handler`. It carries no authentication: the auth layer wraps it.
pub fn router<B: McpBackend>(handler: Handler<B>, policy: HttpPolicy) -> Router {
    let state = Arc::new(McpState {
        svc: Arc::new(handler),
        policy,
        next_id: AtomicU64::new(1),
    });
    Router::new()
        .route(PATH, axum::routing::any(endpoint))
        .with_state(state)
}

/// The one-line registration in `lib.rs`. Mounts nothing unless `[mcp] enabled` on the primary
/// role (config validation already refuses `enabled` while no auth layer exists); the cache role
/// never serves `/mcp`, so it answers 404.
// TODO(M12b): build `ServeBackend` and the `Handler`, then `ext.http.register(router(..))`.
pub fn register(ext: &Extensions, cfg: &ServeConfig) {
    // The metadata document is always served (SPEC 22.7.3), enabled or not.
    let issuers = cfg
        .mcp
        .oauth
        .as_ref()
        .map(|o| o.issuer.clone())
        .unwrap_or_default();
    prm::register(ext, cfg, issuers);
    if cfg.role != Role::Primary || !cfg.mcp.enabled {
        return;
    }
    tracing::error!(
        "mcp.enabled is set but the MCP backend is not wired (M12b); /mcp is not mounted"
    );
}

fn plain(status: StatusCode) -> Response<Body> {
    let mut r = Response::new(Body::empty());
    *r.status_mut() = status;
    r
}

fn rpc_error(status: StatusCode, id: &Value, code: i32, message: &str) -> Response<Body> {
    let body = json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } });
    let mut r = Response::new(Body::from(body.to_string()));
    *r.status_mut() = status;
    r.headers_mut().insert(
        header::CONTENT_TYPE,
        "application/json".parse().expect("static"),
    );
    r
}

fn json_ok(v: &Value) -> Response<Body> {
    let mut r = Response::new(Body::from(v.to_string()));
    r.headers_mut().insert(
        header::CONTENT_TYPE,
        "application/json".parse().expect("static"),
    );
    r.headers_mut()
        .insert(header::CACHE_CONTROL, "no-store".parse().expect("static"));
    r
}

fn wants_sse(h: &HeaderMap, body: &Value) -> bool {
    let accept = h
        .get(header::ACCEPT)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_ascii_lowercase();
    if !accept.contains("text/event-stream") {
        return false;
    }
    let progress = body.pointer("/params/_meta/progressToken").is_some();
    progress || !(accept.contains("application/json") || accept.contains("*/*"))
}

/// Cancels the call when the client goes away before the reply is complete.
struct CancelOnDrop {
    svc: Arc<dyn Service>,
    id: Value,
    done: Arc<AtomicBool>,
}

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        if self.done.load(Ordering::SeqCst) {
            return;
        }
        let msg = json!({ "jsonrpc": "2.0", "method": "notifications/cancelled", "params": { "requestId": self.id } });
        let svc = Arc::clone(&self.svc);
        if let Ok(rt) = tokio::runtime::Handle::try_current() {
            rt.spawn(async move {
                let (tx, _rx) = mpsc::channel(1);
                svc.call(msg, Caller::default(), tx).await;
            });
        }
    }
}

async fn endpoint(State(st): State<Arc<McpState>>, req: Request<Body>) -> Response<Body> {
    if req.method() != Method::POST {
        let mut r = plain(StatusCode::METHOD_NOT_ALLOWED);
        r.headers_mut()
            .insert(header::ALLOW, "POST".parse().expect("static"));
        return r;
    }
    let (parts, body) = req.into_parts();
    // DNS rebinding guard: every name the request carries must be ours.
    let host = parts
        .headers
        .get(header::HOST)
        .map(|v| v.to_str().map(str::to_owned));
    let authority = parts.uri.authority().map(|a| a.as_str().to_owned());
    let host_ok = (host.is_some() || authority.is_some())
        && host
            .as_ref()
            .is_none_or(|h| h.as_ref().is_ok_and(|h| st.policy.host_allowed(h)))
        && authority.as_ref().is_none_or(|a| st.policy.host_allowed(a));
    if !host_ok {
        return plain(StatusCode::MISDIRECTED_REQUEST);
    }
    if let Some(o) = parts.headers.get(header::ORIGIN)
        && !o.to_str().is_ok_and(|o| st.policy.origin_allowed(o))
    {
        return plain(StatusCode::FORBIDDEN);
    }
    let Some(who) = parts.extensions.get::<McpPrincipal>().cloned() else {
        let mut r = plain(StatusCode::UNAUTHORIZED);
        r.headers_mut()
            .insert(header::WWW_AUTHENTICATE, "Bearer".parse().expect("static"));
        return r;
    };
    let ct = parts
        .headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if !ct
        .trim()
        .to_ascii_lowercase()
        .starts_with("application/json")
    {
        return plain(StatusCode::UNSUPPORTED_MEDIA_TYPE);
    }
    let declared = parts
        .headers
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok());
    let (bytes, parsed) = match limits::read_body(body, declared).await {
        Ok(b) => b,
        Err(limits::BodyError::TooLarge) => return plain(StatusCode::PAYLOAD_TOO_LARGE),
        Err(limits::BodyError::Read) => return plain(StatusCode::BAD_REQUEST),
    };
    let mut msg = match parsed {
        Some(v) => v,
        None => match serde_json::from_slice::<Value>(&bytes) {
            Ok(v) => v,
            Err(_) => {
                return rpc_error(StatusCode::BAD_REQUEST, &Value::Null, -32700, "parse error");
            }
        },
    };
    if !msg.is_object() {
        return rpc_error(
            StatusCode::BAD_REQUEST,
            &Value::Null,
            -32600,
            "one JSON-RPC message per request",
        );
    }
    let orig_id = msg.get("id").cloned().unwrap_or(Value::Null);
    if let Err(m) = check_headers(&parts.headers, &msg) {
        return rpc_error(StatusCode::BAD_REQUEST, &orig_id, HEADER_MISMATCH, &m);
    }
    if msg.get("method").and_then(Value::as_str) == Some("notifications/cancelled") {
        // Cancellation over stateless HTTP is closing the connection.
        return plain(StatusCode::ACCEPTED);
    }
    // Stateless HTTP has no session, so request ids are only unique per client. The handler keys
    // its in-flight calls by id: give every call a server-unique id and restore the client's on
    // the reply, so two clients using `1` never cancel or replace each other's call.
    let is_request = orig_id.is_string() || orig_id.is_number();
    let server_id = json!(format!(
        "http-{}",
        st.next_id.fetch_add(1, Ordering::Relaxed)
    ));
    if is_request {
        msg["id"] = server_id.clone();
    }
    // A `tools/list` may show the write tools when `prm::scope_layer` saw write on the named repo;
    // every call is authorised again, so this never grants more than the listing.
    let listing_write = parts.extensions.get::<prm::ListWrite>().is_some()
        && msg.get("method").and_then(Value::as_str) == Some("tools/list");
    let caller = Caller {
        principal: Some(who.id),
        server_write: who.write || listing_write,
    };
    let sse = is_request && wants_sse(&parts.headers, &msg);
    let (tx, rx) = mpsc::channel::<Value>(32);
    let done = Arc::new(AtomicBool::new(false));
    let guard = CancelOnDrop {
        svc: Arc::clone(&st.svc),
        id: server_id.clone(),
        done: Arc::clone(&done),
    };
    let restore = move |mut v: Value| {
        if v.get("id") == Some(&server_id) {
            v["id"] = orig_id.clone();
        }
        v
    };

    if !sse {
        let reply = st.svc.call(msg, caller, tx).await;
        done.store(true, Ordering::SeqCst);
        drop(guard);
        drop(rx);
        return match reply {
            Some(v) => json_ok(&restore(v)),
            None => plain(StatusCode::ACCEPTED),
        };
    }

    let svc = Arc::clone(&st.svc);
    let done2 = Arc::clone(&done);
    tokio::spawn(async move {
        let reply = svc.call(msg, caller, tx.clone()).await;
        done2.store(true, Ordering::SeqCst);
        if let Some(v) = reply {
            let _ = tx.send(v).await;
        }
    });
    let stream = futures::stream::unfold(
        (rx, guard, restore),
        |(mut rx, guard, restore)| async move {
            let v = rx.recv().await?;
            let ev = format!("event: message\ndata: {}\n\n", restore(v));
            Some((
                Ok::<_, std::convert::Infallible>(bytes::Bytes::from(ev)),
                (rx, guard, restore),
            ))
        },
    );
    let mut r = Response::new(Body::from_stream(stream));
    let h = r.headers_mut();
    h.insert(
        header::CONTENT_TYPE,
        "text/event-stream".parse().expect("static"),
    );
    h.insert(header::CACHE_CONTROL, "no-cache".parse().expect("static"));
    h.insert("x-accel-buffering", "no".parse().expect("static"));
    r
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy() -> HttpPolicy {
        HttpPolicy::new(
            "127.0.0.1:8443".parse().unwrap(),
            Some("https://asc.example.org"),
            &["alias.example.org:9000".into()],
            &["https://app.example.org".into()],
        )
    }

    #[test]
    fn host_policy() {
        let p = policy();
        assert!(p.host_allowed("asc.example.org"));
        assert!(p.host_allowed("ASC.example.org:8443"));
        assert!(p.host_allowed("127.0.0.1:8443"));
        assert!(p.host_allowed("localhost:8443"));
        assert!(p.host_allowed("alias.example.org:9000"));
        assert!(!p.host_allowed("alias.example.org:1"));
        assert!(!p.host_allowed("evil.example.net"));
        assert!(!p.host_allowed("127.0.0.1:1"));
        assert!(!p.host_allowed(""));
    }

    #[test]
    fn origin_policy() {
        let p = policy();
        assert!(p.origin_allowed("https://asc.example.org"));
        assert!(p.origin_allowed("https://asc.example.org:443"));
        assert!(p.origin_allowed("https://app.example.org"));
        assert!(!p.origin_allowed("http://asc.example.org"));
        assert!(!p.origin_allowed("https://evil.example.net"));
        assert!(!p.origin_allowed("null"));
        assert!(!p.origin_allowed("https://asc.example.org.evil.net"));
    }
}
