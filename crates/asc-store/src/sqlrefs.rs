//! `SqlRefs`: ref storage in SQLite or Postgres through `sqlx::Any` (SPEC 3.4).

use std::time::{SystemTime, UNIX_EPOCH};

use asc_core::id::RefTarget;
use asc_core::path::RefName;
use async_trait::async_trait;
use sqlx::any::{Any, AnyRow};
use sqlx::{AnyConnection, AnyPool, Row, Transaction};
use uuid::Uuid;

use crate::refs::{
    Actor, Expect, RefConflict, RefEntry, RefError, RefLogEntry, RefTxn, Refs, RepoKey, Result,
};

impl From<sqlx::Error> for RefError {
    fn from(e: sqlx::Error) -> Self {
        Self::Backend(Box::new(e))
    }
}

#[derive(Clone, Copy, Debug)]
enum Backend {
    Sqlite,
    Postgres,
}

/// SQL-backed refs. `new` never migrates; the server calls [`SqlRefs::migrate`] explicitly.
#[derive(Clone, Debug)]
pub struct SqlRefs {
    pool: AnyPool,
    backend: Backend,
}

impl SqlRefs {
    /// `sqlx::any::install_default_drivers()` must have run before the pool was built.
    pub fn new(pool: AnyPool) -> Self {
        let backend = match pool.connect_options().database_url.scheme() {
            "postgres" | "postgresql" => Backend::Postgres,
            _ => Backend::Sqlite,
        };
        Self { pool, backend }
    }

    /// Creates or upgrades the `refs` and `ref_log` tables.
    pub async fn migrate(&self) -> Result<()> {
        let migrator = match self.backend {
            Backend::Sqlite => sqlx::migrate!("migrations/sqlite"),
            Backend::Postgres => sqlx::migrate!("migrations/postgres"),
        };
        migrator
            .run(&self.pool)
            .await
            .map_err(|e| RefError::Backend(Box::new(e)))
    }

    /// Starts a transaction suitable for [`SqlRefs::update_in_tx`]. SQLite takes its write lock up
    /// front so the read-then-write sequence cannot fail with a lock upgrade error.
    pub async fn begin(&self) -> Result<Transaction<'static, Any>> {
        Ok(match self.backend {
            Backend::Sqlite => self.pool.begin_with("BEGIN IMMEDIATE").await?,
            Backend::Postgres => self.pool.begin().await?,
        })
    }

    /// Applies `txn` inside `tx`. On `Conflict` the caller must roll back; callers such as
    /// `asc-serve` may append more statements before committing.
    pub async fn update_in_tx(
        &self,
        tx: &mut Transaction<'_, Any>,
        repo: RepoKey,
        txn: &RefTxn,
    ) -> Result<()> {
        let at = now();
        for u in &txn.updates {
            let name = name_text(&u.name)?;
            let cur = select(tx, repo, name).await?;
            let old = cur.as_ref().map(|e| e.target);
            let holds = match u.expect {
                Expect::Any => true,
                Expect::Absent => cur.is_none(),
                Expect::Value(t) => old == Some(t),
            };
            let conflict = |actual| {
                RefError::Conflict(RefConflict {
                    name: u.name.clone(),
                    actual,
                })
            };
            if !holds {
                return Err(conflict(old));
            }
            let target = u.new.as_ref().map(|t| t.to_bytes().to_vec());
            let rows = match (&cur, &target) {
                (None, None) => continue,
                (None, Some(t)) => sqlx::query(
                    "INSERT INTO refs (repo_id, name, target, version, expires_at) \
                         VALUES ($1, $2, $3, 1, $4) ON CONFLICT (repo_id, name) DO NOTHING",
                )
                .bind(repo.0)
                .bind(name)
                .bind(t.clone())
                .bind(u.expires_at)
                .execute(&mut **tx)
                .await?
                .rows_affected(),
                (Some(c), Some(t)) => sqlx::query(
                    "UPDATE refs SET target = $1, version = version + 1, expires_at = $2 \
                         WHERE repo_id = $3 AND name = $4 AND version = $5",
                )
                .bind(t.clone())
                .bind(u.expires_at)
                .bind(repo.0)
                .bind(name)
                .bind(c.version as i64)
                .execute(&mut **tx)
                .await?
                .rows_affected(),
                (Some(c), None) => sqlx::query(
                    "DELETE FROM refs WHERE repo_id = $1 AND name = $2 AND version = $3",
                )
                .bind(repo.0)
                .bind(name)
                .bind(c.version as i64)
                .execute(&mut **tx)
                .await?
                .rows_affected(),
            };
            if rows == 0 {
                let actual = select(tx, repo, name).await?.map(|e| e.target);
                return Err(conflict(actual));
            }
            sqlx::query(
                "INSERT INTO ref_log (repo_id, name, old, new, actor, via, reason, op_id, at) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
            )
            .bind(repo.0)
            .bind(name)
            .bind(old.map(|t| t.to_bytes().to_vec()))
            .bind(target)
            .bind(&txn.actor.id)
            .bind(&txn.actor.via)
            .bind(&txn.reason)
            .bind(txn.op_id.as_bytes().to_vec())
            .bind(at)
            .execute(&mut **tx)
            .await?;
        }
        Ok(())
    }
}

#[async_trait]
impl Refs for SqlRefs {
    async fn get(&self, repo: RepoKey, name: &RefName) -> Result<Option<RefEntry>> {
        let mut conn = self.pool.acquire().await?;
        select(&mut conn, repo, name_text(name)?).await
    }

    async fn list(&self, repo: RepoKey, prefix: &str) -> Result<Vec<RefEntry>> {
        let rows = sqlx::query(
            "SELECT name, target, version, expires_at FROM refs \
             WHERE repo_id = $1 AND name LIKE $2 ESCAPE '\\' ORDER BY name",
        )
        .bind(repo.0)
        .bind(format!("{}%", escape_like(prefix)))
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(entry).collect()
    }

    async fn update(&self, repo: RepoKey, txn: &RefTxn) -> Result<()> {
        let mut tx = self.begin().await?;
        self.update_in_tx(&mut tx, repo, txn).await?; // dropping `tx` rolls back
        tx.commit().await?;
        Ok(())
    }

    async fn log(&self, repo: RepoKey, name: &RefName, limit: u32) -> Result<Vec<RefLogEntry>> {
        let rows = sqlx::query(
            "SELECT seq, name, old, new, actor, via, reason, op_id, at FROM ref_log \
             WHERE repo_id = $1 AND name = $2 ORDER BY seq DESC LIMIT $3",
        )
        .bind(repo.0)
        .bind(name_text(name)?)
        .bind(i64::from(limit))
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(log_entry).collect()
    }
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

fn name_text(name: &RefName) -> Result<&str> {
    std::str::from_utf8(name.as_bytes()).map_err(|_| RefError::NonUtf8Name(name.clone()))
}

/// Escapes `\`, `%` and `_` for a `LIKE ... ESCAPE '\'` pattern.
fn escape_like(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if matches!(c, '\\' | '%' | '_') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

async fn select(conn: &mut AnyConnection, repo: RepoKey, name: &str) -> Result<Option<RefEntry>> {
    let row = sqlx::query(
        "SELECT name, target, version, expires_at FROM refs WHERE repo_id = $1 AND name = $2",
    )
    .bind(repo.0)
    .bind(name)
    .fetch_optional(conn)
    .await?;
    row.as_ref().map(entry).transpose()
}

fn target(bytes: &[u8]) -> Result<RefTarget> {
    RefTarget::from_bytes(bytes).map_err(|e| RefError::Corrupt(e.to_string()))
}

fn ref_name(s: String) -> Result<RefName> {
    RefName::new_onelevel(s).map_err(|e| RefError::Corrupt(e.to_string()))
}

fn entry(row: &AnyRow) -> Result<RefEntry> {
    Ok(RefEntry {
        name: ref_name(row.try_get("name")?)?,
        target: target(&row.try_get::<Vec<u8>, _>("target")?)?,
        version: row.try_get::<i64, _>("version")? as u64,
        expires_at: row.try_get("expires_at")?,
    })
}

fn log_entry(row: &AnyRow) -> Result<RefLogEntry> {
    let opt = |col: &str| -> Result<Option<RefTarget>> {
        row.try_get::<Option<Vec<u8>>, _>(col)?
            .map(|b| target(&b))
            .transpose()
    };
    let op_id = Uuid::from_slice(&row.try_get::<Vec<u8>, _>("op_id")?)
        .map_err(|e| RefError::Corrupt(e.to_string()))?;
    Ok(RefLogEntry {
        seq: row.try_get("seq")?,
        name: ref_name(row.try_get("name")?)?,
        old: opt("old")?,
        new: opt("new")?,
        actor: Actor {
            id: row.try_get("actor")?,
            via: row.try_get("via")?,
        },
        reason: row.try_get("reason")?,
        op_id,
        at: row.try_get("at")?,
    })
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use sqlx::any::AnyPoolOptions;

    use super::*;
    use crate::conformance_refs::run_refs_suite;

    async fn sqlite(dir: &tempfile::TempDir) -> SqlRefs {
        sqlx::any::install_default_drivers();
        let path = dir
            .path()
            .join("refs.db")
            .to_string_lossy()
            .replace('\\', "/");
        let pool = AnyPoolOptions::new()
            .max_connections(8)
            .connect(&format!("sqlite:{path}?mode=rwc"))
            .await
            .unwrap();
        let refs = SqlRefs::new(pool);
        refs.migrate().await.unwrap();
        refs
    }

    #[tokio::test]
    async fn sqlite_passes_refs_suite() {
        let dir = tempfile::tempdir().unwrap();
        run_refs_suite(Arc::new(sqlite(&dir).await)).await;
    }

    #[test]
    fn like_escaping() {
        assert_eq!(escape_like(r"refs/a_b%c\d"), r"refs/a\_b\%c\\d");
    }
}
