//! The web UI's static serving (docs/WEB-UI.md section 8).
//!
//! `GET /ui/...` answers from an asset table (embedded by `build.rs` or loaded from `[web] dir`):
//! content-hashed files under `static/` are immutable for a year, `index.html` and the other
//! unhashed files revalidate with a strong ETag, bodies are served pre-compressed (brotli, gzip)
//! when the client accepts it, and a client route falls back to `index.html` so it survives a reload
//! (an extension-less path, or a nested path with an extension, a source file, that a browser opens). Every response carries a strict CSP (no inline script or style).
//! The files are public; every data call the UI makes goes through the authenticated RPC.

pub mod assets;
pub mod grpc_web;

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode, header};
use axum::response::Response;
use axum::routing::any;
use bytes::Bytes;

use crate::config::{ConfigError, WebConfig};
use crate::registry::Extensions;
use assets::{Asset, Assets, BASE};

const CSP: &str = "default-src 'none'; script-src 'self'; style-src 'self'; img-src 'self' data:; \
font-src 'self'; connect-src 'self'; manifest-src 'self'; worker-src 'self'; base-uri 'none'; \
form-action 'none'; frame-ancestors 'none'; object-src 'none'";

const SECURITY_HEADERS: [(&str, &str); 7] = [
    ("content-security-policy", CSP),
    ("x-content-type-options", "nosniff"),
    ("referrer-policy", "no-referrer"),
    ("cross-origin-opener-policy", "same-origin"),
    ("cross-origin-resource-policy", "same-origin"),
    (
        "permissions-policy",
        "camera=(), microphone=(), geolocation=(), interest-cohort=()",
    ),
    ("x-frame-options", "DENY"),
];

const IMMUTABLE: &str = "public, max-age=31536000, immutable";
const REVALIDATE: &str = "no-cache";

#[derive(Clone)]
struct WebState {
    assets: Arc<Assets>,
}

/// The routes for `assets`.
pub fn router_for(assets: Assets) -> Router {
    let state = WebState {
        assets: Arc::new(assets),
    };
    Router::new()
        .route(BASE, any(serve_root))
        .route(&format!("{BASE}/"), any(serve_root))
        .route(&format!("{BASE}/{{*path}}"), any(serve_path))
        .with_state(state)
}

/// Builds the router for `cfg`, or `None` when the UI is off. Enabled with nothing to serve is an
/// error: an on switch that serves 404 hides the mistake.
pub fn router(cfg: &WebConfig) -> Result<Option<Router>, ConfigError> {
    if !cfg.enabled {
        return Ok(None);
    }
    let assets = match &cfg.dir {
        Some(dir) => assets::from_dir(dir)
            .map_err(|e| ConfigError::Invalid(format!("web.dir {}: {e}", dir.display())))?,
        None => assets::embedded(),
    };
    if !assets.contains_key("index.html") {
        return Err(ConfigError::Invalid(
            "web.enabled = true, but this build embeds no web UI; run `bun run --cwd web build` \
             before building asc-serve, or set web.dir"
                .into(),
        ));
    }
    tracing::info!(event = "web.enabled", files = assets.len(), base = BASE);
    Ok(Some(router_for(assets)))
}

/// One registration line in `lib.rs`.
pub fn register(ext: &Extensions, cfg: &WebConfig) -> Result<(), ConfigError> {
    if let Some(r) = router(cfg)? {
        ext.http.register(r);
    }
    Ok(())
}

async fn serve_root(State(s): State<WebState>, method: Method, headers: HeaderMap) -> Response {
    respond(&s, "", &method, &headers)
}

async fn serve_path(
    State(s): State<WebState>,
    Path(path): Path<String>,
    method: Method,
    headers: HeaderMap,
) -> Response {
    respond(&s, &path, &method, &headers)
}

fn secure(mut res: Response) -> Response {
    let h = res.headers_mut();
    for (k, v) in SECURITY_HEADERS {
        h.insert(HeaderName::from_static(k), HeaderValue::from_static(v));
    }
    res
}

fn plain(status: StatusCode) -> Response {
    let mut res = Response::new(Body::empty());
    *res.status_mut() = status;
    res.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static(REVALIDATE));
    secure(res)
}

/// Whether `path` is a client route (`/ui/acme/app/changes`), not a missing file. An extension-less
/// path is. A path whose last segment has an extension is too when it is nested (a source file,
/// `/ui/acme/app/blob/main/src/lib.rs`, which a reload or a shared link must open) and the client
/// navigates to a document; `static/` and top-level files with an extension are always files.
fn is_route(path: &str, headers: &HeaderMap) -> bool {
    if path.is_empty() || !path.rsplit('/').next().unwrap_or("").contains('.') {
        return wants_html(headers);
    }
    !path.starts_with("static/") && path.contains('/') && navigates_to_document(headers)
}

fn accept(headers: &HeaderMap) -> Option<&str> {
    headers.get(header::ACCEPT).and_then(|v| v.to_str().ok())
}

fn wants_html(headers: &HeaderMap) -> bool {
    accept(headers).is_none_or(|a| a.contains("text/html") || a.contains("*/*"))
}

/// A browser navigation asks for `text/html` by name; `fetch` and `curl` send `*/*` or nothing.
fn navigates_to_document(headers: &HeaderMap) -> bool {
    accept(headers).is_some_and(|a| a.contains("text/html"))
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Coding {
    Identity,
    Gzip,
    Brotli,
}

/// The best coding the client accepts and `asset` has. A `q=0` refuses a coding.
fn negotiate(headers: &HeaderMap, asset: &Asset) -> Coding {
    let Some(value) = headers
        .get(header::ACCEPT_ENCODING)
        .and_then(|v| v.to_str().ok())
    else {
        return Coding::Identity;
    };
    let accepts = |name: &str| {
        value.split(',').any(|part| {
            let mut it = part.split(';');
            let token = it.next().unwrap_or("").trim();
            if !token.eq_ignore_ascii_case(name) {
                return false;
            }
            let q = it
                .find_map(|p| p.trim().strip_prefix("q="))
                .and_then(|q| q.trim().parse::<f32>().ok())
                .unwrap_or(1.0);
            q > 0.0
        })
    };
    if asset.brotli.is_some() && accepts("br") {
        Coding::Brotli
    } else if asset.gzip.is_some() && accepts("gzip") {
        Coding::Gzip
    } else {
        Coding::Identity
    }
}

/// Whether `If-None-Match` matches `etag` (weak comparison; `*` matches).
fn not_modified(headers: &HeaderMap, etag: &str) -> bool {
    let Some(v) = headers
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
    else {
        return false;
    };
    v.split(',').any(|t| {
        let t = t.trim();
        t == "*" || t.strip_prefix("W/").unwrap_or(t) == etag
    })
}

fn respond(s: &WebState, path: &str, method: &Method, headers: &HeaderMap) -> Response {
    if method != Method::GET && method != Method::HEAD {
        let mut res = plain(StatusCode::METHOD_NOT_ALLOWED);
        res.headers_mut()
            .insert(header::ALLOW, HeaderValue::from_static("GET, HEAD"));
        return res;
    }
    let path = path.trim_end_matches('/');
    if path
        .split('/')
        .any(|seg| seg == ".." || seg == "." || seg.contains(['\\', '\0']))
    {
        return plain(StatusCode::NOT_FOUND);
    }
    let found = match s.assets.get(path) {
        Some(a) if !path.is_empty() => Some((a, path)),
        _ if is_route(path, headers) => s.assets.get("index.html").map(|a| (a, "index.html")),
        _ => None,
    };
    let Some((asset, name)) = found else {
        return plain(StatusCode::NOT_FOUND);
    };
    let coding = negotiate(headers, asset);
    let (body, label): (&Bytes, Option<&str>) = match coding {
        Coding::Brotli => (asset.brotli.as_ref().unwrap_or(&asset.identity), Some("br")),
        Coding::Gzip => (asset.gzip.as_ref().unwrap_or(&asset.identity), Some("gzip")),
        Coding::Identity => (&asset.identity, None),
    };
    let etag = match label {
        Some(l) => format!("\"{}-{l}\"", asset.etag),
        None => format!("\"{}\"", asset.etag),
    };
    let cache = if asset.immutable && name != "index.html" {
        IMMUTABLE
    } else {
        REVALIDATE
    };
    let mut res = if not_modified(headers, &etag) {
        let mut r = Response::new(Body::empty());
        *r.status_mut() = StatusCode::NOT_MODIFIED;
        r
    } else {
        let mut r = Response::new(if method == Method::HEAD {
            Body::empty()
        } else {
            Body::from(body.clone())
        });
        let h = r.headers_mut();
        h.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static(asset.content_type),
        );
        h.insert(header::CONTENT_LENGTH, HeaderValue::from(body.len()));
        if let Some(l) = label {
            h.insert(header::CONTENT_ENCODING, HeaderValue::from_static(l));
        }
        r
    };
    let h = res.headers_mut();
    if let Ok(v) = HeaderValue::from_str(&etag) {
        h.insert(header::ETAG, v);
    }
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static(cache));
    h.insert(header::VARY, HeaderValue::from_static("Accept-Encoding"));
    secure(res)
}

#[cfg(test)]
mod tests;
