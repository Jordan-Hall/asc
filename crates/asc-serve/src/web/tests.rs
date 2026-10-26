use std::path::PathBuf;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use bytes::Bytes;
use tower::ServiceExt;

use super::assets::{self, Asset, Assets};
use super::{CSP, router, router_for};
use crate::config::WebConfig;

const INDEX: &str =
    "<!doctype html><title>asc</title><script defer src=\"/ui/static/js/app.1234.js\"></script>";

fn asset(path: &str, body: &str, gz: Option<&str>, br: Option<&str>) -> Asset {
    Asset {
        content_type: assets::content_type(path),
        etag: format!("tag{}", body.len()),
        identity: Bytes::from(body.to_owned()),
        gzip: gz.map(|b| Bytes::from(b.to_owned())),
        brotli: br.map(|b| Bytes::from(b.to_owned())),
        immutable: path.starts_with("static/"),
    }
}

fn fixture() -> Assets {
    let mut a = Assets::new();
    a.insert(
        "index.html".into(),
        asset("index.html", INDEX, Some("GZ"), Some("BR")),
    );
    a.insert(
        "static/js/app.1234.js".into(),
        asset(
            "static/js/app.1234.js",
            "console.log(1)",
            Some("gz-js"),
            Some("br-js"),
        ),
    );
    a.insert(
        "theme-init.js".into(),
        asset("theme-init.js", "x=1", None, None),
    );
    a
}

struct Reply {
    status: StatusCode,
    headers: axum::http::HeaderMap,
    body: Vec<u8>,
}

async fn call(req: Request<Body>) -> Reply {
    let res = router_for(fixture())
        .oneshot(req)
        .await
        .expect("infallible");
    let status = res.status();
    let headers = res.headers().clone();
    let body = axum::body::to_bytes(res.into_body(), 1 << 20)
        .await
        .expect("body")
        .to_vec();
    Reply {
        status,
        headers,
        body,
    }
}

fn get(path: &str) -> axum::http::request::Builder {
    Request::get(path)
}

fn empty(b: axum::http::request::Builder) -> Request<Body> {
    b.body(Body::empty()).expect("request")
}

fn h<'a>(r: &'a Reply, name: &str) -> &'a str {
    r.headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
}

#[tokio::test]
async fn hashed_assets_are_immutable_and_unhashed_ones_revalidate() {
    let r = call(empty(get("/ui/static/js/app.1234.js"))).await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(
        h(&r, "cache-control"),
        "public, max-age=31536000, immutable"
    );
    assert_eq!(h(&r, "content-type"), "text/javascript; charset=utf-8");
    assert_eq!(r.body, b"console.log(1)");
    assert_eq!(h(&r, "vary"), "Accept-Encoding");

    let r = call(empty(get("/ui/theme-init.js"))).await;
    assert_eq!(h(&r, "cache-control"), "no-cache");
    let r = call(empty(get("/ui/"))).await;
    assert_eq!(h(&r, "cache-control"), "no-cache");
    assert_eq!(r.body, INDEX.as_bytes());
}

#[tokio::test]
async fn etag_gives_304_and_is_per_encoding() {
    let first = call(empty(get("/ui/static/js/app.1234.js"))).await;
    let tag = h(&first, "etag").to_owned();
    assert!(tag.starts_with('"') && tag.ends_with('"'));
    let r = call(empty(
        get("/ui/static/js/app.1234.js").header(header::IF_NONE_MATCH, &tag),
    ))
    .await;
    assert_eq!(r.status, StatusCode::NOT_MODIFIED);
    assert!(r.body.is_empty());
    assert_eq!(
        h(&r, "cache-control"),
        "public, max-age=31536000, immutable"
    );
    assert_eq!(h(&r, "etag"), tag);

    // The identity validator does not match the brotli variant.
    let r = call(empty(
        get("/ui/static/js/app.1234.js")
            .header(header::ACCEPT_ENCODING, "br")
            .header(header::IF_NONE_MATCH, &tag),
    ))
    .await;
    assert_eq!(r.status, StatusCode::OK);
    assert_ne!(h(&r, "etag"), tag);

    let r = call(empty(get("/ui/").header(header::IF_NONE_MATCH, "*"))).await;
    assert_eq!(r.status, StatusCode::NOT_MODIFIED);
}

#[tokio::test]
async fn encoding_negotiation() {
    let enc = |v: &str| empty(get("/ui/static/js/app.1234.js").header(header::ACCEPT_ENCODING, v));
    let r = call(enc("gzip, deflate, br")).await;
    assert_eq!(
        (h(&r, "content-encoding"), r.body.as_slice()),
        ("br", b"br-js".as_slice())
    );
    let r = call(enc("gzip")).await;
    assert_eq!(
        (h(&r, "content-encoding"), r.body.as_slice()),
        ("gzip", b"gz-js".as_slice())
    );
    let r = call(enc("br;q=0, gzip;q=0.5")).await;
    assert_eq!(h(&r, "content-encoding"), "gzip");
    let r = call(enc("identity")).await;
    assert_eq!(h(&r, "content-encoding"), "");
    assert_eq!(r.body, b"console.log(1)");
    // A file with no precompressed variant is served as is.
    let r = call(empty(
        get("/ui/theme-init.js").header(header::ACCEPT_ENCODING, "br, gzip"),
    ))
    .await;
    assert_eq!(h(&r, "content-encoding"), "");
    assert_eq!(r.body, b"x=1");
}

#[tokio::test]
async fn client_routes_fall_back_to_index_and_missing_files_are_404() {
    for p in [
        "/ui",
        "/ui/acme/app",
        "/ui/acme/app/blob/main/src/a",
        // A source file opens on reload and from a shared link.
        "/ui/acme/app/blob/main/src/a.rs",
        "/ui/acme/app/blob/main/README.md",
        "/ui/inbox",
    ] {
        let r = call(empty(get(p).header(header::ACCEPT, "text/html,*/*;q=0.8"))).await;
        assert_eq!(r.status, StatusCode::OK, "{p}");
        assert_eq!(r.body, INDEX.as_bytes(), "{p}");
    }
    for p in [
        "/ui/static/js/missing.js",
        "/ui/static/js/nested/missing.js",
        "/ui/favicon.svg",
    ] {
        let r = call(empty(get(p).header(header::ACCEPT, "text/html"))).await;
        assert_eq!(r.status, StatusCode::NOT_FOUND, "{p}");
        assert!(
            !r.body.starts_with(b"<!doctype"),
            "{p} must not return the app shell"
        );
    }
    // A file path asked for as data (curl, fetch with `*/*`) is not the shell.
    let r = call(empty(
        get("/ui/acme/app/blob/main/src/a.rs").header(header::ACCEPT, "*/*"),
    ))
    .await;
    assert_eq!(r.status, StatusCode::NOT_FOUND);
    // A route asked for as data (fetch with Accept: application/json) is not the shell.
    let r = call(empty(
        get("/ui/acme/app").header(header::ACCEPT, "application/json"),
    ))
    .await;
    assert_eq!(r.status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn traversal_never_reaches_a_file() {
    for p in [
        "/ui/../etc/passwd",
        "/ui/static/../index.html",
        "/ui/%2e%2e/x",
        "/ui/a%5c..%5cb",
    ] {
        let r = call(empty(get(p).header(header::ACCEPT, "text/html"))).await;
        assert!(
            r.status == StatusCode::NOT_FOUND
                || r.body == INDEX.as_bytes()
                || r.status == StatusCode::BAD_REQUEST,
            "{p}: {}",
            r.status
        );
        assert!(!String::from_utf8_lossy(&r.body).contains("root:"), "{p}");
    }
}

#[tokio::test]
async fn only_get_and_head() {
    let r = call(Request::post("/ui/").body(Body::empty()).expect("request")).await;
    assert_eq!(r.status, StatusCode::METHOD_NOT_ALLOWED);
    assert_eq!(h(&r, "allow"), "GET, HEAD");
    let r = call(
        Request::head("/ui/static/js/app.1234.js")
            .body(Body::empty())
            .expect("request"),
    )
    .await;
    assert_eq!(r.status, StatusCode::OK);
    assert!(r.body.is_empty());
    assert_eq!(h(&r, "content-length"), "14");
}

#[tokio::test]
async fn every_response_carries_the_strict_csp() {
    for (p, status) in [
        ("/ui/", StatusCode::OK),
        ("/ui/static/js/app.1234.js", StatusCode::OK),
        ("/ui/nope.js", StatusCode::NOT_FOUND),
    ] {
        let r = call(empty(get(p))).await;
        assert_eq!(r.status, status, "{p}");
        assert_eq!(h(&r, "content-security-policy"), CSP, "{p}");
        assert_eq!(h(&r, "x-content-type-options"), "nosniff");
        assert_eq!(h(&r, "referrer-policy"), "no-referrer");
        assert_eq!(h(&r, "x-frame-options"), "DENY");
    }
    let r = call(Request::post("/ui/").body(Body::empty()).expect("request")).await;
    assert_eq!(h(&r, "content-security-policy"), CSP);
}

/// The policy blocks inline script and style, eval, remote loads and framing, whatever the page
/// contains: there is no `unsafe-inline`, `unsafe-eval`, `data:` script source or wildcard.
#[test]
fn csp_has_no_escape_hatches() {
    for bad in [
        "unsafe-inline",
        "unsafe-eval",
        "unsafe-hashes",
        "*",
        "http:",
        "https:",
        "blob:",
    ] {
        assert!(
            !CSP.split(';')
                .any(|d| d.split_whitespace().skip(1).any(|t| t == bad)),
            "{bad}"
        );
    }
    let directive = |name: &str| {
        CSP.split(';')
            .map(str::trim)
            .find(|d| d.starts_with(name))
            .unwrap_or_default()
            .to_owned()
    };
    assert_eq!(directive("script-src"), "script-src 'self'");
    assert_eq!(directive("style-src"), "style-src 'self'");
    assert_eq!(directive("default-src"), "default-src 'none'");
    assert_eq!(directive("object-src"), "object-src 'none'");
    assert_eq!(directive("frame-ancestors"), "frame-ancestors 'none'");
    assert_eq!(directive("base-uri"), "base-uri 'none'");
    assert_eq!(directive("connect-src"), "connect-src 'self'");
}

#[test]
fn disabled_is_absent_and_enabled_without_assets_is_an_error() {
    assert!(router(&WebConfig::default()).expect("ok").is_none());
    let tmp = tempfile::tempdir().expect("tempdir");
    let cfg = WebConfig {
        enabled: true,
        dir: Some(tmp.path().to_path_buf()),
        ..WebConfig::default()
    };
    let err = router(&cfg)
        .expect_err("empty dir has no index.html")
        .to_string();
    assert!(err.contains("web.dir"), "{err}");
}

#[tokio::test]
async fn serves_a_directory_with_precompressed_siblings() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root: PathBuf = tmp.path().to_path_buf();
    std::fs::create_dir_all(root.join("static/js")).expect("mkdir");
    std::fs::write(root.join("index.html"), INDEX).expect("write");
    std::fs::write(root.join("static/js/a.1.js"), "alert(1)").expect("write");
    std::fs::write(root.join("static/js/a.1.js.gz"), "GZBODY").expect("write");
    std::fs::write(root.join("manifest.json"), "{}").expect("write");
    let cfg = WebConfig {
        enabled: true,
        dir: Some(root),
        ..WebConfig::default()
    };
    let app = router(&cfg).expect("ok").expect("enabled");
    let res = app
        .clone()
        .oneshot(
            Request::get("/ui/static/js/a.1.js")
                .header(header::ACCEPT_ENCODING, "gzip")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("infallible");
    assert_eq!(
        res.headers()
            .get("content-encoding")
            .and_then(|v| v.to_str().ok()),
        Some("gzip")
    );
    let body = axum::body::to_bytes(res.into_body(), 1024)
        .await
        .expect("body");
    assert_eq!(&body[..], b"GZBODY");
    // The build manifest is internal.
    let res = app
        .oneshot(
            Request::get("/ui/manifest.json")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("infallible");
    assert_eq!(res.status(), StatusCode::NOT_FOUND);
}

#[test]
fn config_section_parses_and_validates() {
    use crate::config::ServeConfig;
    let parse = |web: &str| {
        ServeConfig::parse(&format!(
            "store = \"memory:///\"
db = \"sqlite::memory:\"
{web}"
        ))
    };
    let ok = parse(
        "[web]
enabled = true
",
    )
    .expect("parses");
    assert!(ok.web.enabled && ok.web.dir.is_none());
    assert!(!ServeConfig::default().web.enabled, "off by default");
    let err = parse(
        "[web]
dir = \"/opt/asc/web\"
",
    )
    .expect_err("dir needs enabled");
    assert!(err.to_string().contains("web.enabled"), "{err}");
    assert!(
        parse(
            "[web]
bogus = 1
"
        )
        .is_err(),
        "unknown keys are refused"
    );
}
