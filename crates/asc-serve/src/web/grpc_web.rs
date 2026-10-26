//! gRPC-Web and Connect unary on the RPC listener (docs/WEB-UI.md section 4.1, 9, 10).
//!
//! Browsers cannot speak native gRPC (no trailers, no HTTP/2 framing control), so the same service
//! paths, `/<package>.<Service>/<Method>`, also answer:
//!
//! * gRPC-Web (`application/grpc-web[+proto]`): the request is already length-prefixed gRPC; the
//!   response body is the gRPC body with the trailers appended as a final `0x80` frame. Server
//!   streaming works because frames are forwarded as they are produced. The text variant
//!   (`grpc-web-text`) is not offered: `fetch` streams binary.
//! * Connect unary (`application/proto`): one message in, one message out, errors as JSON with the
//!   Connect code and an HTTP status.
//!
//! The layer sits outside [`crate::limits::guarded_routes`]: the translated request is an ordinary
//! gRPC request, so admission, the `Authorizer` and every service limit apply exactly as for native
//! gRPC. The bearer token is the `Authorization` header and nothing else; `Cookie` is dropped and
//! no response sets a cookie or allows credentials.
//!
//! CORS is closed by default. A request from the server's own origin (its `Host`, or `public_url`)
//! needs no CORS headers. A cross-origin request is answered only for an origin in
//! `[web] cors_origins` (exact match, never `*`); any other `Origin` gets `403`.

use std::convert::Infallible;
use std::future::{self, Future};
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use axum::body::Body;
use axum::http::{
    HeaderMap, HeaderName, HeaderValue, Method, Request, Response, StatusCode, Uri, header,
};
use bytes::{Bytes, BytesMut};
use futures::StreamExt;
use http_body::{Frame, SizeHint};
use tonic::service::Routes;
use tower::{Layer, Service};
use url::Url;

use crate::config::WebConfig;

/// Request headers a browser may send. Anything else fails the preflight.
const ALLOW_HEADERS: &str = "authorization, content-type, x-grpc-web, x-user-agent, grpc-timeout, \
x-asc-root, x-asc-op-id, connect-protocol-version, connect-timeout-ms";
/// Response headers a page may read.
const EXPOSE_HEADERS: &str = "grpc-status, grpc-message, grpc-status-details-bin, retry-after";
/// Largest Connect reply that is buffered (gRPC-Web replies stream and are not buffered).
const MAX_CONNECT_REPLY: usize = 16 * 1024 * 1024;

const GRPC_CODE_RESOURCE_EXHAUSTED: i32 = 8;
const GRPC_CODE_UNKNOWN: i32 = 2;
const GRPC_CODE_INTERNAL: i32 = 13;
const GRPC_CODE_UNIMPLEMENTED: i32 = 12;

const CONNECT_CODES: [&str; 17] = [
    "ok",
    "canceled",
    "unknown",
    "invalid_argument",
    "deadline_exceeded",
    "not_found",
    "already_exists",
    "permission_denied",
    "resource_exhausted",
    "failed_precondition",
    "aborted",
    "out_of_range",
    "unimplemented",
    "internal",
    "unavailable",
    "data_loss",
    "unauthenticated",
];

/// The origin as the browser sends it (`scheme://host[:port]`, lower case, default port dropped), or
/// why the value is not acceptable in `[web] cors_origins`: https (or http on a loopback host, for
/// development), no path, query, fragment or credentials, and never `*` or `null`.
pub fn normalise_origin(raw: &str) -> Result<String, String> {
    let url = Url::parse(raw.trim()).map_err(|e| format!("{raw:?} is not a URL: {e}"))?;
    let host = url
        .host_str()
        .ok_or_else(|| format!("{raw:?} has no host"))?;
    let loopback = matches!(host, "localhost" | "127.0.0.1" | "[::1]");
    match url.scheme() {
        "https" => {}
        "http" if loopback => {}
        _ => return Err(format!("{raw:?} must be https (http only for loopback)")),
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(format!("{raw:?} must not carry credentials"));
    }
    if url.path() != "/" || url.query().is_some() || url.fragment().is_some() {
        return Err(format!("{raw:?} must be an origin without path or query"));
    }
    Ok(url.origin().ascii_serialization())
}

#[derive(Debug)]
struct Settings {
    /// Normalised `[web] cors_origins`.
    origins: Vec<String>,
    /// The origin of `public_url`, which counts as same-origin behind a rewriting proxy.
    public_origin: Option<String>,
    max_request: u64,
}

/// What the browser's `Origin` means for this server.
enum Origin {
    /// No `Origin`, or the server's own: no CORS headers.
    Plain,
    /// A configured origin: echoed back.
    Allowed(HeaderValue),
    Denied,
}

impl Settings {
    fn origin(&self, uri: &Uri, headers: &HeaderMap) -> Origin {
        let Some(raw) = headers.get(header::ORIGIN) else {
            return Origin::Plain;
        };
        let Some(text) = raw.to_str().ok().and_then(|s| Url::parse(s).ok()) else {
            return Origin::Denied;
        };
        let origin = text.origin().ascii_serialization();
        if origin == "null" {
            return Origin::Denied;
        }
        if self.public_origin.as_deref() == Some(origin.as_str())
            || same_authority(&text, uri, headers)
        {
            return Origin::Plain;
        }
        if self.origins.contains(&origin) {
            return HeaderValue::from_str(&origin).map_or(Origin::Denied, Origin::Allowed);
        }
        Origin::Denied
    }
}

/// `origin` names the host and port the request was sent to.
fn same_authority(origin: &Url, uri: &Uri, headers: &HeaderMap) -> bool {
    let target = uri
        .authority()
        .map(|a| a.as_str())
        .or_else(|| headers.get(header::HOST).and_then(|h| h.to_str().ok()));
    let Some(target) = target else { return false };
    let Ok(t) = Url::parse(&format!("{}://{target}", origin.scheme())) else {
        return false;
    };
    t.host_str() == origin.host_str() && t.port_or_known_default() == origin.port_or_known_default()
}

/// Adds gRPC-Web and Connect unary to `routes` when `[web]` asks for it.
pub fn wrap(routes: Routes, cfg: &WebConfig, public_url: Option<&str>) -> Routes {
    if !cfg.grpc_web_enabled() {
        return routes;
    }
    tracing::info!(
        event = "web.grpc_web",
        cors_origins = cfg.cors_origins.len(),
        max_request = cfg.max_request.0
    );
    Routes::from(
        routes
            .into_axum_router()
            .layer(GrpcWebLayer::new(cfg, public_url)),
    )
}

/// The tower layer behind [`wrap`].
#[derive(Clone, Debug)]
pub struct GrpcWebLayer {
    settings: Arc<Settings>,
}

impl GrpcWebLayer {
    pub fn new(cfg: &WebConfig, public_url: Option<&str>) -> Self {
        Self {
            settings: Arc::new(Settings {
                origins: cfg
                    .cors_origins
                    .iter()
                    .filter_map(|o| normalise_origin(o).ok())
                    .collect(),
                public_origin: public_url
                    .and_then(|u| Url::parse(u).ok())
                    .map(|u| u.origin().ascii_serialization()),
                max_request: cfg.max_request.0,
            }),
        }
    }
}

impl<S> Layer<S> for GrpcWebLayer {
    type Service = GrpcWebService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        GrpcWebService {
            inner,
            settings: self.settings.clone(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct GrpcWebService<S> {
    inner: S,
    settings: Arc<Settings>,
}

type Reply = Response<Body>;

impl<S> Service<Request<Body>> for GrpcWebService<S>
where
    S: Service<Request<Body>, Response = Reply, Error = Infallible> + Clone + Send + 'static,
    S::Future: Send + 'static,
{
    type Response = Reply;
    type Error = Infallible;
    type Future = Pin<Box<dyn Future<Output = Result<Reply, Infallible>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Infallible>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: Request<Body>) -> Self::Future {
        let ready = self.inner.clone();
        let inner = std::mem::replace(&mut self.inner, ready);
        let settings = self.settings.clone();
        Box::pin(async move { Ok(handle(&settings, inner, req).await) })
    }
}

#[derive(Clone, Copy)]
enum Kind {
    /// gRPC-Web; the flag is whether the content type carries `+proto`.
    Web {
        proto_suffix: bool,
    },
    /// gRPC-Web, base64 text: not served.
    WebText,
    Connect,
}

fn kind_of(headers: &HeaderMap) -> Option<Kind> {
    let ct = headers.get(header::CONTENT_TYPE)?.to_str().ok()?;
    let essence = ct.split(';').next()?.trim().to_ascii_lowercase();
    Some(match essence.as_str() {
        "application/grpc-web" => Kind::Web {
            proto_suffix: false,
        },
        "application/grpc-web+proto" => Kind::Web { proto_suffix: true },
        "application/grpc-web-text" | "application/grpc-web-text+proto" => Kind::WebText,
        "application/proto" => Kind::Connect,
        _ => return None,
    })
}

/// `/<package>.<Service>/<Method>`.
fn is_rpc_path(path: &str) -> bool {
    let Some(rest) = path.strip_prefix('/') else {
        return false;
    };
    let mut parts = rest.split('/');
    let (Some(service), Some(method), None) = (parts.next(), parts.next(), parts.next()) else {
        return false;
    };
    let word = |s: &str| {
        !s.is_empty()
            && s.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'.')
    };
    service.contains('.') && word(service) && word(method) && !method.contains('.')
}

async fn handle<S>(st: &Settings, mut inner: S, req: Request<Body>) -> Reply
where
    S: Service<Request<Body>, Response = Reply, Error = Infallible>,
{
    let rpc = is_rpc_path(req.uri().path());
    if rpc
        && req.method() == Method::OPTIONS
        && req.headers().contains_key("access-control-request-method")
    {
        return match st.origin(req.uri(), req.headers()) {
            Origin::Allowed(o) => preflight(&o),
            Origin::Denied => forbidden(),
            Origin::Plain => pass(&mut inner, req).await,
        };
    }
    let kind = if rpc && req.method() == Method::POST {
        kind_of(req.headers())
    } else {
        None
    };
    let Some(kind) = kind else {
        return pass(&mut inner, req).await;
    };
    let cors = match st.origin(req.uri(), req.headers()) {
        Origin::Plain => None,
        Origin::Allowed(o) => Some(o),
        Origin::Denied => return forbidden(),
    };
    let mut res = match kind {
        Kind::WebText => plain(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "grpc-web-text is not served; use application/grpc-web+proto",
        ),
        Kind::Web { proto_suffix } => web(st, &mut inner, req, proto_suffix).await,
        Kind::Connect => connect(st, &mut inner, req).await,
    };
    if let Some(o) = cors {
        add_cors(res.headers_mut(), &o);
    }
    res
}

async fn pass<S>(inner: &mut S, req: Request<Body>) -> Reply
where
    S: Service<Request<Body>, Response = Reply, Error = Infallible>,
{
    match inner.call(req).await {
        Ok(r) => r,
        Err(never) => match never {},
    }
}

fn plain(status: StatusCode, msg: &'static str) -> Reply {
    let mut res = Response::new(Body::from(msg));
    *res.status_mut() = status;
    res.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/plain; charset=utf-8"),
    );
    res
}

fn forbidden() -> Reply {
    plain(StatusCode::FORBIDDEN, "origin not allowed")
}

fn preflight(origin: &HeaderValue) -> Reply {
    let mut res = Response::new(Body::empty());
    *res.status_mut() = StatusCode::NO_CONTENT;
    let h = res.headers_mut();
    add_cors(h, origin);
    h.insert(
        header::ACCESS_CONTROL_ALLOW_METHODS,
        HeaderValue::from_static("POST"),
    );
    h.insert(
        header::ACCESS_CONTROL_ALLOW_HEADERS,
        HeaderValue::from_static(ALLOW_HEADERS),
    );
    h.insert(
        header::ACCESS_CONTROL_MAX_AGE,
        HeaderValue::from_static("600"),
    );
    res
}

fn add_cors(h: &mut HeaderMap, origin: &HeaderValue) {
    h.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, origin.clone());
    h.insert(
        header::ACCESS_CONTROL_EXPOSE_HEADERS,
        HeaderValue::from_static(EXPOSE_HEADERS),
    );
    h.append(header::VARY, HeaderValue::from_static("origin"));
}

/// Turns the browser request into the native gRPC request the services expect.
fn to_grpc(parts: &mut axum::http::request::Parts) {
    let h = &mut parts.headers;
    // The token is the Authorization header only; a cookie is never a credential here.
    h.remove(header::COOKIE);
    h.remove("x-grpc-web");
    h.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/grpc"),
    );
    h.insert(header::TE, HeaderValue::from_static("trailers"));
}

fn declared_length(h: &HeaderMap) -> Option<u64> {
    h.get(header::CONTENT_LENGTH)?.to_str().ok()?.parse().ok()
}

/// `body`, failing once more than `cap` bytes have been read.
fn limited(body: Body, cap: u64) -> Body {
    let mut seen = 0u64;
    Body::from_stream(body.into_data_stream().map(move |chunk| {
        let chunk = chunk?;
        seen += chunk.len() as u64;
        if seen > cap {
            return Err(axum::Error::new(std::io::Error::other(
                "request body too large",
            )));
        }
        Ok(chunk)
    }))
}

async fn web<S>(st: &Settings, inner: &mut S, req: Request<Body>, proto_suffix: bool) -> Reply
where
    S: Service<Request<Body>, Response = Reply, Error = Infallible>,
{
    if declared_length(req.headers()).is_some_and(|n| n > st.max_request) {
        return web_status(
            GRPC_CODE_RESOURCE_EXHAUSTED,
            "request is too large",
            proto_suffix,
        );
    }
    let (mut parts, body) = req.into_parts();
    to_grpc(&mut parts);
    let req = Request::from_parts(parts, limited(body, st.max_request));
    let (mut parts, body) = pass(inner, req).await.into_parts();
    if !is_grpc(&parts.headers) {
        return Response::from_parts(parts, body);
    }
    parts
        .headers
        .insert(header::CONTENT_TYPE, web_content_type(proto_suffix));
    parts.headers.remove(header::CONTENT_LENGTH);
    let fallback = status_block(&parts.headers);
    Response::from_parts(
        parts,
        Body::new(GrpcWebBody {
            inner: body,
            fallback: Some(fallback),
            done: false,
        }),
    )
}

fn web_content_type(proto_suffix: bool) -> HeaderValue {
    HeaderValue::from_static(if proto_suffix {
        "application/grpc-web+proto"
    } else {
        "application/grpc-web"
    })
}

fn is_grpc(h: &HeaderMap) -> bool {
    h.get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.to_ascii_lowercase().starts_with("application/grpc"))
}

/// A gRPC-Web reply that carries only a status.
fn web_status(code: i32, message: &str, proto_suffix: bool) -> Reply {
    let block = format!("grpc-status: {code}\r\ngrpc-message: {message}\r\n");
    let mut res = Response::new(Body::from(frame(0x80, block.as_bytes())));
    res.headers_mut()
        .insert(header::CONTENT_TYPE, web_content_type(proto_suffix));
    res
}

fn frame(flag: u8, payload: &[u8]) -> Bytes {
    let mut out = BytesMut::with_capacity(5 + payload.len());
    out.extend_from_slice(&[flag]);
    out.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    out.extend_from_slice(payload);
    out.freeze()
}

/// The `name: value` block of a trailer frame.
fn trailer_frame<'a>(entries: impl Iterator<Item = (&'a HeaderName, &'a HeaderValue)>) -> Bytes {
    let mut block = Vec::new();
    for (k, v) in entries {
        block.extend_from_slice(k.as_str().as_bytes());
        block.extend_from_slice(b": ");
        block.extend_from_slice(v.as_bytes());
        block.extend_from_slice(b"\r\n");
    }
    frame(0x80, &block)
}

/// The trailer frame for a reply that had its status in the headers (trailers-only), or a
/// missing-status error when it has none.
fn status_block(headers: &HeaderMap) -> Bytes {
    if headers.contains_key("grpc-status") {
        return trailer_frame(
            headers
                .iter()
                .filter(|(k, _)| *k != header::CONTENT_TYPE && *k != header::CONTENT_LENGTH),
        );
    }
    frame(
        0x80,
        format!("grpc-status: {GRPC_CODE_UNKNOWN}\r\ngrpc-message: reply had no status\r\n")
            .as_bytes(),
    )
}

/// A gRPC body with its trailers moved into a final `0x80` data frame.
struct GrpcWebBody {
    inner: Body,
    /// Sent if the inner body ends without trailers.
    fallback: Option<Bytes>,
    done: bool,
}

impl http_body::Body for GrpcWebBody {
    type Data = Bytes;
    type Error = axum::Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, axum::Error>>> {
        if self.done {
            return Poll::Ready(None);
        }
        match std::task::ready!(Pin::new(&mut self.inner).poll_frame(cx)) {
            Some(Ok(frame)) => match frame.into_trailers() {
                Ok(trailers) => {
                    self.done = true;
                    Poll::Ready(Some(Ok(Frame::data(trailer_frame(trailers.iter())))))
                }
                Err(frame) => Poll::Ready(Some(Ok(frame))),
            },
            Some(Err(e)) => Poll::Ready(Some(Err(e))),
            None => {
                self.done = true;
                Poll::Ready(self.fallback.take().map(|b| Ok(Frame::data(b))))
            }
        }
    }

    fn is_end_stream(&self) -> bool {
        self.done
    }

    fn size_hint(&self) -> SizeHint {
        SizeHint::default()
    }
}

// ------------------------------------------------------------------------------ Connect unary

async fn connect<S>(st: &Settings, inner: &mut S, req: Request<Body>) -> Reply
where
    S: Service<Request<Body>, Response = Reply, Error = Infallible>,
{
    if let Some(enc) = req.headers().get(header::CONTENT_ENCODING)
        && enc.as_bytes() != b"identity"
    {
        return connect_error(GRPC_CODE_UNIMPLEMENTED, "content-encoding is not supported");
    }
    if declared_length(req.headers()).is_some_and(|n| n > st.max_request) {
        return connect_error(GRPC_CODE_RESOURCE_EXHAUSTED, "request is too large");
    }
    let (mut parts, body) = req.into_parts();
    let cap = usize::try_from(st.max_request).unwrap_or(usize::MAX);
    let Ok(message) = axum::body::to_bytes(body, cap).await else {
        return connect_error(GRPC_CODE_RESOURCE_EXHAUSTED, "request is too large");
    };
    to_grpc(&mut parts);
    let h = &mut parts.headers;
    h.remove(header::CONTENT_LENGTH);
    h.remove(header::CONTENT_ENCODING);
    h.remove("grpc-encoding");
    // Replies are never compressed: the caller cannot unframe a compressed message here.
    h.remove("grpc-accept-encoding");
    if let Some(ms) = h
        .remove("connect-timeout-ms")
        .and_then(|v| v.to_str().ok().and_then(|s| s.parse::<u64>().ok()))
        .filter(|ms| *ms > 0 && *ms < 100_000_000)
        && let Ok(v) = HeaderValue::from_str(&format!("{ms}m"))
    {
        h.insert("grpc-timeout", v);
    }
    let req = Request::from_parts(parts, Body::from(frame(0, &message)));
    let (mut parts, body) = pass(inner, req).await.into_parts();
    if !is_grpc(&parts.headers) {
        return Response::from_parts(parts, body);
    }
    let (data, trailers) = match collect(body).await {
        Ok(v) => v,
        Err(msg) => return connect_error(GRPC_CODE_INTERNAL, msg),
    };
    let (code, message) = grpc_status(trailers.as_ref().unwrap_or(&parts.headers))
        .or_else(|| grpc_status(&parts.headers))
        .unwrap_or((GRPC_CODE_UNKNOWN, "reply had no status".to_owned()));
    if code != 0 {
        return connect_error(code, &message);
    }
    let reply = match single_message(&data) {
        Ok(m) => m,
        Err(msg) => return connect_error(GRPC_CODE_INTERNAL, msg),
    };
    let drop: Vec<HeaderName> = parts
        .headers
        .keys()
        .filter(|k| k.as_str().starts_with("grpc-") || *k == header::CONTENT_LENGTH)
        .cloned()
        .collect();
    for k in drop {
        parts.headers.remove(k);
    }
    parts.headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/proto"),
    );
    Response::from_parts(parts, Body::from(reply))
}

async fn collect(mut body: Body) -> Result<(Bytes, Option<HeaderMap>), &'static str> {
    use http_body::Body as _;
    let mut data = BytesMut::new();
    let mut trailers = None;
    while let Some(frame) = future::poll_fn(|cx| Pin::new(&mut body).poll_frame(cx)).await {
        let frame = frame.map_err(|_| "the reply body failed")?;
        match frame.into_data() {
            Ok(d) => {
                if data.len() + d.len() > MAX_CONNECT_REPLY {
                    return Err("reply is too large for a unary call");
                }
                data.extend_from_slice(&d);
            }
            Err(f) => {
                if let Ok(t) = f.into_trailers() {
                    trailers = Some(t);
                }
            }
        }
    }
    Ok((data.freeze(), trailers))
}

/// The one message of a unary reply.
fn single_message(data: &[u8]) -> Result<Bytes, &'static str> {
    if data.is_empty() {
        return Ok(Bytes::new());
    }
    let (Some(&flag), Some(len)) = (data.first(), data.get(1..5)) else {
        return Err("truncated reply frame");
    };
    let len = u32::from_be_bytes([len[0], len[1], len[2], len[3]]) as usize;
    if flag != 0 {
        return Err("reply is compressed");
    }
    match data.len().cmp(&(5 + len)) {
        std::cmp::Ordering::Less => Err("truncated reply frame"),
        std::cmp::Ordering::Greater => Err("method streams; call it with gRPC-Web"),
        std::cmp::Ordering::Equal => Ok(Bytes::copy_from_slice(&data[5..])),
    }
}

/// `grpc-status` and the decoded `grpc-message` of a header or trailer block.
fn grpc_status(h: &HeaderMap) -> Option<(i32, String)> {
    let code = h.get("grpc-status")?.to_str().ok()?.trim().parse().ok()?;
    let message = h
        .get("grpc-message")
        .and_then(|v| v.to_str().ok())
        .map(percent_decode)
        .unwrap_or_default();
    Some((code, message))
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        let hex = |c: u8| (c as char).to_digit(16);
        if b[i] == b'%'
            && let (Some(h), Some(l)) = (
                b.get(i + 1).and_then(|c| hex(*c)),
                b.get(i + 2).and_then(|c| hex(*c)),
            )
        {
            out.push((h * 16 + l) as u8);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn http_status(code: i32) -> StatusCode {
    let n = match code {
        1 => 499,
        3 | 9 | 11 => 400,
        4 => 504,
        5 => 404,
        6 | 10 => 409,
        7 => 403,
        8 => 429,
        12 => 501,
        14 => 503,
        16 => 401,
        _ => 500,
    };
    StatusCode::from_u16(n).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR)
}

/// A Connect error: `{"code": "...", "message": "..."}` with the matching HTTP status.
fn connect_error(code: i32, message: &str) -> Reply {
    let name = usize::try_from(code)
        .ok()
        .and_then(|i| CONNECT_CODES.get(i))
        .copied()
        .unwrap_or("unknown");
    let body = serde_json::json!({ "code": name, "message": message }).to_string();
    let mut res = Response::new(Body::from(body));
    *res.status_mut() = http_status(code);
    res.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    res
}

#[cfg(test)]
mod tests;
