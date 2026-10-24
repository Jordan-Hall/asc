//! The HTTP/3 listener (U89b).
//!
//! When `[tls]` and `http3_port` are both set, `asc serve` also listens for QUIC on UDP
//! `http3_port` (on the address of `listen`) and serves the SAME axum router over h3 that the TCP
//! listener serves over h1 and h2. The router carries authentication, the admission layer and
//! every other limit, so a request over h3 meets exactly the checks one over h2 meets; what is new
//! here is the transport's own limits, each named in [`H3LimitsConfig`] (`[limits.h3]`):
//!
//! - `max_connections`: QUIC connections at once; past it a new one is refused;
//! - `max_streams`: concurrent request streams per connection (QUIC stream credit);
//! - `max_request_bytes`: one request body; a larger one is answered `413`;
//! - `max_header_bytes`: one request header block;
//! - `idle_timeout`: a connection with no packets closes;
//! - `handshake_timeout`, `header_timeout`: a peer that starts and stalls is dropped;
//! - `stall_timeout`: no byte of a body moves for this long, in either direction.
//!
//! The TLS identity is the one tonic serves on TCP (the same PEM files). QUIC needs TLS 1.3; early
//! data is off, so no request is ever replayable from the handshake. A client whose source address
//! is not yet validated gets a retry token first, so a spoofed address cannot be charged for
//! another host's traffic (the admission layer keys limits on the peer address).
//!
//! The feature `http3` (default) builds the listener; without it [`start`] only warns.

use serde::Deserialize;

use crate::config::{ByteSize, ConfigError, Secs};

/// `[limits.h3]`. The defaults suit a server behind a few hundred clients.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct H3LimitsConfig {
    pub max_connections: u32,
    pub max_streams: u32,
    pub max_request_bytes: ByteSize,
    pub max_header_bytes: ByteSize,
    pub idle_timeout: Secs,
    pub handshake_timeout: Secs,
    pub header_timeout: Secs,
    pub stall_timeout: Secs,
}

impl Default for H3LimitsConfig {
    fn default() -> Self {
        Self {
            max_connections: 1024,
            max_streams: 100,
            max_request_bytes: ByteSize(ByteSize::GIB),
            max_header_bytes: ByteSize(64 * ByteSize::KIB),
            idle_timeout: Secs(30),
            handshake_timeout: Secs(10),
            header_timeout: Secs(10),
            stall_timeout: Secs(60),
        }
    }
}

impl H3LimitsConfig {
    /// Every limit is positive and within what QUIC and the listener can hold.
    pub(crate) fn validate(&self) -> Result<(), ConfigError> {
        let bounded = [
            (
                "max_connections",
                u64::from(self.max_connections),
                1_000_000,
            ),
            ("max_streams", u64::from(self.max_streams), 65_535),
            ("max_request_bytes", self.max_request_bytes.0, 1 << 44),
            (
                "max_header_bytes",
                self.max_header_bytes.0,
                16 * ByteSize::MIB,
            ),
            ("idle_timeout", self.idle_timeout.0, 600),
            ("handshake_timeout", self.handshake_timeout.0, 120),
            ("header_timeout", self.header_timeout.0, 600),
            ("stall_timeout", self.stall_timeout.0, 3600),
        ];
        for (name, v, max) in bounded {
            if v == 0 || v > max {
                return Err(ConfigError::Invalid(format!(
                    "limits.h3.{name} must be between 1 and {max}"
                )));
            }
        }
        Ok(())
    }
}

#[cfg(feature = "http3")]
pub use imp::*;

#[cfg(not(feature = "http3"))]
mod off {
    use tonic::service::Routes;

    use crate::ServeError;
    use crate::config::ServeConfig;

    /// Held for the life of the listener.
    pub struct Handle;

    /// Without the feature there is no QUIC listener; say so when it was asked for.
    pub fn start(cfg: &ServeConfig, _routes: &Routes) -> Result<Option<Handle>, ServeError> {
        if cfg.tls.is_some() && cfg.http3_port.is_some() {
            tracing::warn!("http3 is not compiled in; the QUIC listener is not started");
        }
        Ok(None)
    }
}
#[cfg(not(feature = "http3"))]
pub use off::*;

#[cfg(feature = "http3")]
mod imp {
    use std::net::SocketAddr;
    use std::pin::Pin;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;

    use axum::Router;
    use axum::body::Body;
    use axum::http::{HeaderMap, Request, Response, StatusCode, header};
    use bytes::{Buf, Bytes};
    use http_body::Body as _;
    use quinn::crypto::rustls::QuicServerConfig;
    use rustls::pki_types::pem::PemObject;
    use rustls::pki_types::{CertificateDer, PrivateKeyDer};
    use tokio::sync::{OwnedSemaphorePermit, Semaphore};
    use tokio::time::timeout;
    use tonic::service::Routes;
    use tonic::transport::server::TcpConnectInfo;
    use tower::ServiceExt;

    use super::H3LimitsConfig;
    use crate::config::ServeConfig;
    use crate::{ServeError, alt_svc, read_pem};

    /// QUIC unidirectional streams per connection: h3 needs three (control and two QPACK).
    const MAX_UNI_STREAMS: u8 = 16;
    /// The close code of a normal shutdown.
    const CLOSE_SHUTDOWN: u32 = 0;
    const CLOSE_REFUSED: u32 = 0x0100;

    /// The running listener; dropping it stops accepting and closes every connection.
    pub struct Handle {
        task: tokio::task::JoinHandle<()>,
        endpoint: quinn::Endpoint,
    }

    impl Handle {
        /// The UDP address the listener holds.
        pub fn local_addr(&self) -> std::io::Result<SocketAddr> {
            self.endpoint.local_addr()
        }
    }

    impl Drop for Handle {
        fn drop(&mut self) {
            self.endpoint
                .close(CLOSE_SHUTDOWN.into(), b"asc serve is stopping");
            self.task.abort();
        }
    }

    /// Starts the QUIC listener when `[tls]` and `http3_port` are both set.
    ///
    /// The bind happens here, so a bad address or a busy port fails startup. `routes` is the
    /// router the TCP listener serves.
    pub fn start(cfg: &ServeConfig, routes: &Routes) -> Result<Option<Handle>, ServeError> {
        let (Some(tls), Some(port)) = (&cfg.tls, cfg.http3_port) else {
            return Ok(None);
        };
        let addr = SocketAddr::new(cfg.listen.ip(), port);
        let endpoint = bind(
            &read_pem(&tls.cert)?,
            &read_pem(&tls.key)?,
            addr,
            &cfg.limits.h3,
        )?;
        let router = routes
            .clone()
            .into_axum_router()
            .layer(alt_svc::layer(cfg.http3_port));
        tracing::info!(listen = %addr, "asc serve h3 listening");
        Ok(Some(spawn(endpoint, router, cfg.limits.h3.clone())))
    }

    /// Runs the accept loop for `endpoint` on a task.
    pub fn spawn(endpoint: quinn::Endpoint, router: Router, limits: H3LimitsConfig) -> Handle {
        let task = tokio::spawn(serve(endpoint.clone(), router, limits));
        Handle { task, endpoint }
    }

    /// A QUIC endpoint on `addr` with the identity in the PEM bytes. The key may be PKCS#8,
    /// PKCS#1 or SEC1, as on the TCP listener.
    pub fn bind(
        cert_pem: &[u8],
        key_pem: &[u8],
        addr: SocketAddr,
        limits: &H3LimitsConfig,
    ) -> Result<quinn::Endpoint, ServeError> {
        let tls = |e: String| ServeError::Tls(format!("h3: {e}"));
        let certs = CertificateDer::pem_slice_iter(cert_pem)
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| tls(e.to_string()))?;
        let key = PrivateKeyDer::from_pem_slice(key_pem).map_err(|e| tls(e.to_string()))?;
        // TLS 1.3 only (QUIC needs it). `max_early_data_size` stays 0: no 0-RTT.
        let mut conf = rustls::ServerConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_protocol_versions(&[&rustls::version::TLS13])
        .map_err(|e| tls(e.to_string()))?
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .map_err(|e| tls(e.to_string()))?;
        conf.alpn_protocols = vec![b"h3".to_vec()];
        let quic = QuicServerConfig::try_from(conf).map_err(|e| tls(e.to_string()))?;
        let mut server = quinn::ServerConfig::with_crypto(Arc::new(quic));
        let mut transport = quinn::TransportConfig::default();
        transport.max_concurrent_bidi_streams(limits.max_streams.into());
        transport.max_concurrent_uni_streams(MAX_UNI_STREAMS.into());
        transport.max_idle_timeout(Some(
            Duration::from_secs(limits.idle_timeout.0)
                .try_into()
                .map_err(|_| tls("idle_timeout is out of range".into()))?,
        ));
        server.transport_config(Arc::new(transport));
        quinn::Endpoint::server(server, addr)
            .map_err(|e| ServeError::Tls(format!("h3: {addr}: {e}")))
    }

    struct Shared {
        router: Router,
        limits: H3LimitsConfig,
        local: SocketAddr,
    }

    async fn serve(endpoint: quinn::Endpoint, router: Router, limits: H3LimitsConfig) {
        let local = endpoint
            .local_addr()
            .unwrap_or(SocketAddr::from(([0, 0, 0, 0], 0)));
        let slots = Arc::new(Semaphore::new(limits.max_connections as usize));
        let shared = Arc::new(Shared {
            router,
            limits,
            local,
        });
        while let Some(incoming) = endpoint.accept().await {
            // An address nobody has proved to own gets a retry token first.
            if !incoming.remote_address_validated() && incoming.may_retry() {
                let _ = incoming.retry();
                continue;
            }
            let Ok(permit) = slots.clone().try_acquire_owned() else {
                tracing::warn!(peer = %incoming.remote_address(), "h3 connection refused: at max_connections");
                incoming.refuse();
                continue;
            };
            tokio::spawn(connection(incoming, permit, shared.clone()));
        }
    }

    async fn connection(
        incoming: quinn::Incoming,
        _slot: OwnedSemaphorePermit,
        shared: Arc<Shared>,
    ) {
        let peer = incoming.remote_address();
        let Ok(connecting) = incoming.accept() else {
            return;
        };
        let handshake = Duration::from_secs(shared.limits.handshake_timeout.0);
        let Ok(Ok(conn)) = timeout(handshake, connecting).await else {
            return;
        };
        let built = ::h3::server::builder()
            .max_field_section_size(shared.limits.max_header_bytes.0)
            .build::<_, Bytes>(h3_quinn::Connection::new(conn.clone()))
            .await;
        let Ok(mut h3) = built else {
            conn.close(CLOSE_REFUSED.into(), b"h3 setup failed");
            return;
        };
        while let Ok(Some(resolver)) = h3.accept().await {
            tokio::spawn(request(resolver, shared.clone(), peer));
        }
    }

    type Resolver = ::h3::server::RequestResolver<h3_quinn::Connection, Bytes>;

    async fn request(resolver: Resolver, shared: Arc<Shared>, peer: SocketAddr) {
        let header_wait = Duration::from_secs(shared.limits.header_timeout.0);
        let Ok(Ok((req, stream))) = timeout(header_wait, resolver.resolve_request()).await else {
            return;
        };
        let (mut tx, mut rx) = stream.split();
        let max = shared.limits.max_request_bytes.0;
        let stall = Duration::from_secs(shared.limits.stall_timeout.0);

        let declared = req
            .headers()
            .get(header::CONTENT_LENGTH)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<u64>().ok());
        if declared.is_some_and(|n| n > max) {
            let _ = reply(&mut tx, status_only(StatusCode::PAYLOAD_TOO_LARGE), stall).await;
            return;
        }

        // The body, counted. Past `max` it ends in an error and the answer becomes 413.
        let exceeded = Arc::new(AtomicBool::new(false));
        let over = exceeded.clone();
        let (body_tx, body_rx) = tokio::sync::mpsc::channel::<Result<Bytes, std::io::Error>>(2);
        let pump = tokio::spawn(async move {
            let mut total = 0u64;
            loop {
                let chunk = match timeout(stall, rx.recv_data()).await {
                    Ok(Ok(Some(mut c))) => c.copy_to_bytes(c.remaining()),
                    Ok(Ok(None)) => return,
                    Ok(Err(e)) => {
                        let _ = body_tx
                            .send(Err(std::io::Error::other(e.to_string())))
                            .await;
                        return;
                    }
                    Err(_) => {
                        let _ = body_tx.send(Err(std::io::ErrorKind::TimedOut.into())).await;
                        return;
                    }
                };
                total += chunk.len() as u64;
                if total > max {
                    over.store(true, Ordering::Release);
                    let _ = body_tx
                        .send(Err(std::io::Error::other("request body is too large")))
                        .await;
                    return;
                }
                if body_tx.send(Ok(chunk)).await.is_err() {
                    return;
                }
            }
        });
        let body = Body::from_stream(futures::stream::unfold(body_rx, |mut rx| async move {
            rx.recv().await.map(|item| (item, rx))
        }));

        let (mut parts, ()) = req.into_parts();
        parts.extensions.insert(TcpConnectInfo {
            local_addr: Some(shared.local),
            remote_addr: Some(peer),
        });
        let response = match shared
            .router
            .clone()
            .oneshot(Request::from_parts(parts, body))
            .await
        {
            Ok(r) => r,
            Err(infallible) => match infallible {},
        };
        let response = if exceeded.load(Ordering::Acquire) {
            status_only(StatusCode::PAYLOAD_TOO_LARGE)
        } else {
            response
        };
        let _ = reply(&mut tx, response, stall).await;
        pump.abort();
    }

    fn status_only(status: StatusCode) -> Response<Body> {
        let mut r = Response::new(Body::empty());
        *r.status_mut() = status;
        r
    }

    /// Writes `response` to the stream: headers, body frames, trailers, then the end.
    async fn reply<S: ::h3::quic::SendStream<Bytes>>(
        tx: &mut ::h3::server::RequestStream<S, Bytes>,
        response: Response<Body>,
        stall: Duration,
    ) -> Result<(), ()> {
        let (mut parts, mut body) = response.into_parts();
        strip_hop_headers(&mut parts.headers);
        let head = Response::from_parts(parts, ());
        timeout(stall, tx.send_response(head))
            .await
            .map_err(|_| ())?
            .map_err(|_| ())?;
        loop {
            let frame = std::future::poll_fn(|cx| Pin::new(&mut body).poll_frame(cx)).await;
            match frame {
                None => break,
                Some(Ok(frame)) => match frame.into_data() {
                    Ok(data) => {
                        if data.is_empty() {
                            continue;
                        }
                        timeout(stall, tx.send_data(data))
                            .await
                            .map_err(|_| ())?
                            .map_err(|_| ())?;
                    }
                    Err(frame) => {
                        if let Ok(trailers) = frame.into_trailers() {
                            timeout(stall, tx.send_trailers(trailers))
                                .await
                                .map_err(|_| ())?
                                .map_err(|_| ())?;
                            return Ok(());
                        }
                    }
                },
                Some(Err(_)) => {
                    tx.stop_stream(::h3::error::Code::H3_INTERNAL_ERROR);
                    return Err(());
                }
            }
        }
        timeout(stall, tx.finish())
            .await
            .map_err(|_| ())?
            .map_err(|_| ())
    }

    /// Connection-specific fields do not exist in HTTP/3 (RFC 9114 section 4.2).
    fn strip_hop_headers(h: &mut HeaderMap) {
        for name in [
            "connection",
            "keep-alive",
            "proxy-connection",
            "transfer-encoding",
            "upgrade",
        ] {
            h.remove(name);
        }
    }
}
