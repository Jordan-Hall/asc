//! `state.db`: the machine-local SQLite database (SPEC §9.2).
//!
//! One pool serves the typed accessors below and [`SqlRefs`] for local refs. Single statements
//! autocommit; multi-statement writes use [`StateDb::begin`], which takes SQLite's write lock up
//! front so concurrent CLI processes wait on `busy_timeout` instead of failing a lock upgrade.

use std::time::{SystemTime, UNIX_EPOCH};

use asc_store::sqlrefs::SqlRefs;
use sqlx::any::{Any, AnyPoolOptions};
use sqlx::{AnyPool, Row, Transaction};

use crate::home::{AscHome, HomeError};

pub mod repos;
pub mod stage;
pub mod stage_sets;
pub mod tables;
pub mod workspaces;

pub use tables::{
    cs_gen, entry_times, git_map, history_boundary, inodes, journal, switch_intent, ws_ops,
};

/// The schema version this binary reads and writes.
pub const SCHEMA_VERSION: i64 = 1;

/// A write transaction opened by [`StateDb::begin`].
pub type Tx = Transaction<'static, Any>;

/// Failures from `state.db`.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("state.db: {0}")]
    Sql(#[from] sqlx::Error),
    #[error("state.db migration: {0}")]
    Migrate(#[from] sqlx::migrate::MigrateError),
    #[error(transparent)]
    Home(#[from] HomeError),
    /// An older binary met a newer schema (SPEC §9.2).
    #[error("state.db is from asc {0}; run asc upgrade")]
    NewerSchema(String),
    #[error("state.db holds a malformed {0}")]
    Corrupt(&'static str),
    #[error("{0} not found")]
    NotFound(&'static str),
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

/// The `state.db` pool.
#[derive(Clone, Debug)]
pub struct StateDb {
    pool: AnyPool,
}

impl StateDb {
    /// Opens `<home>/state.db` in WAL mode with `busy_timeout = 5000`, applying the migrations.
    pub async fn open(home: &AscHome) -> Result<Self> {
        sqlx::any::install_default_drivers();
        let path = home.state_db().to_string_lossy().replace('\\', "/");
        let path = path
            .replace('%', "%25")
            .replace('?', "%3F")
            .replace('#', "%23")
            .replace(' ', "%20");
        let pool = AnyPoolOptions::new()
            .max_connections(8)
            .after_connect(|conn, _| {
                Box::pin(async move {
                    for pragma in [
                        "PRAGMA busy_timeout = 5000",
                        "PRAGMA journal_mode = WAL",
                        "PRAGMA synchronous = NORMAL",
                    ] {
                        sqlx::query(pragma).execute(&mut *conn).await?;
                    }
                    Ok(())
                })
            })
            .connect(&format!("sqlite:{path}?mode=rwc"))
            .await?;
        let db = Self { pool };
        db.migrate().await?;
        Ok(db)
    }

    async fn migrate(&self) -> Result<()> {
        self.check_schema_not_newer().await?;
        let mut attempt = 0;
        loop {
            // Two processes opening a fresh home race on the first migration; the loser retries
            // and finds it applied.
            match sqlx::migrate!().run(&self.pool).await {
                Ok(()) => break,
                Err(_) if attempt < 3 => {
                    attempt += 1;
                    tokio::time::sleep(std::time::Duration::from_millis(50 * attempt)).await;
                }
                Err(e) => return Err(e.into()),
            }
        }
        sqlx::query("UPDATE meta SET asc_version = ? WHERE asc_version = ''")
            .bind(env!("CARGO_PKG_VERSION"))
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    async fn check_schema_not_newer(&self) -> Result<()> {
        let has_meta: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM sqlite_master WHERE type = 'table' AND name = 'meta'",
        )
        .fetch_one(&self.pool)
        .await?;
        if has_meta == 0 {
            return Ok(());
        }
        let Some(row) = sqlx::query("SELECT schema_version, asc_version FROM meta LIMIT 1")
            .fetch_optional(&self.pool)
            .await?
        else {
            return Ok(());
        };
        let found: i64 = row.try_get("schema_version")?;
        if found > SCHEMA_VERSION {
            let v: String = row.try_get("asc_version")?;
            let v = if v.is_empty() {
                format!("with schema {found}")
            } else {
                v
            };
            return Err(Error::NewerSchema(v));
        }
        Ok(())
    }

    /// The shared pool.
    pub fn pool(&self) -> &AnyPool {
        &self.pool
    }

    /// Local refs over the same pool; `SqlRefs::new` never migrates (SPEC §9.2).
    pub fn refs(&self) -> SqlRefs {
        SqlRefs::new(self.pool.clone())
    }

    /// Starts a write transaction (`BEGIN IMMEDIATE`).
    pub async fn begin(&self) -> Result<Tx> {
        Ok(self.pool.begin_with("BEGIN IMMEDIATE").await?)
    }

    /// `meta.schema_version`.
    pub async fn schema_version(&self) -> Result<i64> {
        Ok(
            sqlx::query_scalar("SELECT schema_version FROM meta LIMIT 1")
                .fetch_one(&self.pool)
                .await?,
        )
    }

    /// `meta.machine_id`: 16 random bytes made when the database was created (SPEC §3.4).
    pub async fn machine_id(&self) -> Result<[u8; 16]> {
        let b: Vec<u8> = sqlx::query_scalar("SELECT machine_id FROM meta LIMIT 1")
            .fetch_one(&self.pool)
            .await?;
        b.try_into().map_err(|_| Error::Corrupt("machine_id"))
    }
}

/// Unix seconds.
pub(crate) fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

/// A fixed-size array from a stored blob.
pub(crate) fn fixed<const N: usize>(b: Vec<u8>, what: &'static str) -> Result<[u8; N]> {
    b.try_into().map_err(|_| Error::Corrupt(what))
}

/// Length-prefixed list of byte strings (`switch_intent.pending`, `ws_ops.pending_renames`).
pub(crate) fn pack<'a>(items: impl IntoIterator<Item = &'a [u8]>) -> Vec<u8> {
    let mut out = Vec::new();
    for it in items {
        out.extend_from_slice(&(it.len() as u32).to_le_bytes());
        out.extend_from_slice(it);
    }
    out
}

/// Inverse of [`pack`].
pub(crate) fn unpack(mut b: &[u8], what: &'static str) -> Result<Vec<Vec<u8>>> {
    let mut out = Vec::new();
    while !b.is_empty() {
        let (len, rest) = b.split_first_chunk::<4>().ok_or(Error::Corrupt(what))?;
        let len = u32::from_le_bytes(*len) as usize;
        if rest.len() < len {
            return Err(Error::Corrupt(what));
        }
        let (item, rest) = rest.split_at(len);
        out.push(item.to_vec());
        b = rest;
    }
    Ok(out)
}

#[cfg(test)]
mod tests;
