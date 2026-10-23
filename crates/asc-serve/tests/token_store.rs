//! M30: the standalone token store. Create, use, expire and revoke; one meaning for Basic, Bearer
//! and `token` on RPC, `/v8`, `/seeds` and `/mcp`; deploy and CI scopes; the secret is nowhere in
//! the database; the table is bounded; the CLI functions.

use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};

use axum::Router;
use axum::body::Body;
use axum::extract::State;
use axum::http::{Request, Response, StatusCode, header};
use axum::response::IntoResponse;
use base64::Engine as _;
use asc_rpc::auth::{
    Access, AuthError, AuthInterceptor, Authorizer, BearerToken, Credential, Principal,
};
use asc_rpc::server::ServerInfo;
use asc_serve::authn::{self, Surface};
use asc_serve::authz::token_cli::{self, CreateArgs};
use asc_serve::authz::token_store::{
    CACHE_TTL_SECS, DEAD_RETENTION_SECS, DEFAULT_CI_LIFETIME_SECS, MAX_LIVE_TOKENS, NewToken,
    TOKEN_LEN, TokenError, TokenKey, TokenKind, TokenStore, TokenStoreAuthz,
};
use asc_serve::config::{ServeConfig, TurboConfig};
use asc_serve::http::Metrics;
use asc_serve::registry::Extensions;
use asc_serve::seeds::{SeedStore, SeedsState};
use asc_serve::turbo::TurboState;
use asc_serve::{build_routes, migrate, seeds, turbo};
use object_store::memory::InMemory;
use object_store::path::Path as StorePath;
use sqlx::{AnyPool, Row};
use tonic::service::Interceptor;
use tower::ServiceExt;

const T0: i64 = 1_800_000_000;

struct Fixture {
    pool: AnyPool,
    store: Arc<TokenStore>,
    authz: Arc<TokenStoreAuthz>,
    clock: Arc<AtomicI64>,
}

async fn fixture() -> Fixture {
    let pool = migrate::connect("sqlite::memory:", 1).await.unwrap();
    migrate::migrate(&pool).await.unwrap();
    let clock = Arc::new(AtomicI64::new(T0));
    let c = clock.clone();
    let store = Arc::new(
        TokenStore::new(pool.clone(), TokenKey::generate().unwrap())
            .with_clock(Arc::new(move || c.load(Ordering::SeqCst))),
    );
    Fixture {
        pool,
        authz: Arc::new(TokenStoreAuthz::new(store.clone())),
        store,
        clock,
    }
}

impl Fixture {
    fn advance(&self, secs: i64) {
        self.clock.fetch_add(secs, Ordering::SeqCst);
    }

    async fn mint(&self, kind: TokenKind, user: Option<&str>, repos: &[&str], a: Access) -> String {
        self.mint_for(kind, user, repos, a, None).await
    }

    async fn mint_for(
        &self,
        kind: TokenKind,
        user: Option<&str>,
        repos: &[&str],
        access: Access,
        expires_in: Option<i64>,
    ) -> String {
        self.store
            .create(&NewToken {
                kind,
                user: user.map(str::to_owned),
                repos: repos.iter().map(|r| (*r).to_owned()).collect(),
                access,
                expires_in,
                label: String::new(),
            })
            .await
            .unwrap()
            .secret()
            .to_owned()
    }

    async fn check(&self, token: &str, repo: &str, access: Access) -> Result<Principal, AuthError> {
        let t = BearerToken::new(token).unwrap();
        self.authz.authorize(Some(&t), repo, access).await
    }
}

fn basic(raw: &str) -> String {
    format!(
        "Basic {}",
        base64::engine::general_purpose::STANDARD.encode(raw)
    )
}

#[tokio::test]
async fn create_use_expire_and_revoke() {
    let f = fixture().await;
    let pat = f
        .mint_for(
            TokenKind::Pat,
            Some("alice"),
            &["acme/web"],
            Access::Write,
            Some(1000),
        )
        .await;
    assert_eq!(pat.len(), TOKEN_LEN);
    assert!(pat.starts_with("asp_"));

    // Use.
    let p = f.check(&pat, "acme/web", Access::Write).await.unwrap();
    assert_eq!(p.id, "pat:alice");
    assert_eq!(
        f.check(&pat, "acme/web", Access::Read).await.unwrap().id,
        "pat:alice"
    );

    // Anonymous and unknown tokens are 401-class.
    assert_eq!(
        f.authz.authorize(None, "acme/web", Access::Read).await,
        Err(AuthError::Unauthenticated)
    );
    let unknown = format!("asp_{}_{}", "0".repeat(16), "A".repeat(43));
    assert_eq!(
        f.check(&unknown, "acme/web", Access::Read).await,
        Err(AuthError::Unauthenticated)
    );

    // Expiry: the last second before and the second of.
    f.advance(999);
    f.check(&pat, "acme/web", Access::Read).await.unwrap();
    f.advance(1);
    assert_eq!(
        f.check(&pat, "acme/web", Access::Read).await,
        Err(AuthError::Unauthenticated)
    );

    // Revoke through this store: effective at once.
    let live = f
        .mint(TokenKind::Pat, Some("bob"), &["acme/web"], Access::Read)
        .await;
    f.check(&live, "acme/web", Access::Read).await.unwrap();
    let id = live[4..20].to_owned();
    assert!(f.store.revoke(&id).await.unwrap());
    assert!(!f.store.revoke(&id).await.unwrap());
    assert_eq!(
        f.check(&live, "acme/web", Access::Read).await,
        Err(AuthError::Unauthenticated)
    );
    assert!(matches!(
        f.store.revoke("ffffffffffffffff").await,
        Err(TokenError::NotFound)
    ));
}

#[tokio::test]
async fn a_revocation_by_another_process_lands_within_the_negative_cache_bound() {
    let f = fixture().await;
    let t = f
        .mint(TokenKind::Pat, Some("alice"), &["acme/web"], Access::Read)
        .await;
    f.check(&t, "acme/web", Access::Read).await.unwrap();
    // The CLI is another process: it writes the row, this store's cache is not told.
    sqlx::query("UPDATE auth_tokens SET revoked = $1")
        .bind(T0)
        .execute(&f.pool)
        .await
        .unwrap();
    f.advance(CACHE_TTL_SECS - 1);
    f.check(&t, "acme/web", Access::Read).await.unwrap();
    f.advance(1);
    assert_eq!(
        f.check(&t, "acme/web", Access::Read).await,
        Err(AuthError::Unauthenticated)
    );
}

#[tokio::test]
async fn a_wrong_or_altered_token_never_authorises() {
    let f = fixture().await;
    let t = f
        .mint(TokenKind::Pat, Some("alice"), &["acme/web"], Access::Admin)
        .await;
    f.check(&t, "acme/web", Access::Admin).await.unwrap();
    // Right id, wrong secret (each character position), wrong kind prefix, wrong length.
    for i in 4..t.len() {
        let mut bytes = t.clone().into_bytes();
        bytes[i] = if bytes[i] == b'B' { b'C' } else { b'B' };
        let bad = String::from_utf8(bytes).unwrap();
        assert_eq!(
            f.check(&bad, "acme/web", Access::Read).await,
            Err(AuthError::Unauthenticated),
            "position {i}"
        );
    }
    for bad in [
        format!("asd_{}", &t[4..]),
        format!("asc_{}", &t[4..]),
        t[..t.len() - 1].to_owned(),
        format!("{t}A"),
        t.to_uppercase(),
    ] {
        assert_eq!(
            f.check(&bad, "acme/web", Access::Read).await,
            Err(AuthError::Unauthenticated)
        );
    }
    // A key that is not the server's makes every stored digest wrong.
    let other = TokenStore::new(f.pool.clone(), TokenKey::generate().unwrap());
    let other = TokenStoreAuthz::new(Arc::new(other));
    assert_eq!(
        other
            .authorize(
                Some(&BearerToken::new(&t).unwrap()),
                "acme/web",
                Access::Read
            )
            .await,
        Err(AuthError::Unauthenticated)
    );
}

#[tokio::test]
async fn the_plaintext_token_is_not_in_the_database() {
    let f = fixture().await;
    let tokens = [
        f.mint(TokenKind::Pat, Some("alice"), &["acme/*"], Access::Write)
            .await,
        f.mint(TokenKind::Deploy, None, &["acme/web"], Access::Read)
            .await,
        f.mint(TokenKind::Ci, None, &["acme/web"], Access::Read)
            .await,
    ];
    let mut dump: Vec<u8> = Vec::new();
    for table in ["auth_tokens", "auth_token_grants", "auth_identity"] {
        let rows = sqlx::query(sqlx::AssertSqlSafe(format!("SELECT * FROM {table}")))
            .fetch_all(&f.pool)
            .await
            .unwrap();
        for row in rows {
            for i in 0..sqlx::Row::len(&row) {
                if let Ok(s) = row.try_get::<String, _>(i) {
                    dump.extend_from_slice(s.as_bytes());
                } else if let Ok(b) = row.try_get::<Vec<u8>, _>(i) {
                    dump.extend_from_slice(&b);
                    dump.extend_from_slice(hex(&b).as_bytes());
                }
            }
        }
    }
    let text = String::from_utf8_lossy(&dump);
    for t in &tokens {
        let secret = &t[21..];
        assert!(!text.contains(t.as_str()));
        assert!(!text.contains(secret));
        // Not a plain digest of the token either.
        let sha = sha2_hex(t);
        assert!(!text.contains(&sha));
        let b3 = blake3::hash(t.as_bytes()).to_hex().to_string();
        assert!(!text.contains(&b3));
    }
    // The digest column is 32 bytes (HMAC-SHA256) for each token.
    let lens: Vec<i64> = sqlx::query_scalar("SELECT LENGTH(digest) FROM auth_tokens")
        .fetch_all(&f.pool)
        .await
        .unwrap();
    assert_eq!(lens, vec![32, 32, 32]);
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn sha2_hex(s: &str) -> String {
    use sha2::{Digest, Sha256};
    hex(&Sha256::digest(s.as_bytes()))
}

#[tokio::test]
async fn a_deploy_token_is_confined_to_its_repo_and_its_mode() {
    let f = fixture().await;
    let ro = f
        .mint(TokenKind::Deploy, None, &["acme/web"], Access::Read)
        .await;
    let rw = f
        .mint(TokenKind::Deploy, None, &["acme/web"], Access::Write)
        .await;
    assert!(ro.starts_with("asd_"));

    let p = f.check(&ro, "acme/web", Access::Read).await.unwrap();
    assert_eq!(p.id, "deploy:acme/web");
    assert_eq!(
        f.check(&ro, "acme/web", Access::Write).await,
        Err(AuthError::Forbidden),
        "a read deploy token cannot write"
    );
    f.check(&rw, "acme/web", Access::Write).await.unwrap();
    for other in ["acme/api", "acme/web2", "evil/web", "acme/web/x"] {
        for t in [&ro, &rw] {
            for a in [Access::Read, Access::Write] {
                assert_eq!(
                    f.check(t, other, a).await,
                    Err(AuthError::Forbidden),
                    "{other} {a:?}"
                );
            }
        }
    }
    // Over-scoped: admin is above any deploy grant.
    assert_eq!(
        f.check(&rw, "acme/web", Access::Admin).await,
        Err(AuthError::Forbidden)
    );
    // Rules at mint time.
    let mint = |kind, user: Option<&str>, repos: &[&str], access| {
        let store = f.store.clone();
        let req = NewToken {
            kind,
            user: user.map(str::to_owned),
            repos: repos.iter().map(|r| (*r).to_owned()).collect(),
            access,
            expires_in: None,
            label: String::new(),
        };
        async move { store.create(&req).await.map(|_| ()) }
    };
    assert!(
        mint(TokenKind::Deploy, Some("alice"), &["a/b"], Access::Read)
            .await
            .is_err()
    );
    assert!(
        mint(TokenKind::Deploy, None, &["a/b", "a/c"], Access::Read)
            .await
            .is_err()
    );
    assert!(
        mint(TokenKind::Deploy, None, &["a/*"], Access::Read)
            .await
            .is_err()
    );
    assert!(
        mint(TokenKind::Deploy, None, &["a/b"], Access::Admin)
            .await
            .is_err()
    );
    assert!(
        mint(TokenKind::Deploy, None, &[], Access::Read)
            .await
            .is_err()
    );
    assert!(
        mint(TokenKind::Pat, None, &["a/b"], Access::Read)
            .await
            .is_err()
    );
    assert!(
        mint(TokenKind::Pat, Some("a:b"), &["a/b"], Access::Read)
            .await
            .is_err()
    );
    assert!(
        mint(TokenKind::Pat, Some("a"), &["nonsense"], Access::Read)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn pat_globs_and_the_strongest_grant_win() {
    let f = fixture().await;
    let pat = f
        .mint(TokenKind::Pat, Some("alice"), &["acme/*"], Access::Write)
        .await;
    f.check(&pat, "acme/web", Access::Write).await.unwrap();
    assert_eq!(
        f.check(&pat, "other/web", Access::Read).await,
        Err(AuthError::Forbidden)
    );
    assert_eq!(
        f.check(&pat, "acme/web", Access::Admin).await,
        Err(AuthError::Forbidden)
    );
    let all = f
        .mint(TokenKind::Pat, Some("root"), &["*"], Access::Admin)
        .await;
    f.check(&all, "any/thing", Access::Admin).await.unwrap();
}

#[tokio::test]
async fn ci_tokens_always_expire_and_are_never_admin() {
    let f = fixture().await;
    let ci = f
        .mint(TokenKind::Ci, None, &["acme/*"], Access::Write)
        .await;
    assert!(ci.starts_with("asc_"));
    let p = f.check(&ci, "acme/web", Access::Write).await.unwrap();
    assert!(p.id.starts_with("ci:"));
    f.advance(DEFAULT_CI_LIFETIME_SECS);
    assert_eq!(
        f.check(&ci, "acme/web", Access::Read).await,
        Err(AuthError::Unauthenticated)
    );
    for (repos, access, life) in [
        (vec!["acme/web"], Access::Admin, None),
        (vec!["*"], Access::Read, None),
        (vec!["acme/web"], Access::Read, Some(31 * 86_400)),
    ] {
        let r = f
            .store
            .create(&NewToken {
                kind: TokenKind::Ci,
                user: None,
                repos: repos.into_iter().map(str::to_owned).collect(),
                access,
                expires_in: life,
                label: String::new(),
            })
            .await;
        assert!(r.is_err());
    }
}

/// The same token, however it is presented, reaches the same principal on RPC, `/v8`, `/seeds`
/// and `/mcp`, through the one parser.
#[tokio::test]
async fn one_token_authorises_rpc_v8_and_mcp_identically() {
    let f = fixture().await;
    let rw = f
        .mint(TokenKind::Pat, Some("alice"), &["acme/web"], Access::Write)
        .await;
    let ro = f
        .mint(TokenKind::Deploy, None, &["acme/web"], Access::Read)
        .await;
    let headers = |t: &str| {
        vec![
            format!("Bearer {t}"),
            format!("token {t}"),
            basic(&format!("x-access-token:{t}")),
            basic(&format!("alice:{t}")),
            basic(&format!("{t}:")),
        ]
    };

    // RPC: what `RepoService` does with a request, interceptor then authorizer.
    for h in headers(&rw) {
        let mut req = tonic::Request::new(());
        req.metadata_mut()
            .insert("authorization", h.parse().unwrap());
        let req = AuthInterceptor.call(req).unwrap();
        let cred = req.extensions().get::<Credential>();
        let p = f
            .authz
            .authorize_credential(cred, "acme/web", Access::Write)
            .await
            .unwrap();
        assert_eq!(p.id, "pat:alice", "{h}");
    }

    // HTTP: `/v8`, `/seeds`, and a stand-in for `/mcp` behind the same edge.
    let app = routes(f.authz.clone());
    let mcp = mcp(f.authz.clone());
    let put = "/v8/artifacts/abc123?slug=acme.web&teamId=team_x";
    for h in headers(&rw) {
        assert_eq!(
            send(&app, "PUT", put, &h).await,
            StatusCode::ACCEPTED,
            "{h}"
        );
        assert_eq!(send(&app, "GET", put, &h).await, StatusCode::OK, "{h}");
        assert_eq!(
            send(&app, "GET", "/seeds/acme/web/latest", &h).await,
            StatusCode::NOT_FOUND,
            "{h}"
        );
        assert_eq!(send(&mcp, "POST", "/mcp", &h).await, StatusCode::OK, "{h}");
    }
    // The read-only deploy token reads and cannot write, however it is presented.
    for h in headers(&ro) {
        // 404, not 403: the read is allowed; turbo scopes artifacts per principal, so it just
        // does not see the ones `alice` wrote.
        assert_eq!(
            send(&app, "GET", put, &h).await,
            StatusCode::NOT_FOUND,
            "{h}"
        );
        assert_eq!(
            send(&app, "PUT", put, &h).await,
            StatusCode::FORBIDDEN,
            "{h}"
        );
        assert_eq!(send(&mcp, "POST", "/mcp", &h).await, StatusCode::OK, "{h}");
    }
    // A token for another repo is refused on every route.
    let elsewhere = format!(
        "Bearer {}",
        f.mint(TokenKind::Pat, Some("eve"), &["eve/own"], Access::Admin)
            .await
    );
    assert_eq!(
        send(&app, "GET", put, &elsewhere).await,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        send(&mcp, "POST", "/mcp", &elsewhere).await,
        StatusCode::FORBIDDEN
    );
    // A revoked token is a 401 with the challenge on every route.
    let id = rw[4..20].to_owned();
    f.store.revoke(&id).await.unwrap();
    for (router, method, path) in [(&app, "GET", put), (&mcp, "POST", "/mcp")] {
        let res = send_full(router, method, path, &format!("Bearer {rw}")).await;
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
        assert!(
            res.headers()[header::WWW_AUTHENTICATE]
                .to_str()
                .unwrap()
                .starts_with("Basic realm=")
        );
    }
}

fn routes(authz: Arc<TokenStoreAuthz>) -> Router {
    let ext = Extensions::new();
    let mem = Arc::new(InMemory::new());
    let store = SeedStore::new(mem.clone(), StorePath::from("root"));
    seeds::register(
        &ext,
        Arc::new(SeedsState::new(authz.clone(), Arc::new(store))),
    );
    let state = TurboState::new(
        authz.clone(),
        mem,
        StorePath::from("root"),
        TurboConfig::default(),
        Arc::new(Metrics::new()),
    );
    turbo::register(&ext, Arc::new(state));
    build_routes(&ext, authz, ServerInfo::default()).into_axum_router()
}

/// What `/mcp` will be (M13): a POST route behind the edge that asks the authorizer.
fn mcp(authz: Arc<TokenStoreAuthz>) -> Router {
    async fn handler(
        State(authz): State<Arc<TokenStoreAuthz>>,
        headers: axum::http::HeaderMap,
    ) -> axum::response::Response {
        let cred = authn::credential(&headers).ok().flatten();
        match authz
            .authorize_credential(cred.as_ref(), "acme/web", Access::Read)
            .await
        {
            Ok(_) => StatusCode::OK.into_response(),
            Err(AuthError::Forbidden) => StatusCode::FORBIDDEN.into_response(),
            Err(_) => StatusCode::UNAUTHORIZED.into_response(),
        }
    }
    let routes = Router::new()
        .route("/mcp", axum::routing::post(handler))
        .with_state(authz);
    authn::guard(
        routes,
        Surface::Mcp {
            resource_metadata: Arc::from("https://asc.test/.well-known/oauth-protected-resource"),
        },
    )
}

async fn send_full(app: &Router, method: &str, path: &str, auth: &str) -> Response<Body> {
    let req = Request::builder()
        .method(method)
        .uri(path)
        .header(header::HOST, "asc.test")
        .header(header::AUTHORIZATION, auth)
        .body(Body::from("payload"))
        .unwrap();
    app.clone().oneshot(req).await.unwrap()
}

async fn send(app: &Router, method: &str, path: &str, auth: &str) -> StatusCode {
    send_full(app, method, path, auth).await.status()
}

#[tokio::test]
async fn the_table_is_bounded_and_old_dead_rows_are_pruned() {
    let f = fixture().await;
    // Fill the live set to its cap directly (creating 10 000 tokens one by one is slow).
    sqlx::query(
        "WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < $1) \
         INSERT INTO auth_tokens (id, kind, principal, digest, label, created) \
         SELECT 'fill' || i, 'pat', 'u', x'00', '', 1 FROM n",
    )
    .bind(MAX_LIVE_TOKENS)
    .execute(&f.pool)
    .await
    .unwrap();
    let req = NewToken {
        kind: TokenKind::Pat,
        user: Some("alice".into()),
        repos: vec!["a/b".into()],
        access: Access::Read,
        expires_in: None,
        label: String::new(),
    };
    assert!(matches!(
        f.store.create(&req).await,
        Err(TokenError::TableFull)
    ));
    // Revoke one: there is room again. A row revoked long ago is deleted by the next create.
    sqlx::query("UPDATE auth_tokens SET revoked = $1 WHERE id = 'fill1'")
        .bind(T0 - DEAD_RETENTION_SECS - 10)
        .execute(&f.pool)
        .await
        .unwrap();
    f.store.create(&req).await.unwrap();
    let gone: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM auth_tokens WHERE id = 'fill1'")
        .fetch_one(&f.pool)
        .await
        .unwrap();
    assert_eq!(gone, 0);
    assert!(matches!(
        f.store.create(&req).await,
        Err(TokenError::TableFull)
    ));
}

#[tokio::test]
async fn too_many_repos_and_bad_labels_are_refused() {
    let f = fixture().await;
    let many: Vec<String> = (0..40).map(|i| format!("acme/r{i}")).collect();
    let mut req = NewToken {
        kind: TokenKind::Pat,
        user: Some("alice".into()),
        repos: many,
        access: Access::Read,
        expires_in: None,
        label: String::new(),
    };
    assert!(f.store.create(&req).await.is_err());
    req.repos = vec!["a/b".into()];
    req.label = "x".repeat(200);
    assert!(f.store.create(&req).await.is_err());
    req.label = "line\nbreak".into();
    assert!(f.store.create(&req).await.is_err());
    req.label = "ci laptop".into();
    f.store.create(&req).await.unwrap();
}

#[tokio::test]
async fn list_shows_status_and_never_a_secret() {
    let f = fixture().await;
    let live = f
        .mint(TokenKind::Pat, Some("alice"), &["acme/web"], Access::Read)
        .await;
    let gone = f
        .mint(TokenKind::Deploy, None, &["acme/web"], Access::Write)
        .await;
    let short = f
        .mint_for(
            TokenKind::Pat,
            Some("bob"),
            &["a/b"],
            Access::Read,
            Some(10),
        )
        .await;
    f.store.revoke(&gone[4..20]).await.unwrap();
    f.advance(10);
    let rows = f.store.list().await.unwrap();
    assert_eq!(rows.len(), 3);
    let now = f.store.now();
    let status = |t: &str| {
        rows.iter()
            .find(|r| r.id == t[4..20])
            .map(|r| r.status(now))
            .unwrap()
    };
    assert_eq!(status(&live), "active");
    assert_eq!(status(&gone), "revoked");
    assert_eq!(status(&short), "expired");
    for r in &rows {
        assert!(!format!("{r:?}").contains(&live[21..]));
        assert!(!r.grants.is_empty());
    }
}

#[tokio::test]
async fn identities_link_oidc_subjects_to_principals() {
    let f = fixture().await;
    assert_eq!(
        f.store
            .resolve_identity("https://idp", "sub-1")
            .await
            .unwrap(),
        None
    );
    f.store
        .link_identity("https://idp", "sub-1", "alice")
        .await
        .unwrap();
    f.store
        .link_identity("https://idp", "sub-1", "alice2")
        .await
        .unwrap();
    assert_eq!(
        f.store
            .resolve_identity("https://idp", "sub-1")
            .await
            .unwrap(),
        Some("alice2".into())
    );
    assert_eq!(
        f.store
            .resolve_identity("https://other", "sub-1")
            .await
            .unwrap(),
        None
    );
    assert!(f.store.link_identity("", "s", "p").await.is_err());
}

#[test]
fn the_key_file_is_private_and_never_overwritten() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("token.key");
    TokenKey::load_or_create(&path).unwrap();
    let first = std::fs::read_to_string(&path).unwrap();
    assert_eq!(first.trim().len(), 64);
    TokenKey::load_or_create(&path).unwrap();
    assert_eq!(std::fs::read_to_string(&path).unwrap(), first);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(TokenKey::load(&path).is_err());
    }
    // A server never makes a key: loading a missing file fails.
    assert!(TokenKey::load(&dir.path().join("missing")).is_err());
    std::fs::write(dir.path().join("short"), "abcd\n").unwrap();
    assert!(TokenKey::load(&dir.path().join("short")).is_err());
}

/// Windows: the key file's access list names only the current user and SYSTEM, and a file that
/// grants anyone else access is refused.
#[cfg(windows)]
#[test]
fn the_windows_key_file_allows_only_the_user_and_system() {
    use std::process::Command;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("token.key");
    TokenKey::load_or_create(&path).unwrap();
    TokenKey::load(&path).unwrap();

    // The list is protected (no inherited entries) and has two entries.
    let shown = Command::new("icacls").arg(&path).output().unwrap();
    let shown = String::from_utf8_lossy(&shown.stdout).into_owned();
    for other in ["Everyone", "Users", "Administrators", "Authenticated"] {
        assert!(!shown.contains(other), "{shown}");
    }
    assert!(shown.contains("SYSTEM"), "{shown}");
    assert!(!shown.contains("(I)"), "no inherited entries: {shown}");

    // Granting anyone else read access makes the key unusable.
    for sid in ["*S-1-1-0", "*S-1-5-32-545", "*S-1-5-32-544"] {
        let ok = Command::new("icacls")
            .arg(&path)
            .args(["/grant", &format!("{sid}:R")])
            .status()
            .unwrap()
            .success();
        assert!(ok);
        let err = TokenKey::load(&path).unwrap_err().to_string();
        assert!(err.contains("grants access to"), "{sid}: {err}");
        assert!(
            Command::new("icacls")
                .arg(&path)
                .args(["/remove:g", sid])
                .status()
                .unwrap()
                .success()
        );
        TokenKey::load(&path).unwrap();
    }

    // A file with no protection of its own (it inherits the directory's list, which names
    // Administrators or other accounts) is refused even though its text is a valid key.
    let loose = dir.path().join("loose.key");
    std::fs::write(
        &loose,
        format!(
            "{}
",
            "ab".repeat(32)
        ),
    )
    .unwrap();
    let ok = Command::new("icacls")
        .arg(&loose)
        .args(["/grant", "*S-1-1-0:R"])
        .status()
        .unwrap()
        .success();
    assert!(ok);
    assert!(TokenKey::load(&loose).is_err());
}

/// A PAT user named like another principal gets a distinct principal id and a distinct turbo
/// scope: nothing one token writes is visible to a token of a look-alike principal.
#[tokio::test]
async fn look_alike_principals_have_distinct_ids_and_scopes() {
    let f = fixture().await;
    let app = routes(f.authz.clone());
    let put = |hash: &str| format!("/v8/artifacts/{hash}?slug=acme.web&teamId=team_x");

    // (user or None for a deploy token, repo grant)
    let cases: [(Option<&str>, &str); 6] = [
        (Some("deploy_acme_web"), "acme/web"), // cleaned like `deploy:acme/web`
        (None, "acme/web"),                    // deploy:acme/web
        (Some("a.b"), "acme/web"),             // cleaned like a_b
        (Some("a_b"), "acme/web"),
        (Some("forgejo-internal"), "acme/web"), // must not equal the forge's internal principal
        (Some("alice"), "acme/web"),
    ];
    let mut tokens = Vec::new();
    for (user, repo) in cases {
        let kind = if user.is_some() {
            TokenKind::Pat
        } else {
            TokenKind::Deploy
        };
        tokens.push(f.mint(kind, user, &[repo], Access::Write).await);
    }
    let mut ids = std::collections::HashSet::new();
    for t in &tokens {
        let id = f.check(t, "acme/web", Access::Write).await.unwrap().id;
        assert_ne!(id, asc_serve::forgejo::INTERNAL_PRINCIPAL);
        assert!(ids.insert(id));
    }
    // Each token writes its own artifact; no other token can read it back.
    for (i, t) in tokens.iter().enumerate() {
        let auth = format!("Bearer {t}");
        let hash = format!("h{i}");
        assert_eq!(
            send(&app, "PUT", &put(&hash), &auth).await,
            StatusCode::ACCEPTED
        );
        assert_eq!(send(&app, "GET", &put(&hash), &auth).await, StatusCode::OK);
    }
    for (i, _) in tokens.iter().enumerate() {
        for (j, t) in tokens.iter().enumerate() {
            if i != j {
                let got = send(&app, "GET", &put(&format!("h{i}")), &format!("Bearer {t}")).await;
                assert_eq!(
                    got,
                    StatusCode::NOT_FOUND,
                    "token {j} saw the artifact of token {i}"
                );
            }
        }
    }

    // Deploy tokens of repositories whose names clean to the same text stay apart too.
    let a = f
        .mint(TokenKind::Deploy, None, &["acme/we_b"], Access::Write)
        .await;
    let b = f
        .mint(TokenKind::Deploy, None, &["acme_we/b"], Access::Write)
        .await;
    let pa = f.check(&a, "acme/we_b", Access::Write).await.unwrap().id;
    let pb = f.check(&b, "acme_we/b", Access::Write).await.unwrap().id;
    assert_ne!(pa, pb);
}

fn config(dir: &std::path::Path) -> ServeConfig {
    let db = dir.join("asc.db").to_string_lossy().replace('\\', "/");
    let key = dir.join("token.key").to_string_lossy().replace('\\', "/");
    ServeConfig::parse(&format!(
        "store = \"memory:///\"\ndb = \"sqlite:{db}?mode=rwc\"\n[auth]\nprovider = \"token_store\"\ntoken_key_file = \"{key}\"\n"
    ))
    .unwrap()
}

#[tokio::test]
async fn the_cli_creates_lists_and_revokes() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = config(dir.path());
    asc_serve::run_migrate(&cfg).await.unwrap();

    let made = token_cli::create(
        &cfg,
        CreateArgs {
            kind: TokenKind::Deploy,
            user: None,
            repos: vec!["acme/web".into()],
            access: Access::Write,
            expires: Some("30d".into()),
            label: "ci runner".into(),
        },
    )
    .await
    .unwrap();
    assert_eq!(made.token.len(), TOKEN_LEN);
    assert!(made.summary.contains("deploy:acme/web"));
    assert!(made.summary.contains("only time"));
    assert!(!made.summary.contains(&made.token[21..]));
    let id = made.token[4..20].to_owned();

    // The running server (a separate authorizer over the same database) accepts it.
    let server = asc_serve::authz::authorizer(&cfg).await.unwrap();
    let t = BearerToken::new(&made.token).unwrap();
    assert_eq!(
        server
            .authorize(Some(&t), "acme/web", Access::Write)
            .await
            .unwrap()
            .id,
        "deploy:acme/web"
    );

    let listed = token_cli::list(&cfg).await.unwrap();
    assert!(listed.contains(&id));
    assert!(listed.contains("active"));
    assert!(listed.contains("acme/web:write"));
    assert!(listed.contains("ci runner"));
    assert!(!listed.contains(&made.token[21..]));

    let said = token_cli::revoke(&cfg, &id).await.unwrap();
    assert!(said.contains("revoked"));
    assert!(
        token_cli::revoke(&cfg, &id)
            .await
            .unwrap()
            .contains("already")
    );
    assert!(token_cli::revoke(&cfg, "ffffffffffffffff").await.is_err());
    assert!(token_cli::list(&cfg).await.unwrap().contains("revoked"));

    // The server's cache lets the old answer live for the bound, then refuses.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(8);
    loop {
        match server.authorize(Some(&t), "acme/web", Access::Read).await {
            Err(AuthError::Unauthenticated) => break,
            _ if std::time::Instant::now() > deadline => {
                panic!("a revoked token was still accepted after the cache bound")
            }
            _ => tokio::time::sleep(std::time::Duration::from_millis(250)).await,
        }
    }
}

#[tokio::test]
async fn the_server_refuses_to_start_without_its_key_and_the_cli_validates() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = config(dir.path());
    asc_serve::run_migrate(&cfg).await.unwrap();
    // No key file yet: the authorizer cannot be built (it would mint a key nothing matches).
    assert!(asc_serve::authz::authorizer(&cfg).await.is_err());
    // `list` and `revoke` do not make one either.
    assert!(token_cli::list(&cfg).await.is_err());
    assert!(
        token_cli::create(
            &cfg,
            CreateArgs {
                kind: TokenKind::Ci,
                user: None,
                repos: vec!["acme/web".into()],
                access: Access::Admin,
                expires: None,
                label: String::new(),
            },
        )
        .await
        .is_err()
    );
    assert!(
        token_cli::create(
            &cfg,
            CreateArgs {
                kind: TokenKind::Pat,
                user: Some("a".into()),
                repos: vec!["a/b".into()],
                access: Access::Read,
                expires: Some("soon".into()),
                label: String::new(),
            },
        )
        .await
        .is_err()
    );
}

#[test]
fn config_validates_the_auth_keys() {
    let base = "store = \"memory:///\"\ndb = \"sqlite:x.db\"\n";
    assert!(ServeConfig::parse(&format!("{base}[auth]\nprovider = \"token_store\"\n")).is_err());
    assert!(ServeConfig::parse(&format!("{base}[auth]\nprovider = \"forge\"\n")).is_err());
    assert!(ServeConfig::parse(&format!("{base}[auth]\nprovider = \"nope\"\n")).is_err());
    assert!(ServeConfig::parse(&format!("{base}[auth]\nsurprise = 1\n")).is_err());
    // token_store runs with no [forgejo] section.
    let cfg = ServeConfig::parse(&format!(
        "{base}[auth]\nprovider = \"token_store\"\ntoken_key_file = \"/etc/asc/k\"\n"
    ))
    .unwrap();
    assert!(cfg.forgejo.is_none());
    // Absent provider with [forgejo] set means forge.
    let cfg =
        ServeConfig::parse(&format!("{base}[forgejo]\nurl = \"https://f.example\"\n")).unwrap();
    assert_eq!(
        cfg.auth_provider(),
        Some(asc_serve::config::AuthProvider::Forge)
    );
}
