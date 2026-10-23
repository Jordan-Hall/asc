//! The standalone token store (SPEC §15.1, §15.6.2, §22.7.3; PLAN M30).
//!
//! [`TokenStore`] keeps personal access tokens (`pat`), deploy tokens (`deploy`) and CI tokens
//! (`ci`) in the server database; [`TokenStoreAuthz`] is the [`Authorizer`] over it, for
//! deployments with no forge (`[auth] provider = "token_store"`). Basic, Bearer and `token`
//! credentials all reach it through [`Authorizer::authorize_credential`] and mean the same thing.
//!
//! # Token format
//!
//! `asc<k>_<id>_<secret>`: `<k>` is `p`, `d` or `c`; `<id>` is 8 random bytes in hex (public, the
//! row key, shown by `list`); `<secret>` is 32 random bytes in unpadded base64url. The length is
//! fixed at [`TOKEN_LEN`]. A token is shown once, when it is created.
//!
//! # What is stored
//!
//! Never the token. The database holds `HMAC-SHA256(server key, "asc-token-v1" NUL token)`, with the
//! server key in a file only the server user can read ([`TokenKey`]). A token is 256 bits of
//! randomness, so a keyed fast hash is sufficient (a slow hash only defends low-entropy secrets),
//! and a stolen database alone yields nothing that verifies. The stored digest is compared with
//! the presented token's digest in constant time, after the row is found by its public id. An
//! unknown id costs the same HMAC and compare as a known one.
//!
//! # Decisions
//!
//! A token has a principal and grants `(repo or glob, read|write|admin)`. A glob is `*` or
//! `owner/*`. The decision for `(repo, access)` is the strongest matching grant. A missing,
//! malformed, unknown, expired or revoked token is [`AuthError::Unauthenticated`]; a valid token
//! without enough access is [`AuthError::Forbidden`]. Lookups are cached for [`CACHE_TTL_SECS`],
//! positive and negative alike, so a revocation made by another process (the CLI) is effective
//! within that bound; a revocation made through this store clears the cache at once.
//!
//! # Bounds
//!
//! At most [`MAX_LIVE_TOKENS`] live tokens and [`MAX_TOKEN_ROWS`] rows. Rows dead for longer than
//! [`DEAD_RETENTION_SECS`] are deleted when a token is created. Grants per token are capped at
//! [`MAX_GRANTS_PER_TOKEN`], identity links at [`MAX_IDENTITIES`], the decision cache at
//! [`CACHE_ENTRIES`].
//!
//! Nothing here logs or returns a token except [`IssuedToken::secret`], once.

use std::collections::HashMap;
use std::fmt;
use std::path::Path;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use asc_rpc::auth::{Access, AuthError, Authorizer, BearerToken, Principal};
use asc_rpc::server::RepoName;
use hmac::{Hmac, KeyInit, Mac};
use sha2::Sha256;
use sqlx::{AnyPool, Row};

/// Random bytes in a token secret.
pub const SECRET_BYTES: usize = 32;
/// Random bytes in a token id.
pub const ID_BYTES: usize = 8;
/// The exact length of every token: `asp_` + 16 hex + `_` + 43 base64url.
pub const TOKEN_LEN: usize = 4 + ID_BYTES * 2 + 1 + 43;
/// Most live (unrevoked, unexpired) tokens.
pub const MAX_LIVE_TOKENS: i64 = 10_000;
/// Most rows in `auth_tokens`, dead ones included.
pub const MAX_TOKEN_ROWS: i64 = 50_000;
/// Most grants one token may carry.
pub const MAX_GRANTS_PER_TOKEN: usize = 32;
/// Most `auth_identity` links.
pub const MAX_IDENTITIES: i64 = 100_000;
/// Revoked or expired rows older than this are deleted when a token is created.
pub const DEAD_RETENTION_SECS: i64 = 90 * 86_400;
/// How long a lookup, positive or negative, is cached.
pub const CACHE_TTL_SECS: i64 = 5;
/// Most cached lookups.
pub const CACHE_ENTRIES: usize = 8 * 1024;
/// The default lifetime of a CI token.
pub const DEFAULT_CI_LIFETIME_SECS: i64 = 86_400;
/// The longest lifetime of a CI token.
pub const MAX_CI_LIFETIME_SECS: i64 = 30 * 86_400;
const MAX_LABEL: usize = 128;
const MAX_USER: usize = 64;
const MAX_IDENTITY_FIELD: usize = 512;
const MAC_DOMAIN: &[u8] = b"asc-token-v1\0";
const KEY_BYTES: usize = 32;

/// Why a token operation failed.
#[derive(Debug, thiserror::Error)]
pub enum TokenError {
    #[error("{0}")]
    Invalid(String),
    #[error("the token table is full; revoke tokens or let them expire")]
    TableFull,
    #[error("no such token")]
    NotFound,
    #[error("token key: {0}")]
    Key(String),
    #[error("random source: {0}")]
    Random(String),
    #[error("database: {0}")]
    Db(#[from] sqlx::Error),
}

fn invalid<T>(msg: impl Into<String>) -> Result<T, TokenError> {
    Err(TokenError::Invalid(msg.into()))
}

// ------------------------------------------------------------------------------ kinds, grants

/// What a token is for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TokenKind {
    /// A user's personal access token: acts as that user, any grants, optional expiry.
    Pat,
    /// One repository, no user: read unless minted writable. Optional expiry.
    Deploy,
    /// A pipeline's token: repositories or an owner glob, no user, never admin, and it always
    /// expires (a day by default, 30 days at most).
    Ci,
}

impl TokenKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pat => "pat",
            Self::Deploy => "deploy",
            Self::Ci => "ci",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "pat" => Some(Self::Pat),
            "deploy" => Some(Self::Deploy),
            "ci" => Some(Self::Ci),
            _ => None,
        }
    }

    fn prefix(self) -> &'static str {
        match self {
            Self::Pat => "asp_",
            Self::Deploy => "asd_",
            Self::Ci => "asc_",
        }
    }
}

fn access_str(a: Access) -> &'static str {
    match a {
        Access::Read => "read",
        Access::Write => "write",
        Access::Admin => "admin",
    }
}

fn access_parse(s: &str) -> Option<Access> {
    match s {
        "read" => Some(Access::Read),
        "write" => Some(Access::Write),
        "admin" => Some(Access::Admin),
        _ => None,
    }
}

/// One grant: an access level on a repository (`owner/name`) or a glob (`*`, `owner/*`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Grant {
    pub repo: String,
    pub access: Access,
}

impl Grant {
    /// Whether this grant's repo pattern covers `repo`.
    pub fn covers(&self, repo: &str) -> bool {
        pattern_matches(&self.repo, repo)
    }
}

fn pattern_matches(pattern: &str, repo: &str) -> bool {
    if pattern == "*" {
        return true;
    }
    match pattern.strip_suffix("/*") {
        Some(owner) => repo
            .strip_prefix(owner)
            .and_then(|r| r.strip_prefix('/'))
            .is_some_and(|name| !name.is_empty() && !name.contains('/')),
        None => pattern == repo,
    }
}

/// `*`, `owner/*` or an exact `owner/name`; `exact_only` refuses the globs.
fn validate_pattern(p: &str, exact_only: bool) -> Result<(), TokenError> {
    let ok = if p == "*" {
        !exact_only
    } else if let Some(owner) = p.strip_suffix("/*") {
        !exact_only && RepoName::parse(&format!("{owner}/x")).is_ok()
    } else {
        RepoName::parse(p).is_ok()
    };
    if ok {
        Ok(())
    } else if exact_only {
        invalid(format!(
            "`{p}` is not an exact repository name (owner/name)"
        ))
    } else {
        invalid(format!("`{p}` is not a repository, `owner/*` or `*`"))
    }
}

// ------------------------------------------------------------------------------ the server key

/// The server's HMAC key: 32 random bytes kept in a file, hex encoded, readable by the server
/// user only. `Debug` never prints it.
#[derive(Clone)]
pub struct TokenKey([u8; KEY_BYTES]);

impl fmt::Debug for TokenKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("TokenKey(<redacted>)")
    }
}

fn random_bytes<const N: usize>() -> Result<[u8; N], TokenError> {
    let mut buf = [0u8; N];
    getrandom::fill(&mut buf).map_err(|e| TokenError::Random(e.to_string()))?;
    Ok(buf)
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes.iter().fold(String::new(), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    })
}

fn unhex(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) || !s.is_ascii() {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok())
        .collect()
}

impl TokenKey {
    /// A fresh random key.
    pub fn generate() -> Result<Self, TokenError> {
        Ok(Self(random_bytes()?))
    }

    /// Reads the key file. On Unix a file readable by group or others is refused.
    pub fn load(path: &Path) -> Result<Self, TokenError> {
        let bad = |m: String| TokenError::Key(format!("{}: {m}", path.display()));
        let file = std::fs::File::open(path).map_err(|e| bad(e.to_string()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = file
                .metadata()
                .map_err(|e| bad(e.to_string()))?
                .permissions()
                .mode();
            if mode & 0o077 != 0 {
                return Err(bad(format!(
                    "mode {:04o} lets other users read the key; chmod 600",
                    mode & 0o7777
                )));
            }
        }
        let mut text = String::new();
        std::io::Read::read_to_string(&mut std::io::Read::take(file, 1024), &mut text)
            .map_err(|e| bad(e.to_string()))?;
        let bytes = unhex(text.trim()).ok_or_else(|| bad("not hexadecimal".into()))?;
        let key: [u8; KEY_BYTES] = bytes.try_into().map_err(|_| {
            bad(format!(
                "must be {KEY_BYTES} bytes ({} hex digits)",
                KEY_BYTES * 2
            ))
        })?;
        Ok(Self(key))
    }

    /// Reads the key file, or creates it (mode 0600 on Unix, never overwriting) when missing.
    /// Only the token CLI creates keys: a server that starts without its key must fail, not mint
    /// a new one that no stored digest matches.
    pub fn load_or_create(path: &Path) -> Result<Self, TokenError> {
        if path.exists() {
            return Self::load(path);
        }
        let key = Self::generate()?;
        let bad = |e: std::io::Error| TokenError::Key(format!("{}: {e}", path.display()));
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            opts.mode(0o600);
        }
        match opts.open(path) {
            Ok(mut f) => {
                std::io::Write::write_all(&mut f, format!("{}\n", hex(&key.0)).as_bytes())
                    .map_err(bad)?;
                Ok(key)
            }
            // Another process created it first: use theirs.
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Self::load(path),
            Err(e) => Err(bad(e)),
        }
    }

    /// The keyed digest of a whole token.
    fn tag(&self, token: &str) -> [u8; 32] {
        let mut mac = Hmac::<Sha256>::new_from_slice(&self.0).expect("HMAC takes any key length");
        mac.update(MAC_DOMAIN);
        mac.update(token.as_bytes());
        mac.finalize().into_bytes().into()
    }
}

/// Compares a digest with stored bytes without an early exit.
fn ct_eq(a: &[u8; 32], b: &[u8]) -> bool {
    let diff = a
        .iter()
        .zip(b.iter().chain(std::iter::repeat(&0)))
        .fold(0u8, |d, (x, y)| d | (x ^ y));
    let len_ok = b.len() == a.len();
    std::hint::black_box(diff) == 0 && len_ok
}

// ------------------------------------------------------------------------------ requests

/// What to mint.
#[derive(Clone, Debug)]
pub struct NewToken {
    pub kind: TokenKind,
    /// The user a `pat` acts as; `None` for `deploy` and `ci`.
    pub user: Option<String>,
    /// Repositories (and, for `pat` and `ci`, globs) the token covers.
    pub repos: Vec<String>,
    pub access: Access,
    /// Lifetime in seconds from now. `None` is no expiry (a `ci` token then gets the default).
    pub expires_in: Option<i64>,
    pub label: String,
}

/// A freshly minted token. The secret is available once; `Debug` does not print it.
pub struct IssuedToken {
    pub id: String,
    pub kind: TokenKind,
    pub principal: String,
    pub grants: Vec<Grant>,
    pub expires: Option<i64>,
    secret: String,
}

impl IssuedToken {
    /// The token itself. Show it to the person who asked and drop it; it is not stored.
    pub fn secret(&self) -> &str {
        &self.secret
    }
}

impl fmt::Debug for IssuedToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("IssuedToken")
            .field("id", &self.id)
            .field("kind", &self.kind)
            .field("principal", &self.principal)
            .field("secret", &"<redacted>")
            .finish()
    }
}

/// One row of `list`: everything but the secret and its digest.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TokenInfo {
    pub id: String,
    pub kind: TokenKind,
    pub principal: String,
    pub label: String,
    pub created: i64,
    pub expires: Option<i64>,
    pub revoked: Option<i64>,
    pub grants: Vec<Grant>,
}

impl TokenInfo {
    /// `active`, `revoked` or `expired` at `now`.
    pub fn status(&self, now: i64) -> &'static str {
        if self.revoked.is_some() {
            "revoked"
        } else if self.expires.is_some_and(|e| now >= e) {
            "expired"
        } else {
            "active"
        }
    }
}

struct Plan {
    user: Option<String>,
    grants: Vec<Grant>,
    expires: Option<i64>,
}

fn validate_user(u: &str) -> Result<(), TokenError> {
    let ok = !u.is_empty()
        && u.len() <= MAX_USER
        && u.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'));
    if ok {
        Ok(())
    } else {
        invalid("a user name is 1 to 64 characters of letters, digits, `.`, `_` and `-`")
    }
}

fn plan(req: &NewToken, now: i64) -> Result<Plan, TokenError> {
    if req.label.len() > MAX_LABEL || req.label.chars().any(char::is_control) {
        return invalid("a label is at most 128 characters, without control characters");
    }
    let mut repos: Vec<&str> = Vec::new();
    for r in &req.repos {
        if !repos.contains(&r.as_str()) {
            repos.push(r);
        }
    }
    if repos.is_empty() {
        return invalid("at least one repository is required");
    }
    if repos.len() > MAX_GRANTS_PER_TOKEN {
        return invalid(format!(
            "at most {MAX_GRANTS_PER_TOKEN} repositories per token"
        ));
    }
    if req.expires_in.is_some_and(|s| s <= 0) {
        return invalid("the lifetime must be positive");
    }
    let (user, exact_only, expires_in) = match req.kind {
        TokenKind::Pat => {
            let Some(u) = req.user.as_deref() else {
                return invalid("a personal token needs a user");
            };
            validate_user(u)?;
            (Some(u.to_owned()), false, req.expires_in)
        }
        TokenKind::Deploy => {
            if req.user.is_some() {
                return invalid("a deploy token has no user");
            }
            if repos.len() != 1 {
                return invalid("a deploy token covers exactly one repository");
            }
            if req.access == Access::Admin {
                return invalid("a deploy token is read or write, never admin");
            }
            (None, true, req.expires_in)
        }
        TokenKind::Ci => {
            if req.user.is_some() {
                return invalid("a CI token has no user");
            }
            if req.access == Access::Admin {
                return invalid("a CI token is read or write, never admin");
            }
            if repos.contains(&"*") {
                return invalid("a CI token covers repositories or `owner/*`, not `*`");
            }
            let life = req.expires_in.unwrap_or(DEFAULT_CI_LIFETIME_SECS);
            if life > MAX_CI_LIFETIME_SECS {
                return invalid("a CI token lives at most 30 days");
            }
            (None, false, Some(life))
        }
    };
    for r in &repos {
        validate_pattern(r, exact_only)?;
    }
    let expires = match expires_in {
        Some(s) => Some(
            now.checked_add(s)
                .ok_or_else(|| TokenError::Invalid("the lifetime is too long".into()))?,
        ),
        None => None,
    };
    Ok(Plan {
        user,
        grants: repos
            .into_iter()
            .map(|r| Grant {
                repo: r.to_owned(),
                access: req.access,
            })
            .collect(),
        expires,
    })
}

// ------------------------------------------------------------------------------ the store

/// A verified, live token as the authorizer sees it.
#[derive(Debug)]
struct Verified {
    principal: Principal,
    expires: Option<i64>,
    grants: Vec<Grant>,
}

#[derive(Clone)]
struct Cached {
    at: i64,
    /// `None` is a cached refusal.
    rec: Option<Arc<Verified>>,
}

/// The unix clock, replaceable for tests.
pub type Clock = Arc<dyn Fn() -> i64 + Send + Sync>;

fn system_clock() -> Clock {
    Arc::new(|| {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
    })
}

/// Tokens and identity links in the server database. The schema is migration `0103`.
pub struct TokenStore {
    pool: AnyPool,
    key: TokenKey,
    now: Clock,
    cache: quick_cache::sync::Cache<[u8; 32], Cached>,
}

impl TokenStore {
    pub fn new(pool: AnyPool, key: TokenKey) -> Self {
        Self {
            pool,
            key,
            now: system_clock(),
            cache: quick_cache::sync::Cache::new(CACHE_ENTRIES),
        }
    }

    /// Replaces the clock (tests).
    #[must_use]
    pub fn with_clock(mut self, now: Clock) -> Self {
        self.now = now;
        self
    }

    pub fn now(&self) -> i64 {
        (self.now)()
    }

    /// Mints a token. The returned [`IssuedToken`] is the only place the secret ever exists.
    pub async fn create(&self, req: &NewToken) -> Result<IssuedToken, TokenError> {
        let now = self.now();
        let plan = plan(req, now)?;
        let id = hex(&random_bytes::<ID_BYTES>()?);
        let secret = URL_SAFE_NO_PAD.encode(random_bytes::<SECRET_BYTES>()?);
        let token = format!("{}{id}_{secret}", req.kind.prefix());
        debug_assert_eq!(token.len(), TOKEN_LEN);
        let principal = match (req.kind, &plan.user) {
            (_, Some(user)) => user.clone(),
            (TokenKind::Deploy, None) => format!("deploy:{}", plan.grants[0].repo),
            _ => format!("ci:{id}"),
        };

        let mut tx = self.pool.begin().await?;
        let cutoff = now.saturating_sub(DEAD_RETENTION_SECS);
        sqlx::query(
            "DELETE FROM auth_token_grants WHERE token_id IN (SELECT id FROM auth_tokens \
             WHERE revoked < $1 OR expires < $1)",
        )
        .bind(cutoff)
        .execute(&mut *tx)
        .await?;
        sqlx::query("DELETE FROM auth_tokens WHERE revoked < $1 OR expires < $1")
            .bind(cutoff)
            .execute(&mut *tx)
            .await?;
        let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM auth_tokens")
            .fetch_one(&mut *tx)
            .await?;
        let live: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM auth_tokens \
             WHERE revoked IS NULL AND (expires IS NULL OR expires > $1)",
        )
        .bind(now)
        .fetch_one(&mut *tx)
        .await?;
        if rows >= MAX_TOKEN_ROWS || live >= MAX_LIVE_TOKENS {
            return Err(TokenError::TableFull);
        }
        sqlx::query(
            "INSERT INTO auth_tokens (id, kind, principal, digest, label, created, expires) \
             VALUES ($1, $2, $3, $4, $5, $6, $7)",
        )
        .bind(id.clone())
        .bind(req.kind.as_str())
        .bind(principal.clone())
        .bind(self.key.tag(&token).to_vec())
        .bind(req.label.clone())
        .bind(now)
        .bind(plan.expires)
        .execute(&mut *tx)
        .await?;
        for g in &plan.grants {
            sqlx::query(
                "INSERT INTO auth_token_grants (token_id, repo, access) VALUES ($1, $2, $3)",
            )
            .bind(id.clone())
            .bind(g.repo.clone())
            .bind(access_str(g.access))
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        tracing::info!(
            token_id = %id,
            kind = req.kind.as_str(),
            principal = %principal,
            "token created"
        );
        Ok(IssuedToken {
            id,
            kind: req.kind,
            principal,
            grants: plan.grants,
            expires: plan.expires,
            secret: token,
        })
    }

    /// Every token, oldest first, without secrets.
    pub async fn list(&self) -> Result<Vec<TokenInfo>, TokenError> {
        let rows = sqlx::query(
            "SELECT id, kind, principal, label, CAST(created AS BIGINT) AS created, \
                    CAST(expires AS BIGINT) AS expires, CAST(revoked AS BIGINT) AS revoked \
             FROM auth_tokens ORDER BY created, id LIMIT $1",
        )
        .bind(MAX_TOKEN_ROWS)
        .fetch_all(&self.pool)
        .await?;
        let grant_rows = sqlx::query(
            "SELECT token_id, repo, access FROM auth_token_grants ORDER BY token_id, repo LIMIT $1",
        )
        .bind(MAX_TOKEN_ROWS * i64::try_from(MAX_GRANTS_PER_TOKEN).unwrap_or(32))
        .fetch_all(&self.pool)
        .await?;
        let mut grants: HashMap<String, Vec<Grant>> = HashMap::new();
        for r in grant_rows {
            let id: String = r.try_get("token_id")?;
            let repo: String = r.try_get("repo")?;
            let access: String = r.try_get("access")?;
            if let Some(access) = access_parse(&access) {
                grants.entry(id).or_default().push(Grant { repo, access });
            }
        }
        let mut out = Vec::with_capacity(rows.len());
        for r in rows {
            let id: String = r.try_get("id")?;
            let kind: String = r.try_get("kind")?;
            out.push(TokenInfo {
                kind: TokenKind::parse(&kind).unwrap_or(TokenKind::Pat),
                principal: r.try_get("principal")?,
                label: r.try_get("label")?,
                created: r.try_get("created")?,
                expires: r.try_get("expires")?,
                revoked: r.try_get("revoked")?,
                grants: grants.remove(&id).unwrap_or_default(),
                id,
            });
        }
        Ok(out)
    }

    /// Revokes the token with public id `id`. `Ok(false)` when it was already revoked. Takes
    /// effect at once in this process and within [`CACHE_TTL_SECS`] elsewhere.
    pub async fn revoke(&self, id: &str) -> Result<bool, TokenError> {
        let done =
            sqlx::query("UPDATE auth_tokens SET revoked = $1 WHERE id = $2 AND revoked IS NULL")
                .bind(self.now())
                .bind(id.to_owned())
                .execute(&self.pool)
                .await?;
        self.cache.clear();
        if done.rows_affected() > 0 {
            tracing::info!(token_id = %id, "token revoked");
            return Ok(true);
        }
        let exists: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM auth_tokens WHERE id = $1")
            .bind(id.to_owned())
            .fetch_one(&self.pool)
            .await?;
        if exists == 0 {
            Err(TokenError::NotFound)
        } else {
            Ok(false)
        }
    }

    /// Links an OIDC `(issuer, subject)` to a principal (used by M33).
    pub async fn link_identity(
        &self,
        issuer: &str,
        subject: &str,
        principal: &str,
    ) -> Result<(), TokenError> {
        for (what, v) in [
            ("issuer", issuer),
            ("subject", subject),
            ("principal", principal),
        ] {
            if v.is_empty() || v.len() > MAX_IDENTITY_FIELD || v.chars().any(char::is_control) {
                return invalid(format!(
                    "the {what} must be 1 to 512 characters, without control characters"
                ));
            }
        }
        let mut tx = self.pool.begin().await?;
        let updated = sqlx::query(
            "UPDATE auth_identity SET principal = $1 WHERE issuer = $2 AND subject = $3",
        )
        .bind(principal.to_owned())
        .bind(issuer.to_owned())
        .bind(subject.to_owned())
        .execute(&mut *tx)
        .await?;
        if updated.rows_affected() == 0 {
            let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM auth_identity")
                .fetch_one(&mut *tx)
                .await?;
            if n >= MAX_IDENTITIES {
                return Err(TokenError::TableFull);
            }
            sqlx::query(
                "INSERT INTO auth_identity (issuer, subject, principal) VALUES ($1, $2, $3)",
            )
            .bind(issuer.to_owned())
            .bind(subject.to_owned())
            .bind(principal.to_owned())
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// The principal an OIDC `(issuer, subject)` maps to.
    pub async fn resolve_identity(
        &self,
        issuer: &str,
        subject: &str,
    ) -> Result<Option<String>, TokenError> {
        Ok(sqlx::query_scalar(
            "SELECT principal FROM auth_identity WHERE issuer = $1 AND subject = $2",
        )
        .bind(issuer.to_owned())
        .bind(subject.to_owned())
        .fetch_optional(&self.pool)
        .await?)
    }

    /// Looks `token` up and checks it. `Unauthenticated` for anything that is not a live token.
    async fn verify(&self, token: &str) -> Result<Arc<Verified>, AuthError> {
        let now = self.now();
        let tag = self.key.tag(token);
        if let Some(hit) = self.cache.get(&tag)
            && now >= hit.at
            && now - hit.at < CACHE_TTL_SECS
        {
            return match hit.rec {
                Some(rec) if rec.expires.is_none_or(|e| now < e) => Ok(rec),
                _ => Err(AuthError::Unauthenticated),
            };
        }
        let rec = self.lookup(token, &tag, now).await?;
        self.cache.insert(
            tag,
            Cached {
                at: now,
                rec: rec.clone(),
            },
        );
        rec.ok_or(AuthError::Unauthenticated)
    }

    /// The database read behind [`Self::verify`]. A database failure refuses the call and is not
    /// cached.
    async fn lookup(
        &self,
        token: &str,
        tag: &[u8; 32],
        now: i64,
    ) -> Result<Option<Arc<Verified>>, AuthError> {
        let unavailable = |e: sqlx::Error| {
            tracing::warn!(error = %e, "token store lookup failed");
            AuthError::Forbidden
        };
        let id = parse_token(token);
        let row = match id {
            Some(id) => sqlx::query(
                "SELECT principal, digest, CAST(expires AS BIGINT) AS expires, \
                        CAST(revoked AS BIGINT) AS revoked \
                 FROM auth_tokens WHERE id = $1",
            )
            .bind(id.to_owned())
            .fetch_optional(&self.pool)
            .await
            .map_err(unavailable)?,
            None => None,
        };
        // The same compare runs whether or not a row was found.
        let stored: Vec<u8> = match &row {
            Some(r) => r.try_get("digest").map_err(unavailable)?,
            None => vec![0; 32],
        };
        let digest_ok = ct_eq(tag, &stored);
        let (Some(row), Some(id), true) = (row, id, digest_ok) else {
            return Ok(None);
        };
        let expires: Option<i64> = row.try_get("expires").map_err(unavailable)?;
        let revoked: Option<i64> = row.try_get("revoked").map_err(unavailable)?;
        if revoked.is_some() || expires.is_some_and(|e| now >= e) {
            return Ok(None);
        }
        let principal: String = row.try_get("principal").map_err(unavailable)?;
        let grant_rows =
            sqlx::query("SELECT repo, access FROM auth_token_grants WHERE token_id = $1")
                .bind(id.to_owned())
                .fetch_all(&self.pool)
                .await
                .map_err(unavailable)?;
        let mut grants = Vec::with_capacity(grant_rows.len());
        for r in grant_rows {
            let repo: String = r.try_get("repo").map_err(unavailable)?;
            let access: String = r.try_get("access").map_err(unavailable)?;
            if let Some(access) = access_parse(&access) {
                grants.push(Grant { repo, access });
            }
        }
        Ok(Some(Arc::new(Verified {
            principal: Principal { id: principal },
            expires,
            grants,
        })))
    }
}

/// The public id of a well-formed token, else `None`. Shape only; it proves nothing.
fn parse_token(token: &str) -> Option<&str> {
    let well_formed = token.len() == TOKEN_LEN
        && token.is_ascii()
        && matches!(&token[..4], "asp_" | "asd_" | "asc_")
        && token.as_bytes()[4 + ID_BYTES * 2] == b'_';
    well_formed.then(|| &token[4..4 + ID_BYTES * 2])
}

// ------------------------------------------------------------------------------ the authorizer

/// The [`Authorizer`] over a [`TokenStore`] (`[auth] provider = "token_store"`).
#[derive(Clone)]
pub struct TokenStoreAuthz {
    store: Arc<TokenStore>,
}

impl TokenStoreAuthz {
    pub fn new(store: Arc<TokenStore>) -> Self {
        Self { store }
    }

    pub fn store(&self) -> &Arc<TokenStore> {
        &self.store
    }
}

#[async_trait]
impl Authorizer for TokenStoreAuthz {
    async fn authorize(
        &self,
        token: Option<&BearerToken>,
        repo: &str,
        access: Access,
    ) -> Result<Principal, AuthError> {
        let token = token.ok_or(AuthError::Unauthenticated)?;
        let verified = self.store.verify(token.expose()).await?;
        let best = verified
            .grants
            .iter()
            .filter(|g| g.covers(repo))
            .map(|g| g.access)
            .max();
        match best {
            Some(have) if have >= access => Ok(verified.principal.clone()),
            _ => Err(AuthError::Forbidden),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn globs_cover_what_they_say() {
        assert!(pattern_matches("*", "a/b"));
        assert!(pattern_matches("acme/*", "acme/web"));
        assert!(!pattern_matches("acme/*", "acme/web/x"));
        assert!(!pattern_matches("acme/*", "acmex/web"));
        assert!(!pattern_matches("acme/*", "acme/"));
        assert!(pattern_matches("acme/web", "acme/web"));
        assert!(!pattern_matches("acme/web", "acme/web2"));
    }

    #[test]
    fn patterns_validate() {
        for ok in ["*", "acme/*", "acme/web", "a.b/c_d-e"] {
            validate_pattern(ok, false).unwrap();
        }
        for bad in [
            "",
            "acme",
            "acme/web/x",
            "*/web",
            "acme/we*",
            "../x",
            ".a/b",
            "acme/*/x",
        ] {
            assert!(validate_pattern(bad, false).is_err(), "{bad}");
        }
        assert!(validate_pattern("*", true).is_err());
        assert!(validate_pattern("acme/*", true).is_err());
        validate_pattern("acme/web", true).unwrap();
    }

    #[test]
    fn digest_compare_is_exact() {
        let a = [7u8; 32];
        assert!(ct_eq(&a, &a));
        let mut b = a;
        b[31] ^= 1;
        assert!(!ct_eq(&a, &b));
        assert!(!ct_eq(&a, &a[..31]));
        assert!(!ct_eq(&a, &[a.as_slice(), &[0]].concat()));
    }

    #[test]
    fn token_shape() {
        let id = "0123456789abcdef";
        let secret = "A".repeat(43);
        let t = format!("asp_{id}_{secret}");
        assert_eq!(t.len(), TOKEN_LEN);
        assert_eq!(parse_token(&t), Some(id));
        assert_eq!(parse_token(&t[1..]), None);
        assert_eq!(parse_token(&format!("asx_{id}_{secret}")), None);
        assert_eq!(parse_token(&format!("asp_{id}-{secret}")), None);
        assert_eq!(parse_token(""), None);
    }

    #[test]
    fn hex_round_trips() {
        let bytes = [0u8, 1, 0xab, 0xff];
        assert_eq!(unhex(&hex(&bytes)).unwrap(), bytes);
        assert!(unhex("abc").is_none());
        assert!(unhex("zz").is_none());
    }

    #[test]
    fn debug_never_prints_secrets() {
        let key = TokenKey([9; 32]);
        assert!(!format!("{key:?}").contains("09"));
        let issued = IssuedToken {
            id: "i".into(),
            kind: TokenKind::Pat,
            principal: "p".into(),
            grants: vec![],
            expires: None,
            secret: "SECRET-VALUE".into(),
        };
        assert!(!format!("{issued:?}").contains("SECRET-VALUE"));
    }
}
