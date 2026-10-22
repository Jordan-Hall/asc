//! Schema migration (SPEC §8.2). Run by `asc serve migrate`, never at replica start.
//!
//! The server's migrations live in their own bookkeeping table so they never collide with the
//! `refs` migrations that `SqlRefs::migrate` applies from asc-store. Migrations are expand/contract:
//! a release only adds, so replicas N and N-1 share a database during a rollout.

use std::collections::HashSet;

use asc_store::sqlrefs::SqlRefs;
use sqlx::AnyPool;
use sqlx::migrate::Migrator;

/// The bookkeeping table of the server's own migrations.
pub const TABLE: &str = "_asc_serve_migrations";

/// Why a migration or schema check failed.
#[derive(Debug, thiserror::Error)]
pub enum MigrateError {
    #[error("refs migration: {0}")]
    Refs(#[from] asc_store::refs::RefError),
    #[error("migration: {0}")]
    Migrate(#[from] sqlx::migrate::MigrateError),
    #[error("database: {0}")]
    Sql(#[from] sqlx::Error),
    #[error("schema is behind this release (missing migration {0}); run `asc serve migrate`")]
    Behind(i64),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Backend {
    Sqlite,
    Postgres,
}

fn backend(pool: &AnyPool) -> Backend {
    match pool.connect_options().database_url.scheme() {
        "postgres" | "postgresql" => Backend::Postgres,
        _ => Backend::Sqlite,
    }
}

fn migrator(b: Backend) -> Migrator {
    let mut m = match b {
        Backend::Sqlite => sqlx::migrate!("migrations/sqlite"),
        Backend::Postgres => sqlx::migrate!("migrations/postgres"),
    };
    m.dangerous_set_table_name(TABLE);
    m
}

/// Opens a pool for `url`. The caller owns sizing; the server uses a small fixed pool.
pub async fn connect(url: &str, max_connections: u32) -> Result<AnyPool, sqlx::Error> {
    sqlx::any::install_default_drivers();
    sqlx::any::AnyPoolOptions::new()
        .max_connections(max_connections)
        .connect(url)
        .await
}

/// Applies the `refs` migrations, then the server's.
pub async fn migrate(pool: &AnyPool) -> Result<(), MigrateError> {
    SqlRefs::new(pool.clone()).migrate().await?;
    migrator(backend(pool)).run(pool).await?;
    Ok(())
}

/// Fails with [`MigrateError::Behind`] when a migration this release ships is not applied.
/// A schema that is ahead is fine: that is the N-1 replica case.
pub async fn ensure_current(pool: &AnyPool) -> Result<(), MigrateError> {
    let b = backend(pool);
    let applied: HashSet<i64> = match sqlx::query_scalar::<_, i64>(
        "SELECT version FROM _asc_serve_migrations WHERE success",
    )
    .fetch_all(pool)
    .await
    {
        Ok(rows) => rows.into_iter().collect(),
        // The table does not exist before the first `migrate`.
        Err(sqlx::Error::Database(_)) => HashSet::new(),
        Err(e) => return Err(e.into()),
    };
    match migrator(b).iter().find(|m| !applied.contains(&m.version)) {
        Some(m) => Err(MigrateError::Behind(m.version)),
        None => Ok(()),
    }
}

/// The marker a migration's first line carries to be allowed to drop or rename (a contract step).
pub const CONTRACT_MARKER: &str = "-- contract:";

/// Checks that `sql` only expands the schema: new tables, new indexes, new nullable or
/// defaulted columns. A file starting with [`CONTRACT_MARKER`] is exempt.
pub fn check_expand_only(sql: &str) -> Result<(), String> {
    if sql.trim_start().starts_with(CONTRACT_MARKER) {
        return Ok(());
    }
    let code: String = sql
        .lines()
        .map(|l| l.split("--").next().unwrap_or(""))
        .collect::<Vec<_>>()
        .join(" ");
    for stmt in code.split(';').map(|s| s.trim().to_ascii_uppercase()) {
        if stmt.is_empty() {
            continue;
        }
        let ok = if stmt.starts_with("CREATE TABLE")
            || stmt.starts_with("CREATE INDEX")
            || stmt.starts_with("CREATE UNIQUE INDEX")
        {
            true
        } else if stmt.starts_with("ALTER TABLE") && stmt.contains(" ADD COLUMN ") {
            !stmt.contains("NOT NULL") || stmt.contains("DEFAULT")
        } else {
            false
        };
        if !ok {
            return Err(format!("not expand-only: {stmt}"));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn pool() -> AnyPool {
        // One connection: every connection to `:memory:` is a separate database.
        connect("sqlite::memory:", 1).await.unwrap()
    }

    #[tokio::test]
    async fn migrate_is_idempotent_and_ensure_current_tracks_it() {
        let p = pool().await;
        assert!(matches!(
            ensure_current(&p).await,
            Err(MigrateError::Behind(_))
        ));
        migrate(&p).await.unwrap();
        migrate(&p).await.unwrap();
        ensure_current(&p).await.unwrap();
        for table in [
            "refs",
            "ref_log",
            "repos",
            "cs_graph",
            "git_map",
            "file_meta",
            "op_results",
        ] {
            sqlx::query(sqlx::AssertSqlSafe(format!(
                "SELECT * FROM {table} LIMIT 0"
            )))
            .execute(&p)
            .await
            .unwrap();
        }
    }

    #[test]
    fn every_shipped_migration_is_expand_only() {
        for b in [Backend::Sqlite, Backend::Postgres] {
            for m in migrator(b).iter() {
                check_expand_only(m.sql.as_str())
                    .unwrap_or_else(|e| panic!("{}: {e}", m.description));
            }
        }
    }

    #[test]
    fn expand_only_checker() {
        assert!(
            check_expand_only("CREATE TABLE t (a INT);\nALTER TABLE t ADD COLUMN b TEXT;").is_ok()
        );
        assert!(check_expand_only("ALTER TABLE t ADD COLUMN b TEXT NOT NULL DEFAULT 'x';").is_ok());
        assert!(check_expand_only("ALTER TABLE t ADD COLUMN b TEXT NOT NULL;").is_err());
        assert!(check_expand_only("DROP TABLE t;").is_err());
        assert!(check_expand_only("ALTER TABLE t RENAME COLUMN a TO b;").is_err());
        assert!(check_expand_only("-- contract: N stopped using t\nDROP TABLE t;").is_ok());
        assert!(check_expand_only("-- DROP TABLE t;\nCREATE TABLE u (a INT);").is_ok());
    }

    /// The statements release N-1 (this release's baseline, `0100_server`) runs must keep
    /// working on the schema of every later release: columns they name still exist with the
    /// same meaning, and columns they omit are nullable or defaulted.
    #[tokio::test]
    async fn n_minus_1_compat() {
        let p = pool().await;
        migrate(&p).await.unwrap();
        let n1 = [
            "INSERT INTO repos (name, ns, created) VALUES ('a/b', x'00112233445566778899aabbccddeeff', 1)",
            "INSERT INTO git_map (ns, git, kind, asc) VALUES (x'00', x'01', 1, x'02') ON CONFLICT DO NOTHING",
            "INSERT INTO cs_graph (ns, cs, gen, parents) VALUES (x'00', x'01', 1, x'')",
            "INSERT INTO file_meta (ns, file, size, git) VALUES (x'00', x'01', 5, x'02')",
            "INSERT INTO op_results (op_id, repo_id, result, at) VALUES (x'01', 1, x'', 1)",
            "INSERT INTO outbox (repo_id, op_id, effects, next_at) VALUES (1, x'01', x'7b7d', 1)",
            "INSERT INTO mirror_state (repo_id, last_import) VALUES (1, 1)",
            "INSERT INTO gc_marks (run, ns, key) VALUES (1, x'00', x'01')",
            "SELECT id, name, ns, mode, default_branch FROM repos WHERE name = 'a/b'",
            "SELECT asc FROM git_map WHERE ns = x'00' AND git = x'01' AND kind = 1",
            "SELECT size, git FROM file_meta WHERE ns = x'00' AND file = x'01'",
        ];
        for q in n1 {
            sqlx::query(q)
                .execute(&p)
                .await
                .unwrap_or_else(|e| panic!("{q}: {e}"));
        }
    }
}
