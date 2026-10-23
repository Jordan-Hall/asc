//! U81: the disabled git stub (SPEC §25.9). With the Cargo feature off, with `[git] enabled =
//! false`, or while git is unusable, the git paths answer the Basic challenge to an anonymous
//! request and `403` with `X-Asc-Git: disabled` to an authenticated one that may read; a repo the
//! principal cannot read stays `404`. The stub is compiled into every build, so these tests run
//! in both; the symbol check runs only without `git-compat`.
//!
//! Run without the layer: `cargo test -p asc-serve --no-default-features --test git_disabled`.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use base64::Engine as _;
use asc_rpc::auth::{Access, AuthError, Authorizer, BearerToken, Principal};
use asc_rpc::server::ServerInfo;
use asc_serve::build_routes;
use asc_serve::registry::Extensions;
use tower::ServiceExt;

const READER: &str = "reader-secret-token";
const WRITER: &str = "writer-secret-token";
const OUTSIDER: &str = "outsider-secret-token";

/// `acme/web` is readable by the reader and the writer; the outsider is valid but reads nothing.
struct Acl(HashMap<&'static str, (&'static str, Option<Access>)>);

impl Acl {
    fn new() -> Arc<Self> {
        Arc::new(Self(HashMap::from([
            (READER, ("reader", Some(Access::Read))),
            (WRITER, ("writer", Some(Access::Write))),
            (OUTSIDER, ("outsider", None)),
        ])))
    }
}

#[async_trait]
impl Authorizer for Acl {
    async fn authorize(
        &self,
        token: Option<&BearerToken>,
        repo: &str,
        access: Access,
    ) -> Result<Principal, AuthError> {
        let token = token.ok_or(AuthError::Unauthenticated)?;
        let (id, max) = self
            .0
            .get(token.expose())
            .ok_or(AuthError::Unauthenticated)?;
        match max {
            Some(max) if repo == "acme/web" && access <= *max => Ok(Principal {
                id: (*id).to_owned(),
            }),
            _ => Err(AuthError::Forbidden),
        }
    }
}

fn basic(user: &str, secret: &str) -> String {
    format!(
        "Basic {}",
        base64::engine::general_purpose::STANDARD.encode(format!("{user}:{secret}"))
    )
}

fn app() -> Router {
    build_routes(&Extensions::new(), Acl::new(), ServerInfo::default()).into_axum_router()
}

struct Answer {
    status: StatusCode,
    flag: Option<String>,
    content_type: Option<String>,
    challenges: Vec<String>,
    body: String,
}

async fn send(method: &str, path: &str, auth: Option<&str>) -> Answer {
    let mut req = Request::builder()
        .method(method)
        .uri(path)
        .header(header::HOST, "asc.example.test");
    if let Some(a) = auth {
        req = req.header(header::AUTHORIZATION, a);
    }
    let res = app().oneshot(req.body(Body::empty()).unwrap()).await.unwrap();
    let status = res.status();
    let h = |n: &str| {
        res.headers()
            .get(n)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned)
    };
    let flag = h("x-asc-git");
    let content_type = h("content-type");
    let challenges = res
        .headers()
        .get_all(header::WWW_AUTHENTICATE)
        .iter()
        .filter_map(|v| v.to_str().ok().map(str::to_owned))
        .collect();
    let bytes = axum::body::to_bytes(res.into_body(), 1 << 20).await.unwrap();
    Answer {
        status,
        flag,
        content_type,
        challenges,
        body: String::from_utf8_lossy(&bytes).into_owned(),
    }
}

const PATHS: &[(&str, &str)] = &[
    ("GET", "/acme/web.git/info/refs?service=git-upload-pack"),
    ("GET", "/acme/web/info/refs?service=git-receive-pack"),
    ("POST", "/acme/web.git/git-upload-pack"),
    ("POST", "/acme/web/git-receive-pack"),
];

#[tokio::test]
async fn anonymous_requests_get_the_basic_challenge_first() {
    for (method, path) in PATHS {
        let a = send(method, path, None).await;
        assert_eq!(a.status, StatusCode::UNAUTHORIZED, "{method} {path}");
        assert!(
            a.challenges[0].starts_with("Basic realm=\"asc asc.example.test\""),
            "{:?}",
            a.challenges
        );
        assert!(a.flag.is_none(), "the flag is for the disabled answer only");
    }
}

#[tokio::test]
async fn an_authenticated_reader_gets_403_and_the_flag_whatever_the_user_name() {
    for (method, path) in PATHS {
        for user in ["git", "x-access-token", "oauth2", "someone"] {
            let a = send(method, path, Some(&basic(user, READER))).await;
            assert_eq!(a.status, StatusCode::FORBIDDEN, "{method} {path} {user}");
            assert_eq!(a.flag.as_deref(), Some("disabled"));
            assert!(
                a.content_type.as_deref().is_some_and(|c| c.starts_with("text/plain")),
                "{:?}",
                a.content_type
            );
            assert_eq!(a.body.trim(), "asc: git protocol disabled on this server");
        }
    }
    // The write token and a Bearer or `token` credential are the same.
    let a = send("GET", PATHS[0].1, Some(&format!("Bearer {WRITER}"))).await;
    assert_eq!((a.status, a.flag.as_deref()), (StatusCode::FORBIDDEN, Some("disabled")));
    let a = send("GET", PATHS[0].1, Some(&format!("token {WRITER}"))).await;
    assert_eq!((a.status, a.flag.as_deref()), (StatusCode::FORBIDDEN, Some("disabled")));
}

#[tokio::test]
async fn a_repo_the_principal_cannot_read_is_404_and_a_bad_token_is_401() {
    // Valid credential, no read access: the same 404 as a missing repo, with no flag.
    for path in [
        "/acme/web.git/info/refs?service=git-upload-pack",
        "/acme/missing.git/info/refs?service=git-upload-pack",
        "/other/web/git-upload-pack",
    ] {
        let a = send("GET", path, Some(&basic("git", OUTSIDER))).await;
        assert_eq!(a.status, StatusCode::NOT_FOUND, "{path}");
        assert!(a.flag.is_none());
    }
    let a = send("GET", PATHS[0].1, Some(&basic("git", "revoked"))).await;
    assert_eq!(a.status, StatusCode::UNAUTHORIZED);
    assert!(a.challenges[0].starts_with("Basic "));
    // A name that cannot be a repository is 404 after a good credential, never a probe oracle.
    let a = send("GET", "/acme/..%2fx/info/refs?service=git-upload-pack", Some(&basic("git", READER))).await;
    assert_eq!(a.status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn a_token_in_the_query_and_a_malformed_header_are_400() {
    let a = send("GET", "/acme/web/info/refs?service=git-upload-pack&token=abc", None).await;
    assert_eq!(a.status, StatusCode::BAD_REQUEST);
    let a = send("GET", PATHS[0].1, Some("Digest nope")).await;
    assert_eq!(a.status, StatusCode::BAD_REQUEST);
}

/// Real git against the stub: it meets the challenge, retries with the credential it was given
/// and then fails on the `403`, as with any host that does not serve git.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn real_git_is_told_the_protocol_is_disabled() {
    let Ok(real_git) = asc_git::realgit::RealGit::resolve(None, None) else {
        eprintln!("git is not installed; skipping");
        return;
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let router = app();
    tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });
    let dir = tempfile::tempdir().unwrap();
    let empty_config = dir.path().join("gitconfig");
    std::fs::write(&empty_config, "").unwrap();
    let url = format!("http://git:{READER}@{addr}/acme/web.git");
    let out = tokio::task::spawn_blocking(move || {
        real_git
            .command()
            .unwrap()
            .args(["ls-remote", &url])
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", &empty_config)
            .output()
            .unwrap()
    })
    .await
    .unwrap();
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("403"), "{stderr}");
    assert!(!stderr.contains(READER), "no secret on git's stderr: {stderr}");
}

/// Without the feature the binary holds no `compat` code (SPEC §25.9: what compiles out).
#[cfg(not(feature = "git-compat"))]
#[test]
fn the_binary_has_no_compat_symbols() {
    let exe = std::fs::read(std::env::current_exe().unwrap()).unwrap();
    let has = |needle: &str| {
        let n = needle.as_bytes();
        exe.windows(n.len()).any(|w| w == n)
    };
    // Written backwards, so that this test's own constants do not hold the words.
    let rev = |s: &str| s.chars().rev().collect::<String>();
    for needle in [
        rev("tcapmoc::tig_gc"),
        rev("tcapmoc/crs/tig-gc"),
        rev(r"tcapmoc\crs\tig-gc"),
        rev("tiGrevreS"),
        rev("ecivreSweiVtiG"),
        rev("stseuqer_tig_gc"),
    ] {
        assert!(!has(&needle), "the binary mentions {needle:?}");
    }
}
