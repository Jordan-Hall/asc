//! M33: optional OAuth 2.1 / OIDC on `/mcp`. The protected-resource metadata lists the configured
//! issuers; an opaque (forge) token goes to the wrapped authorizer unchanged; a JWT of a
//! configured issuer is accepted only with a verified signature, `iss`, `exp`, `nbf`, `aud` (the
//! canonical `/mcp` URL) and a subject linked through `auth_identity`; its rights are the linked
//! user's, a `scope` claim only narrows them; no token is forwarded.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64;
use asc_rpc::auth::{Access, AuthError, Authorizer, BearerToken, Credential, Principal};
use asc_serve::authz::token_store::{NewToken, TokenKey, TokenKind, TokenStore};
use asc_serve::mcp::HttpPolicy;
use asc_serve::mcp::oauth::{
    HttpKeySource, KeyError, KeySet, KeySource, McpOauthConfig, OauthAuthorizer, OidcVerifier, audience, wrap,
};
use asc_serve::mcp::prm::{Prm, router};
use asc_serve::migrate;
use ring::rand::SystemRandom;
use ring::signature::{ECDSA_P256_SHA256_FIXED_SIGNING, EcdsaKeyPair, KeyPair};
use serde_json::{Value, json};
use tower::ServiceExt;

const FORGE: &str = "https://forge.example.org";
const IDP: &str = "https://idp.example.org";
const AUD: &str = "https://asc.example.org/mcp";
const NOW: i64 = 1_800_000_000;

struct Signer {
    pair: EcdsaKeyPair,
    kid: String,
}

impl Signer {
    fn new(kid: &str) -> Self {
        let rng = SystemRandom::new();
        let pkcs8 = EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, &rng).unwrap();
        let pair = EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, pkcs8.as_ref(), &rng).unwrap();
        Self { pair, kid: kid.into() }
    }

    fn jwk(&self) -> Value {
        let p = self.pair.public_key().as_ref();
        json!({"kty": "EC", "crv": "P-256", "use": "sig", "kid": self.kid,
               "x": B64.encode(&p[1..33]), "y": B64.encode(&p[33..65])})
    }

    fn sign(&self, header: &Value, claims: &Value) -> String {
        let input = format!("{}.{}", B64.encode(header.to_string()), B64.encode(claims.to_string()));
        let sig = self.pair.sign(&SystemRandom::new(), input.as_bytes()).unwrap();
        format!("{input}.{}", B64.encode(sig.as_ref()))
    }

    fn token(&self, claims: &Value) -> String {
        self.sign(&json!({"alg": "ES256", "kid": self.kid, "typ": "JWT"}), claims)
    }
}

fn claims() -> Value {
    json!({"iss": IDP, "sub": "sub-alice", "aud": AUD, "exp": NOW + 600})
}

fn with(mut c: Value, k: &str, v: Value) -> Value {
    c[k] = v;
    c
}

fn without(mut c: Value, k: &str) -> Value {
    c.as_object_mut().unwrap().remove(k);
    c
}

/// A key source over fixed JWKS documents; counts calls.
struct FakeKeys {
    sets: Vec<(String, Arc<KeySet>)>,
    calls: AtomicUsize,
    refreshes: AtomicUsize,
}

impl FakeKeys {
    fn new(issuer: &str, jwks: &Value) -> Arc<Self> {
        Arc::new(Self {
            sets: vec![(issuer.into(), Arc::new(KeySet::from_jwks(jwks.to_string().as_bytes()).unwrap()))],
            calls: AtomicUsize::new(0),
            refreshes: AtomicUsize::new(0),
        })
    }
}

#[async_trait]
impl KeySource for FakeKeys {
    async fn keys(&self, issuer: &str, refresh: bool) -> Result<Arc<KeySet>, KeyError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if refresh {
            self.refreshes.fetch_add(1, Ordering::SeqCst);
        }
        self.sets
            .iter()
            .find(|(i, _)| i == issuer)
            .map(|(_, s)| s.clone())
            .ok_or_else(|| KeyError("unknown issuer".into()))
    }
}

/// The forge authorizer stand-in: knows one opaque token, records every secret it is shown.
struct Forge {
    seen: Mutex<Vec<String>>,
}

#[async_trait]
impl Authorizer for Forge {
    async fn authorize(&self, token: Option<&BearerToken>, _repo: &str, _a: Access) -> Result<Principal, AuthError> {
        let t = token.ok_or(AuthError::Unauthenticated)?;
        self.seen.lock().unwrap().push(t.expose().to_owned());
        if t.expose() == "forge-opaque-token" {
            Ok(Principal { id: "forge:alice".into() })
        } else {
            Err(AuthError::Unauthenticated)
        }
    }
}

struct Fixture {
    authz: OauthAuthorizer,
    forge: Arc<Forge>,
    store: Arc<TokenStore>,
    keys: Arc<FakeKeys>,
    idp: Signer,
}

async fn fixture() -> Fixture {
    let pool = migrate::connect("sqlite::memory:", 1).await.unwrap();
    migrate::migrate(&pool).await.unwrap();
    let store = Arc::new(TokenStore::new(pool, TokenKey::generate().unwrap()));
    // alice may read acme/web and write acme/api; bob has no token at all.
    for (repo, access) in [("acme/web", Access::Read), ("acme/api", Access::Write)] {
        store
            .create(&NewToken {
                kind: TokenKind::Pat,
                user: Some("alice".into()),
                repos: vec![repo.into()],
                access,
                expires_in: None,
                label: String::new(),
            })
            .await
            .unwrap();
    }
    store.link_identity(IDP, "sub-alice", "alice").await.unwrap();
    store.link_identity(IDP, "sub-bob", "bob").await.unwrap();
    store.link_identity(IDP, "sub-deploy", "deploy:acme/web").await.unwrap();
    let idp = Signer::new("k1");
    let keys = FakeKeys::new(IDP, &json!({"keys": [idp.jwk()]}));
    let forge = Arc::new(Forge { seen: Mutex::new(Vec::new()) });
    let verifier = OidcVerifier::new(vec![FORGE.into(), IDP.into()], AUD.into(), keys.clone())
        .with_clock(Arc::new(|| NOW));
    let authz = OauthAuthorizer::new(forge.clone(), verifier, store.clone());
    Fixture { authz, forge, store, keys, idp }
}

fn bearer(t: &str) -> Credential {
    Credential::bearer(BearerToken::new(t).unwrap())
}

impl Fixture {
    async fn check(&self, token: &str, repo: &str, a: Access) -> Result<Principal, AuthError> {
        self.authz.authorize_credential(Some(&bearer(token)), repo, a).await
    }
}

fn is_unauth<T>(r: Result<T, AuthError>) -> bool {
    matches!(r, Err(AuthError::Unauthenticated))
}

fn is_forbidden<T>(r: Result<T, AuthError>) -> bool {
    matches!(r, Err(AuthError::Forbidden))
}

// ---------------------------------------------------------------------------- metadata

fn prm_for(issuers: Vec<String>) -> axum::Router {
    let listen: SocketAddr = "127.0.0.1:7890".parse().unwrap();
    let policy = HttpPolicy::new(listen, Some("https://asc.example.org"), &[], &[]);
    router(Prm::new(Some("https://asc.example.org"), issuers, policy))
}

async fn get_json(app: axum::Router, path: &str) -> Value {
    let req = Request::get(path).header("host", "asc.example.org").body(Body::empty()).unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20).await.unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

#[tokio::test]
async fn the_prm_document_lists_the_configured_issuers_and_nothing_assumes_forgejo() {
    for path in ["/.well-known/oauth-protected-resource", "/.well-known/oauth-protected-resource/mcp"] {
        let doc = get_json(prm_for(vec![FORGE.into(), IDP.into()]), path).await;
        assert_eq!(doc["authorization_servers"], json!([FORGE, IDP]));
        assert_eq!(doc["resource"], AUD);
    }
    // A generic IdP alone, and no OAuth at all (an empty list: clients use a pre-issued token).
    let doc = get_json(prm_for(vec![IDP.into()]), "/.well-known/oauth-protected-resource").await;
    assert_eq!(doc["authorization_servers"], json!([IDP]));
    let doc = get_json(prm_for(vec![]), "/.well-known/oauth-protected-resource").await;
    assert_eq!(doc["authorization_servers"], json!([]));
}

fn config_rejects(toml_text: &str) {
    let c: asc_serve::config::ServeConfig = toml::from_str(toml_text).unwrap();
    assert!(c.validate().is_err(), "{toml_text}");
}

#[test]
fn the_config_keys_parse_and_off_means_absent() {
    let c: asc_serve::config::ServeConfig = toml::from_str("[mcp]\nenabled = false\n").unwrap();
    assert!(c.mcp.oauth.is_none());
    let text = "[mcp.oauth]\nissuer = [\"https://idp.example.org\"]\naudience = \"https://asc.example.org/mcp\"\n";
    let c: asc_serve::config::ServeConfig = toml::from_str(text).unwrap();
    let o = c.mcp.oauth.unwrap();
    assert_eq!(o.issuer, [IDP]);
    assert_eq!(o.audience.as_deref(), Some(AUD));
    assert!(!o.own_issuer);
    config_rejects("[mcp.oauth]\nissuer = [\"http://idp.example.org\"]\n");
    config_rejects("[mcp.oauth]\nown_issuer = true\n");
    assert!(toml::from_str::<asc_serve::config::ServeConfig>("[mcp.oauth]\nbogus = 1\n").is_err());
}

#[test]
fn the_audience_defaults_to_the_canonical_mcp_url() {
    let c = McpOauthConfig::default();
    assert_eq!(audience(&c, Some("https://asc.example.org/")).unwrap(), AUD);
    assert!(audience(&c, None).is_err());
    let c = McpOauthConfig { audience: Some("https://other.example/mcp".into()), ..Default::default() };
    assert_eq!(audience(&c, Some("https://asc.example.org")).unwrap(), "https://other.example/mcp");
}

#[tokio::test]
async fn off_means_the_wrapped_authorizer_itself() {
    let f = fixture().await;
    let forge: Arc<dyn Authorizer> = f.forge.clone();
    let url = Some("https://asc.example.org");
    let out = wrap(None, url, forge.clone(), f.store.clone(), f.keys.clone()).unwrap();
    assert!(Arc::ptr_eq(&out, &forge));
    let empty = McpOauthConfig::default();
    let out = wrap(Some(&empty), url, forge.clone(), f.store.clone(), f.keys.clone()).unwrap();
    assert!(Arc::ptr_eq(&out, &forge));
    let on = McpOauthConfig { issuer: vec![IDP.into()], ..Default::default() };
    let out = wrap(Some(&on), url, forge.clone(), f.store.clone(), f.keys.clone()).unwrap();
    assert!(!Arc::ptr_eq(&out, &forge));
}

// ---------------------------------------------------------------------------- forge tokens

#[tokio::test]
async fn a_forge_issued_token_goes_to_the_forge_authorizer_as_a_pat_does() {
    let f = fixture().await;
    let p = f.check("forge-opaque-token", "acme/web", Access::Read).await.unwrap();
    assert_eq!(p.id, "forge:alice");
    assert_eq!(f.forge.seen.lock().unwrap().as_slice(), ["forge-opaque-token"]);
    assert_eq!(f.keys.calls.load(Ordering::SeqCst), 0, "no JWKS for an opaque token");
}

#[tokio::test]
async fn a_jwt_of_an_unconfigured_issuer_is_just_another_credential_for_the_forge() {
    let f = fixture().await;
    let t = f.idp.token(&with(claims(), "iss", json!("https://evil.example.org")));
    assert!(is_unauth(f.check(&t, "acme/web", Access::Read).await));
    assert_eq!(f.forge.seen.lock().unwrap().len(), 1);
    assert_eq!(f.keys.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn a_jwt_is_never_accepted_as_a_basic_password() {
    let f = fixture().await;
    let t = f.idp.token(&claims());
    let c = Credential::basic("alice", &t).unwrap();
    let r = f.authz.authorize_credential(Some(&c), "acme/web", Access::Read).await;
    assert!(is_unauth(r));
    assert_eq!(f.keys.calls.load(Ordering::SeqCst), 0);
}

// ---------------------------------------------------------------------------- generic OIDC

#[tokio::test]
async fn a_valid_jwt_maps_to_the_principal_and_its_grants_decide() {
    let f = fixture().await;
    let t = f.idp.token(&claims());
    let p = f.check(&t, "acme/web", Access::Read).await.unwrap();
    assert_eq!(p.id, "pat:alice");
    // Scopes never exceed the principal's grants.
    assert!(is_forbidden(f.check(&t, "acme/web", Access::Write).await));
    assert!(is_forbidden(f.check(&t, "acme/api", Access::Admin).await));
    assert!(is_forbidden(f.check(&t, "other/repo", Access::Read).await));
    f.check(&t, "acme/api", Access::Write).await.unwrap();
    // Even a token that names `asc:write` cannot write what the user cannot.
    let w = f.idp.token(&with(claims(), "scope", json!("asc:read asc:write")));
    assert!(is_forbidden(f.check(&w, "acme/web", Access::Write).await));
    f.check(&w, "acme/api", Access::Write).await.unwrap();
    // A scope claim without `asc:write` narrows a writer to read.
    let r = f.idp.token(&with(claims(), "scope", json!("openid asc:read")));
    f.check(&r, "acme/api", Access::Read).await.unwrap();
    assert!(is_forbidden(f.check(&r, "acme/api", Access::Write).await));
    assert!(f.forge.seen.lock().unwrap().is_empty(), "nothing reached the forge");
}

#[tokio::test]
async fn the_audience_may_be_an_array_but_must_name_this_server() {
    let f = fixture().await;
    let ok = f.idp.token(&with(claims(), "aud", json!(["https://other.example/mcp", AUD])));
    f.check(&ok, "acme/web", Access::Read).await.unwrap();
    for aud in [
        json!("https://other.example/mcp"),
        json!("https://asc.example.org"),
        json!("https://asc.example.org/mcp/"),
        json!(["https://other.example/mcp"]),
        json!([]),
        json!(7),
    ] {
        let t = f.idp.token(&with(claims(), "aud", aud.clone()));
        assert!(is_unauth(f.check(&t, "acme/web", Access::Read).await), "{aud}");
    }
    let t = f.idp.token(&without(claims(), "aud"));
    assert!(is_unauth(f.check(&t, "acme/web", Access::Read).await));
}

#[tokio::test]
async fn expiry_and_not_before_are_enforced() {
    let f = fixture().await;
    for exp in [NOW - 61, NOW - 3600, 0] {
        let t = f.idp.token(&with(claims(), "exp", json!(exp)));
        assert!(is_unauth(f.check(&t, "acme/web", Access::Read).await), "exp {exp}");
    }
    let t = f.idp.token(&without(claims(), "exp"));
    assert!(is_unauth(f.check(&t, "acme/web", Access::Read).await), "no exp");
    let t = f.idp.token(&with(claims(), "exp", json!("tomorrow")));
    assert!(is_unauth(f.check(&t, "acme/web", Access::Read).await));
    let t = f.idp.token(&with(claims(), "nbf", json!(NOW + 3600)));
    assert!(is_unauth(f.check(&t, "acme/web", Access::Read).await), "nbf");
    let t = f.idp.token(&with(claims(), "nbf", json!(NOW - 10)));
    f.check(&t, "acme/web", Access::Read).await.unwrap();
}

#[tokio::test]
async fn the_signature_must_verify_against_the_issuers_keys() {
    let f = fixture().await;
    // Signed by a key the issuer does not publish, with the right kid.
    let other = Signer::new("k1");
    let forged = other.token(&claims());
    assert!(is_unauth(f.check(&forged, "acme/web", Access::Read).await));
    // A valid token with its payload swapped for another subject's.
    let good = f.idp.token(&claims());
    let evil = f.idp.token(&with(claims(), "sub", json!("sub-bob")));
    let parts: Vec<&str> = good.split('.').collect();
    let mixed = format!("{}.{}.{}", parts[0], evil.split('.').nth(1).unwrap(), parts[2]);
    assert!(is_unauth(f.check(&mixed, "acme/web", Access::Read).await));
    // A truncated or empty signature.
    assert!(is_unauth(f.check(&format!("{}.{}.", parts[0], parts[1]), "acme/web", Access::Read).await));
    assert!(is_unauth(f.check(&format!("{}.{}.AAAA", parts[0], parts[1]), "acme/web", Access::Read).await));
}

#[tokio::test]
async fn only_rs256_and_es256_are_accepted() {
    let f = fixture().await;
    let c = B64.encode(claims().to_string());
    for alg in ["none", "None", "HS256", "HS384", "PS256", "ES384", "EdDSA", "RS512"] {
        let h = B64.encode(json!({"alg": alg, "kid": "k1"}).to_string());
        let t = format!("{h}.{c}.AAAA");
        assert!(is_unauth(f.check(&t, "acme/web", Access::Read).await), "{alg}");
    }
    // HS256 keyed with the public key bytes (the classic confusion) is no better.
    use hmac::{Hmac, KeyInit, Mac};
    let h = B64.encode(json!({"alg": "HS256", "kid": "k1"}).to_string());
    let mut mac = Hmac::<sha2::Sha256>::new_from_slice(f.idp.pair.public_key().as_ref()).unwrap();
    mac.update(format!("{h}.{c}").as_bytes());
    let t = format!("{h}.{c}.{}", B64.encode(mac.finalize().into_bytes()));
    assert!(is_unauth(f.check(&t, "acme/web", Access::Read).await));
    // `crit` headers are refused; so are non-string kids.
    let h = B64.encode(json!({"alg": "ES256", "kid": "k1", "crit": ["x"]}).to_string());
    assert!(is_unauth(f.check(&format!("{h}.{c}.AAAA"), "acme/web", Access::Read).await));
    let t = f.idp.sign(&json!({"alg": "ES256", "kid": 5}), &claims());
    assert!(is_unauth(f.check(&t, "acme/web", Access::Read).await));
}

#[tokio::test]
async fn the_issuer_claim_must_be_a_configured_issuer_that_publishes_the_key() {
    let f = fixture().await;
    // The forge is configured but publishes no key in this fixture: its JWT cannot verify.
    let t = f.idp.token(&with(claims(), "iss", json!(FORGE)));
    assert!(is_unauth(f.check(&t, "acme/web", Access::Read).await));
    // A JWT of a configured issuer never falls through to the forge as an opaque token.
    assert!(f.forge.seen.lock().unwrap().is_empty());
    // A differently spelled issuer is another issuer.
    let t = f.idp.token(&with(claims(), "iss", json!("https://idp.example.org/")));
    assert!(is_unauth(f.check(&t, "acme/web", Access::Read).await));
}

#[tokio::test]
async fn an_unmapped_or_untyped_subject_is_401() {
    let f = fixture().await;
    let t = f.idp.token(&with(claims(), "sub", json!("sub-nobody")));
    assert!(is_unauth(f.check(&t, "acme/web", Access::Read).await));
    // Linked to a deploy principal: a link reaches a user's rights only.
    let t = f.idp.token(&with(claims(), "sub", json!("sub-deploy")));
    assert!(is_unauth(f.check(&t, "acme/web", Access::Read).await));
    // Mapped, but the user holds nothing: not permitted (never a success).
    let t = f.idp.token(&with(claims(), "sub", json!("sub-bob")));
    assert!(is_forbidden(f.check(&t, "acme/web", Access::Read).await));
    for sub in ["", " "] {
        let t = f.idp.token(&with(claims(), "sub", json!(sub)));
        assert!(f.check(&t, "acme/web", Access::Read).await.is_err());
    }
}

#[tokio::test]
async fn the_rights_are_the_authorizers_and_follow_it() {
    let f = fixture().await;
    let t = f.idp.token(&claims());
    f.check(&t, "acme/api", Access::Write).await.unwrap();
    // Revoke every token of alice: the next call loses the rights (no token-side cache).
    for info in f.store.list().await.unwrap() {
        f.store.revoke(&info.id).await.unwrap();
    }
    assert!(is_forbidden(f.check(&t, "acme/api", Access::Read).await));
}

#[tokio::test]
async fn an_unknown_kid_refreshes_the_keys_once_and_then_fails() {
    let f = fixture().await;
    let stranger = Signer::new("rotated");
    let t = stranger.token(&claims());
    assert!(is_unauth(f.check(&t, "acme/web", Access::Read).await));
    assert_eq!(f.keys.calls.load(Ordering::SeqCst), 2);
    assert_eq!(f.keys.refreshes.load(Ordering::SeqCst), 1);
}

// ---------------------------------------------------------------------------- RSA

const RSA_N: &str = "kZM3vMjJyqJA1ji8uHCqfyfMJW_oRBABPEJjc0OqQcw1yCFQ7-qsvo4Qnaust7-ST4HVEcUUVISzpR-OfKhbTYc3Mnjlihad97JSW1KI14LjPQ3CpsmQTm4utaKbgUydAvU-IMfJboHzdexDi-gs1OHUypMc0CLe5NLTWxI9iNn6pQclHoGTI-eNi5P02Uk0fr2-KUFk6j7_piKyg_JxpWcAFnyUwptEX4bpLSx_m_QMrwlmK300KA53V-F7W-kQRfCkBH7twB9tyMVnAdm4fvtuHiLTjquJ6vTogK5y_rQwLJHW_T-JrWylIrY8y2Ww8XuSnxIRt9c7g8Unl-wqrw";
const RSA_TOKEN: &str = "eyJhbGciOiJSUzI1NiIsImtpZCI6InJzYTEiLCJ0eXAiOiJKV1QifQ.eyJpc3MiOiJodHRwczovL2lkcC5leGFtcGxlLm9yZyIsInN1YiI6InJzYS11c2VyIiwiYXVkIjoiaHR0cHM6Ly9hc2MuZXhhbXBsZS5vcmcvbWNwIiwiZXhwIjo0MTAyNDQ0ODAwfQ.gb3x84W6zh1TL9jFfzcQ29e6OsE7BJ0LbeHmmeUGcg6SICuY7VoUEw3U1G2WWZFQPnOo7r-xjaZbrirpSnhTBAkkKvXB8zgoaKFMMxmDO5ILRuGxB03ckVmM6kvtZkF5R4tQUu58U7Td-Vn4Rb7bMr8mc_7nuiRwFUeJtYJ6hV2a80KQo2zut19ig9N0dv66bWJj75BpZgYxJjH0WLIKstNxQuv6Gsp_abHdwFIaXoIzSWm0VZ9vq_7F27kcmWmXje4NEcO9V6TNtVXEx8iPkxEZ8h1EBPGPmZvxVsZYNd74hJ54jdrdLlkdu9egufa91KT-Ibbx7OmU5eJAvuun2A";

#[tokio::test]
async fn an_rs256_token_verifies_against_the_issuers_rsa_key() {
    let f = fixture().await;
    f.store.link_identity(IDP, "rsa-user", "alice").await.unwrap();
    let jwks = json!({"keys": [{"kty": "RSA", "kid": "rsa1", "use": "sig", "alg": "RS256", "n": RSA_N, "e": "AQAB"}]});
    let keys = FakeKeys::new(IDP, &jwks);
    let verifier = OidcVerifier::new(vec![IDP.into()], AUD.into(), keys).with_clock(Arc::new(|| NOW));
    let authz = OauthAuthorizer::new(f.forge.clone(), verifier, f.store.clone());
    let p = authz.authorize_credential(Some(&bearer(RSA_TOKEN)), "acme/web", Access::Read).await.unwrap();
    assert_eq!(p.id, "pat:alice");
    // A different payload under the same signature is a different message.
    let parts: Vec<&str> = RSA_TOKEN.split('.').collect();
    let other = B64.encode(json!({"iss": IDP, "sub": "sub-bob", "aud": AUD, "exp": 4_102_444_800_i64}).to_string());
    let bad = format!("{}.{other}.{}", parts[0], parts[2]);
    assert!(is_unauth(authz.authorize_credential(Some(&bearer(&bad)), "acme/web", Access::Read).await));
    // A modulus under 2048 bits is not a key.
    let weak = json!({"keys": [{"kty": "RSA", "kid": "rsa1", "n": B64.encode([0xc3u8; 128]), "e": "AQAB"}]});
    assert!(KeySet::from_jwks(weak.to_string().as_bytes()).unwrap().is_empty());
}

// ---------------------------------------------------------------------------- fetching

#[tokio::test]
async fn discovery_and_jwks_fetches_refuse_non_https_and_private_addresses() {
    let src = HttpKeySource::new();
    for issuer in [
        "http://idp.example.org",
        "https://127.0.0.1",
        "https://127.0.0.1:8443",
        "https://[::1]",
        "https://10.0.0.5",
        "https://192.168.1.10",
        "https://169.254.169.254",
        "https://[::ffff:10.0.0.1]",
        "https://localhost",
        "https://user:pw@idp.example.org",
    ] {
        assert!(src.keys(issuer, false).await.is_err(), "{issuer}");
    }
}
