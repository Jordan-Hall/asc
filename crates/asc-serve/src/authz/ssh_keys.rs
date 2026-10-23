//! SSH public keys in the standalone token store (SPEC §15.6.2, §25.11.3; PLAN U84).
//!
//! The table `ssh_keys` (migration `0104`) holds public keys only: the canonical wire blob and its
//! SHA-256 fingerprint, which is the key's identity. [`TokenStore`] adds and revokes keys here and
//! [`TokenStoreAuthz`](super::token_store::TokenStoreAuthz) answers
//! [`Authorizer::identify_ssh_key`](asc_rpc::auth::Authorizer::identify_ssh_key) and
//! [`Authorizer::authorize_ssh_key`](asc_rpc::auth::Authorizer::authorize_ssh_key) from it. All of
//! this is behind the Cargo feature `git-ssh`: the key types come from the SSH library
//! (`russh::keys`), and the table and the `asc-rpc` types exist in every build.
//!
//! # Two kinds
//!
//! A **user key** acts as its user (`pat:<user>`, the principal of that user's personal tokens)
//! with that user's rights and never more. The token store keeps rights on tokens, so a user's
//! rights are the grants of the user's live personal tokens; a user with no live token has none.
//! A **deploy key** acts as `deploy:<owner/repo>`, reaches that one repository, and is read-only
//! unless registered with write. A fingerprint belongs to one row, so a key cannot belong to two
//! users, two repositories, or to a user and a repository.
//!
//! # Rules
//!
//! * Keys are stored and compared by fingerprint of the canonical wire blob (the parsed key,
//!   re-encoded), never by text or comment.
//! * Only `ssh-ed25519`, `ecdsa-sha2-nistp256`, `-nistp384`, `-nistp521` and `ssh-rsa` keys of
//!   [`DEFAULT_MIN_RSA_BITS`] to [`MAX_RSA_BITS`] bits are accepted (SPEC §25.11.1); DSA, security
//!   keys, certificates and weak RSA are refused when added and again when looked up.
//! * A key file is one `authorized_keys` line, type, base64 and comment. Option prefixes such as
//!   `command=` or `restrict` are refused: they would promise restrictions this store does not
//!   enforce.
//! * The principal is rebuilt from the typed columns on every lookup. The stored text only
//!   supplies a user name, which must be valid; a row that cannot form a typed principal is
//!   refused.
//! * Tables are bounded: [`MAX_SSH_KEY_ROWS`] rows, [`MAX_LIVE_SSH_KEYS`] live keys and
//!   [`MAX_KEYS_PER_OWNER`] live keys per user or repository. Rows dead longer than
//!   [`DEAD_RETENTION_SECS`] are deleted when a key is added.
//! * Lookups are cached by fingerprint for [`CACHE_TTL_SECS`], positive and negative alike (the
//!   §15.1 cache, here without the forge's 60 s positive window so that a revocation by another
//!   process, the CLI, is effective within the bound the SPEC states for it). A revocation made
//!   through this store clears the cache at once.
//!
//! Nothing here logs a blob; logs carry the fingerprint.

use std::fmt::Write as _;
use std::sync::Arc;

use asc_rpc::auth::{
    Access, AuthError, Decision, Principal, SshFingerprint, SshIdentity, SshKey, SshKeyKind,
};
use russh::keys::ssh_key::{Algorithm, EcdsaCurve, PublicKey, public::KeyData};
use sqlx::Row;

use super::token_cli::format_utc;
use super::token_store::{
    CACHE_TTL_SECS, DEAD_RETENTION_SECS, Grant, ID_BYTES, TokenError, TokenKind, TokenStore,
    TokenStoreAuthz, access_parse, hex, principal_id, random_bytes, validate_pattern,
    validate_user,
};
use crate::forgejo::INTERNAL_PRINCIPAL;

/// The smallest RSA modulus accepted by default (SPEC §25.11.1 `min_rsa_bits`).
pub const DEFAULT_MIN_RSA_BITS: u32 = 2048;
/// The largest RSA modulus accepted: bigger keys only cost signature-verification time.
pub const MAX_RSA_BITS: u32 = 16_384;
/// Most rows in `ssh_keys`, dead ones included.
pub const MAX_SSH_KEY_ROWS: i64 = 50_000;
/// Most live (unrevoked, unexpired) keys.
pub const MAX_LIVE_SSH_KEYS: i64 = 20_000;
/// Most live keys one user or one repository may hold.
pub const MAX_KEYS_PER_OWNER: i64 = 100;
/// Longest key file read, in bytes.
pub const MAX_KEY_FILE: usize = 16 * 1024;
/// Most grants read for one user key.
const MAX_USER_GRANTS: i64 = 4096;
const MAX_TITLE: usize = 128;
/// `last_used` is written at most this often per key.
const LAST_USED_GRANULARITY_SECS: i64 = 60;

// ------------------------------------------------------------------------------ key files

/// The key types accepted at the start of an `authorized_keys` line.
const KEY_TYPES: [&str; 5] = [
    "ssh-ed25519",
    "ecdsa-sha2-nistp256",
    "ecdsa-sha2-nistp384",
    "ecdsa-sha2-nistp521",
    "ssh-rsa",
];

/// A parsed, policy-checked public key.
#[derive(Clone, Debug)]
pub struct ParsedKey {
    /// The key type name (`ssh-ed25519`, `ssh-rsa`, ...).
    pub algo: String,
    /// The canonical wire blob.
    pub blob: Vec<u8>,
    /// The line's comment, sanitised, for a default title.
    pub comment: String,
}

impl ParsedKey {
    pub fn fingerprint(&self) -> SshFingerprint {
        SshFingerprint::of_blob(&self.blob)
    }

    /// The key as the listener hands it to the authorizer.
    pub fn ssh_key(&self) -> Result<SshKey, TokenError> {
        SshKey::new(&self.algo, &self.blob)
            .map_err(|_| TokenError::Invalid("the key is too large".into()))
    }
}

fn refuse<T>(msg: impl Into<String>) -> Result<T, TokenError> {
    Err(TokenError::Invalid(msg.into()))
}

/// Whether `key` is allowed: the same list as the listener (SPEC §25.11.1).
///
/// # Errors
/// [`TokenError::Invalid`] with the reason: DSA, a security key, an unknown type, a curve other
/// than the NIST ones, or RSA outside `min_rsa_bits..=`[`MAX_RSA_BITS`].
pub fn check_policy(key: &PublicKey, min_rsa_bits: u32) -> Result<(), TokenError> {
    match (key.algorithm(), key.key_data()) {
        (Algorithm::Ed25519, KeyData::Ed25519(_)) => Ok(()),
        (
            Algorithm::Ecdsa {
                curve: EcdsaCurve::NistP256 | EcdsaCurve::NistP384 | EcdsaCurve::NistP521,
            },
            KeyData::Ecdsa(_),
        ) => Ok(()),
        (Algorithm::Rsa { .. }, KeyData::Rsa(rsa)) => {
            let bits = rsa.key_size();
            if bits < min_rsa_bits.max(1) {
                refuse(format!(
                    "RSA keys under {min_rsa_bits} bits are refused (this one has {bits})"
                ))
            } else if bits > MAX_RSA_BITS {
                refuse(format!("RSA keys over {MAX_RSA_BITS} bits are refused"))
            } else {
                Ok(())
            }
        }
        (Algorithm::Dsa, _) => refuse("DSA keys (ssh-dss) are refused"),
        (Algorithm::SkEcdsaSha2NistP256 | Algorithm::SkEd25519, _) => {
            refuse("security-key (sk-) keys are not supported")
        }
        (other, _) => refuse(format!(
            "the key type `{}` is not supported",
            other.as_str()
        )),
    }
}

/// Parses and checks a wire blob: the policy of [`check_policy`], and the blob must be the
/// canonical encoding of the key it holds.
///
/// # Errors
/// [`TokenError::Invalid`] when the blob is not one accepted key in canonical form.
pub fn check_blob(blob: &[u8], min_rsa_bits: u32) -> Result<PublicKey, TokenError> {
    let key = PublicKey::from_bytes(blob)
        .map_err(|_| TokenError::Invalid("not an SSH public key blob".into()))?;
    check_policy(&key, min_rsa_bits)?;
    match key.to_bytes() {
        Ok(canonical) if canonical == blob => Ok(key),
        _ => refuse("the key blob is not in canonical form"),
    }
}

/// Parses the text of a key file: one `authorized_keys` line (type, base64, comment).
///
/// Blank lines and `#` comment lines are skipped; a second key, a line with an option prefix, a
/// key of a refused type or a weak RSA key is an error.
///
/// # Errors
/// [`TokenError::Invalid`] with the reason.
pub fn parse_key_file(text: &str, min_rsa_bits: u32) -> Result<ParsedKey, TokenError> {
    if text.len() > MAX_KEY_FILE {
        return refuse(format!("a key file is at most {MAX_KEY_FILE} bytes"));
    }
    let mut lines = text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'));
    let Some(line) = lines.next() else {
        return refuse("the key file holds no key");
    };
    if lines.next().is_some() {
        return refuse("the key file holds more than one key; register one key at a time");
    }
    if line.chars().any(|c| c.is_control() && c != '\t') {
        return refuse("the key line has control characters");
    }
    let first = line.split_ascii_whitespace().next().unwrap_or_default();
    if !KEY_TYPES.contains(&first) {
        return match first {
            "ssh-dss" => refuse("DSA keys (ssh-dss) are refused"),
            f if f.starts_with("sk-") => refuse("security-key (sk-) keys are not supported"),
            f if f.contains("-cert-") => refuse("SSH certificates are not supported; use the key"),
            f if f.starts_with("ssh-") || f.starts_with("ecdsa-") => {
                refuse(format!("the key type `{f}` is not supported"))
            }
            _ => refuse(
                "options before the key (such as `command=` or `restrict`) are refused: \
                 give the bare `type base64 comment` line",
            ),
        };
    }
    // `from_openssh` wants single spaces and a type that matches the blob's own.
    let line = line.split_ascii_whitespace().collect::<Vec<_>>().join(" ");
    let key = PublicKey::from_openssh(&line)
        .map_err(|_| TokenError::Invalid("not a valid OpenSSH public key line".into()))?;
    check_policy(&key, min_rsa_bits)?;
    let blob = key
        .to_bytes()
        .map_err(|_| TokenError::Invalid("not a valid OpenSSH public key line".into()))?;
    let comment: String = key
        .comment()
        .as_str_lossy()
        .chars()
        .filter(|c| !c.is_control())
        .take(MAX_TITLE)
        .collect();
    Ok(ParsedKey {
        algo: key.algorithm().as_str().to_owned(),
        blob,
        comment,
    })
}

// ------------------------------------------------------------------------------ requests

/// What to register.
#[derive(Clone, Debug)]
pub struct NewSshKey {
    pub kind: SshKeyKind,
    /// The user a user key acts as; `None` for a deploy key.
    pub user: Option<String>,
    /// The repository (`owner/name`) a deploy key is bound to; `None` for a user key.
    pub repo: Option<String>,
    /// A deploy key may push. A user key's rights come from the user, so `true` is refused.
    pub write: bool,
    /// The text of the key file (one `authorized_keys` line).
    pub key_file: String,
    /// A note shown by `list`; the key's comment when empty.
    pub title: String,
    /// Lifetime in seconds from now; `None` is no expiry.
    pub expires_in: Option<i64>,
    /// The RSA floor: [`DEFAULT_MIN_RSA_BITS`], or the listener's `min_rsa_bits`.
    pub min_rsa_bits: u32,
}

/// A registered key, as `add` and `list` show it. No blob.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SshKeyInfo {
    pub id: String,
    pub kind: SshKeyKind,
    /// The typed principal text as stored; display only, never an identity.
    pub principal: String,
    pub repo: Option<String>,
    pub write: bool,
    pub algo: String,
    pub fingerprint: SshFingerprint,
    pub title: String,
    pub created: i64,
    pub expires: Option<i64>,
    pub revoked: Option<i64>,
    pub last_used: Option<i64>,
}

impl SshKeyInfo {
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

fn kind_parse(s: &str) -> Option<SshKeyKind> {
    match s {
        "user" => Some(SshKeyKind::User),
        "deploy" => Some(SshKeyKind::Deploy),
        _ => None,
    }
}

fn validate_title(t: &str) -> Result<(), TokenError> {
    if t.chars().count() > MAX_TITLE || t.chars().any(char::is_control) {
        return refuse("a title is at most 128 characters, without control characters");
    }
    Ok(())
}

/// The typed principal text and the bound repository of a request, validated.
fn owner_of(req: &NewSshKey) -> Result<(String, Option<String>), TokenError> {
    match req.kind {
        SshKeyKind::User => {
            let Some(user) = req.user.as_deref() else {
                return refuse("a user key needs a user");
            };
            if req.repo.is_some() {
                return refuse("a user key is not bound to a repository");
            }
            if req.write {
                return refuse("a user key has its user's rights; `--write` is for deploy keys");
            }
            validate_user(user)?;
            Ok((principal_id(TokenKind::Pat, Some(user), "", ""), None))
        }
        SshKeyKind::Deploy => {
            let Some(repo) = req.repo.as_deref() else {
                return refuse("a deploy key needs a repository");
            };
            if req.user.is_some() {
                return refuse("a deploy key has no user");
            }
            validate_pattern(repo, true)?;
            Ok((
                principal_id(TokenKind::Deploy, None, repo, ""),
                Some(repo.to_owned()),
            ))
        }
    }
}

// ------------------------------------------------------------------------------ the records

/// A live, verified key as the authorizer sees it.
#[derive(Debug)]
pub struct SshRecord {
    key_id: String,
    kind: SshKeyKind,
    principal: Principal,
    repo: Option<String>,
    expires: Option<i64>,
    /// What the key may do, each with the expiry of whatever backs it.
    grants: Vec<(Grant, Option<i64>)>,
}

impl SshRecord {
    fn identity(&self) -> SshIdentity {
        SshIdentity {
            key_id: self.key_id.clone(),
            kind: self.kind,
            principal: self.principal.clone(),
            repo: self.repo.clone(),
        }
    }

    fn live(&self, now: i64) -> bool {
        self.expires.is_none_or(|e| now < e)
    }

    /// The strongest access this key holds on `repo` at `now`.
    fn best(&self, repo: &str, now: i64) -> Option<Access> {
        self.grants
            .iter()
            .filter(|(g, exp)| g.covers(repo) && exp.is_none_or(|e| now < e))
            .map(|(g, _)| g.access)
            .max()
    }
}

/// A cached lookup. `None` is a cached refusal.
#[derive(Clone)]
pub struct SshCached {
    at: i64,
    rec: Option<Arc<SshRecord>>,
}

/// The principal of a verified row, rebuilt with [`principal_id`] from its typed columns. The
/// stored text is never an identity: for a user key it only supplies the user name (a leading
/// `pat:` is dropped and the rest must be a valid user name), a deploy key's principal comes from
/// its repository column. `None` refuses the row: an unknown kind, a user key with a repository,
/// a deploy key without a valid exact one, a bad user name, or the internal principal.
fn rebuilt(
    kind: &str,
    stored: &str,
    repo: Option<&str>,
) -> Option<(SshKeyKind, String, Option<String>)> {
    let kind = kind_parse(kind)?;
    let (id, repo) = match (kind, repo) {
        (SshKeyKind::User, None) => {
            let user = stored.strip_prefix("pat:").unwrap_or(stored);
            validate_user(user).ok()?;
            (principal_id(TokenKind::Pat, Some(user), "", ""), None)
        }
        (SshKeyKind::Deploy, Some(repo)) if validate_pattern(repo, true).is_ok() => (
            principal_id(TokenKind::Deploy, None, repo, ""),
            Some(repo.to_owned()),
        ),
        _ => return None,
    };
    (id != INTERNAL_PRINCIPAL && id.contains(':')).then_some((kind, id, repo))
}

fn unavailable(e: sqlx::Error) -> AuthError {
    // The text can carry bound values; the fingerprint is all that is safe to say, and the
    // caller knows it.
    tracing::warn!(error = %e, "ssh key lookup failed");
    AuthError::Forbidden
}

// ------------------------------------------------------------------------------ the store

impl TokenStore {
    /// Registers a key. Returns what `list` would show.
    ///
    /// # Errors
    /// [`TokenError::Invalid`] for a bad request, a refused key or a key already registered
    /// (also under another kind or owner); [`TokenError::Db`] otherwise.
    pub async fn add_ssh_key(&self, req: &NewSshKey) -> Result<SshKeyInfo, TokenError> {
        let now = self.now();
        let (principal, repo) = owner_of(req)?;
        validate_title(&req.title)?;
        if req.expires_in.is_some_and(|s| s <= 0) {
            return refuse("the lifetime must be positive");
        }
        let expires = match req.expires_in {
            Some(s) => Some(
                now.checked_add(s)
                    .ok_or_else(|| TokenError::Invalid("the lifetime is too long".into()))?,
            ),
            None => None,
        };
        let parsed = parse_key_file(&req.key_file, req.min_rsa_bits)?;
        let fp = parsed.fingerprint();
        let title = if req.title.is_empty() {
            parsed.comment.clone()
        } else {
            req.title.clone()
        };
        let id = hex(&random_bytes::<ID_BYTES>()?);

        let mut tx = self.pool.begin().await?;
        let cutoff = now.saturating_sub(DEAD_RETENTION_SECS);
        sqlx::query("DELETE FROM ssh_keys WHERE revoked < $1 OR expires < $1")
            .bind(cutoff)
            .execute(&mut *tx)
            .await?;
        // A dead row must not lock its key out for good: registering the key again replaces it.
        sqlx::query(
            "DELETE FROM ssh_keys WHERE fingerprint = $1 \
             AND (revoked IS NOT NULL OR (expires IS NOT NULL AND expires <= $2))",
        )
        .bind(fp.as_bytes().to_vec())
        .bind(now)
        .execute(&mut *tx)
        .await?;
        if let Some(r) = sqlx::query("SELECT kind, principal FROM ssh_keys WHERE fingerprint = $1")
            .bind(fp.as_bytes().to_vec())
            .fetch_optional(&mut *tx)
            .await?
        {
            let kind: String = r.try_get("kind")?;
            let owner: String = r.try_get("principal")?;
            return refuse(format!(
                "the key {fp} is already registered as a {kind} key for {owner}"
            ));
        }
        let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM ssh_keys")
            .fetch_one(&mut *tx)
            .await?;
        let live: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM ssh_keys \
             WHERE revoked IS NULL AND (expires IS NULL OR expires > $1)",
        )
        .bind(now)
        .fetch_one(&mut *tx)
        .await?;
        if rows >= MAX_SSH_KEY_ROWS || live >= MAX_LIVE_SSH_KEYS {
            return refuse("the SSH key table is full; revoke keys or let them expire");
        }
        let owned: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM ssh_keys WHERE principal = $1 \
             AND revoked IS NULL AND (expires IS NULL OR expires > $2)",
        )
        .bind(principal.clone())
        .bind(now)
        .fetch_one(&mut *tx)
        .await?;
        if owned >= MAX_KEYS_PER_OWNER {
            return refuse(format!(
                "{principal} already has {MAX_KEYS_PER_OWNER} keys; revoke one first"
            ));
        }
        let inserted = sqlx::query(
            "INSERT INTO ssh_keys (id, kind, principal, repo_id, \"write\", algo, public_key, \
             fingerprint, title, expires, created) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)",
        )
        .bind(id.clone())
        .bind(req.kind.as_str())
        .bind(principal.clone())
        .bind(repo.clone())
        .bind(i64::from(req.write))
        .bind(parsed.algo.clone())
        .bind(parsed.blob.clone())
        .bind(fp.as_bytes().to_vec())
        .bind(title.clone())
        .bind(expires)
        .bind(now)
        .execute(&mut *tx)
        .await;
        match inserted {
            Ok(_) => {}
            // Two adds of one key raced; the unique index decided.
            Err(sqlx::Error::Database(d)) if d.is_unique_violation() => {
                return refuse(format!("the key {fp} is already registered"));
            }
            Err(e) => return Err(e.into()),
        }
        tx.commit().await?;
        self.ssh_cache.clear();
        tracing::info!(
            key_id = %id,
            kind = req.kind.as_str(),
            principal = %principal,
            fingerprint = %fp,
            "ssh key added"
        );
        Ok(SshKeyInfo {
            id,
            kind: req.kind,
            principal,
            repo,
            write: req.write,
            algo: parsed.algo,
            fingerprint: fp,
            title,
            created: now,
            expires,
            revoked: None,
            last_used: None,
        })
    }

    /// Every key, oldest first, without blobs.
    pub async fn list_ssh_keys(&self) -> Result<Vec<SshKeyInfo>, TokenError> {
        let rows = sqlx::query(
            "SELECT id, kind, principal, repo_id, CAST(\"write\" AS BIGINT) AS w, algo, \
                    fingerprint, title, CAST(created AS BIGINT) AS created, \
                    CAST(expires AS BIGINT) AS expires, CAST(revoked AS BIGINT) AS revoked, \
                    CAST(last_used AS BIGINT) AS last_used \
             FROM ssh_keys ORDER BY created, id LIMIT $1",
        )
        .bind(MAX_SSH_KEY_ROWS)
        .fetch_all(&self.pool)
        .await?;
        let mut out = Vec::with_capacity(rows.len());
        for r in rows {
            let kind: String = r.try_get("kind")?;
            let fp: Vec<u8> = r.try_get("fingerprint")?;
            let w: i64 = r.try_get("w")?;
            let (Some(kind), Ok(fp)) = (kind_parse(&kind), <[u8; 32]>::try_from(fp)) else {
                continue;
            };
            out.push(SshKeyInfo {
                id: r.try_get("id")?,
                kind,
                principal: r.try_get("principal")?,
                repo: r.try_get("repo_id")?,
                write: w != 0,
                algo: r.try_get("algo")?,
                fingerprint: SshFingerprint::from_bytes(fp),
                title: r.try_get("title")?,
                created: r.try_get("created")?,
                expires: r.try_get("expires")?,
                revoked: r.try_get("revoked")?,
                last_used: r.try_get("last_used")?,
            });
        }
        Ok(out)
    }

    /// Revokes the key with this id or fingerprint (`SHA256:...`). `Ok(false)` when it was
    /// already revoked. Takes effect at once in this process and within [`CACHE_TTL_SECS`]
    /// elsewhere.
    ///
    /// # Errors
    /// [`TokenError::NotFound`] when no key matches.
    pub async fn revoke_ssh_key(&self, id_or_fingerprint: &str) -> Result<bool, TokenError> {
        let sel = id_or_fingerprint.trim();
        let done = if sel.starts_with("SHA256:") {
            let Some(fp) = SshFingerprint::parse(sel) else {
                return refuse("not a SHA256:<base64> fingerprint");
            };
            sqlx::query(
                "UPDATE ssh_keys SET revoked = $1 WHERE fingerprint = $2 AND revoked IS NULL",
            )
            .bind(self.now())
            .bind(fp.as_bytes().to_vec())
            .execute(&self.pool)
            .await?
        } else {
            sqlx::query("UPDATE ssh_keys SET revoked = $1 WHERE id = $2 AND revoked IS NULL")
                .bind(self.now())
                .bind(sel.to_owned())
                .execute(&self.pool)
                .await?
        };
        self.ssh_cache.clear();
        if done.rows_affected() > 0 {
            tracing::info!(selector = %sel, "ssh key revoked");
            return Ok(true);
        }
        let exists: i64 = if sel.starts_with("SHA256:") {
            let fp = SshFingerprint::parse(sel).map(|f| f.as_bytes().to_vec());
            sqlx::query_scalar("SELECT COUNT(*) FROM ssh_keys WHERE fingerprint = $1")
                .bind(fp.unwrap_or_default())
                .fetch_one(&self.pool)
                .await?
        } else {
            sqlx::query_scalar("SELECT COUNT(*) FROM ssh_keys WHERE id = $1")
                .bind(sel.to_owned())
                .fetch_one(&self.pool)
                .await?
        };
        if exists == 0 {
            Err(TokenError::NotFound)
        } else {
            Ok(false)
        }
    }

    /// How many grants a user's live personal tokens hold (what a user key can use).
    pub async fn live_grants_of(&self, user: &str) -> Result<i64, TokenError> {
        let now = self.now();
        Ok(sqlx::query_scalar(
            "SELECT COUNT(*) FROM auth_tokens t JOIN auth_token_grants g ON g.token_id = t.id \
             WHERE t.kind = 'pat' AND (t.principal = $1 OR t.principal = $2) \
             AND t.revoked IS NULL AND (t.expires IS NULL OR t.expires > $3)",
        )
        .bind(format!("pat:{user}"))
        .bind(user.to_owned())
        .bind(now)
        .fetch_one(&self.pool)
        .await?)
    }

    /// The live record of `key`, through the cache. `Ok(None)` for anything that is not a live,
    /// well-formed, allowed key. A database failure is an error and is not cached.
    pub(super) async fn ssh_lookup(
        &self,
        key: &SshKey,
    ) -> Result<Option<Arc<SshRecord>>, AuthError> {
        let now = self.now();
        let fp = key.fingerprint();
        if let Some(hit) = self.ssh_cache.get(fp.as_bytes())
            && now >= hit.at
            && now - hit.at < CACHE_TTL_SECS
        {
            return Ok(hit.rec.filter(|r| r.live(now)));
        }
        let rec = self.ssh_fetch(key, fp, now).await?;
        self.ssh_cache.insert(
            *fp.as_bytes(),
            SshCached {
                at: now,
                rec: rec.clone(),
            },
        );
        Ok(rec)
    }

    /// The database read behind [`Self::ssh_lookup`].
    async fn ssh_fetch(
        &self,
        key: &SshKey,
        fp: SshFingerprint,
        now: i64,
    ) -> Result<Option<Arc<SshRecord>>, AuthError> {
        let Some(row) = sqlx::query(
            "SELECT id, kind, principal, repo_id, CAST(\"write\" AS BIGINT) AS w, public_key, \
                    fingerprint, CAST(expires AS BIGINT) AS expires, \
                    CAST(revoked AS BIGINT) AS revoked, CAST(last_used AS BIGINT) AS last_used \
             FROM ssh_keys WHERE fingerprint = $1",
        )
        .bind(fp.as_bytes().to_vec())
        .fetch_optional(&self.pool)
        .await
        .map_err(unavailable)?
        else {
            return Ok(None);
        };
        let id: String = row.try_get("id").map_err(unavailable)?;
        let stored_blob: Vec<u8> = row.try_get("public_key").map_err(unavailable)?;
        let stored_fp: Vec<u8> = row.try_get("fingerprint").map_err(unavailable)?;
        let expires: Option<i64> = row.try_get("expires").map_err(unavailable)?;
        let revoked: Option<i64> = row.try_get("revoked").map_err(unavailable)?;
        if revoked.is_some() || expires.is_some_and(|e| now >= e) {
            return Ok(None);
        }
        // The row must hold exactly the key that was presented, and its fingerprint column must
        // be the digest of its blob: a hand-edited row is refused, not trusted.
        let consistent = stored_blob == key.blob()
            && stored_fp == fp.as_bytes()
            && SshFingerprint::of_blob(&stored_blob) == fp;
        if !consistent || check_blob(&stored_blob, DEFAULT_MIN_RSA_BITS).is_err() {
            tracing::warn!(key_id = %id, fingerprint = %fp, "refusing an ssh key row that fails the key policy");
            return Ok(None);
        }
        let kind: String = row.try_get("kind").map_err(unavailable)?;
        let stored: String = row.try_get("principal").map_err(unavailable)?;
        let repo: Option<String> = row.try_get("repo_id").map_err(unavailable)?;
        let write: i64 = row.try_get("w").map_err(unavailable)?;
        let Some((kind, principal, repo)) = rebuilt(&kind, &stored, repo.as_deref()) else {
            tracing::warn!(key_id = %id, "refusing an ssh key row that cannot form a typed principal");
            return Ok(None);
        };
        let grants = match (&kind, &repo) {
            (SshKeyKind::Deploy, Some(repo)) => vec![(
                Grant {
                    repo: repo.clone(),
                    access: if write != 0 {
                        Access::Write
                    } else {
                        Access::Read
                    },
                },
                None,
            )],
            _ => self.user_grants(&principal, now).await?,
        };
        let last_used: Option<i64> = row.try_get("last_used").map_err(unavailable)?;
        if last_used.is_none_or(|t| now.saturating_sub(t) >= LAST_USED_GRANULARITY_SECS) {
            // Best effort: a failed write must not refuse a login.
            let _ = sqlx::query("UPDATE ssh_keys SET last_used = $1 WHERE id = $2")
                .bind(now)
                .bind(id.clone())
                .execute(&self.pool)
                .await;
        }
        Ok(Some(Arc::new(SshRecord {
            key_id: id,
            kind,
            principal: Principal { id: principal },
            repo,
            expires,
            grants,
        })))
    }

    /// The grants of the live personal tokens of the user in `principal` (`pat:<user>`), each with
    /// its token's expiry.
    async fn user_grants(
        &self,
        principal: &str,
        now: i64,
    ) -> Result<Vec<(Grant, Option<i64>)>, AuthError> {
        let user = principal.strip_prefix("pat:").unwrap_or(principal);
        let rows = sqlx::query(
            "SELECT g.repo AS repo, g.access AS access, CAST(t.expires AS BIGINT) AS expires \
             FROM auth_tokens t JOIN auth_token_grants g ON g.token_id = t.id \
             WHERE t.kind = 'pat' AND (t.principal = $1 OR t.principal = $2) \
             AND t.revoked IS NULL AND (t.expires IS NULL OR t.expires > $3) LIMIT $4",
        )
        .bind(principal.to_owned())
        .bind(user.to_owned())
        .bind(now)
        .bind(MAX_USER_GRANTS)
        .fetch_all(&self.pool)
        .await
        .map_err(unavailable)?;
        let mut grants = Vec::with_capacity(rows.len());
        for r in rows {
            let repo: String = r.try_get("repo").map_err(unavailable)?;
            let access: String = r.try_get("access").map_err(unavailable)?;
            let expires: Option<i64> = r.try_get("expires").map_err(unavailable)?;
            if let Some(access) = access_parse(&access) {
                grants.push((Grant { repo, access }, expires));
            }
        }
        Ok(grants)
    }
}

// ------------------------------------------------------------------------------ the authorizer

impl TokenStoreAuthz {
    /// `identify_ssh_key` over the table.
    pub(super) async fn ssh_identify(
        &self,
        key: &SshKey,
    ) -> Result<Option<SshIdentity>, AuthError> {
        Ok(self
            .store()
            .ssh_lookup(key)
            .await?
            .map(|rec| rec.identity()))
    }

    /// `authorize_ssh_key` over the table.
    pub(super) async fn ssh_authorize(
        &self,
        key: &SshKey,
        repo: &str,
        access: Access,
    ) -> Result<Decision, AuthError> {
        let store = self.store();
        let Some(rec) = store.ssh_lookup(key).await? else {
            return Ok(Decision::Denied);
        };
        Ok(match rec.best(repo, store.now()) {
            Some(have) if have >= access => Decision::Allowed(rec.principal.clone()),
            _ => Decision::Denied,
        })
    }
}

// ------------------------------------------------------------------------------ CLI text

/// The `ssh-key add` summary for the operator.
pub fn add_text(info: &SshKeyInfo, user_grants: Option<i64>) -> String {
    let mut s = String::new();
    let _ = writeln!(s, "id:          {}", info.id);
    let _ = writeln!(s, "kind:        {}", info.kind.as_str());
    let _ = writeln!(s, "principal:   {}", info.principal);
    if let Some(repo) = &info.repo {
        let _ = writeln!(
            s,
            "repository:  {repo} ({})",
            if info.write {
                "read and write"
            } else {
                "read only"
            }
        );
    }
    let _ = writeln!(s, "type:        {}", info.algo);
    let _ = writeln!(s, "fingerprint: {}", info.fingerprint);
    let _ = writeln!(s, "expires:     {}", expiry_text(info.expires));
    if let Some(n) = user_grants {
        let _ = writeln!(
            s,
            "rights:      those of the user's live personal tokens ({n} grant{} now)",
            if n == 1 { "" } else { "s" }
        );
    }
    s
}

fn expiry_text(expires: Option<i64>) -> String {
    expires.map_or_else(|| "never".to_owned(), format_utc)
}

/// The `ssh-key list` table: one line per key, never a blob.
pub fn list_text(rows: &[SshKeyInfo], now: i64) -> String {
    let mut out = String::new();
    let _ = writeln!(
        out,
        "{:<16}  {:<6}  {:<8}  {:<24}  {:<5}  {:<20}  {:<50}  TITLE",
        "ID", "KIND", "STATUS", "OWNER", "WRITE", "EXPIRES", "FINGERPRINT"
    );
    for r in rows {
        let owner = r.repo.as_deref().unwrap_or(&r.principal);
        let write = match r.kind {
            SshKeyKind::Deploy if r.write => "yes",
            SshKeyKind::Deploy => "no",
            SshKeyKind::User => "-",
        };
        let _ = writeln!(
            out,
            "{:<16}  {:<6}  {:<8}  {:<24}  {:<5}  {:<20}  {:<50}  {}",
            r.id,
            r.kind.as_str(),
            r.status(now),
            owner,
            write,
            expiry_text(r.expires),
            r.fingerprint,
            r.title
        );
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const ED25519: &str = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAILM+rvN+ot98qgEN796jTiQfZfG1KaT0PtFDJ/XFSqti foo@bar.com";

    #[test]
    fn a_plain_line_parses_and_fingerprints_the_canonical_blob() {
        let k = parse_key_file(&format!("# a note\n\n{ED25519}\n"), 2048).unwrap();
        assert_eq!(k.algo, "ssh-ed25519");
        assert_eq!(k.comment, "foo@bar.com");
        assert_eq!(k.blob.len(), 51);
        assert!(k.fingerprint().to_string().starts_with("SHA256:"));
        // The comment and the spacing do not change the identity.
        let again =
            parse_key_file(&ED25519.replace("foo@bar.com", "other  comment"), 2048).unwrap();
        assert_eq!(again.fingerprint(), k.fingerprint());
    }

    #[test]
    fn option_prefixes_extra_keys_and_odd_types_are_refused() {
        for bad in [
            format!("command=\"ls\" {ED25519}"),
            format!("restrict {ED25519}"),
            format!("no-pty,from=\"1.2.3.4\" {ED25519}"),
            format!("{ED25519}\n{ED25519}"),
            String::new(),
            "# only a comment".to_owned(),
            "ssh-dss AAAAB3NzaC1kc3M= x".to_owned(),
            "sk-ssh-ed25519@openssh.com AAAA x".to_owned(),
            "ssh-ed25519-cert-v01@openssh.com AAAA x".to_owned(),
            "ssh-ed25519 !!!notbase64 x".to_owned(),
            "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAILM+rvN+ot98qgEN796jTiQfZfG1KaT0PtFDJ/XFSqti\u{7}"
                .to_owned(),
            ED25519.replace("ssh-ed25519 ", "ssh-rsa "),
        ] {
            assert!(parse_key_file(&bad, 2048).is_err(), "{bad:?}");
        }
        assert!(parse_key_file(&"a".repeat(MAX_KEY_FILE + 1), 2048).is_err());
    }

    #[test]
    fn rebuilt_principals_are_typed_or_refused() {
        let (kind, id, repo) = rebuilt("user", "alice", None).unwrap();
        assert_eq!(
            (kind, id.as_str(), repo),
            (SshKeyKind::User, "pat:alice", None)
        );
        assert_eq!(rebuilt("user", "pat:alice", None).unwrap().1, "pat:alice");
        let (_, id, repo) = rebuilt("deploy", "whatever", Some("acme/web")).unwrap();
        assert_eq!(
            (id.as_str(), repo.as_deref()),
            ("deploy:acme/web", Some("acme/web"))
        );
        for (k, p, r) in [
            ("user", "", None),
            ("user", "a b", None),
            ("user", "deploy:acme/web", None),
            ("user", "alice", Some("acme/web")),
            ("deploy", "x", None),
            ("deploy", "x", Some("acme/*")),
            ("deploy", "x", Some("*")),
            ("admin", "alice", None),
        ] {
            assert!(rebuilt(k, p, r).is_none(), "{k} {p:?} {r:?}");
        }
    }
}
