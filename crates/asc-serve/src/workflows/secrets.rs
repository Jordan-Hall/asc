//! The secrets command steps read (`secrets = { TOKEN = "deploy_token" }`).
//!
//! A value is sealed by the vault when it arrives (XChaCha20-Poly1305 under the master key,
//! bound to the repository and the name, so a copied row does not open elsewhere) and is opened
//! only to hand to the process of a command step that names it. It is never returned by the
//! service, never in a log, an error or an audit event, and the output of the step is scrubbed of
//! it before it is stored. A list shows the name and, for a value long enough that four
//! characters reveal nothing, the last four.

use async_trait::async_trait;
use sqlx::{AnyPool, Row};
use zeroize::Zeroizing;

use crate::ai::clock;
use crate::ai::store::class;
use crate::vault::{Resealer, SecretContext, Vault, VaultError};

/// The `kind` of the vault contexts of these secrets.
pub const SECRET_KIND: &str = "workflow-secret";
/// Longest value, in bytes.
pub const MAX_VALUE: usize = 16 * 1024;
/// Most secrets one repository holds.
pub const MAX_PER_REPO: i64 = 64;
/// A hint is shown only for values at least this long.
const HINT_FROM: usize = 16;

/// A listed secret: no value.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SecretInfo {
    pub name: String,
    pub hint: String,
    pub created: i64,
    pub updated: i64,
    pub updated_by: String,
}

/// Why a secret call failed.
#[derive(Debug)]
pub enum SecretError {
    Invalid(String),
    TooMany,
    Vault(VaultError),
    Sql(String),
}

impl From<sqlx::Error> for SecretError {
    fn from(e: sqlx::Error) -> Self {
        Self::Sql(class(&e))
    }
}

impl From<VaultError> for SecretError {
    fn from(e: VaultError) -> Self {
        Self::Vault(e)
    }
}

/// Whether `name` is a secret name: 1 to 64 bytes of `[A-Za-z0-9_.-]`.
pub fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-'))
}

fn hint(value: &str) -> String {
    if value.chars().count() < HINT_FROM {
        return String::new();
    }
    let n = value.chars().count();
    value.chars().skip(n - 4).collect()
}

/// The secrets of repositories.
#[derive(Clone)]
pub struct Secrets {
    pool: AnyPool,
    vault: std::sync::Arc<Vault>,
}

fn ctx<'a>(repo: &'a str, name: &'a str) -> SecretContext<'a> {
    SecretContext {
        kind: SECRET_KIND,
        owner: repo,
        id: name,
    }
}

impl Secrets {
    pub fn new(pool: AnyPool, vault: std::sync::Arc<Vault>) -> Self {
        Self { pool, vault }
    }

    /// Registers the re-sealer that keeps these rows readable across a master key rotation.
    pub fn register(&self) {
        self.vault.register(std::sync::Arc::new(SecretResealer {
            pool: self.pool.clone(),
        }));
    }

    /// Stores `value` under `name`, replacing the old one.
    pub async fn set(
        &self,
        repo: &str,
        name: &str,
        value: &Zeroizing<String>,
        by: &str,
    ) -> Result<SecretInfo, SecretError> {
        let repo = repo.to_ascii_lowercase();
        if !valid_name(name) {
            return Err(SecretError::Invalid(
                "a secret name is 1 to 64 bytes of A-Z, a-z, 0-9, _, . and -".into(),
            ));
        }
        if value.is_empty() || value.len() > MAX_VALUE || value.contains('\0') {
            return Err(SecretError::Invalid(format!(
                "a secret value is 1 to {MAX_VALUE} bytes of text without NUL"
            )));
        }
        let exists: Option<i64> = sqlx::query_scalar(
            "SELECT created FROM workflow_secrets WHERE repo = $1 AND name = $2",
        )
        .bind(repo.clone())
        .bind(name.to_owned())
        .fetch_optional(&self.pool)
        .await?;
        if exists.is_none() {
            let n: i64 = sqlx::query_scalar(
                "SELECT CAST(COUNT(*) AS BIGINT) FROM workflow_secrets WHERE repo = $1",
            )
            .bind(repo.clone())
            .fetch_one(&self.pool)
            .await?;
            if n >= MAX_PER_REPO {
                return Err(SecretError::TooMany);
            }
        }
        let (generation, sealed) = self.vault.seal(&ctx(&repo, name), value.as_bytes())?;
        let now = clock::now();
        let h = hint(value);
        if exists.is_some() {
            sqlx::query(
                "UPDATE workflow_secrets SET sealed = $3, key_gen = $4, hint = $5, updated = $6, updated_by = $7 \
                 WHERE repo = $1 AND name = $2",
            )
            .bind(repo.clone())
            .bind(name.to_owned())
            .bind(sealed)
            .bind(i64::from(generation))
            .bind(h.clone())
            .bind(now)
            .bind(by.to_owned())
            .execute(&self.pool)
            .await?;
        } else {
            sqlx::query(
                "INSERT INTO workflow_secrets (repo, name, sealed, key_gen, hint, created, updated, updated_by) \
                 VALUES ($1, $2, $3, $4, $5, $6, $6, $7) ON CONFLICT (repo, name) DO NOTHING",
            )
            .bind(repo.clone())
            .bind(name.to_owned())
            .bind(sealed)
            .bind(i64::from(generation))
            .bind(h.clone())
            .bind(now)
            .bind(by.to_owned())
            .execute(&self.pool)
            .await?;
        }
        self.info(&repo, name)
            .await?
            .ok_or_else(|| SecretError::Sql("missing".into()))
    }

    async fn info(&self, repo: &str, name: &str) -> Result<Option<SecretInfo>, sqlx::Error> {
        let row = sqlx::query(
            "SELECT name, hint, created, updated, updated_by FROM workflow_secrets WHERE repo = $1 AND name = $2",
        )
        .bind(repo.to_owned())
        .bind(name.to_owned())
        .fetch_optional(&self.pool)
        .await?;
        row.map(|r| info_of(&r)).transpose()
    }

    pub async fn list(&self, repo: &str) -> Result<Vec<SecretInfo>, SecretError> {
        let rows = sqlx::query(
            "SELECT name, hint, created, updated, updated_by FROM workflow_secrets WHERE repo = $1 ORDER BY name",
        )
        .bind(repo.to_ascii_lowercase())
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.iter().map(info_of).collect::<Result<_, _>>()?)
    }

    /// Whether the secret exists.
    pub async fn exists(&self, repo: &str, name: &str) -> Result<bool, SecretError> {
        Ok(self.info(&repo.to_ascii_lowercase(), name).await?.is_some())
    }

    pub async fn delete(&self, repo: &str, name: &str) -> Result<bool, SecretError> {
        let r = sqlx::query("DELETE FROM workflow_secrets WHERE repo = $1 AND name = $2")
            .bind(repo.to_ascii_lowercase())
            .bind(name.to_owned())
            .execute(&self.pool)
            .await?;
        Ok(r.rows_affected() == 1)
    }

    /// The plaintext, for the one process that needs it. `None`: no such secret.
    pub async fn open(
        &self,
        repo: &str,
        name: &str,
    ) -> Result<Option<Zeroizing<String>>, SecretError> {
        let repo = repo.to_ascii_lowercase();
        let sealed: Option<Vec<u8>> =
            sqlx::query_scalar("SELECT sealed FROM workflow_secrets WHERE repo = $1 AND name = $2")
                .bind(repo.clone())
                .bind(name.to_owned())
                .fetch_optional(&self.pool)
                .await?;
        let Some(sealed) = sealed else {
            return Ok(None);
        };
        let plain = self.vault.open(&ctx(&repo, name), &sealed)?;
        String::from_utf8(plain.to_vec())
            .map(|s| Some(Zeroizing::new(s)))
            .map_err(|_| SecretError::Vault(VaultError::Corrupt))
    }
}

fn info_of(r: &sqlx::any::AnyRow) -> Result<SecretInfo, sqlx::Error> {
    Ok(SecretInfo {
        name: r.try_get("name")?,
        hint: r.try_get("hint")?,
        created: r.try_get("created")?,
        updated: r.try_get("updated")?,
        updated_by: r.try_get("updated_by")?,
    })
}

/// Re-seals the secrets when the vault rotates its master key.
struct SecretResealer {
    pool: AnyPool,
}

#[async_trait]
impl Resealer for SecretResealer {
    fn kind(&self) -> &'static str {
        SECRET_KIND
    }

    async fn counts(&self) -> Result<Vec<(u32, u64)>, VaultError> {
        let rows = sqlx::query("SELECT key_gen, CAST(COUNT(*) AS BIGINT) AS n FROM workflow_secrets GROUP BY key_gen ORDER BY key_gen")
            .fetch_all(&self.pool)
            .await
            .map_err(|e| VaultError::Sql(class(&e)))?;
        rows.iter()
            .map(|r| {
                let g: i64 = r
                    .try_get("key_gen")
                    .map_err(|e| VaultError::Sql(class(&e)))?;
                let n: i64 = r.try_get("n").map_err(|e| VaultError::Sql(class(&e)))?;
                Ok((u32::try_from(g).unwrap_or(0), u64::try_from(n).unwrap_or(0)))
            })
            .collect()
    }

    async fn reseal_batch(
        &self,
        vault: &Vault,
        target: u32,
        limit: usize,
    ) -> Result<usize, VaultError> {
        let rows = sqlx::query("SELECT repo, name, sealed FROM workflow_secrets WHERE key_gen <> $1 ORDER BY repo, name LIMIT $2")
            .bind(i64::from(target))
            .bind(i64::try_from(limit).unwrap_or(i64::MAX))
            .fetch_all(&self.pool)
            .await
            .map_err(|e| VaultError::Sql(class(&e)))?;
        let n = rows.len();
        for r in &rows {
            let sql = |e: sqlx::Error| VaultError::Sql(class(&e));
            let repo: String = r.try_get("repo").map_err(sql)?;
            let name: String = r.try_get("name").map_err(sql)?;
            let sealed: Vec<u8> = r.try_get("sealed").map_err(sql)?;
            let (generation, blob) = vault
                .reseal(&ctx(&repo, &name), &sealed)?
                .unwrap_or((target, sealed.clone()));
            sqlx::query("UPDATE workflow_secrets SET sealed = $4, key_gen = $5 WHERE repo = $1 AND name = $2 AND sealed = $3")
                .bind(repo)
                .bind(name)
                .bind(sealed)
                .bind(blob)
                .bind(i64::from(generation))
                .execute(&self.pool)
                .await
                .map_err(sql)?;
        }
        Ok(n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_and_hints_follow_the_rules() {
        assert!(valid_name("deploy_token") && valid_name("A.b-c_9"));
        for bad in ["", "a b", "a/b", "é", &"n".repeat(65), "a\n"] {
            assert!(!valid_name(bad), "{bad:?}");
        }
        assert_eq!(hint("short"), "", "a short value shows nothing");
        assert_eq!(hint("0123456789abcdefWXYZ"), "WXYZ");
    }
}
