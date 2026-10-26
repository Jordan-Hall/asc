//! Forgejo's push webhook (SPEC §8.6): the signed `POST /hooks/forgejo` that tells the mirror a
//! repository changed. The route, the signature scheme and the payload shape are Forgejo's; the
//! mirror only knows [`Mirror::trigger`].

use std::path::Path;
use std::sync::Arc;

use asc_rpc::server::RepoName;
use axum::Router;
use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, State};
use axum::http::{HeaderMap, StatusCode};
use axum::routing::post;
use hmac::{Hmac, KeyInit, Mac};
use sha2::Sha256;

use super::ForgejoError;
use crate::mirror::{Mirror, MirrorError};

/// The route Forgejo's push webhook posts to.
pub const WEBHOOK_PATH: &str = "/hooks/forgejo";
/// The header carrying the hex HMAC-SHA256 of the body.
const SIGNATURE_HEADER: &str = "x-forgejo-signature";
/// Largest accepted webhook body.
const MAX_WEBHOOK_BYTES: usize = 1 << 20;
/// Largest webhook secret file.
const MAX_SECRET_BYTES: u64 = 4096;

/// Reads the shared secret from `path` (at most 4 KiB, trailing line ends removed).
pub fn read_secret(path: &Path) -> Result<Vec<u8>, ForgejoError> {
    let bad = |e: std::io::Error| ForgejoError::Config(format!("{}: {e}", path.display()));
    let mut buf = Vec::new();
    std::io::Read::read_to_end(
        &mut std::io::Read::take(
            std::fs::File::open(path).map_err(bad)?,
            MAX_SECRET_BYTES + 1,
        ),
        &mut buf,
    )
    .map_err(bad)?;
    if buf.len() as u64 > MAX_SECRET_BYTES {
        return Err(ForgejoError::Config(
            "webhook secret file is too large".into(),
        ));
    }
    while buf.last().is_some_and(|b| matches!(b, b'\n' | b'\r')) {
        buf.pop();
    }
    if buf.is_empty() {
        return Err(ForgejoError::Config("webhook secret file is empty".into()));
    }
    Ok(buf)
}

fn unhex(s: &str) -> Option<Vec<u8>> {
    let b = s.as_bytes();
    if !b.len().is_multiple_of(2) {
        return None;
    }
    let nibble = |c: u8| match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    };
    b.chunks_exact(2)
        .map(|p| Some(nibble(p[0])? << 4 | nibble(p[1])?))
        .collect()
}

/// Whether `signature` (hex, optionally `sha256=`-prefixed) is HMAC-SHA256 of `body` under
/// `secret`. The comparison is constant time.
pub fn verify_signature(secret: &[u8], body: &[u8], signature: &str) -> bool {
    let hex = signature.trim();
    let hex = hex.strip_prefix("sha256=").unwrap_or(hex);
    let Some(want) = unhex(hex) else {
        return false;
    };
    let Ok(mut mac) = Hmac::<Sha256>::new_from_slice(secret) else {
        return false;
    };
    mac.update(body);
    mac.verify_slice(&want).is_ok()
}

/// The repository a Forgejo push payload names (`repository.full_name`).
fn repo_of_payload(body: &[u8]) -> Option<RepoName> {
    let v: serde_json::Value = serde_json::from_slice(body).ok()?;
    RepoName::parse(v.get("repository")?.get("full_name")?.as_str()?).ok()
}

#[derive(Clone)]
struct Hook {
    mirror: Arc<Mirror>,
    secret: Arc<[u8]>,
}

async fn handle(State(h): State<Hook>, headers: HeaderMap, body: Bytes) -> StatusCode {
    let signed = headers
        .get(SIGNATURE_HEADER)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|s| verify_signature(&h.secret, &body, s));
    if !signed {
        return StatusCode::UNAUTHORIZED;
    }
    let Some(name) = repo_of_payload(&body) else {
        return StatusCode::BAD_REQUEST;
    };
    match h.mirror.trigger(&name).await {
        Ok(_) => StatusCode::OK,
        Err(MirrorError::NotFound(_) | MirrorError::NotMirror(_)) => StatusCode::NOT_FOUND,
        Err(e) => {
            tracing::warn!(error = %e, "mirror webhook sync failed");
            StatusCode::BAD_GATEWAY
        }
    }
}

/// The `/hooks/forgejo` router: every signed push syncs the repository it names.
pub fn router(mirror: Arc<Mirror>, secret: Vec<u8>) -> Router {
    Router::new()
        .route(WEBHOOK_PATH, post(handle))
        .layer(DefaultBodyLimit::max(MAX_WEBHOOK_BYTES))
        .with_state(Hook {
            mirror,
            secret: secret.into(),
        })
}

#[cfg(test)]
mod tests {
    use asc_git::realgit::RealGit;
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    use super::*;
    use crate::ingest::testutil::registry;
    use crate::mirror::MirrorConfig;
    use crate::registry::NewRepo;
    use crate::update::PipelineHooks;

    const SECRET: &[u8] = b"s3cret";

    fn sign(body: &[u8]) -> String {
        let mut mac = Hmac::<Sha256>::new_from_slice(SECRET).unwrap();
        mac.update(body);
        mac.finalize()
            .into_bytes()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    }

    #[test]
    fn signatures_are_verified() {
        let body = b"{}";
        let good = sign(body);
        assert!(verify_signature(SECRET, body, &good));
        assert!(verify_signature(SECRET, body, &format!("sha256={good}")));
        assert!(!verify_signature(b"other", body, &good));
        assert!(!verify_signature(SECRET, b"{ }", &good));
        assert!(!verify_signature(SECRET, body, ""));
        assert!(!verify_signature(SECRET, body, "zz"));
        assert!(!verify_signature(SECRET, body, &good[1..]));
    }

    #[test]
    fn the_payload_names_its_repository() {
        let r = repo_of_payload(br#"{"repository":{"full_name":"o/m"},"ref":"x"}"#);
        assert_eq!(r, Some(RepoName::parse("o/m").unwrap()));
        assert_eq!(repo_of_payload(b"{}"), None);
        assert_eq!(repo_of_payload(b"nope"), None);
    }

    #[test]
    fn the_secret_file_is_trimmed_and_bounded() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("s");
        std::fs::write(&f, "abc\r\n").unwrap();
        assert_eq!(read_secret(&f).unwrap(), b"abc");
        std::fs::write(&f, "\n").unwrap();
        assert!(read_secret(&f).is_err());
        std::fs::write(&f, vec![b'a'; 5000]).unwrap();
        assert!(read_secret(&f).is_err());
    }

    async fn post_body(m: &Arc<Mirror>, body: String, signature: Option<String>) -> StatusCode {
        let sig = signature.unwrap_or_else(|| sign(body.as_bytes()));
        router(m.clone(), SECRET.to_vec())
            .oneshot(
                Request::post(WEBHOOK_PATH)
                    .header(SIGNATURE_HEADER, sig)
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap()
            .status()
    }

    fn push_of(repo: &str) -> String {
        format!(r#"{{"repository":{{"full_name":"{repo}"}},"ref":"refs/heads/master"}}"#)
    }

    #[tokio::test]
    async fn the_route_checks_the_signature_and_maps_the_trigger_outcome() {
        let tmp = tempfile::tempdir().unwrap();
        let registry = registry().await;
        registry
            .create(&RepoName::parse("o/p").unwrap(), &NewRepo::default())
            .await
            .unwrap();
        let cfg = MirrorConfig {
            dir: tmp.path().join("mirrors"),
            forgejo_url: None,
            allowed_hosts: Vec::new(),
            interval: std::time::Duration::from_secs(60),
            fetch_timeout: std::time::Duration::from_secs(120),
        };
        let real = RealGit::resolve(None, None).expect("git is installed");
        let m = Arc::new(Mirror::new(registry, &PipelineHooks::default(), real, cfg));
        let bad_sig = Some("00".repeat(32));
        assert_eq!(
            post_body(&m, push_of("o/m"), bad_sig).await,
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            post_body(&m, push_of("o/zzz"), None).await,
            StatusCode::NOT_FOUND
        );
        // A primary repo is not a mirror.
        assert_eq!(
            post_body(&m, push_of("o/p"), None).await,
            StatusCode::NOT_FOUND
        );
        // A signed body that names no repository.
        assert_eq!(
            post_body(&m, "{}".into(), None).await,
            StatusCode::BAD_REQUEST
        );
    }
}
