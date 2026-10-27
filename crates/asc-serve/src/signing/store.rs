//! Signing keys and signature rows (`signing_keys`, `change_signatures`; migration 0521).

use asc_core::id::ChangesetId;
use asc_core::sshsig::KeyKind;
use sqlx::{AnyConnection, AnyPool, Row};

use crate::audit::NewEvent;

/// Appends `event` in the caller's transaction, if there is one to append.
async fn audit_in_tx(
    conn: &mut AnyConnection,
    event: Option<&NewEvent>,
) -> Result<(), sqlx::Error> {
    if let Some(e) = event {
        crate::audit::store::append_in_tx(conn, e)
            .await
            .map_err(|e| sqlx::Error::Protocol(e.to_string()))?;
    }
    Ok(())
}

/// A signing key as stored.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeyRow {
    pub id: String,
    pub owner: String,
    pub title: String,
    pub kind: KeyKind,
    pub fingerprint: String,
    pub public_key: String,
    pub created: i64,
    /// 0 = never.
    pub expires_at: i64,
    /// 0 = live.
    pub revoked_at: i64,
    pub revoked_reason: String,
    pub last_used: i64,
}

impl KeyRow {
    /// Whether the key could verify a signature submitted at `at`: not expired, not revoked.
    pub fn live_at(&self, at: i64) -> bool {
        (self.expires_at == 0 || self.expires_at > at)
            && (self.revoked_at == 0 || self.revoked_at > at)
    }
}

/// A key to add.
#[derive(Clone, Debug)]
pub struct NewKey {
    pub id: String,
    pub owner: String,
    pub title: String,
    pub kind: KeyKind,
    pub fingerprint: String,
    pub public_key: String,
    pub created: i64,
    pub expires_at: i64,
}

/// Why a key was not added.
#[derive(Debug)]
pub enum AddError {
    /// The owner holds the most live keys they may.
    Limit,
    /// The fingerprint is registered already.
    Duplicate,
    Sql(sqlx::Error),
}

impl From<sqlx::Error> for AddError {
    fn from(e: sqlx::Error) -> Self {
        Self::Sql(e)
    }
}

pub fn kind_code(k: KeyKind) -> i64 {
    match k {
        KeyKind::Ed25519 => 1,
        KeyKind::EcdsaP256 => 2,
        KeyKind::Rsa => 3,
    }
}

fn kind_of(code: i64) -> Option<KeyKind> {
    Some(match code {
        1 => KeyKind::Ed25519,
        2 => KeyKind::EcdsaP256,
        3 => KeyKind::Rsa,
        _ => return None,
    })
}

const KEY_COLS: &str = "id, owner, title, kind, fingerprint, public_key, created, expires_at, \
                        revoked_at, revoked_reason, last_used";

fn key_of(r: &sqlx::any::AnyRow) -> Result<KeyRow, sqlx::Error> {
    let kind: i64 = r.try_get("kind")?;
    Ok(KeyRow {
        id: r.try_get("id")?,
        owner: r.try_get("owner")?,
        title: r.try_get("title")?,
        kind: kind_of(kind).ok_or_else(|| sqlx::Error::Decode("unknown key kind".into()))?,
        fingerprint: r.try_get("fingerprint")?,
        public_key: r.try_get("public_key")?,
        created: r.try_get("created")?,
        expires_at: r.try_get("expires_at")?,
        revoked_at: r.try_get("revoked_at")?,
        revoked_reason: r.try_get("revoked_reason")?,
        last_used: r.try_get("last_used")?,
    })
}

/// A stable advisory-lock key for one owner's key list (Postgres).
fn owner_lock_key(owner: &str) -> i64 {
    let mut h = blake3::Hasher::new();
    h.update(b"asc-serve signing keys");
    h.update(owner.as_bytes());
    let d = h.finalize();
    i64::from_le_bytes(std::array::from_fn(|i| d.as_bytes()[i]))
}

/// Adds `key` unless the owner already holds `max_live` live keys or the fingerprint exists. The
/// count and the insert are one statement under a per-owner lock, so concurrent adds cannot
/// overshoot the limit.
pub async fn add_key(
    pool: &AnyPool,
    key: &NewKey,
    max_live: usize,
    event: Option<&NewEvent>,
) -> Result<KeyRow, AddError> {
    let mut tx = crate::audit::store::begin(pool).await?;
    if tx.backend_name() == "PostgreSQL" {
        sqlx::query("SELECT pg_advisory_xact_lock($1)")
            .bind(owner_lock_key(&key.owner))
            .execute(&mut *tx)
            .await?;
    }
    let taken: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM signing_keys WHERE fingerprint = $1")
        .bind(&key.fingerprint)
        .fetch_one(&mut *tx)
        .await?;
    if taken > 0 {
        return Err(AddError::Duplicate);
    }
    let max = i64::try_from(max_live).unwrap_or(i64::MAX);
    let inserted = sqlx::query(
        "INSERT INTO signing_keys (id, owner, title, kind, fingerprint, public_key, created, \
         expires_at) SELECT $1, $2, $3, $4, $5, $6, $7, $8 WHERE (SELECT COUNT(*) FROM signing_keys \
         WHERE owner = $2 AND revoked_at = 0 AND (expires_at = 0 OR expires_at > $7)) < $9",
    )
    .bind(&key.id)
    .bind(&key.owner)
    .bind(&key.title)
    .bind(kind_code(key.kind))
    .bind(&key.fingerprint)
    .bind(&key.public_key)
    .bind(key.created)
    .bind(key.expires_at)
    .bind(max)
    .execute(&mut *tx)
    .await;
    match inserted {
        Ok(r) if r.rows_affected() == 0 => return Err(AddError::Limit),
        Ok(_) => {}
        Err(sqlx::Error::Database(d)) if d.is_unique_violation() => {
            return Err(AddError::Duplicate);
        }
        Err(e) => return Err(e.into()),
    }
    audit_in_tx(&mut tx, event).await?;
    tx.commit().await?;
    Ok(KeyRow {
        id: key.id.clone(),
        owner: key.owner.clone(),
        title: key.title.clone(),
        kind: key.kind,
        fingerprint: key.fingerprint.clone(),
        public_key: key.public_key.clone(),
        created: key.created,
        expires_at: key.expires_at,
        revoked_at: 0,
        revoked_reason: String::new(),
        last_used: 0,
    })
}

/// Every key of `owner`, revoked ones included, newest first.
pub async fn list_keys(pool: &AnyPool, owner: &str) -> Result<Vec<KeyRow>, sqlx::Error> {
    let q = format!(
        "SELECT {KEY_COLS} FROM signing_keys WHERE owner = $1 ORDER BY created DESC, id DESC"
    );
    sqlx::query(sqlx::AssertSqlSafe(q))
        .bind(owner)
        .fetch_all(pool)
        .await?
        .iter()
        .map(key_of)
        .collect()
}

pub async fn get_key(pool: &AnyPool, id: &str) -> Result<Option<KeyRow>, sqlx::Error> {
    let q = format!("SELECT {KEY_COLS} FROM signing_keys WHERE id = $1");
    sqlx::query(sqlx::AssertSqlSafe(q))
        .bind(id)
        .fetch_optional(pool)
        .await?
        .as_ref()
        .map(key_of)
        .transpose()
}

pub async fn by_fingerprint(pool: &AnyPool, fp: &str) -> Result<Option<KeyRow>, sqlx::Error> {
    let q = format!("SELECT {KEY_COLS} FROM signing_keys WHERE fingerprint = $1");
    sqlx::query(sqlx::AssertSqlSafe(q))
        .bind(fp)
        .fetch_optional(pool)
        .await?
        .as_ref()
        .map(key_of)
        .transpose()
}

/// Revokes key `id` at `at` unless it is revoked already; returns the key either way.
pub async fn revoke_key(
    pool: &AnyPool,
    id: &str,
    reason: &str,
    at: i64,
    event: Option<&NewEvent>,
) -> Result<Option<KeyRow>, sqlx::Error> {
    let mut tx = pool.begin().await?;
    let revoked = sqlx::query(
        "UPDATE signing_keys SET revoked_at = $2, revoked_reason = $3 WHERE id = $1 AND revoked_at = 0",
    )
    .bind(id)
    .bind(at)
    .bind(reason)
    .execute(&mut *tx)
    .await?;
    // Only the call that revoked it is audited; a repeat changes nothing.
    if revoked.rows_affected() > 0 {
        audit_in_tx(&mut tx, event).await?;
    }
    tx.commit().await?;
    get_key(pool, id).await
}

/// Records that key `id` verified a signature at `at`.
pub async fn touch_key(pool: &AnyPool, id: &str, at: i64) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE signing_keys SET last_used = $2 WHERE id = $1 AND last_used < $2")
        .bind(id)
        .bind(at)
        .execute(pool)
        .await?;
    Ok(())
}

/// A stored signature.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SigRow {
    pub signature: String,
    /// A signing key id, or [`super::SERVER_KEY_ID`].
    pub key_id: String,
    pub status: i64,
    pub signed_at: i64,
}

/// Stores `sig` as the signature of `changeset`, replacing an earlier one.
pub async fn put_signature(
    pool: &AnyPool,
    repo_id: i64,
    changeset: &ChangesetId,
    sig: &SigRow,
    event: Option<&NewEvent>,
) -> Result<(), sqlx::Error> {
    let mut tx = pool.begin().await?;
    sqlx::query(
        "INSERT INTO change_signatures (repo_id, changeset, signature, key_id, status, signed_at) \
         VALUES ($1, $2, $3, $4, $5, $6) ON CONFLICT (repo_id, changeset) DO UPDATE SET \
         signature = excluded.signature, key_id = excluded.key_id, status = excluded.status, \
         signed_at = excluded.signed_at",
    )
    .bind(repo_id)
    .bind(changeset.as_bytes().to_vec())
    .bind(&sig.signature)
    .bind(&sig.key_id)
    .bind(sig.status)
    .bind(sig.signed_at)
    .execute(&mut *tx)
    .await?;
    audit_in_tx(&mut tx, event).await?;
    tx.commit().await
}

pub async fn get_signature(
    pool: &AnyPool,
    repo_id: i64,
    changeset: &ChangesetId,
) -> Result<Option<SigRow>, sqlx::Error> {
    let row = sqlx::query(
        "SELECT signature, key_id, status, signed_at FROM change_signatures \
         WHERE repo_id = $1 AND changeset = $2",
    )
    .bind(repo_id)
    .bind(changeset.as_bytes().to_vec())
    .fetch_optional(pool)
    .await?;
    row.map(|r| {
        Ok(SigRow {
            signature: r.try_get("signature")?,
            key_id: r.try_get("key_id")?,
            status: r.try_get("status")?,
            signed_at: r.try_get("signed_at")?,
        })
    })
    .transpose()
}

/// The principal that first pushed `changeset` into the change index, if the index knows it.
pub async fn author_of(
    pool: &AnyPool,
    ns: &[u8],
    changeset: &ChangesetId,
) -> Result<Option<String>, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT actor FROM change_versions WHERE ns = $1 AND cs = $2 ORDER BY at ASC LIMIT 1",
    )
    .bind(ns.to_vec())
    .bind(changeset.as_bytes().to_vec())
    .fetch_optional(pool)
    .await
}
