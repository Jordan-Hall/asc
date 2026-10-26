//! `asc serve` logging: one access line per request, the principal named once authenticated, and
//! no credential anywhere in the output, even with every level on.

use std::io;
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use asc_rpc::auth::{Access, StaticTokens};
use asc_rpc::server::ServerInfo;
use asc_serve::config::LogFormat;
use asc_serve::http::{HttpState, Metrics};
use asc_serve::logging::{AccessLogLayer, LoggingAuthorizer, build};
use asc_serve::outbox::{Outbox, OutboxConfig};
use asc_serve::registry::Extensions;
use asc_serve::{build_routes, migrate};
use tower::ServiceExt;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt::MakeWriter;

const TOKEN: &str = "tok-live-0123456789abcdef";
const BAD_TOKEN: &str = "tok-bad-fedcba9876543210";
const QUERY_SECRET: &str = "query-secret-777";

/// Collects everything the subscriber writes.
#[derive(Clone, Default)]
struct Capture(Arc<Mutex<Vec<u8>>>);

impl Capture {
    fn text(&self) -> String {
        String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
    }
}

impl io::Write for Capture {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl<'a> MakeWriter<'a> for Capture {
    type Writer = Capture;

    fn make_writer(&'a self) -> Capture {
        self.clone()
    }
}

/// The server's public routes behind the access log, as `serve` assembles them.
async fn app() -> Router {
    let pool = migrate::connect("sqlite::memory:", 1).await.unwrap();
    migrate::migrate(&pool).await.unwrap();
    let metrics = Arc::new(Metrics::new());
    let outbox = Arc::new(Outbox::new(
        pool.clone(),
        metrics.clone(),
        OutboxConfig::default(),
    ));
    let ext = Extensions::new();
    asc_serve::http::register(
        &ext,
        HttpState::new(pool, outbox, metrics, Default::default()),
    );
    let authorizer = Arc::new(LoggingAuthorizer::new(Arc::new(
        StaticTokens::new().with_token(TOKEN, "ci-bot", Access::Write),
    )));
    build_routes(&ext, authorizer, ServerInfo::default())
        .into_axum_router()
        .layer(AccessLogLayer)
}

async fn get(app: &Router, uri: &str, bearer: Option<&str>) -> (StatusCode, Option<String>) {
    let mut req = Request::get(uri);
    if let Some(t) = bearer {
        req = req.header(header::AUTHORIZATION, format!("Bearer {t}"));
    }
    let res = app
        .clone()
        .oneshot(req.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let id = res
        .headers()
        .get("x-request-id")
        .map(|v| v.to_str().unwrap().to_owned());
    (res.status(), id)
}

fn capture(format: LogFormat, filter: &str) -> (Capture, tracing::subscriber::DefaultGuard) {
    let out = Capture::default();
    let sub = build(format, EnvFilter::new(filter), out.clone(), false);
    (out, tracing::subscriber::set_default(sub))
}

#[tokio::test]
async fn a_request_writes_one_access_line_and_no_secret() {
    let (out, _guard) = capture(LogFormat::Text, "trace");
    let app = app().await;

    let uri = format!("/acme/web/info/refs?service=git-upload-pack&sig={QUERY_SECRET}");
    let (status, id) = get(&app, &uri, Some(TOKEN)).await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "git is disabled in this build of the app"
    );
    let id = id.expect("the response carries the request id");
    let (status, _) = get(&app, "/acme/web/info/refs", Some(BAD_TOKEN)).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let (status, _) = get(&app, "/.well-known/asc", Some(TOKEN)).await;
    assert_eq!(status, StatusCode::OK);

    let log = out.text();
    let access: Vec<&str> = log
        .lines()
        .filter(|l| l.contains("asc_serve::access"))
        .collect();
    assert_eq!(access.len(), 3, "one line per request:\n{log}");

    let ok = access[0];
    assert!(
        ok.contains(" WARN "),
        "an authenticated 4xx is a warning: {ok}"
    );
    assert!(ok.contains("method=GET"), "{ok}");
    assert!(ok.contains("path=/acme/web/info/refs"), "{ok}");
    assert!(ok.contains("status=403"), "{ok}");
    assert!(ok.contains("principal=\"ci-bot\""), "{ok}");
    assert!(ok.contains(&format!("request_id={id}")), "{ok}");
    assert!(ok.contains("latency_ms="), "{ok}");
    assert!(!ok.contains("service="), "the query is not logged: {ok}");

    let refused = access[1];
    assert!(refused.contains(" WARN "), "{refused}");
    assert!(refused.contains("status=401"), "{refused}");
    assert!(!refused.contains("principal="), "{refused}");

    let served = access[2];
    assert!(served.contains(" INFO "), "{served}");
    assert!(served.contains("path=/.well-known/asc"), "{served}");
    assert!(served.contains("status=200"), "{served}");

    for secret in [TOKEN, BAD_TOKEN, QUERY_SECRET] {
        assert!(!log.contains(secret), "{secret} leaked into:\n{log}");
    }
    assert!(!log.to_ascii_lowercase().contains("bearer "), "{log}");
}

#[tokio::test]
async fn probes_are_debug_only_and_json_lines_carry_the_fields() {
    let app = app().await;
    {
        let (out, _guard) = capture(LogFormat::Text, "info");
        assert_eq!(get(&app, "/healthz", None).await.0, StatusCode::OK);
        assert!(
            out.text().is_empty(),
            "healthz is quiet at info: {}",
            out.text()
        );
    }
    let (out, _guard) = capture(LogFormat::Json, "debug");
    assert_eq!(get(&app, "/healthz", None).await.0, StatusCode::OK);
    get(&app, "/acme/web/info/refs", Some(TOKEN)).await;
    get(&app, "/.well-known/asc", None).await;
    let lines: Vec<serde_json::Value> = out
        .text()
        .lines()
        .map(|l| serde_json::from_str::<serde_json::Value>(l).unwrap())
        .filter(|v| v["target"] == "asc_serve::access")
        .collect();
    assert_eq!(lines.len(), 3);
    assert_eq!(lines[0]["level"], "DEBUG");
    assert_eq!(lines[0]["path"], "/healthz");
    assert_eq!(lines[0]["status"], 200);
    assert_eq!(lines[1]["level"], "WARN");
    assert_eq!(lines[1]["principal"], "ci-bot");
    assert_eq!(lines[2]["level"], "INFO");
    assert_eq!(lines[1]["request_id"].as_str().unwrap().len(), 32);
    assert!(!out.text().contains(TOKEN));
}

#[tokio::test]
async fn a_caller_supplied_request_id_is_echoed_when_plain() {
    let (out, _guard) = capture(LogFormat::Text, "info");
    let app = app().await;
    let res = app
        .clone()
        .oneshot(
            Request::get("/acme/web/info/refs")
                .header("x-request-id", "trace-42")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.headers()["x-request-id"], "trace-42");
    assert!(out.text().contains("request_id=trace-42"), "{}", out.text());
}
