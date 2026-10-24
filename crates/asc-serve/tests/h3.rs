//! U89b: the HTTP/3 listener. The same router answers over QUIC with the same credential edge and
//! admission as over TCP, and the transport's own limits hold. Each test talks to a real listener
//! on the loopback interface with a test CA.
#![cfg(feature = "http3")]

use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::http::{HeaderMap, Method, Request, StatusCode, header};
use bytes::{Buf, Bytes};
use asc_rpc::auth::{Access, StaticTokens};
use asc_rpc::server::ServerInfo;
use asc_serve::config::{ByteSize, Limits, Secs, ServeConfig, Tls, TurboConfig};
use asc_serve::h3_listener::{self, H3LimitsConfig};
use asc_serve::http::Metrics;
use asc_serve::limits::{Admission, guarded_routes};
use asc_serve::registry::Extensions;
use asc_serve::turbo::TurboState;
use asc_serve::{build_routes, turbo};
use object_store::memory::InMemory;
use object_store::path::Path as StorePath;
use quinn::crypto::rustls::QuicClientConfig;
use rcgen::{BasicConstraints, CertificateParams, CertifiedIssuer, DnType, IsCa, KeyPair};
use rustls::pki_types::CertificateDer;
use tokio::time::timeout;

const TOKEN: &str = "ci-secret-token";
const WAIT: Duration = Duration::from_secs(10);

type Send = ::h3::client::SendRequest<h3_quinn::OpenStreams, Bytes>;

struct Pki {
    ca: CertificateDer<'static>,
    cert_pem: String,
    key_pem: String,
}

fn pki() -> Pki {
    let ca_key = KeyPair::generate().unwrap();
    let mut params = CertificateParams::new(Vec::<String>::new()).unwrap();
    params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    // Distinct names, or the leaf looks self-signed to a platform verifier.
    params
        .distinguished_name
        .push(DnType::CommonName, "asc h3 test ca");
    let ca = CertifiedIssuer::self_signed(params, ca_key).unwrap();
    let leaf_key = KeyPair::generate().unwrap();
    let mut leaf = CertificateParams::new(vec!["127.0.0.1".to_owned()]).unwrap();
    leaf.distinguished_name
        .push(DnType::CommonName, "asc h3 test leaf");
    let leaf = leaf.signed_by(&leaf_key, &ca).unwrap();
    Pki {
        ca: ca.der().clone(),
        cert_pem: leaf.pem(),
        key_pem: leaf_key.serialize_pem(),
    }
}

fn lo() -> SocketAddr {
    SocketAddr::from((Ipv4Addr::LOCALHOST, 0))
}

/// The public routes of a server: the turbo cache behind the credential edge.
fn public() -> Router {
    let ext = Extensions::new();
    let mem = Arc::new(InMemory::new());
    let auth = Arc::new(StaticTokens::new().with_token(TOKEN, "ci", Access::Write));
    let state = TurboState::new(
        auth.clone(),
        mem,
        StorePath::from("root"),
        TurboConfig::default(),
        Arc::new(Metrics::new()),
    )
    .with_trusted_token(TOKEN);
    turbo::register(&ext, Arc::new(state));
    build_routes(&ext, auth, ServerInfo::default()).into_axum_router()
}

fn listener(
    pki: &Pki,
    router: Router,
    limits: H3LimitsConfig,
) -> (h3_listener::Handle, SocketAddr) {
    let ep = h3_listener::bind(
        pki.cert_pem.as_bytes(),
        pki.key_pem.as_bytes(),
        lo(),
        &limits,
    )
    .unwrap();
    let h = h3_listener::spawn(ep, router, limits);
    let addr = h.local_addr().unwrap();
    (h, addr)
}

fn client_tls(ca: &CertificateDer<'static>, early_data: bool) -> quinn::ClientConfig {
    let mut roots = rustls::RootCertStore::empty();
    roots.add(ca.clone()).unwrap();
    let mut tls = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_protocol_versions(&[&rustls::version::TLS13])
    .unwrap()
    .with_root_certificates(roots)
    .with_no_client_auth();
    tls.alpn_protocols = vec![b"h3".to_vec()];
    tls.enable_early_data = early_data;
    quinn::ClientConfig::new(Arc::new(QuicClientConfig::try_from(tls).unwrap()))
}

struct Conn {
    quic: quinn::Connection,
    send: Send,
    _ep: quinn::Endpoint,
}

async fn connect(addr: SocketAddr, ca: &CertificateDer<'static>) -> Result<Conn, String> {
    let ep = quinn::Endpoint::client(lo()).unwrap();
    let quic = timeout(
        WAIT,
        ep.connect_with(client_tls(ca, false), addr, "127.0.0.1")
            .unwrap(),
    )
    .await
    .map_err(|_| "timed out".to_owned())?
    .map_err(|e| e.to_string())?;
    let (mut driver, send) = ::h3::client::new(h3_quinn::Connection::new(quic.clone()))
        .await
        .map_err(|e| e.to_string())?;
    tokio::spawn(async move {
        let _ = std::future::poll_fn(|cx| driver.poll_close(cx)).await;
    });
    Ok(Conn {
        quic,
        send,
        _ep: ep,
    })
}

/// A request and what came back: status, headers, body.
async fn call(
    send: &mut Send,
    method: Method,
    path: &str,
    headers: &[(&str, String)],
    body: Option<Bytes>,
) -> Result<(StatusCode, HeaderMap, Bytes), String> {
    let mut req = Request::builder()
        .method(method)
        .uri(format!("https://127.0.0.1{path}"));
    for (k, v) in headers {
        req = req.header(*k, v);
    }
    let req = req.body(()).unwrap();
    timeout(WAIT, async {
        let mut s = send.send_request(req).await.map_err(|e| e.to_string())?;
        if let Some(b) = body {
            s.send_data(b).await.map_err(|e| e.to_string())?;
        }
        s.finish().await.map_err(|e| e.to_string())?;
        let (parts, ()) = s
            .recv_response()
            .await
            .map_err(|e| e.to_string())?
            .into_parts();
        let mut out = Vec::new();
        while let Some(mut c) = s.recv_data().await.map_err(|e| e.to_string())? {
            out.extend_from_slice(&c.copy_to_bytes(c.remaining()));
        }
        Ok((parts.status, parts.headers, Bytes::from(out)))
    })
    .await
    .map_err(|_| "timed out".to_owned())?
}

fn bearer() -> (&'static str, String) {
    ("authorization", format!("Bearer {TOKEN}"))
}

#[tokio::test]
async fn h3_serves_the_routes_behind_the_same_credential_edge() {
    let p = pki();
    let (_h, addr) = listener(&p, public(), H3LimitsConfig::default());
    let mut c = connect(addr, &p.ca).await.unwrap();
    let path = "/v8/artifacts/abc123?slug=acme.web&teamId=team_x";

    let (status, headers, _) = call(&mut c.send, Method::GET, path, &[], None)
        .await
        .unwrap();
    assert_eq!(status, StatusCode::UNAUTHORIZED, "no credential, no entry");
    assert!(headers.contains_key(header::WWW_AUTHENTICATE));

    let (status, ..) = call(
        &mut c.send,
        Method::GET,
        path,
        &[("authorization", "Bearer wrong".to_owned())],
        None,
    )
    .await
    .unwrap();
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    let (status, ..) = call(&mut c.send, Method::GET, path, &[bearer()], None)
        .await
        .unwrap();
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, ..) = call(
        &mut c.send,
        Method::PUT,
        path,
        &[bearer()],
        Some(Bytes::from_static(b"artifact bytes")),
    )
    .await
    .unwrap();
    assert_eq!(status, StatusCode::ACCEPTED, "a body crosses h3");
    let (status, _, body) = call(&mut c.send, Method::GET, path, &[bearer()], None)
        .await
        .unwrap();
    assert_eq!(status, StatusCode::OK);
    assert_eq!(&body[..], b"artifact bytes");
}

#[tokio::test]
async fn h3_runs_admission_and_names_the_peer() {
    let p = pki();
    let ext = Extensions::new();
    let limits = Limits {
        cheap_burst: 2,
        cheap_rps: 1,
        ..Limits::default()
    };
    let admission = Arc::new(Admission::new(&limits));
    let routes = guarded_routes(
        &ext,
        Arc::new(asc_serve::DenyAll),
        admission.clone(),
        ServerInfo::default(),
    );
    let (_h, addr) = listener(&p, routes.into_axum_router(), H3LimitsConfig::default());
    let mut c = connect(addr, &p.ca).await.unwrap();
    let mut refused = 0;
    for _ in 0..6 {
        let (_, headers, _) = call(
            &mut c.send,
            Method::POST,
            "/asc.v1.Repo/Version",
            &[("content-type", "application/grpc".to_owned())],
            Some(Bytes::from_static(&[0, 0, 0, 0, 0])),
        )
        .await
        .unwrap();
        // gRPC code 8 is RESOURCE_EXHAUSTED.
        if headers.get("grpc-status").is_some_and(|s| s == "8") {
            refused += 1;
        }
    }
    assert_eq!(
        refused, 4,
        "all but the burst of 2 are refused, as over TCP"
    );
    assert_eq!(
        admission.unknown_peers(),
        0,
        "every h3 request carries its peer address"
    );
}

#[tokio::test]
async fn h3_start_serves_only_with_tls_and_a_port_and_sends_alt_svc() {
    let p = pki();
    let dir = tempfile::tempdir().unwrap();
    let (cert, key) = (dir.path().join("c.pem"), dir.path().join("k.pem"));
    std::fs::write(&cert, &p.cert_pem).unwrap();
    std::fs::write(&key, &p.key_pem).unwrap();
    let routes = || {
        build_routes(
            &Extensions::new(),
            Arc::new(asc_serve::DenyAll),
            ServerInfo::default(),
        )
    };
    let tls = Tls {
        cert: cert.clone(),
        key: key.clone(),
    };
    let cfg = |tls: Option<&Tls>, port: Option<u16>| ServeConfig {
        listen: lo(),
        tls: tls.cloned(),
        http3_port: port,
        ..ServeConfig::default()
    };

    assert!(
        h3_listener::start(&cfg(None, None), &routes())
            .unwrap()
            .is_none()
    );
    assert!(
        h3_listener::start(&cfg(Some(&tls), None), &routes())
            .unwrap()
            .is_none(),
        "no port"
    );
    assert!(
        h3_listener::start(&cfg(None, Some(4433)), &routes())
            .unwrap()
            .is_none(),
        "no [tls]"
    );

    let h = h3_listener::start(&cfg(Some(&tls), Some(0)), &routes())
        .unwrap()
        .expect("listening");
    let mut c = connect(h.local_addr().unwrap(), &p.ca).await.unwrap();
    let (status, headers, _) = call(&mut c.send, Method::GET, "/healthz", &[], None)
        .await
        .unwrap();
    assert_ne!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(
        headers.get(header::ALT_SVC).unwrap(),
        &asc_serve::alt_svc::header_value(0),
        "h3 responses keep the Alt-Svc of the TCP ones"
    );
}

#[tokio::test]
async fn h3_a_client_that_trusts_the_root_upgrades_through_alt_svc() {
    let p = pki();
    let (_h, addr) = listener(&p, public(), H3LimitsConfig::default());
    let up = asc_rpc::h3::Upgrader::with_extra_roots(vec![p.ca.clone()], Default::default());
    let uri: axum::http::Uri = "https://127.0.0.1:8443/v8/artifacts/zzz?slug=acme.web&teamId=t"
        .parse()
        .unwrap();
    let mut adv = HeaderMap::new();
    adv.insert(
        header::ALT_SVC,
        format!("h3=\":{}\"; ma=60", addr.port()).parse().unwrap(),
    );
    up.note_response(&uri, &adv);
    let req = Request::get(&uri)
        .header("authorization", format!("Bearer {TOKEN}"))
        .body(Bytes::new())
        .unwrap();
    match up.send(&req, 1000).await {
        asc_rpc::h3::Attempt::Done(r) => assert_eq!(r.status, StatusCode::NOT_FOUND),
        other => panic!("expected h3, got {other:?}"),
    }
}

fn limits(f: impl FnOnce(&mut H3LimitsConfig)) -> H3LimitsConfig {
    let mut l = H3LimitsConfig::default();
    f(&mut l);
    l
}

/// A router with one route that echoes the number of body bytes it read.
fn counter() -> Router {
    counting().0
}

/// The same, with the number of times the handler ran.
fn counting() -> (Router, Arc<std::sync::atomic::AtomicUsize>) {
    let ran = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let seen = ran.clone();
    let router = Router::new().route(
        "/len",
        axum::routing::post(move |body: Body| {
            seen.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            async move {
                match axum::body::to_bytes(body, usize::MAX).await {
                    Ok(b) => (StatusCode::OK, b.len().to_string()),
                    Err(_) => (StatusCode::BAD_REQUEST, "body error".to_owned()),
                }
            }
        }),
    );
    (router, ran)
}

#[tokio::test]
async fn h3_request_body_limit_is_enforced() {
    let p = pki();
    let (router, ran) = counting();
    let (_h, addr) = listener(&p, router, limits(|l| l.max_request_bytes = ByteSize(1000)));
    let mut c = connect(addr, &p.ca).await.unwrap();
    let ok = Bytes::from(vec![1u8; 900]);
    let (status, _, body) = call(&mut c.send, Method::POST, "/len", &[], Some(ok))
        .await
        .unwrap();
    assert_eq!((status, &body[..]), (StatusCode::OK, &b"900"[..]));
    assert_eq!(ran.load(std::sync::atomic::Ordering::SeqCst), 1);

    // Declared too large: refused before the handler runs.
    let big = Bytes::from(vec![1u8; 5000]);
    let (status, ..) = call(
        &mut c.send,
        Method::POST,
        "/len",
        &[("content-length", "5000".to_owned())],
        Some(big.clone()),
    )
    .await
    .unwrap();
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(
        ran.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "a declared oversize body never reaches the handler"
    );

    // No declared length: counted as it arrives.
    let (status, ..) = call(&mut c.send, Method::POST, "/len", &[], Some(big))
        .await
        .unwrap();
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
}

#[tokio::test]
async fn h3_max_connections_refuses_the_next_one() {
    let p = pki();
    let (_h, addr) = listener(&p, counter(), limits(|l| l.max_connections = 1));
    let first = connect(addr, &p.ca).await.unwrap();
    assert!(
        connect(addr, &p.ca).await.is_err(),
        "the second connection is refused"
    );
    drop(first);
    // The slot frees when the first connection ends.
    let mut ok = false;
    for _ in 0..40 {
        if connect(addr, &p.ca).await.is_ok() {
            ok = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(ok, "a freed slot admits a new connection");
}

#[tokio::test]
async fn h3_max_streams_limits_concurrent_requests_per_connection() {
    let p = pki();
    let gate = Arc::new(tokio::sync::Notify::new());
    let g = gate.clone();
    let router = Router::new()
        .route(
            "/hold",
            axum::routing::get(move || {
                let g = g.clone();
                async move {
                    g.notified().await;
                    "done"
                }
            }),
        )
        .route("/fast", axum::routing::get(|| async { "fast" }));
    let (_h, addr) = listener(&p, router, limits(|l| l.max_streams = 1));
    let c = connect(addr, &p.ca).await.unwrap();
    let (mut a, mut b) = (c.send.clone(), c.send.clone());
    let first = tokio::spawn(async move { call(&mut a, Method::GET, "/hold", &[], None).await });
    tokio::time::sleep(Duration::from_millis(300)).await;
    let mut second =
        tokio::spawn(async move { call(&mut b, Method::GET, "/fast", &[], None).await });
    // The second stream cannot even open while the first is in flight, though its route is instant.
    assert!(
        timeout(Duration::from_millis(700), &mut second)
            .await
            .is_err(),
        "a second concurrent stream was admitted past max_streams = 1"
    );
    gate.notify_waiters();
    assert_eq!(first.await.unwrap().unwrap().0, StatusCode::OK);
    assert_eq!(second.await.unwrap().unwrap().0, StatusCode::OK);
}

#[tokio::test]
async fn h3_idle_timeout_closes_a_silent_connection() {
    let p = pki();
    let (_h, addr) = listener(&p, counter(), limits(|l| l.idle_timeout = Secs(1)));
    let c = connect(addr, &p.ca).await.unwrap();
    let closed = timeout(Duration::from_secs(6), c.quic.closed()).await;
    assert!(
        matches!(closed, Ok(quinn::ConnectionError::TimedOut)),
        "an idle connection must time out, got {closed:?}"
    );
}

#[tokio::test]
async fn h3_header_block_limit_is_enforced() {
    let p = pki();
    let (_h, addr) = listener(
        &p,
        counter(),
        limits(|l| l.max_header_bytes = ByteSize(2048)),
    );
    let mut c = connect(addr, &p.ca).await.unwrap();
    let r = call(
        &mut c.send,
        Method::POST,
        "/len",
        &[("x-big", "a".repeat(10_000))],
        Some(Bytes::from_static(b"x")),
    )
    .await;
    assert!(
        r.as_ref().is_err() || r.as_ref().is_ok_and(|(s, ..)| *s != StatusCode::OK),
        "an oversized header block must not reach the handler: {r:?}"
    );
}

#[tokio::test]
async fn h3_a_stalled_upload_is_cut_off() {
    let p = pki();
    let (_h, addr) = listener(&p, counter(), limits(|l| l.stall_timeout = Secs(1)));
    let c = connect(addr, &p.ca).await.unwrap();
    let mut send = c.send.clone();
    let req = Request::post("https://127.0.0.1/len").body(()).unwrap();
    let mut s = send.send_request(req).await.unwrap();
    s.send_data(Bytes::from_static(b"some")).await.unwrap();
    // No more bytes and no end of stream.
    let got = timeout(Duration::from_secs(6), async {
        let resp = s.recv_response().await?;
        Ok::<_, ::h3::error::StreamError>(resp.status())
    })
    .await;
    match got {
        Ok(Ok(status)) => assert_eq!(status, StatusCode::BAD_REQUEST),
        Ok(Err(_)) => {}
        Err(_) => panic!("a stalled upload held its stream open"),
    }
}

#[tokio::test]
async fn h3_does_not_accept_early_data() {
    let p = pki();
    let (_h, addr) = listener(&p, counter(), H3LimitsConfig::default());
    let ep = quinn::Endpoint::client(lo()).unwrap();
    let cfg = client_tls(&p.ca, true);
    // The first connection leaves a resumption ticket with the client.
    let first = ep
        .connect_with(cfg.clone(), addr, "127.0.0.1")
        .unwrap()
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    first.close(0u32.into(), b"done");
    tokio::time::sleep(Duration::from_millis(100)).await;
    // The second would send 0-RTT data if the server allowed it.
    match ep.connect_with(cfg, addr, "127.0.0.1").unwrap().into_0rtt() {
        Ok(_) => panic!("the server offered 0-RTT"),
        Err(connecting) => {
            connecting.await.unwrap();
        }
    }
}
