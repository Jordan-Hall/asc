//! Optional OAuth 2.1 / OIDC for `/mcp` clients that insist on it (SPEC 22.7.3, 22.5; PLAN M33).
//!
//! Off unless `[mcp.oauth]` is set. `asc serve` is only the resource server: the configured
//! `issuer` URLs are advertised in the protected-resource metadata (`prm`), and
//! [`OauthAuthorizer`] wraps the one git [`Authorizer`] for `/mcp` so an access token is one more
//! way to present a credential.
//!
//! * An opaque token (a forge OAuth access token, a PAT) goes to the wrapped authorizer unchanged,
//!   exactly as a PAT does.
//! * A `Bearer` token that is a JWT whose (unverified) `iss` is a configured issuer is accepted
//!   only if the signature verifies against that issuer's JWKS (RS256 or ES256; `none`, HMAC and
//!   every other algorithm are refused), `iss` matches, `exp` (and `nbf`) hold, `aud` contains the
//!   canonical `/mcp` URL (RFC 8707), and `(iss, sub)` maps through `auth_identity` (M30) to a
//!   principal. The token's repo rights are those the authorizer side holds for that principal
//!   ([`IdentityDirectory::access`]); a `scope` claim can only narrow them. An unmapped subject is
//!   401. A JWT of a configured issuer that fails any check is never retried as an opaque token.
//! * Discovery and JWKS fetches are `https` only, refuse private, loopback and link-local
//!   addresses (the resolved address is pinned for the connection), do not follow redirects, are
//!   size and time bounded, and are cached.
//! * No token is forwarded anywhere, logged, or put in an error.
//!
//! [`super::mount`] composes [`wrap`] around the authorizer it gives `auth::layer` when
//! `[mcp.oauth]` names issuers. asc serve's own issuer (authorization code with PKCE ending in an M30 token) is v1.x.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use asc_rpc::auth::{Access, AuthError, Authorizer, BearerToken, Credential, Principal, Scheme};
use ring::signature::{ECDSA_P256_SHA256_FIXED, RSA_PKCS1_2048_8192_SHA256, UnparsedPublicKey};
use serde::Deserialize;
use serde_json::Value;

use super::PATH;
use crate::authz::TokenStore;

/// Most configured issuers.
pub const MAX_ISSUERS: usize = 8;
/// Longest accepted JWT, in bytes.
pub const MAX_JWT: usize = 4096;
/// Largest discovery or JWKS body.
pub const MAX_DOC: usize = 256 * 1024;
/// Most keys read from one JWKS.
pub const MAX_KEYS: usize = 64;
/// How long a JWKS is used before it is fetched again.
pub const KEYS_TTL: Duration = Duration::from_secs(600);
/// The least time between two fetches for one issuer (unknown `kid` refreshes included).
pub const MIN_REFETCH: Duration = Duration::from_secs(30);
/// Clock skew accepted on `exp` and `nbf`.
pub const LEEWAY_SECS: i64 = 60;
/// Timeout of one fetch.
const FETCH_TIMEOUT: Duration = Duration::from_secs(5);
/// The least RSA modulus, in bytes (2048 bits).
const MIN_RSA_BYTES: usize = 256;

// ------------------------------------------------------------------------------ config

/// `[mcp.oauth]`. Absent means no OAuth.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct McpOauthConfig {
    /// The authorization servers to advertise: `https` issuer URLs (the forge, any OIDC IdP).
    pub issuer: Vec<String>,
    /// The audience an access token must carry; default `<public url>/mcp`.
    pub audience: Option<String>,
    /// asc serve's own issuer (v1.x). Refused until it exists.
    pub own_issuer: bool,
}

impl McpOauthConfig {
    /// Checks the keys; the error text names the problem.
    pub fn validate(&self) -> Result<(), String> {
        if self.own_issuer {
            return Err("own_issuer is not built yet (v1.x)".into());
        }
        if self.issuer.len() > MAX_ISSUERS {
            return Err(format!("at most {MAX_ISSUERS} issuers"));
        }
        for (i, iss) in self.issuer.iter().enumerate() {
            check_https_url(iss, "issuer")?;
            if iss.contains('?') || iss.contains('#') {
                return Err("an issuer has no query or fragment".into());
            }
            if self.issuer[..i].iter().any(|o| same_issuer(o, iss)) {
                return Err(format!("issuer {iss} is listed twice"));
            }
        }
        if let Some(a) = &self.audience {
            check_https_url(a, "audience")?;
        }
        Ok(())
    }
}

fn check_https_url(s: &str, what: &str) -> Result<(), String> {
    let u = url::Url::parse(s).map_err(|_| format!("{what} must be an https URL"))?;
    let ok = u.scheme() == "https"
        && u.host().is_some()
        && u.username().is_empty()
        && u.password().is_none()
        && s.len() <= 512
        && !s.chars().any(char::is_control);
    if ok {
        Ok(())
    } else {
        Err(format!("{what} must be an https URL without credentials"))
    }
}

fn same_issuer(a: &str, b: &str) -> bool {
    a.trim_end_matches('/') == b.trim_end_matches('/')
}

// ------------------------------------------------------------------------------ keys

/// A verification key of a JWKS.
#[derive(Clone, Debug)]
struct Jwk {
    kid: Option<String>,
    alg: Alg,
    /// RS256: the DER `RSAPublicKey`. ES256: the uncompressed point.
    key: Vec<u8>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Alg {
    Rs256,
    Es256,
}

impl Alg {
    fn parse(s: &str) -> Option<Self> {
        match s {
            "RS256" => Some(Self::Rs256),
            "ES256" => Some(Self::Es256),
            _ => None,
        }
    }
}

/// The usable keys of one issuer.
#[derive(Clone, Debug, Default)]
pub struct KeySet(Vec<Jwk>);

impl KeySet {
    /// Reads a JWKS document. Keys that are not RSA (2048 bits or more) or P-256 signature keys
    /// are skipped.
    pub fn from_jwks(doc: &[u8]) -> Result<Self, String> {
        let v: Value =
            serde_json::from_slice(doc).map_err(|_| "the JWKS is not JSON".to_owned())?;
        let keys = v
            .get("keys")
            .and_then(Value::as_array)
            .ok_or("the JWKS has no keys")?;
        Ok(Self(keys.iter().take(MAX_KEYS).filter_map(jwk).collect()))
    }

    /// The number of usable keys.
    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    fn find(&self, alg: Alg, kid: Option<&str>) -> Option<&Jwk> {
        let mut with_alg = self.0.iter().filter(|k| k.alg == alg);
        match kid {
            Some(kid) => with_alg.find(|k| k.kid.as_deref() == Some(kid)),
            None => {
                let first = with_alg.next()?;
                with_alg.next().is_none().then_some(first)
            }
        }
    }
}

fn b64(v: &Value, name: &str) -> Option<Vec<u8>> {
    URL_SAFE_NO_PAD.decode(v.get(name)?.as_str()?).ok()
}

fn jwk(v: &Value) -> Option<Jwk> {
    if v.get("use").is_some_and(|u| u.as_str() != Some("sig")) {
        return None;
    }
    let declared = match v.get("alg") {
        Some(a) => Some(Alg::parse(a.as_str()?)?),
        None => None,
    };
    let kid = v.get("kid").and_then(Value::as_str).map(str::to_owned);
    match v.get("kty")?.as_str()? {
        "RSA" if declared.is_none_or(|a| a == Alg::Rs256) => {
            let n = b64(v, "n")?;
            let n = strip_zeros(&n);
            let e = b64(v, "e")?;
            let e = strip_zeros(&e);
            if n.len() < MIN_RSA_BYTES || n.len() > 1024 || e.is_empty() || e.len() > 8 {
                return None;
            }
            Some(Jwk {
                kid,
                alg: Alg::Rs256,
                key: rsa_der(n, e),
            })
        }
        "EC" if v.get("crv")?.as_str()? == "P-256" && declared.is_none_or(|a| a == Alg::Es256) => {
            let (x, y) = (b64(v, "x")?, b64(v, "y")?);
            if x.len() != 32 || y.len() != 32 {
                return None;
            }
            let mut key = vec![4u8];
            key.extend_from_slice(&x);
            key.extend_from_slice(&y);
            Some(Jwk {
                kid,
                alg: Alg::Es256,
                key,
            })
        }
        _ => None,
    }
}

fn strip_zeros(b: &[u8]) -> &[u8] {
    let n = b.iter().take_while(|&&x| x == 0).count();
    &b[n..]
}

fn der_len(out: &mut Vec<u8>, n: usize) {
    if n < 0x80 {
        out.push(u8::try_from(n).unwrap_or(0));
    } else if n < 0x100 {
        out.extend_from_slice(&[0x81, u8::try_from(n).unwrap_or(0)]);
    } else {
        out.extend_from_slice(&[0x82, u8::try_from(n >> 8).unwrap_or(0), (n & 0xff) as u8]);
    }
}

fn der_uint(out: &mut Vec<u8>, v: &[u8]) {
    let pad = v.first().is_some_and(|b| b & 0x80 != 0);
    out.push(0x02);
    der_len(out, v.len() + usize::from(pad));
    if pad {
        out.push(0);
    }
    out.extend_from_slice(v);
}

/// `RSAPublicKey ::= SEQUENCE { modulus INTEGER, publicExponent INTEGER }`.
fn rsa_der(n: &[u8], e: &[u8]) -> Vec<u8> {
    let mut body = Vec::with_capacity(n.len() + e.len() + 12);
    der_uint(&mut body, n);
    der_uint(&mut body, e);
    let mut out = vec![0x30];
    der_len(&mut out, body.len());
    out.extend_from_slice(&body);
    out
}

// ------------------------------------------------------------------------------ key sources

/// Why a key fetch failed. Never carries a token.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct KeyError(pub String);

/// Where an issuer's keys come from.
#[async_trait]
pub trait KeySource: Send + Sync {
    /// The keys of `issuer`. `refresh` asks for a newer set (an unknown `kid`); the source may
    /// refuse to fetch more often than [`MIN_REFETCH`].
    async fn keys(&self, issuer: &str, refresh: bool) -> Result<Arc<KeySet>, KeyError>;
}

struct Entry {
    keys: Option<Arc<KeySet>>,
    at: Instant,
}

/// OIDC discovery and JWKS over `https`, cached.
pub struct HttpKeySource {
    cache: tokio::sync::Mutex<HashMap<String, Entry>>,
}

impl Default for HttpKeySource {
    fn default() -> Self {
        Self::new()
    }
}

impl HttpKeySource {
    pub fn new() -> Self {
        Self {
            cache: tokio::sync::Mutex::new(HashMap::new()),
        }
    }

    async fn fetch(issuer: &str) -> Result<KeySet, KeyError> {
        let base = issuer.trim_end_matches('/');
        let disc = fetch_json(&format!("{base}/.well-known/openid-configuration")).await?;
        let doc: Value =
            serde_json::from_slice(&disc).map_err(|_| KeyError("discovery is not JSON".into()))?;
        if !doc
            .get("issuer")
            .and_then(Value::as_str)
            .is_some_and(|i| same_issuer(i, issuer))
        {
            return Err(KeyError("discovery names another issuer".into()));
        }
        let uri = doc
            .get("jwks_uri")
            .and_then(Value::as_str)
            .ok_or_else(|| KeyError("discovery has no jwks_uri".into()))?;
        let jwks = fetch_json(uri).await?;
        KeySet::from_jwks(&jwks).map_err(KeyError)
    }
}

#[async_trait]
impl KeySource for HttpKeySource {
    async fn keys(&self, issuer: &str, refresh: bool) -> Result<Arc<KeySet>, KeyError> {
        let mut cache = self.cache.lock().await;
        if let Some(e) = cache.get(issuer) {
            let age = e.at.elapsed();
            let fresh = if refresh {
                age < MIN_REFETCH
            } else {
                age < KEYS_TTL
            };
            // A failed fetch is not retried for MIN_REFETCH either.
            if fresh || (e.keys.is_none() && age < MIN_REFETCH) {
                return e
                    .keys
                    .clone()
                    .ok_or_else(|| KeyError("the issuer's keys are unavailable".into()));
            }
        }
        match Self::fetch(issuer).await {
            Ok(set) => {
                let set = Arc::new(set);
                cache.insert(
                    issuer.to_owned(),
                    Entry {
                        keys: Some(set.clone()),
                        at: Instant::now(),
                    },
                );
                Ok(set)
            }
            Err(e) => {
                tracing::warn!(issuer, error = %e, "OIDC key fetch failed");
                match cache.get_mut(issuer) {
                    // Keep serving the old keys; try again after MIN_REFETCH.
                    Some(old) if old.keys.is_some() => {
                        old.at = Instant::now() - KEYS_TTL.saturating_sub(MIN_REFETCH);
                        Ok(old.keys.clone().unwrap_or_default())
                    }
                    _ => {
                        cache.insert(
                            issuer.to_owned(),
                            Entry {
                                keys: None,
                                at: Instant::now(),
                            },
                        );
                        Err(e)
                    }
                }
            }
        }
    }
}

/// Whether `ip` is a public, routable address. Everything a server must not be pointed at
/// (loopback, private, link-local, CGNAT, documentation, multicast, transition prefixes) is not.
#[must_use]
pub fn is_public_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(a) => public_v4(a),
        IpAddr::V6(a) => public_v6(a),
    }
}

fn public_v4(a: Ipv4Addr) -> bool {
    let o = a.octets();
    !(a.is_unspecified()
        || a.is_loopback()
        || a.is_private()
        || a.is_link_local()
        || a.is_broadcast()
        || a.is_multicast()
        || a.is_documentation()
        || o[0] == 0
        || (o[0] == 100 && (o[1] & 0xc0) == 64)
        || (o[0] == 192 && o[1] == 0 && o[2] == 0)
        || (o[0] == 198 && (o[1] & 0xfe) == 18)
        || o[0] >= 240)
}

fn public_v6(a: Ipv6Addr) -> bool {
    if let Some(v4) = a.to_ipv4_mapped() {
        return public_v4(v4);
    }
    let s = a.segments();
    !(a.is_unspecified()
        || a.is_loopback()
        || a.is_multicast()
        || (s[0] & 0xfe00) == 0xfc00
        || (s[0] & 0xffc0) == 0xfe80
        || (s[0] & 0xffc0) == 0xfec0
        || (s[0] == 0x2001 && s[1] == 0x0db8)
        || s[0] == 0x2002
        || (s[0] == 0x0064 && s[1] == 0xff9b)
        || (s[0] == 0 && s[1] == 0 && s[2] == 0 && s[3] == 0 && s[4] == 0 && s[5] == 0))
}

/// A URL a fetch may use: `https`, a host, no credentials.
pub fn check_fetch_url(raw: &str) -> Result<url::Url, KeyError> {
    check_https_url(raw, "url").map_err(KeyError)?;
    url::Url::parse(raw).map_err(|_| KeyError("bad url".into()))
}

async fn fetch_json(raw: &str) -> Result<Vec<u8>, KeyError> {
    let u = check_fetch_url(raw)?;
    let host = u
        .host_str()
        .ok_or_else(|| KeyError("no host".into()))?
        .to_owned();
    let port = u.port_or_known_default().unwrap_or(443);
    let bare = host.trim_start_matches('[').trim_end_matches(']');
    let addrs: Vec<std::net::SocketAddr> = if let Ok(ip) = bare.parse::<IpAddr>() {
        vec![std::net::SocketAddr::new(ip, port)]
    } else {
        tokio::net::lookup_host((host.as_str(), port))
            .await
            .map_err(|_| KeyError("the host does not resolve".into()))?
            .collect()
    };
    if addrs.is_empty() || addrs.iter().any(|a| !is_public_ip(a.ip())) {
        return Err(KeyError("the host is not a public address".into()));
    }
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .https_only(true)
        .timeout(FETCH_TIMEOUT)
        .resolve_to_addrs(&host, &addrs)
        .build()
        .map_err(|_| KeyError("http client".into()))?;
    let mut resp = client
        .get(u)
        .header(reqwest::header::ACCEPT, "application/json")
        .send()
        .await
        .map_err(|_| KeyError("the fetch failed".into()))?;
    if resp.status() != reqwest::StatusCode::OK {
        return Err(KeyError(format!(
            "the fetch answered {}",
            resp.status().as_u16()
        )));
    }
    if resp.content_length().is_some_and(|n| n > MAX_DOC as u64) {
        return Err(KeyError("the document is too large".into()));
    }
    let mut body = Vec::new();
    while let Some(chunk) = resp
        .chunk()
        .await
        .map_err(|_| KeyError("the fetch failed".into()))?
    {
        if body.len() + chunk.len() > MAX_DOC {
            return Err(KeyError("the document is too large".into()));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

// ------------------------------------------------------------------------------ the verifier

/// The claims of a verified token that the authorizer uses.
#[derive(Debug, Clone)]
pub struct Verified {
    pub issuer: String,
    pub subject: String,
    /// The `scope` claim, if the token has one.
    pub scope: Option<String>,
}

/// The clock, in Unix seconds.
pub type Clock = Arc<dyn Fn() -> i64 + Send + Sync>;

fn system_clock() -> Clock {
    Arc::new(|| {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
    })
}

/// Validates JWT access tokens of the configured issuers.
pub struct OidcVerifier {
    issuers: Vec<String>,
    audience: String,
    keys: Arc<dyn KeySource>,
    now: Clock,
}

struct Parts<'a> {
    signing: &'a str,
    sig: &'a str,
    alg: Alg,
    kid: Option<String>,
    claims: Value,
}

fn parse_jwt(token: &str) -> Option<Parts<'_>> {
    if token.len() > MAX_JWT {
        return None;
    }
    let mut it = token.split('.');
    let (h, p, s) = (it.next()?, it.next()?, it.next()?);
    if it.next().is_some() || h.is_empty() || p.is_empty() || s.is_empty() || h.len() > 1024 {
        return None;
    }
    let header: Value = serde_json::from_slice(&URL_SAFE_NO_PAD.decode(h).ok()?).ok()?;
    let claims: Value = serde_json::from_slice(&URL_SAFE_NO_PAD.decode(p).ok()?).ok()?;
    if !claims.is_object() || header.get("crit").is_some() {
        return None;
    }
    // `none`, HMAC and everything else but RS256 and ES256 end here.
    let alg = Alg::parse(header.get("alg")?.as_str()?)?;
    let kid = header
        .get("kid")
        .map(|k| k.as_str().map(str::to_owned))
        .unwrap_or(None);
    if header.get("kid").is_some() && kid.is_none() {
        return None;
    }
    let signing = &token[..h.len() + 1 + p.len()];
    Some(Parts {
        signing,
        sig: s,
        alg,
        kid,
        claims,
    })
}

impl OidcVerifier {
    /// `audience` is the canonical `/mcp` URL.
    pub fn new(issuers: Vec<String>, audience: String, keys: Arc<dyn KeySource>) -> Self {
        Self {
            issuers,
            audience,
            keys,
            now: system_clock(),
        }
    }

    #[must_use]
    pub fn with_clock(mut self, now: Clock) -> Self {
        self.now = now;
        self
    }

    /// The configured issuer a JWT claims, from an unverified peek. This only routes the token
    /// (a JWT of a configured issuer is never passed on as opaque); it proves nothing.
    #[must_use]
    pub fn claimed_issuer(&self, token: &str) -> Option<&str> {
        let p = parse_jwt_loose(token)?;
        self.issuers.iter().map(String::as_str).find(|i| *i == p)
    }

    /// Verifies `token`: signature first, then `iss`, `aud`, `exp`, `nbf` and `sub`.
    pub async fn verify(&self, token: &str) -> Result<Verified, AuthError> {
        let p = parse_jwt(token).ok_or(AuthError::Unauthenticated)?;
        let iss = p
            .claims
            .get("iss")
            .and_then(Value::as_str)
            .and_then(|c| self.issuers.iter().find(|i| i.as_str() == c))
            .ok_or(AuthError::Unauthenticated)?;
        let sig = URL_SAFE_NO_PAD
            .decode(p.sig)
            .map_err(|_| AuthError::Unauthenticated)?;
        let mut refreshed = false;
        loop {
            let set = self.keys.keys(iss, refreshed).await.map_err(|e| {
                tracing::warn!(issuer = %iss, error = %e, "no OIDC keys");
                AuthError::Unauthenticated
            })?;
            match set.find(p.alg, p.kid.as_deref()) {
                Some(k) => {
                    let alg: &dyn ring::signature::VerificationAlgorithm = match p.alg {
                        Alg::Rs256 => &RSA_PKCS1_2048_8192_SHA256,
                        Alg::Es256 => &ECDSA_P256_SHA256_FIXED,
                    };
                    UnparsedPublicKey::new(alg, &k.key)
                        .verify(p.signing.as_bytes(), &sig)
                        .map_err(|_| AuthError::Unauthenticated)?;
                    break;
                }
                None if !refreshed => refreshed = true,
                None => return Err(AuthError::Unauthenticated),
            }
        }
        self.check_claims(&p.claims, iss)
    }

    fn check_claims(&self, c: &Value, iss: &str) -> Result<Verified, AuthError> {
        let now = (self.now)();
        let exp = num(c.get("exp")).ok_or(AuthError::Unauthenticated)?;
        if now >= exp.saturating_add(LEEWAY_SECS) {
            return Err(AuthError::Unauthenticated);
        }
        if let Some(nbf) = c.get("nbf") {
            let nbf = num(Some(nbf)).ok_or(AuthError::Unauthenticated)?;
            if now.saturating_add(LEEWAY_SECS) < nbf {
                return Err(AuthError::Unauthenticated);
            }
        }
        let aud_ok = match c.get("aud") {
            Some(Value::String(a)) => *a == self.audience,
            Some(Value::Array(a)) => a.iter().any(|a| a.as_str() == Some(self.audience.as_str())),
            _ => false,
        };
        if !aud_ok {
            return Err(AuthError::Unauthenticated);
        }
        let sub = c
            .get("sub")
            .and_then(Value::as_str)
            .ok_or(AuthError::Unauthenticated)?;
        if sub.is_empty() || sub.len() > 512 || sub.chars().any(char::is_control) {
            return Err(AuthError::Unauthenticated);
        }
        Ok(Verified {
            issuer: iss.to_owned(),
            subject: sub.to_owned(),
            scope: c.get("scope").and_then(Value::as_str).map(str::to_owned),
        })
    }
}

fn num(v: Option<&Value>) -> Option<i64> {
    let v = v?;
    v.as_i64().or_else(|| {
        let f = v.as_f64()?;
        (f.is_finite() && f.abs() < 1e15).then(|| f.floor() as i64)
    })
}

/// The `iss` claim of a JWT-shaped token, without any check.
fn parse_jwt_loose(token: &str) -> Option<String> {
    if token.len() > MAX_JWT || !token.starts_with("eyJ") {
        return None;
    }
    let mut it = token.split('.');
    let (_, p, _) = (it.next()?, it.next()?, it.next()?);
    if it.next().is_some() {
        return None;
    }
    let claims: Value = serde_json::from_slice(&URL_SAFE_NO_PAD.decode(p).ok()?).ok()?;
    claims.get("iss")?.as_str().map(str::to_owned)
}

// ------------------------------------------------------------------------------ the authorizer

/// Who a subject is and what that principal may do: the `Authorizer` side of an OIDC login.
#[async_trait]
pub trait IdentityDirectory: Send + Sync {
    /// The principal an OIDC `(issuer, subject)` is linked to (`auth_identity`, M30).
    async fn resolve(&self, issuer: &str, subject: &str) -> Result<Option<String>, AuthError>;
    /// The strongest access `principal` holds on `repo`.
    async fn access(&self, principal: &str, repo: &str) -> Result<Option<Access>, AuthError>;
}

#[async_trait]
impl IdentityDirectory for TokenStore {
    async fn resolve(&self, issuer: &str, subject: &str) -> Result<Option<String>, AuthError> {
        self.resolve_identity(issuer, subject).await.map_err(|e| {
            tracing::warn!(error = %e, "identity lookup failed");
            AuthError::Forbidden
        })
    }

    async fn access(&self, principal: &str, repo: &str) -> Result<Option<Access>, AuthError> {
        self.user_access(principal, repo).await
    }
}

/// The `/mcp` authorizer with OAuth: JWTs of the configured issuers are verified here, everything
/// else goes to the wrapped authorizer unchanged.
pub struct OauthAuthorizer {
    inner: Arc<dyn Authorizer>,
    verifier: OidcVerifier,
    directory: Arc<dyn IdentityDirectory>,
}

impl OauthAuthorizer {
    pub fn new(
        inner: Arc<dyn Authorizer>,
        verifier: OidcVerifier,
        directory: Arc<dyn IdentityDirectory>,
    ) -> Self {
        Self {
            inner,
            verifier,
            directory,
        }
    }

    /// Verifies a JWT and maps its subject to a linked user: no repository involved.
    async fn jwt_user(&self, token: &str) -> Result<(String, Verified), AuthError> {
        let v = self.verifier.verify(token).await?;
        let mapped = self
            .directory
            .resolve(&v.issuer, &v.subject)
            .await?
            .ok_or(AuthError::Unauthenticated)?;
        let id = user_principal(&mapped).ok_or(AuthError::Unauthenticated)?;
        Ok((id, v))
    }

    async fn jwt(&self, token: &str, repo: &str, access: Access) -> Result<Principal, AuthError> {
        let (id, v) = self.jwt_user(token).await?;
        // A scope claim can only narrow: write needs `asc:write` when the token names scopes.
        if access > Access::Read
            && v.scope
                .as_deref()
                .is_some_and(|s| !s.split(' ').any(|x| x == "asc:write"))
        {
            return Err(AuthError::Forbidden);
        }
        match self.directory.access(&id, repo).await? {
            Some(have) if have >= access => Ok(Principal { id }),
            _ => Err(AuthError::Forbidden),
        }
    }
}

/// The principal id of a linked user: `pat:<user>`. A bare name gets the prefix; any other typed
/// id (deploy, CI, the host service) is refused, so a link can only reach a user's rights.
fn user_principal(mapped: &str) -> Option<String> {
    let user = mapped.strip_prefix("pat:").unwrap_or(mapped);
    (!user.is_empty() && !user.contains(':')).then(|| format!("pat:{user}"))
}

#[async_trait]
impl Authorizer for OauthAuthorizer {
    async fn authorize(
        &self,
        token: Option<&BearerToken>,
        repo: &str,
        access: Access,
    ) -> Result<Principal, AuthError> {
        // A bare token carries no scheme: only `authorize_credential` takes a JWT.
        self.inner.authorize(token, repo, access).await
    }

    async fn authorize_credential(
        &self,
        credential: Option<&Credential>,
        repo: &str,
        access: Access,
    ) -> Result<Principal, AuthError> {
        if let Some(c) = credential
            && c.scheme() == Scheme::Bearer
            && self.verifier.claimed_issuer(c.secret().expose()).is_some()
        {
            return self.jwt(c.secret().expose(), repo, access).await;
        }
        self.inner
            .authorize_credential(credential, repo, access)
            .await
    }

    async fn authenticate(&self, credential: Option<&Credential>) -> Result<Principal, AuthError> {
        if let Some(c) = credential
            && c.scheme() == Scheme::Bearer
            && self.verifier.claimed_issuer(c.secret().expose()).is_some()
        {
            let (id, _) = self.jwt_user(c.secret().expose()).await?;
            return Ok(Principal { id });
        }
        self.inner.authenticate(credential).await
    }

    async fn identify_ssh_key(
        &self,
        key: &asc_rpc::auth::SshKey,
    ) -> Result<Option<asc_rpc::auth::SshIdentity>, AuthError> {
        self.inner.identify_ssh_key(key).await
    }

    async fn authorize_ssh_key(
        &self,
        key: &asc_rpc::auth::SshKey,
        repo: &str,
        access: Access,
    ) -> Result<asc_rpc::auth::Decision, AuthError> {
        self.inner.authorize_ssh_key(key, repo, access).await
    }
}

/// The canonical audience: `[mcp.oauth] audience`, else `<public url>/mcp`.
pub fn audience(cfg: &McpOauthConfig, public_url: Option<&str>) -> Result<String, String> {
    match (&cfg.audience, public_url) {
        (Some(a), _) => Ok(a.clone()),
        (None, Some(u)) => Ok(format!("{}{PATH}", u.trim_end_matches('/'))),
        (None, None) => Err("mcp.oauth needs `audience` or the server's `public_url`".into()),
    }
}

/// `inner` for `/mcp` with OAuth when `[mcp.oauth]` names issuers; `inner` itself otherwise.
pub fn wrap(
    cfg: Option<&McpOauthConfig>,
    public_url: Option<&str>,
    inner: Arc<dyn Authorizer>,
    directory: Arc<dyn IdentityDirectory>,
    keys: Arc<dyn KeySource>,
) -> Result<Arc<dyn Authorizer>, String> {
    let Some(cfg) = cfg.filter(|c| !c.issuer.is_empty()) else {
        return Ok(inner);
    };
    cfg.validate()?;
    let verifier = OidcVerifier::new(cfg.issuer.clone(), audience(cfg, public_url)?, keys);
    Ok(Arc::new(OauthAuthorizer::new(inner, verifier, directory)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn public_addresses_only() {
        for bad in [
            "127.0.0.1",
            "10.1.2.3",
            "172.16.0.1",
            "192.168.1.1",
            "169.254.169.254",
            "0.0.0.0",
            "100.64.0.1",
            "224.0.0.1",
            "255.255.255.255",
            "198.18.0.1",
            "::1",
            "::",
            "fe80::1",
            "fc00::1",
            "fd12::1",
            "::ffff:127.0.0.1",
            "::ffff:10.0.0.1",
            "64:ff9b::7f00:1",
            "2002::1",
        ] {
            assert!(!is_public_ip(bad.parse().unwrap()), "{bad}");
        }
        for good in ["8.8.8.8", "1.1.1.1", "2606:4700::1111"] {
            assert!(is_public_ip(good.parse().unwrap()), "{good}");
        }
    }

    #[test]
    fn fetch_urls_are_https() {
        assert!(check_fetch_url("https://idp.example.org/x").is_ok());
        for bad in [
            "http://idp.example.org",
            "ftp://a/b",
            "https://u:p@idp.example.org",
            "file:///etc/passwd",
            "idp",
        ] {
            assert!(check_fetch_url(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn rsa_der_matches_known_encoding() {
        // n = 0x80.. (needs a pad byte), e = 65537.
        let mut n = vec![0u8; 256];
        n[0] = 0x80;
        let der = rsa_der(&n, &[1, 0, 1]);
        assert_eq!(&der[..4], &[0x30, 0x82, 0x01, 0x0a]);
        assert_eq!(&der[4..9], &[0x02, 0x82, 0x01, 0x01, 0x00]);
        assert_eq!(&der[der.len() - 5..], &[0x02, 0x03, 1, 0, 1]);
    }

    #[test]
    fn config_rules() {
        let ok = McpOauthConfig {
            issuer: vec!["https://idp.example.org".into()],
            ..Default::default()
        };
        assert!(ok.validate().is_ok());
        for bad in [
            McpOauthConfig {
                issuer: vec!["http://idp.example.org".into()],
                ..Default::default()
            },
            McpOauthConfig {
                issuer: vec!["https://a/?x=1".into()],
                ..Default::default()
            },
            McpOauthConfig {
                issuer: vec!["https://a".into(), "https://a/".into()],
                ..Default::default()
            },
            McpOauthConfig {
                own_issuer: true,
                ..Default::default()
            },
            McpOauthConfig {
                audience: Some("http://x/mcp".into()),
                ..Default::default()
            },
        ] {
            assert!(bad.validate().is_err(), "{bad:?}");
        }
    }

    #[test]
    fn linked_principals_are_users_only() {
        assert_eq!(user_principal("alice").as_deref(), Some("pat:alice"));
        assert_eq!(user_principal("pat:alice").as_deref(), Some("pat:alice"));
        for bad in ["", "deploy:a/b", "ci:x", "pat:", "pat:a:b"] {
            assert_eq!(user_principal(bad), None, "{bad}");
        }
    }
}
