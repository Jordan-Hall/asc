//! The h3 client against a local h3 server with a test CA.

use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use bytes::{Buf, Bytes};
use http::{HeaderMap, HeaderValue, Method, Request, Response};
use quinn::crypto::rustls::QuicServerConfig;
use rcgen::{BasicConstraints, CertificateParams, CertifiedIssuer, DnType, IsCa, KeyPair};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};

use super::*;

/// A test CA and a leaf for the names asked.
struct Pki {
    ca: CertificateDer<'static>,
    chain: Vec<CertificateDer<'static>>,
    key: PrivateKeyDer<'static>,
}

fn pki(names: &[&str]) -> Pki {
    let ca_key = KeyPair::generate().unwrap();
    let mut params = CertificateParams::new(Vec::<String>::new()).unwrap();
    params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    // Distinct names, or the leaf looks self-signed to a platform verifier.
    params
        .distinguished_name
        .push(DnType::CommonName, "asc h3 test ca");
    let ca = CertifiedIssuer::self_signed(params, ca_key).unwrap();
    let leaf_key = KeyPair::generate().unwrap();
    let mut leaf =
        CertificateParams::new(names.iter().map(|n| (*n).to_owned()).collect::<Vec<_>>()).unwrap();
    leaf.distinguished_name
        .push(DnType::CommonName, "asc h3 test leaf");
    let leaf = leaf.signed_by(&leaf_key, &ca).unwrap();
    Pki {
        ca: ca.der().clone(),
        chain: vec![leaf.der().clone()],
        key: PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(leaf_key.serialize_der())),
    }
}

/// What the test server counts.
#[derive(Default)]
struct Stats {
    connections: AtomicUsize,
    requests: AtomicUsize,
}

struct Server {
    port: u16,
    stats: Arc<Stats>,
    pki: Pki,
    _endpoint: quinn::Endpoint,
}

/// An h3 server on a UDP port of 127.0.0.1. `GET /redirect` answers 302, `/drop` resets the
/// stream without a response, `/big` answers 10 000 bytes, and anything else echoes
/// `<method> <path> <body length>`.
fn serve() -> Server {
    serve_for(&["127.0.0.1"])
}

/// A server whose certificate names `names`.
fn serve_for(names: &[&str]) -> Server {
    let pki = pki(names);
    let mut tls = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_protocol_versions(&[&rustls::version::TLS13])
    .unwrap()
    .with_no_client_auth()
    .with_single_cert(pki.chain.clone(), pki.key.clone_key())
    .unwrap();
    tls.alpn_protocols = vec![b"h3".to_vec()];
    let cfg = quinn::ServerConfig::with_crypto(Arc::new(QuicServerConfig::try_from(tls).unwrap()));
    let endpoint =
        quinn::Endpoint::server(cfg, SocketAddr::from((Ipv4Addr::LOCALHOST, 0))).unwrap();
    let port = endpoint.local_addr().unwrap().port();
    let stats = Arc::new(Stats::default());
    let (ep, st) = (endpoint.clone(), stats.clone());
    tokio::spawn(async move {
        while let Some(incoming) = ep.accept().await {
            let st = st.clone();
            tokio::spawn(async move {
                let Ok(conn) = incoming.await else { return };
                st.connections.fetch_add(1, Ordering::SeqCst);
                let mut h3 = ::h3::server::Connection::new(h3_quinn::Connection::new(conn))
                    .await
                    .unwrap();
                while let Ok(Some(resolver)) = h3.accept().await {
                    let st = st.clone();
                    tokio::spawn(async move {
                        let Ok((req, mut stream)) = resolver.resolve_request().await else {
                            return;
                        };
                        st.requests.fetch_add(1, Ordering::SeqCst);
                        let mut len = 0;
                        while let Ok(Some(chunk)) = stream.recv_data().await {
                            len += chunk.remaining();
                        }
                        let path = req.uri().path().to_owned();
                        if path == "/drop" {
                            return;
                        }
                        let (status, body) = match path.as_str() {
                            "/redirect" => (302, Bytes::new()),
                            "/big" => (200, Bytes::from(vec![7u8; 10_000])),
                            _ => (200, Bytes::from(format!("{} {path} {len}", req.method()))),
                        };
                        let resp = Response::builder()
                            .status(status)
                            .header("alt-svc", "h3=\":1\"; ma=1")
                            .body(())
                            .unwrap();
                        let _ = stream.send_response(resp).await;
                        let _ = stream.send_data(body).await;
                        let _ = stream.finish().await;
                    });
                }
            });
        }
    });
    Server {
        port,
        stats,
        pki,
        _endpoint: endpoint,
    }
}

fn limits() -> ClientLimits {
    ClientLimits {
        connect_timeout: Duration::from_millis(600),
        request_timeout: Duration::from_secs(10),
    }
}

/// A long handshake timeout, for tests that must show a rejection and not a timeout.
fn patient() -> ClientLimits {
    ClientLimits {
        connect_timeout: Duration::from_secs(5),
        ..limits()
    }
}

fn advert(port: u16) -> HeaderMap {
    let mut h = HeaderMap::new();
    h.insert(
        http::header::ALT_SVC,
        HeaderValue::from_str(&format!("h3=\":{port}\"; ma=3600")).unwrap(),
    );
    h
}

fn get(uri: &str) -> Request<Bytes> {
    Request::get(uri).body(Bytes::new()).unwrap()
}

/// An https origin whose TCP port nothing listens on: only its `Alt-Svc` matters here.
const ORIGIN: &str = "https://127.0.0.1:8443";

#[tokio::test]
async fn h3_upgrades_after_alt_svc_and_reuses_the_connection() {
    let s = serve();
    let up = Upgrader::with_extra_roots(vec![s.pki.ca.clone()], limits());
    let uri: Uri = format!("{ORIGIN}/x").parse().unwrap();

    // Nothing advertised yet: h2.
    assert!(matches!(
        up.send(&get(&uri.to_string()), 100).await,
        Attempt::Fallback
    ));
    assert_eq!(s.stats.requests.load(Ordering::SeqCst), 0);

    up.note_response(&uri, &advert(s.port));
    assert!(up.would_try(&uri));
    let Attempt::Done(r) = up.send(&get(&uri.to_string()), 100).await else {
        panic!("expected h3");
    };
    assert_eq!(r.status, 200);
    assert_eq!(&r.body[..], b"GET /x 0");
    let put = Request::builder()
        .method(Method::PUT)
        .uri(&uri)
        .body(Bytes::from_static(b"hello"))
        .unwrap();
    let Attempt::Done(r) = up.send(&put, 100).await else {
        panic!("expected h3");
    };
    assert_eq!(&r.body[..], b"PUT /x 5");
    assert_eq!(
        s.stats.connections.load(Ordering::SeqCst),
        1,
        "one connection is reused"
    );
    // The server's own `Alt-Svc` (a different port) replaced ours: it is the origin's word.
    assert_eq!(
        up.cache.lookup(&Origin::of(&uri).unwrap(), Instant::now()),
        Some(1)
    );
}

#[tokio::test]
async fn h3_falls_back_and_remembers_when_udp_is_blocked() {
    // A UDP socket that never answers: the handshake gets no reply.
    let silent = std::net::UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let port = silent.local_addr().unwrap().port();
    let up = Upgrader::with_extra_roots(Vec::new(), limits());
    let uri: Uri = format!("{ORIGIN}/x").parse().unwrap();
    up.note_response(&uri, &advert(port));

    let t = Instant::now();
    assert!(matches!(
        up.send(&get(&uri.to_string()), 100).await,
        Attempt::Fallback
    ));
    assert!(t.elapsed() < Duration::from_secs(5));
    // Cached: no second handshake wait, and no more h3 tries for this origin.
    assert!(!up.would_try(&uri));
    let t = Instant::now();
    assert!(matches!(
        up.send(&get(&uri.to_string()), 100).await,
        Attempt::Fallback
    ));
    assert!(t.elapsed() < Duration::from_millis(100));
    // A fresh advertisement does not end the backoff.
    up.note_response(&uri, &advert(port));
    assert!(!up.would_try(&uri));
}

#[tokio::test]
async fn h3_ignores_alt_svc_that_names_another_host() {
    let s = serve();
    let up = Upgrader::with_extra_roots(vec![s.pki.ca.clone()], limits());
    let uri: Uri = format!("{ORIGIN}/x").parse().unwrap();
    let mut h = HeaderMap::new();
    h.insert(
        http::header::ALT_SVC,
        HeaderValue::from_str(&format!("h3=\"evil.example:{}\"; ma=3600", s.port)).unwrap(),
    );
    up.note_response(&uri, &h);
    assert!(!up.would_try(&uri));
    assert!(matches!(
        up.send(&get(&uri.to_string()), 100).await,
        Attempt::Fallback
    ));
    assert_eq!(s.stats.connections.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn h3_is_never_tried_for_plain_http() {
    let s = serve();
    let up = Upgrader::with_extra_roots(vec![s.pki.ca.clone()], limits());
    let uri: Uri = "http://127.0.0.1:8080/x".parse().unwrap();
    up.note_response(&uri, &advert(s.port));
    assert!(!up.would_try(&uri));
    assert!(matches!(
        up.send(&get(&uri.to_string()), 100).await,
        Attempt::Fallback
    ));
    assert_eq!(s.stats.connections.load(Ordering::SeqCst), 0);
    assert!(up.cache.is_empty());
}

#[tokio::test]
async fn h3_does_not_trust_a_certificate_the_platform_does_not() {
    let s = serve();
    // No extra roots: the test CA is unknown, so the handshake must fail, not proceed.
    let up = Upgrader::new(patient());
    let uri: Uri = format!("{ORIGIN}/x").parse().unwrap();
    up.note_response(&uri, &advert(s.port));
    let t = Instant::now();
    assert!(matches!(
        up.send(&get(&uri.to_string()), 100).await,
        Attempt::Fallback
    ));
    assert!(
        t.elapsed() < Duration::from_secs(3),
        "rejected by the verifier, not by the timeout"
    );
    assert_eq!(
        s.stats.requests.load(Ordering::SeqCst),
        0,
        "no request crossed the failed handshake"
    );
    assert!(!up.would_try(&uri), "the failure is cached");
}

#[tokio::test]
async fn h3_checks_the_host_name_in_the_certificate() {
    // Trusted CA, reachable server, but the certificate is for `localhost` and the origin is an
    // IP address: the name does not match, so the handshake must fail.
    let s = serve_for(&["localhost"]);
    let up = Upgrader::with_extra_roots(vec![s.pki.ca.clone()], patient());
    let uri: Uri = format!("{ORIGIN}/x").parse().unwrap();
    up.note_response(&uri, &advert(s.port));
    let t = Instant::now();
    assert!(matches!(
        up.send(&get(&uri.to_string()), 100).await,
        Attempt::Fallback
    ));
    assert!(
        t.elapsed() < Duration::from_secs(3),
        "rejected by the verifier, not by the timeout"
    );
    assert_eq!(s.stats.requests.load(Ordering::SeqCst), 0);
    assert!(
        !up.would_try(&uri),
        "refused for the name, cached as a failure"
    );
    // Not a timeout: the server was reached and its certificate was rejected.
    assert_eq!(
        s.stats.connections.load(Ordering::SeqCst),
        0,
        "no connection was established"
    );
}

#[tokio::test]
async fn h3_leaves_redirects_to_h2_without_a_failure() {
    let s = serve();
    let up = Upgrader::with_extra_roots(vec![s.pki.ca.clone()], limits());
    let uri: Uri = format!("{ORIGIN}/redirect").parse().unwrap();
    up.note_response(&uri, &advert(s.port));
    assert!(matches!(
        up.send(&get(&uri.to_string()), 100).await,
        Attempt::Fallback
    ));
    assert!(up.would_try(&uri), "a redirect is not a failure");
}

#[tokio::test]
async fn h3_does_not_replay_a_non_idempotent_request_that_may_have_run() {
    let s = serve();
    let up = Upgrader::with_extra_roots(vec![s.pki.ca.clone()], limits());
    let uri: Uri = format!("{ORIGIN}/drop").parse().unwrap();
    up.note_response(&uri, &advert(s.port));
    let post = Request::post(&uri).body(Bytes::from_static(b"x")).unwrap();
    assert!(matches!(up.send(&post, 100).await, Attempt::Failed(_)));
    assert_eq!(
        s.stats.requests.load(Ordering::SeqCst),
        1,
        "sent once, not retried"
    );

    // The same failure for an idempotent method goes back to h2.
    let up = Upgrader::with_extra_roots(vec![s.pki.ca.clone()], limits());
    up.note_response(&uri, &advert(s.port));
    assert!(matches!(
        up.send(&get(&uri.to_string()), 100).await,
        Attempt::Fallback
    ));
}

#[tokio::test]
async fn h3_bounds_the_response_body() {
    let s = serve();
    let up = Upgrader::with_extra_roots(vec![s.pki.ca.clone()], limits());
    let uri: Uri = format!("{ORIGIN}/big").parse().unwrap();
    up.note_response(&uri, &advert(s.port));
    let Attempt::Done(r) = up.send(&get(&uri.to_string()), 1000).await else {
        panic!("expected h3");
    };
    assert!(r.truncated);
    assert_eq!(r.body.len(), 1000);
    let Attempt::Done(r) = up.send(&get(&uri.to_string()), 20_000).await else {
        panic!("expected h3");
    };
    assert!(!r.truncated);
    assert_eq!(r.body.len(), 10_000);
}

#[test]
fn h3_client_config_is_tls13_h3_without_early_data() {
    let cfg = rustls_config(Vec::new()).unwrap();
    assert!(!cfg.enable_early_data, "0-RTT data can be replayed");
    assert_eq!(cfg.alpn_protocols, [b"h3".to_vec()]);
}

#[test]
fn h3_only_idempotent_methods_are_replayed() {
    for m in [
        Method::GET,
        Method::HEAD,
        Method::PUT,
        Method::DELETE,
        Method::OPTIONS,
    ] {
        assert!(is_idempotent(&m), "{m}");
    }
    for m in [Method::POST, Method::PATCH, Method::CONNECT] {
        assert!(!is_idempotent(&m), "{m}");
    }
}
