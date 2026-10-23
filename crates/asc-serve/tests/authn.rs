//! U78: the server credential edge (SPEC §15.6.2, §15.6.4). Basic, Bearer and `token` carry one
//! secret to one `Authorizer` call; every surface answers a bad or missing credential with the
//! Basic-first challenge; a real `git` pointed at the server asks its credential helper.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use axum::Router;
use axum::body::Body;
use axum::http::{Request, Response, StatusCode, header};
use base64::Engine as _;
use asc_rpc::auth::{
    Access, AuthError, Authorizer, BearerToken, Credential, Principal, Scheme, StaticTokens,
};
use asc_rpc::server::ServerInfo;
use asc_serve::authn::{self, Surface};
use asc_serve::config::TurboConfig;
use asc_serve::http::{HttpState, Metrics, admin_router};
use asc_serve::outbox::{Outbox, OutboxConfig};
use asc_serve::registry::Extensions;
use asc_serve::seeds::{SeedStore, SeedsState};
use asc_serve::turbo::TurboState;
use asc_serve::{build_routes, migrate, seeds, turbo};
use object_store::memory::InMemory;
use object_store::path::Path as StorePath;
use tower::ServiceExt;

const TOKEN: &str = "ci-secret-token";
const ADMIN: &str = "admin-secret-token";
const HOST: &str = "asc.example.test";

/// Records every credential the routes hand to the authorizer.
struct Spy {
    inner: StaticTokens,
    calls: AtomicUsize,
    seen: Mutex<Vec<(Scheme, Option<String>)>>,
}

impl Spy {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            inner: StaticTokens::new().with_token(TOKEN, "ci", Access::Write),
            calls: AtomicUsize::new(0),
            seen: Mutex::new(Vec::new()),
        })
    }
}

#[async_trait]
impl Authorizer for Spy {
    async fn authorize(
        &self,
        token: Option<&BearerToken>,
        repo: &str,
        access: Access,
    ) -> Result<Principal, AuthError> {
        self.inner.authorize(token, repo, access).await
    }

    async fn authorize_credential(
        &self,
        credential: Option<&Credential>,
        repo: &str,
        access: Access,
    ) -> Result<Principal, AuthError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if let Some(c) = credential {
            self.seen
                .lock()
                .unwrap()
                .push((c.scheme(), c.user().map(str::to_owned)));
        }
        self.authorize(credential.map(Credential::secret), repo, access)
            .await
    }
}

fn basic(raw: &str) -> String {
    format!(
        "Basic {}",
        base64::engine::general_purpose::STANDARD.encode(raw)
    )
}

/// The public routes of a server: gRPC, `/v8`, `/seeds` and the git paths.
fn public(spy: &Arc<Spy>) -> Router {
    let ext = Extensions::new();
    let mem = Arc::new(InMemory::new());
    let store = SeedStore::new(mem.clone(), StorePath::from("root"));
    seeds::register(
        &ext,
        Arc::new(SeedsState::new(spy.clone(), Arc::new(store))),
    );
    let state = TurboState::new(
        spy.clone(),
        mem,
        StorePath::from("root"),
        TurboConfig::default(),
        Arc::new(Metrics::new()),
    )
    .with_trusted_token(TOKEN);
    turbo::register(&ext, Arc::new(state));
    build_routes(&ext, spy.clone(), ServerInfo::default()).into_axum_router()
}

async fn admin() -> Router {
    let pool = migrate::connect("sqlite::memory:", 1).await.unwrap();
    migrate::migrate(&pool).await.unwrap();
    let metrics = Arc::new(Metrics::new());
    let outbox = Arc::new(Outbox::new(
        pool.clone(),
        metrics.clone(),
        OutboxConfig::default(),
    ));
    let state = HttpState::new(pool, outbox, metrics, asc_serve::http::WellKnown::default());
    admin_router(state, Some(Arc::from(ADMIN)))
}

/// What `/mcp` will be: a route behind the edge with the `resource_metadata` challenge (M13).
fn mcp() -> Router {
    let routes = Router::new().route(
        "/mcp",
        axum::routing::post(|| async { StatusCode::UNAUTHORIZED }),
    );
    authn::guard(
        routes,
        Surface::Mcp {
            resource_metadata: Arc::from(
                "https://asc.example.test/.well-known/oauth-protected-resource",
            ),
        },
    )
}

struct Route {
    name: &'static str,
    method: &'static str,
    path: &'static str,
    /// What the secret may do here, for the success test: `(status when allowed)`.
    allowed: StatusCode,
}

/// Every HTTP route the public listener serves behind the edge.
const ROUTES: &[Route] = &[
    Route {
        name: "seeds",
        method: "GET",
        path: "/seeds/acme/web/latest",
        allowed: StatusCode::NOT_FOUND,
    },
    Route {
        name: "turbo read",
        method: "GET",
        path: "/v8/artifacts/abc123?slug=acme.web&teamId=team_x",
        allowed: StatusCode::NOT_FOUND,
    },
    Route {
        name: "turbo write",
        method: "PUT",
        path: "/v8/artifacts/abc123?slug=acme.web&teamId=team_x",
        allowed: StatusCode::ACCEPTED,
    },
    Route {
        name: "git info/refs",
        method: "GET",
        path: "/acme/web.git/info/refs?service=git-upload-pack",
        allowed: StatusCode::NOT_FOUND,
    },
    Route {
        name: "git upload-pack",
        method: "POST",
        path: "/acme/web/git-upload-pack",
        allowed: StatusCode::NOT_FOUND,
    },
    Route {
        name: "git receive-pack",
        method: "POST",
        path: "/acme/web.git/git-receive-pack",
        allowed: StatusCode::NOT_FOUND,
    },
];

fn request(method: &str, path: &str, auth: &[&str]) -> Request<Body> {
    let mut r = Request::builder()
        .method(method)
        .uri(path)
        .header(header::HOST, HOST);
    for a in auth {
        r = r.header(header::AUTHORIZATION, *a);
    }
    r.body(Body::from("payload")).unwrap()
}

async fn send(app: &Router, method: &str, path: &str, auth: &[&str]) -> Response<Body> {
    app.clone()
        .oneshot(request(method, path, auth))
        .await
        .unwrap()
}

fn challenges(res: &Response<Body>) -> Vec<String> {
    res.headers()
        .get_all(header::WWW_AUTHENTICATE)
        .iter()
        .map(|v| v.to_str().unwrap().to_owned())
        .collect()
}

/// A 401 whose first challenge is Basic, then Bearer.
fn assert_challenged(res: &Response<Body>, what: &str) {
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED, "{what}");
    let c = challenges(res);
    assert_eq!(
        c[0],
        format!("Basic realm=\"asc {HOST}\", charset=\"UTF-8\""),
        "{what}"
    );
    assert_eq!(c[1], format!("Bearer realm=\"asc {HOST}\""), "{what}");
}

async fn body_text(res: Response<Body>) -> String {
    let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
        .await
        .unwrap();
    String::from_utf8_lossy(&bytes).into_owned()
}

#[tokio::test]
async fn basic_bearer_and_token_are_accepted_for_the_same_token() {
    let spy = Spy::new();
    let app = public(&spy);
    let headers = [
        format!("Bearer {TOKEN}"),
        format!("bearer {TOKEN}"),
        format!("token {TOKEN}"),
        basic(&format!("x-access-token:{TOKEN}")),
        basic(&format!("oauth2:{TOKEN}")),
        basic(&format!("gitlab-ci-token:{TOKEN}")),
        basic(&format!("alice:{TOKEN}")),
        basic(&format!(":{TOKEN}")),
        // `https://TOKEN@host`: the token is the user name.
        basic(&format!("{TOKEN}:")),
    ];
    for route in ROUTES {
        for h in &headers {
            let res = send(&app, route.method, route.path, &[h]).await;
            assert_eq!(res.status(), route.allowed, "{} with {h}", route.name);
        }
    }
    // One authorizer call per request, never two.
    assert_eq!(
        spy.calls.load(Ordering::SeqCst),
        ROUTES.len() * headers.len()
    );
    let seen = spy.seen.lock().unwrap();
    assert!(seen.contains(&(Scheme::Basic, Some("alice".into()))));
    assert!(seen.contains(&(Scheme::Bearer, None)));
    assert!(seen.contains(&(Scheme::Token, None)));
}

#[tokio::test]
async fn a_token_written_with_basic_is_read_back_with_bearer() {
    let spy = Spy::new();
    let app = public(&spy);
    let path = "/v8/artifacts/feed01?slug=acme.web&teamId=team_x";
    let put = send(&app, "PUT", path, &[&basic(&format!("git:{TOKEN}"))]).await;
    assert_eq!(put.status(), StatusCode::ACCEPTED);
    let get = send(&app, "GET", path, &[&format!("Bearer {TOKEN}")]).await;
    assert_eq!(get.status(), StatusCode::OK);
    assert_eq!(body_text(get).await, "payload");
}

#[tokio::test]
async fn malformed_credentials_are_401_with_the_challenge_on_every_surface() {
    let spy = Spy::new();
    let app = public(&spy);
    let admin = admin().await;
    let mcp = mcp();
    let bad = [
        "Basic !!!not-base64",
        "Basic bm9jb2xvbg==", // "nocolon"
        "Basic /w==",         // not UTF-8
        "Basic",
        "Bearer",
        "Bearer two words",
        "token ",
    ];
    for h in bad {
        for route in ROUTES {
            let res = send(&app, route.method, route.path, &[h]).await;
            assert_challenged(&res, &format!("{} with {h:?}", route.name));
        }
        assert_challenged(&send(&admin, "GET", "/metrics", &[h]).await, h);
        assert_challenged(&send(&admin, "GET", "/readyz", &[h]).await, h);
        let res = send(&mcp, "POST", "/mcp", &[h]).await;
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED, "mcp {h:?}");
        assert!(challenges(&res)[0].starts_with("Basic realm="), "mcp {h:?}");
    }
    // Refused before any handler ran: the authorizer was never asked.
    assert_eq!(spy.calls.load(Ordering::SeqCst), 0);

    // gRPC answers UNAUTHENTICATED (16) in trailers-only form, with the same challenges.
    for h in bad {
        let req = Request::post("/asc.v1.Repo/HasObjects")
            .header(header::CONTENT_TYPE, "application/grpc")
            .header(header::AUTHORIZATION, h)
            .body(Body::empty())
            .unwrap();
        let res = app.clone().oneshot(req).await.unwrap();
        assert_eq!(res.headers()["grpc-status"], "16", "grpc {h:?}");
        let c = challenges(&res);
        assert!(c[0].starts_with("Basic realm="), "grpc {h:?}: {c:?}");
        assert!(c[1].starts_with("Bearer realm="), "grpc {h:?}: {c:?}");
    }
}

#[tokio::test]
async fn missing_and_wrong_credentials_get_the_same_challenge() {
    let spy = Spy::new();
    let app = public(&spy);
    let admin = admin().await;
    let wrong = [
        None,
        Some(format!("Bearer {TOKEN}x")),
        Some(basic("alice:wrong-password")),
        Some(basic(&format!("{TOKEN}x:"))),
    ];
    for route in ROUTES {
        for h in &wrong {
            let auth: Vec<&str> = h.iter().map(String::as_str).collect();
            let res = send(&app, route.method, route.path, &auth).await;
            assert_challenged(&res, &format!("{} with {h:?}", route.name));
        }
    }
    for h in &wrong {
        let auth: Vec<&str> = h.iter().map(String::as_str).collect();
        assert_challenged(&send(&admin, "GET", "/metrics", &auth).await, "admin");
    }
    // The admin token takes Bearer or Basic.
    for ok in [
        format!("Bearer {ADMIN}"),
        basic(&format!("admin:{ADMIN}")),
        basic(&format!(":{ADMIN}")),
    ] {
        assert_eq!(send(&admin, "GET", "/metrics", &[&ok]).await.status(), 200);
    }
}

#[tokio::test]
async fn mcp_adds_its_resource_metadata_challenge() {
    let res = send(&mcp(), "POST", "/mcp", &[]).await;
    assert_challenged(&res, "mcp");
    let c = challenges(&res);
    assert_eq!(
        c[2],
        "Bearer resource_metadata=\"https://asc.example.test/.well-known/oauth-protected-resource\", scope=\"asc:read\""
    );
}

#[tokio::test]
async fn oversized_repeated_unknown_and_query_credentials_are_refused_with_400() {
    let spy = Spy::new();
    let app = public(&spy);
    let huge_bearer = format!("Bearer {}", "x".repeat(9000));
    let huge_basic = basic(&format!("u:{}", "x".repeat(5000)));
    let long_secret = format!("Bearer {}", "x".repeat(4097));
    for route in ROUTES {
        for h in [
            huge_bearer.as_str(),
            huge_basic.as_str(),
            long_secret.as_str(),
            "Digest username=\"a\"",
            "Negotiate abc",
            "garbage",
        ] {
            let res = send(&app, route.method, route.path, &[h]).await;
            assert_eq!(
                res.status(),
                StatusCode::BAD_REQUEST,
                "{} {}",
                route.name,
                h.len()
            );
            assert!(challenges(&res).is_empty());
        }
        let two = [format!("Bearer {TOKEN}"), format!("Bearer {TOKEN}")];
        let two: Vec<&str> = two.iter().map(String::as_str).collect();
        let res = send(&app, route.method, route.path, &two).await;
        assert_eq!(
            res.status(),
            StatusCode::BAD_REQUEST,
            "{} two headers",
            route.name
        );
        // A token in the query string is refused even next to a valid header.
        let sep = if route.path.contains('?') { '&' } else { '?' };
        for key in ["token", "access_token", "private_token"] {
            let path = format!("{}{sep}{key}={TOKEN}", route.path);
            let res = send(&app, route.method, &path, &[&format!("Bearer {TOKEN}")]).await;
            assert_eq!(
                res.status(),
                StatusCode::BAD_REQUEST,
                "{} ?{key}",
                route.name
            );
        }
    }
    // Exactly at the limit is a credential like any other.
    let at_limit = format!("Bearer {}", "x".repeat(4096));
    let res = send(&app, "GET", ROUTES[0].path, &[&at_limit]).await;
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(spy.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn git_paths_challenge_anonymous_callers_then_say_where_git_is_served() {
    let spy = Spy::new();
    let app = public(&spy);
    let path = "/acme/web.git/info/refs?service=git-upload-pack";
    assert_challenged(&send(&app, "GET", path, &[]).await, "anonymous");
    for repo in [
        "/acme/web",
        "/acme/web.git",
        "/acme/nope.git",
        "/nobody/none",
    ] {
        let path = format!("{repo}/info/refs?service=git-upload-pack");
        let res = send(&app, "GET", &path, &[&format!("token {TOKEN}")]).await;
        assert_eq!(res.status(), StatusCode::NOT_FOUND, "{repo}");
        assert!(body_text(res).await.contains("served by the forge"));
    }
}

#[tokio::test]
async fn no_response_carries_the_credential() {
    let spy = Spy::new();
    let app = public(&spy);
    let admin = admin().await;
    let canary = "CANARY-secret-8f3a";
    let b64 = base64::engine::general_purpose::STANDARD.encode(format!("u:{canary}"));
    let headers = [
        format!("Bearer {canary}"),
        basic(&format!("u:{canary}")),
        format!("Basic {b64}!"),
        format!("Digest {canary}"),
        format!("Bearer {canary} {canary}"),
    ];
    for h in &headers {
        let mut all = Vec::new();
        for route in ROUTES {
            all.push(send(&app, route.method, route.path, &[h]).await);
        }
        all.push(send(&admin, "GET", "/metrics", &[h]).await);
        for res in all {
            let shown = format!("{:?}", res.headers());
            let status = res.status();
            let body = body_text(res).await;
            for text in [&shown, &body] {
                assert!(
                    !text.contains(canary) && !text.contains(&b64),
                    "{status}: {text}"
                );
            }
        }
    }
}

/// A real `git` against a minimal endpoint: the anonymous request meets the Basic challenge,
/// which makes git run its credential helper, and the retry carries the helper's answer.
#[tokio::test(flavor = "multi_thread")]
async fn real_git_asks_its_credential_helper_after_the_challenge() {
    let Ok(real_git) = asc_git::realgit::RealGit::resolve(None, None) else {
        eprintln!("git is not installed; skipping");
        return;
    };
    let spy = Spy::new();
    let log: Arc<Mutex<Vec<(bool, u16)>>> = Arc::default();
    let seen = log.clone();
    let app = authn::git_router(spy.clone()).layer(axum::middleware::from_fn(
        move |req: Request<Body>, next: axum::middleware::Next| {
            let seen = seen.clone();
            async move {
                let authed = req.headers().contains_key(header::AUTHORIZATION);
                let res = next.run(req).await;
                seen.lock().unwrap().push((authed, res.status().as_u16()));
                res
            }
        },
    ));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    let dir = tempfile::tempdir().unwrap();
    let fwd = |p: &std::path::Path| p.to_string_lossy().replace('\\', "/");
    let helper_log = dir.path().join("helper.log");
    let helper = dir.path().join("helper.sh");
    std::fs::write(
        &helper,
        format!(
            "#!/bin/sh\necho \"$1\" >> \"{}\"\nif [ \"$1\" = get ]; then\n  echo username=x-access-token\n  echo password={TOKEN}\nfi\ncat >/dev/null\n",
            fwd(&helper_log)
        ),
    )
    .unwrap();
    let empty_config = dir.path().join("gitconfig");
    std::fs::write(&empty_config, "").unwrap();

    let url = format!("http://{addr}/acme/web.git");
    let helper_arg = format!("credential.helper=!sh {}", fwd(&helper));
    let out = tokio::task::spawn_blocking(move || {
        real_git
            .command()
            .unwrap()
            .args(["-c", "credential.helper=", "-c", &helper_arg])
            .args(["ls-remote", &url])
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", &empty_config)
            .env_remove("GIT_ASKPASS")
            .env_remove("SSH_ASKPASS")
            .output()
            .unwrap()
    })
    .await
    .unwrap();

    // asc serve does not serve the git protocol, so git ends in "not found", as with any host.
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    assert!(!stderr.contains(TOKEN), "{stderr}");
    let asked = std::fs::read_to_string(&helper_log).unwrap_or_default();
    assert!(
        asked.lines().any(|l| l == "get"),
        "the helper was never asked; git said: {stderr}"
    );
    // Anonymous requests (git probes more than once) get 401; the one with the credential, 404.
    let log = log.lock().unwrap();
    let (last, anonymous) = log.split_last().unwrap();
    assert_eq!(*last, (true, 404), "{stderr}");
    assert!(!anonymous.is_empty() && anonymous.iter().all(|r| *r == (false, 401)));
    assert!(
        spy.seen
            .lock()
            .unwrap()
            .contains(&(Scheme::Basic, Some("x-access-token".into())))
    );
}
