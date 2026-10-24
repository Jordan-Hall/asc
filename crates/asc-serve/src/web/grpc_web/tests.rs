use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use tower::ServiceExt;

use super::*;
use crate::config::ByteSize;

/// A body that yields prepared frames.
struct Frames(VecDeque<Frame<Bytes>>);

impl http_body::Body for Frames {
    type Data = Bytes;
    type Error = axum::Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        _: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, axum::Error>>> {
        Poll::Ready(self.0.pop_front().map(Ok))
    }
}

fn trailers(code: i32, message: &str) -> HeaderMap {
    let mut t = HeaderMap::new();
    t.insert(
        "grpc-status",
        HeaderValue::from_str(&code.to_string()).unwrap(),
    );
    if !message.is_empty() {
        t.insert("grpc-message", HeaderValue::from_str(message).unwrap());
    }
    t
}

/// What the tonic services answer: headers, length-prefixed messages, then trailers.
fn native(messages: &[&[u8]], code: i32) -> Reply {
    let mut frames: VecDeque<_> = messages.iter().map(|m| Frame::data(frame(0, m))).collect();
    frames.push_back(Frame::trailers(trailers(code, "")));
    let mut res = Response::new(Body::new(Frames(frames)));
    res.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/grpc"),
    );
    res
}

/// A status with no message: the status rides in the headers.
fn trailers_only(code: i32, message: &str) -> Reply {
    let mut res = Response::new(Body::empty());
    let h = res.headers_mut();
    h.extend(trailers(code, message));
    h.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/grpc"),
    );
    res
}

type Seen = Vec<(HeaderMap, Bytes)>;

async fn run(
    cfg: &WebConfig,
    reply: impl Fn() -> Reply + Clone + Send + 'static,
    req: Request<Body>,
) -> (Reply, Seen) {
    let seen: Arc<Mutex<Seen>> = Arc::default();
    let log = seen.clone();
    let inner = tower::service_fn(move |req: Request<Body>| {
        let reply = reply.clone();
        let log = log.clone();
        async move {
            let (parts, body) = req.into_parts();
            let bytes = axum::body::to_bytes(body, usize::MAX)
                .await
                .unwrap_or_default();
            log.lock().unwrap().push((parts.headers, bytes));
            Ok::<_, Infallible>(reply())
        }
    });
    let res = GrpcWebLayer::new(cfg, Some("https://public.example"))
        .layer(inner)
        .oneshot(req)
        .await
        .unwrap();
    let seen = seen.lock().unwrap().clone();
    (res, seen)
}

fn cfg() -> WebConfig {
    WebConfig {
        enabled: true,
        cors_origins: vec!["https://app.example".to_owned()],
        ..WebConfig::default()
    }
}

fn post(content_type: &str, body: Bytes, headers: &[(&str, &str)]) -> Request<Body> {
    let mut b = Request::post("/asc.v1.Repo/Version")
        .header(header::HOST, "asc.example")
        .header(header::CONTENT_TYPE, content_type);
    for (k, v) in headers {
        b = b.header(*k, *v);
    }
    b.body(Body::from(body)).unwrap()
}

fn web_post(headers: &[(&str, &str)]) -> Request<Body> {
    post("application/grpc-web+proto", frame(0, b"req"), headers)
}

async fn bytes(res: Reply) -> Bytes {
    axum::body::to_bytes(res.into_body(), usize::MAX)
        .await
        .unwrap()
}

fn header_of<'a>(res: &'a Reply, name: &str) -> Option<&'a str> {
    res.headers().get(name).and_then(|v| v.to_str().ok())
}

#[tokio::test]
async fn unary_gets_a_trailer_frame_and_the_inner_sees_native_grpc() {
    let (res, seen) = run(
        &cfg(),
        || native(&[b"reply"], 0),
        web_post(&[("authorization", "Bearer t"), ("cookie", "sid=1")]),
    )
    .await;
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(
        header_of(&res, "content-type"),
        Some("application/grpc-web+proto")
    );
    let mut want = frame(0, b"reply").to_vec();
    want.extend_from_slice(&frame(0x80, b"grpc-status: 0\r\n"));
    assert_eq!(bytes(res).await.as_ref(), want.as_slice());

    let (h, body) = &seen[0];
    assert_eq!(h["content-type"], "application/grpc");
    assert_eq!(h["te"], "trailers");
    assert_eq!(h["authorization"], "Bearer t");
    assert!(!h.contains_key("cookie"), "a cookie is never forwarded");
    assert_eq!(body.as_ref(), frame(0, b"req").as_ref());
}

#[tokio::test]
async fn server_streaming_replies_keep_every_message_in_order() {
    let (res, _) = run(&cfg(), || native(&[b"a", b"bb", b"ccc"], 0), web_post(&[])).await;
    let mut want = Vec::new();
    for m in [&b"a"[..], b"bb", b"ccc"] {
        want.extend_from_slice(&frame(0, m));
    }
    want.extend_from_slice(&frame(0x80, b"grpc-status: 0\r\n"));
    assert_eq!(bytes(res).await.as_ref(), want.as_slice());
}

#[tokio::test]
async fn a_status_only_reply_becomes_a_trailer_frame() {
    let (res, _) = run(&cfg(), || trailers_only(16, "bad%20token"), web_post(&[])).await;
    assert_eq!(res.status(), StatusCode::OK);
    let body = bytes(res).await;
    assert_eq!(body[0], 0x80);
    let text = String::from_utf8_lossy(&body[5..]).into_owned();
    assert!(text.contains("grpc-status: 16\r\n"), "{text}");
    assert!(text.contains("grpc-message: bad%20token\r\n"), "{text}");
}

#[tokio::test]
async fn same_origin_needs_no_cors_headers() {
    for origin in ["https://asc.example", "https://public.example"] {
        let (res, seen) = run(&cfg(), || native(&[], 0), web_post(&[("origin", origin)])).await;
        assert_eq!(res.status(), StatusCode::OK, "{origin}");
        assert!(res.headers().get("access-control-allow-origin").is_none());
        assert_eq!(seen.len(), 1);
    }
}

#[tokio::test]
async fn a_configured_origin_is_echoed_without_credentials() {
    let (res, _) = run(
        &cfg(),
        || native(&[], 0),
        web_post(&[("origin", "https://app.example")]),
    )
    .await;
    assert_eq!(
        header_of(&res, "access-control-allow-origin"),
        Some("https://app.example")
    );
    assert!(
        header_of(&res, "access-control-expose-headers")
            .unwrap()
            .contains("grpc-status")
    );
    assert!(
        res.headers()
            .get("access-control-allow-credentials")
            .is_none()
    );
    assert!(header_of(&res, "vary").unwrap().contains("origin"));
    assert!(res.headers().get("set-cookie").is_none());
}

#[tokio::test]
async fn any_other_origin_is_refused_before_the_service_runs() {
    for origin in ["https://evil.example", "null", "http://app.example", "*"] {
        let (res, seen) = run(&cfg(), || native(&[], 0), web_post(&[("origin", origin)])).await;
        assert_eq!(res.status(), StatusCode::FORBIDDEN, "{origin}");
        assert!(res.headers().get("access-control-allow-origin").is_none());
        assert!(seen.is_empty(), "{origin} reached the service");
    }
}

#[tokio::test]
async fn with_no_configured_origins_only_the_own_origin_passes() {
    let cfg = WebConfig {
        enabled: true,
        ..WebConfig::default()
    };
    let (res, _) = run(
        &cfg,
        || native(&[], 0),
        web_post(&[("origin", "https://app.example")]),
    )
    .await;
    assert_eq!(res.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn preflight_answers_configured_origins_only() {
    let pre = |origin: &str| {
        Request::options("/asc.v1.Repo/Version")
            .header(header::HOST, "asc.example")
            .header("origin", origin)
            .header("access-control-request-method", "POST")
            .body(Body::empty())
            .unwrap()
    };
    let (res, seen) = run(&cfg(), || native(&[], 0), pre("https://app.example")).await;
    assert_eq!(res.status(), StatusCode::NO_CONTENT);
    assert_eq!(
        header_of(&res, "access-control-allow-origin"),
        Some("https://app.example")
    );
    assert_eq!(
        header_of(&res, "access-control-allow-methods"),
        Some("POST")
    );
    assert!(
        header_of(&res, "access-control-allow-headers")
            .unwrap()
            .contains("authorization")
    );
    assert!(
        res.headers()
            .get("access-control-allow-credentials")
            .is_none()
    );
    assert!(seen.is_empty());

    let (res, seen) = run(&cfg(), || native(&[], 0), pre("https://evil.example")).await;
    assert_eq!(res.status(), StatusCode::FORBIDDEN);
    assert!(res.headers().get("access-control-allow-origin").is_none());
    assert!(seen.is_empty());
}

#[tokio::test]
async fn a_request_over_the_cap_gets_resource_exhausted() {
    let cfg = WebConfig {
        max_request: ByteSize(8),
        ..cfg()
    };
    let big = post(
        "application/grpc-web+proto",
        frame(0, &[0; 64]),
        &[("content-length", "69")],
    );
    let (res, seen) = run(&cfg, || native(&[], 0), big).await;
    assert!(seen.is_empty());
    let text = String::from_utf8_lossy(&bytes(res).await).into_owned();
    assert!(text.contains("grpc-status: 8"), "{text}");
}

#[tokio::test]
async fn a_streamed_body_over_the_cap_is_cut_off() {
    let cfg = WebConfig {
        max_request: ByteSize(8),
        ..cfg()
    };
    let chunks =
        futures::stream::iter((0..3).map(|_| Ok::<_, std::io::Error>(Bytes::from(vec![0; 6]))));
    let req = Request::post("/asc.v1.Repo/Version")
        .header(header::CONTENT_TYPE, "application/grpc-web+proto")
        .body(Body::from_stream(chunks))
        .unwrap();
    let (_, seen) = run(&cfg, || native(&[], 0), req).await;
    // The read failed past the cap, so the service saw no complete body.
    assert!(seen[0].1.is_empty());
}

#[tokio::test]
async fn the_text_variant_is_not_served() {
    let (res, seen) = run(
        &cfg(),
        || native(&[], 0),
        post("application/grpc-web-text", Bytes::new(), &[]),
    )
    .await;
    assert_eq!(res.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);
    assert!(seen.is_empty());
}

#[tokio::test]
async fn native_grpc_and_other_paths_pass_through_untouched() {
    let (res, seen) = run(
        &cfg(),
        || native(&[b"x"], 0),
        post(
            "application/grpc",
            frame(0, b"req"),
            &[("origin", "https://evil.example")],
        ),
    )
    .await;
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(header_of(&res, "content-type"), Some("application/grpc"));
    assert_eq!(seen.len(), 1);

    let req = Request::post("/healthz")
        .header(header::CONTENT_TYPE, "application/grpc-web+proto")
        .body(Body::empty())
        .unwrap();
    let (_, seen) = run(&cfg(), || native(&[], 0), req).await;
    assert_eq!(seen[0].0["content-type"], "application/grpc-web+proto");
}

#[tokio::test]
async fn connect_unary_returns_the_bare_message() {
    let (res, seen) = run(
        &cfg(),
        || native(&[b"reply"], 0),
        post(
            "application/proto",
            Bytes::from_static(b"req"),
            &[("authorization", "Bearer t"), ("connect-timeout-ms", "250")],
        ),
    )
    .await;
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(header_of(&res, "content-type"), Some("application/proto"));
    assert_eq!(bytes(res).await.as_ref(), b"reply");
    let (h, body) = &seen[0];
    assert_eq!(h["content-type"], "application/grpc");
    assert_eq!(h["grpc-timeout"], "250m");
    assert_eq!(h["authorization"], "Bearer t");
    assert_eq!(body.as_ref(), frame(0, b"req").as_ref());
}

#[tokio::test]
async fn connect_errors_are_json_with_an_http_status() {
    let (res, _) = run(
        &cfg(),
        || trailers_only(16, "token%20expired"),
        post("application/proto", Bytes::new(), &[]),
    )
    .await;
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(header_of(&res, "content-type"), Some("application/json"));
    let v: serde_json::Value = serde_json::from_slice(&bytes(res).await).unwrap();
    assert_eq!(v["code"], "unauthenticated");
    assert_eq!(v["message"], "token expired");

    let (res, _) = run(
        &cfg(),
        || native(&[], 7),
        post("application/proto", Bytes::new(), &[]),
    )
    .await;
    assert_eq!(res.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn connect_refuses_a_streaming_method() {
    let (res, _) = run(
        &cfg(),
        || native(&[b"a", b"b"], 0),
        post("application/proto", Bytes::new(), &[]),
    )
    .await;
    assert_eq!(res.status(), StatusCode::INTERNAL_SERVER_ERROR);
}

#[test]
fn origins_are_normalised_and_wildcards_refused() {
    assert_eq!(
        normalise_origin("https://App.Example:443").unwrap(),
        "https://app.example"
    );
    assert_eq!(
        normalise_origin("https://app.example/").unwrap(),
        "https://app.example"
    );
    assert_eq!(
        normalise_origin("http://localhost:5173").unwrap(),
        "http://localhost:5173"
    );
    for bad in [
        "*",
        "null",
        "http://app.example",
        "https://app.example/path",
        "https://u:p@app.example",
        "https://app.example?x=1",
        "app.example",
    ] {
        assert!(normalise_origin(bad).is_err(), "{bad}");
    }
}

#[test]
fn rpc_paths_have_a_dotted_service_and_a_method() {
    assert!(is_rpc_path("/asc.v1.Repo/Version"));
    assert!(is_rpc_path("/grpc.health.v1.Health/Check"));
    for bad in [
        "/healthz",
        "/ui/index.html",
        "/o/r",
        "/asc.v1.Repo/",
        "/asc.v1.Repo/A/B",
        "/asc.v1.Repo/A.b",
    ] {
        assert!(!is_rpc_path(bad), "{bad}");
    }
}

#[test]
fn grpc_messages_are_percent_decoded() {
    assert_eq!(percent_decode("a%20b%2Fc%zz%"), "a b/c%zz%");
}
