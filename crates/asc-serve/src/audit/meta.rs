//! The request facts an audit event records: the network prefix, the user agent and the request
//! id (docs/BUILDOUT.md section 3).
//!
//! [`MetaLayer`] wraps the whole server. It keeps the `User-Agent` and `X-Request-Id` of the
//! request in a task-local for the length of the call, so any code on the call path can append an
//! event without every `CallCtx` carrying them. The network address is the one admission already
//! settled (the TCP peer, or the client behind a trusted proxy, `limits.trusted_proxies`);
//! [`current`] masks it to a /24 for IPv4 and a /48 for IPv6 before it is stored.

use std::future::Future;
use std::task::{Context, Poll};

use axum::http::{HeaderMap, Request, Response};
use futures::future::BoxFuture;
use tower::{Layer, Service};

use super::{MAX_USER_AGENT_BYTES, catalogue::plain};
use crate::limits::{Peer, current_peer};

/// Longest request id kept, in bytes.
pub const MAX_REQUEST_ID: usize = 64;

/// What is known about the request in progress.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RequestMeta {
    /// The masked network address (`203.0.113.0/24`), empty when unknown.
    pub remote_addr: String,
    pub user_agent: String,
    pub request_id: String,
}

#[derive(Clone, Debug, Default)]
struct Http {
    user_agent: String,
    request_id: String,
}

tokio::task_local! {
    static HTTP: Http;
}

/// The coarse form of a peer: IPv4 to a /24, IPv6 to a /48. Admission keeps IPv4 whole and IPv6
/// to a /64, so both are masked further.
pub fn coarse(peer: Peer) -> String {
    match peer {
        Peer::V4([a, b, c, _]) => format!("{a}.{b}.{c}.0/24"),
        Peer::V6(o) => format!(
            "{:x}:{:x}:{:x}::/48",
            u16::from_be_bytes([o[0], o[1]]),
            u16::from_be_bytes([o[2], o[3]]),
            u16::from_be_bytes([o[4], o[5]])
        ),
        Peer::Unknown => String::new(),
    }
}

/// The facts of the request in progress; empty outside one.
pub fn current() -> RequestMeta {
    let (user_agent, request_id) = HTTP
        .try_with(|h| (h.user_agent.clone(), h.request_id.clone()))
        .unwrap_or_default();
    RequestMeta {
        remote_addr: coarse(current_peer()),
        user_agent,
        request_id,
    }
}

/// A user agent as the log keeps it: control characters removed, cut to
/// [`MAX_USER_AGENT_BYTES`].
pub fn clean_user_agent(raw: &str) -> String {
    let mut s = plain(raw);
    if s.len() > MAX_USER_AGENT_BYTES {
        let mut end = MAX_USER_AGENT_BYTES;
        while !s.is_char_boundary(end) {
            end -= 1;
        }
        s.truncate(end);
    }
    s
}

/// A request id the caller may supply (short, token characters), else a fresh one.
fn request_id(headers: &HeaderMap) -> String {
    headers
        .get("x-request-id")
        .and_then(|v| v.to_str().ok())
        .filter(|v| {
            !v.is_empty()
                && v.len() <= MAX_REQUEST_ID
                && v.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
        })
        .map_or_else(|| uuid::Uuid::new_v4().simple().to_string(), str::to_owned)
}

/// Runs `fut` with `user_agent` and `request_id` as the request facts. Transports without HTTP
/// headers (the SSH listener) use it to give their commands the same record.
pub fn scope<F: Future>(
    user_agent: &str,
    request_id: &str,
    fut: F,
) -> impl Future<Output = F::Output> {
    HTTP.scope(
        Http {
            user_agent: clean_user_agent(user_agent),
            request_id: request_id.chars().take(MAX_REQUEST_ID).collect(),
        },
        fut,
    )
}

/// Keeps the request facts for the length of each request.
#[derive(Clone, Copy, Debug, Default)]
pub struct MetaLayer;

impl<S> Layer<S> for MetaLayer {
    type Service = Meta<S>;

    fn layer(&self, inner: S) -> Meta<S> {
        Meta { inner }
    }
}

/// The service of [`MetaLayer`].
#[derive(Clone, Debug)]
pub struct Meta<S> {
    inner: S,
}

impl<S, B, RB> Service<Request<B>> for Meta<S>
where
    S: Service<Request<B>, Response = Response<RB>> + Clone + Send + 'static,
    S::Future: Send + 'static,
    B: Send + 'static,
{
    type Response = Response<RB>;
    type Error = S::Error;
    type Future = BoxFuture<'static, Result<Response<RB>, S::Error>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: Request<B>) -> Self::Future {
        let clone = self.inner.clone();
        let mut inner = std::mem::replace(&mut self.inner, clone);
        let http = Http {
            user_agent: req
                .headers()
                .get("user-agent")
                .and_then(|v| v.to_str().ok())
                .map(clean_user_agent)
                .unwrap_or_default(),
            request_id: request_id(req.headers()),
        };
        Box::pin(HTTP.scope(http, async move { inner.call(req).await }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn addresses_are_masked() {
        assert_eq!(coarse(Peer::V4([203, 0, 113, 77])), "203.0.113.0/24");
        assert_eq!(
            coarse(Peer::V6([0x20, 0x01, 0x0d, 0xb8, 0x12, 0x34, 0xaa, 0xbb])),
            "2001:db8:1234::/48"
        );
        assert_eq!(coarse(Peer::Unknown), "");
    }

    #[tokio::test]
    async fn outside_a_request_there_is_nothing() {
        assert_eq!(current(), RequestMeta::default());
    }

    #[tokio::test]
    async fn a_scope_gives_the_request_facts() {
        let m = scope("asc/1.0 (x)\nspoof", "req-1", async { current() }).await;
        assert_eq!(m.user_agent, "asc/1.0 (x) spoof");
        assert_eq!(m.request_id, "req-1");
        assert_eq!(m.remote_addr, "");
    }

    #[tokio::test]
    async fn the_layer_reads_the_headers() {
        use tower::ServiceExt;
        #[derive(Clone)]
        struct Echo;
        impl Service<Request<()>> for Echo {
            type Response = Response<String>;
            type Error = std::convert::Infallible;
            type Future = std::future::Ready<Result<Response<String>, Self::Error>>;
            fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
                Poll::Ready(Ok(()))
            }
            fn call(&mut self, _: Request<()>) -> Self::Future {
                let m = current();
                std::future::ready(Ok(Response::new(format!(
                    "{}|{}",
                    m.user_agent, m.request_id
                ))))
            }
        }
        let mut svc = MetaLayer.layer(Echo);
        let req = Request::builder()
            .header("user-agent", "curl/8")
            .header("x-request-id", "abc-1")
            .body(())
            .unwrap();
        let res = svc.ready().await.unwrap().call(req).await.unwrap();
        assert_eq!(res.into_body(), "curl/8|abc-1");
        // A request id with bad characters is replaced.
        let req = Request::builder()
            .header("x-request-id", "a b")
            .body(())
            .unwrap();
        let res = svc.ready().await.unwrap().call(req).await.unwrap();
        let body = res.into_body();
        let id = body.split('|').nth(1).unwrap();
        assert_eq!(id.len(), 32);
    }

    #[test]
    fn user_agents_are_bounded() {
        let ua = clean_user_agent(&"é".repeat(400));
        assert!(ua.len() <= MAX_USER_AGENT_BYTES);
    }
}
