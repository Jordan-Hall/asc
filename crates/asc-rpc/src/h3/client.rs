//! The h3 client: [`Upgrader`].

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::time::{Duration, Instant};

use bytes::{Buf, BufMut, Bytes, BytesMut};
use http::{HeaderMap, Method, Request, Uri};
use quinn::crypto::rustls::QuicClientConfig;
use rustls::pki_types::CertificateDer;

use super::alt_svc::{self, AltSvcCache, Origin};
use super::{Attempt, Reply};

type Sender = ::h3::client::SendRequest<h3_quinn::OpenStreams, Bytes>;

/// Longest wait for the QUIC and TLS handshake. A network that drops UDP costs this once per
/// [`alt_svc::FAILURE_BACKOFF`].
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(3);
/// Longest wait for one request, from the first byte sent to the last byte of the response.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(300);
/// QUIC connections kept for reuse.
pub const MAX_POOLED: usize = 64;
/// Largest response header block accepted.
const MAX_FIELD_SECTION: u64 = 64 * 1024;

/// The limits of one client.
#[derive(Clone, Copy, Debug)]
pub struct ClientLimits {
    pub connect_timeout: Duration,
    pub request_timeout: Duration,
}

impl Default for ClientLimits {
    fn default() -> Self {
        Self {
            connect_timeout: CONNECT_TIMEOUT,
            request_timeout: REQUEST_TIMEOUT,
        }
    }
}

/// Why one h3 exchange failed, and whether the request had been sent.
struct Failure {
    message: String,
    /// Nothing of the request reached the server.
    unsent: bool,
}

impl Failure {
    fn unsent(e: impl std::fmt::Display) -> Self {
        Self {
            message: e.to_string(),
            unsent: true,
        }
    }

    fn sent(e: impl std::fmt::Display) -> Self {
        Self {
            message: e.to_string(),
            unsent: false,
        }
    }
}

#[derive(Clone)]
struct Conn {
    send: Sender,
    alive: Arc<AtomicBool>,
}

/// Tries h3 for origins that advertised it. See the module docs of [`super`].
pub struct Upgrader {
    cache: AltSvcCache,
    /// `None` when the platform verifier could not start: the client then never upgrades.
    tls: Option<quinn::ClientConfig>,
    pool: Mutex<HashMap<Origin, Conn>>,
    limits: ClientLimits,
}

impl std::fmt::Debug for Upgrader {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Upgrader")
            .field("origins", &self.cache.len())
            .finish_non_exhaustive()
    }
}

impl Default for Upgrader {
    fn default() -> Self {
        Self::new(ClientLimits::default())
    }
}

impl Upgrader {
    /// A client that trusts what the platform trusts, as the h2 client does.
    pub fn new(limits: ClientLimits) -> Self {
        Self::build(Vec::new(), limits)
    }

    /// The platform's roots plus `roots`. For tests and for a deployment with a private CA that
    /// h2 trusts through the same roots.
    pub fn with_extra_roots(roots: Vec<CertificateDer<'static>>, limits: ClientLimits) -> Self {
        Self::build(roots, limits)
    }

    /// The process-wide client, so every request of a command shares one `Alt-Svc` memory.
    pub fn shared() -> &'static Self {
        static ONE: OnceLock<Upgrader> = OnceLock::new();
        ONE.get_or_init(Upgrader::default)
    }

    fn build(roots: Vec<CertificateDer<'static>>, limits: ClientLimits) -> Self {
        let tls = match tls_config(roots) {
            Ok(c) => Some(c),
            Err(e) => {
                tracing::warn!(error = %e, "h3 is off: the certificate verifier did not start");
                None
            }
        };
        Self {
            cache: AltSvcCache::default(),
            tls,
            pool: Mutex::default(),
            limits,
        }
    }

    /// Learns from a response that `uri` answered over h1 or h2 (or h3). The origin is the one
    /// the request went to, never anything the response names.
    pub fn note_response(&self, uri: &Uri, headers: &HeaderMap) {
        let Some(origin) = Origin::of(uri) else {
            return;
        };
        // Several `Alt-Svc` fields are one list.
        let mut advert = alt_svc::Advert::None;
        for v in headers.get_all(http::header::ALT_SVC) {
            let Ok(v) = v.to_str() else { continue };
            match alt_svc::parse(v, &origin.host) {
                alt_svc::Advert::None => {}
                a @ alt_svc::Advert::Clear => {
                    advert = a;
                    break;
                }
                a if advert == alt_svc::Advert::None => advert = a,
                _ => {}
            }
        }
        self.cache.note(&origin, advert, Instant::now());
        if advert == alt_svc::Advert::Clear {
            self.pool
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .remove(&origin);
        }
    }

    /// Sends `req` over h3 when its origin advertised h3 and is not in a failure backoff.
    /// `max_body` bounds the response body read.
    pub async fn send(&self, req: &Request<Bytes>, max_body: usize) -> Attempt {
        let Some(tls) = &self.tls else {
            return Attempt::Fallback;
        };
        let Some(origin) = Origin::of(req.uri()) else {
            return Attempt::Fallback;
        };
        let Some(port) = self.cache.lookup(&origin, Instant::now()) else {
            return Attempt::Fallback;
        };
        let idempotent = is_idempotent(req.method());
        let mut last = None;
        // A pooled connection may have died unseen: ask again once on a fresh one, when the
        // request could not have been processed twice.
        for _ in 0..2 {
            let (conn, reused) = match self.connection(&origin, port, tls).await {
                Ok(c) => c,
                Err(e) => return self.give_up(&origin, &e, true),
            };
            match self.exchange(&conn, req, max_body).await {
                Ok(reply) if reply.status.is_redirection() && idempotent => {
                    // h2 follows redirects under asc's redirect policy; h3 does not follow any.
                    return Attempt::Fallback;
                }
                Ok(reply) => {
                    self.note_response(req.uri(), &reply.headers);
                    return Attempt::Done(reply);
                }
                Err(f) => {
                    self.forget(&origin);
                    let retry = reused && (f.unsent || idempotent);
                    last = Some(f);
                    if !retry {
                        break;
                    }
                }
            }
        }
        match last {
            Some(f) => self.give_up(&origin, &f, idempotent || f.unsent),
            None => Attempt::Fallback,
        }
    }

    /// Caches the failure and chooses between h2 and an error.
    fn give_up(&self, origin: &Origin, f: &Failure, replay: bool) -> Attempt {
        self.cache.failed(origin, Instant::now());
        self.forget(origin);
        tracing::debug!(host = %origin.host, error = %f.message, "h3 failed");
        if replay {
            Attempt::Fallback
        } else {
            Attempt::Failed(f.message.clone())
        }
    }

    fn forget(&self, origin: &Origin) {
        self.pool
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(origin);
    }

    /// A live connection to `origin`, and whether it was already open.
    async fn connection(
        &self,
        origin: &Origin,
        port: u16,
        tls: &quinn::ClientConfig,
    ) -> Result<(Conn, bool), Failure> {
        {
            let pool = self.pool.lock().unwrap_or_else(PoisonError::into_inner);
            if let Some(c) = pool.get(origin)
                && c.alive.load(Ordering::Acquire)
            {
                return Ok((c.clone(), true));
            }
        }
        let conn = tokio::time::timeout(
            self.limits.connect_timeout,
            open(&origin.host, port, tls.clone()),
        )
        .await
        .map_err(|_| Failure::unsent("handshake timed out"))??;
        let mut pool = self.pool.lock().unwrap_or_else(PoisonError::into_inner);
        pool.retain(|_, c| c.alive.load(Ordering::Acquire));
        if pool.len() < MAX_POOLED {
            pool.insert(origin.clone(), conn.clone());
        }
        Ok((conn, false))
    }

    async fn exchange(
        &self,
        conn: &Conn,
        req: &Request<Bytes>,
        max_body: usize,
    ) -> Result<Reply, Failure> {
        let mut head = Request::builder().method(req.method()).uri(req.uri());
        for (k, v) in req.headers() {
            head = head.header(k, v);
        }
        let head = head.body(()).map_err(Failure::unsent)?;
        let mut send = conn.send.clone();
        let work = async {
            let mut stream = send.send_request(head).await.map_err(Failure::unsent)?;
            if !req.body().is_empty() {
                stream
                    .send_data(req.body().clone())
                    .await
                    .map_err(Failure::sent)?;
            }
            stream.finish().await.map_err(Failure::sent)?;
            let (parts, ()) = stream
                .recv_response()
                .await
                .map_err(Failure::sent)?
                .into_parts();
            let mut body = BytesMut::new();
            let mut truncated = false;
            while let Some(mut chunk) = stream.recv_data().await.map_err(Failure::sent)? {
                let room = max_body.saturating_sub(body.len());
                if chunk.remaining() > room {
                    body.put(chunk.copy_to_bytes(room));
                    truncated = true;
                    break;
                }
                body.put(chunk);
            }
            Ok(Reply {
                status: parts.status,
                headers: parts.headers,
                body: body.freeze(),
                truncated,
            })
        };
        tokio::time::timeout(self.limits.request_timeout, work)
            .await
            .map_err(|_| Failure::sent("request timed out"))?
    }

    /// Whether `origin` would be tried over h3 now.
    pub fn would_try(&self, uri: &Uri) -> bool {
        self.tls.is_some()
            && Origin::of(uri).is_some_and(|o| self.cache.lookup(&o, Instant::now()).is_some())
    }
}

/// RFC 9110 section 9.2.2: a repeat of these has the effect of one.
fn is_idempotent(m: &Method) -> bool {
    matches!(
        *m,
        Method::GET | Method::HEAD | Method::PUT | Method::DELETE | Method::OPTIONS | Method::TRACE
    )
}

/// The client TLS configuration: TLS 1.3, ALPN `h3`, no early data, the platform's verifier.
fn rustls_config(
    extra_roots: Vec<CertificateDer<'static>>,
) -> Result<rustls::ClientConfig, String> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let verifier =
        rustls_platform_verifier::Verifier::new_with_extra_roots(extra_roots, provider.clone())
            .map_err(|e| e.to_string())?;
    let mut cfg = rustls::ClientConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])
        .map_err(|e| e.to_string())?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(verifier))
        .with_no_client_auth();
    cfg.alpn_protocols = vec![b"h3".to_vec()];
    // 0-RTT data can be replayed by an attacker; asc never sends any.
    cfg.enable_early_data = false;
    Ok(cfg)
}

fn tls_config(extra_roots: Vec<CertificateDer<'static>>) -> Result<quinn::ClientConfig, String> {
    let cfg = rustls_config(extra_roots)?;
    let quic = QuicClientConfig::try_from(cfg).map_err(|e| e.to_string())?;
    Ok(quinn::ClientConfig::new(Arc::new(quic)))
}

/// Opens a QUIC connection to `host:port` and starts the h3 driver.
async fn open(host: &str, port: u16, tls: quinn::ClientConfig) -> Result<Conn, Failure> {
    let addr: SocketAddr = tokio::net::lookup_host((host, port))
        .await
        .map_err(Failure::unsent)?
        .next()
        .ok_or_else(|| Failure::unsent("no address"))?;
    let bind: SocketAddr = if addr.is_ipv6() {
        (std::net::Ipv6Addr::UNSPECIFIED, 0).into()
    } else {
        (std::net::Ipv4Addr::UNSPECIFIED, 0).into()
    };
    let endpoint = quinn::Endpoint::client(bind).map_err(Failure::unsent)?;
    let quic = endpoint
        .connect_with(tls, addr, host)
        .map_err(Failure::unsent)?
        .await
        .map_err(Failure::unsent)?;
    let (mut driver, send) = ::h3::client::builder()
        .max_field_section_size(MAX_FIELD_SECTION)
        .build(h3_quinn::Connection::new(quic))
        .await
        .map_err(Failure::unsent)?;
    let alive = Arc::new(AtomicBool::new(true));
    let flag = alive.clone();
    tokio::spawn(async move {
        // Ends when the connection closes. The endpoint lives as long as the driver.
        let _ = std::future::poll_fn(|cx| driver.poll_close(cx)).await;
        flag.store(false, Ordering::Release);
        drop(endpoint);
    });
    Ok(Conn { send, alive })
}

#[cfg(test)]
mod tests;
