//! The SQL of storage targets, repository routes and migrations.
//!
//! The credentials column (`sealed_credentials`) is read by [`Db::sealed`] only; no other query
//! selects it, so a response can never carry it by accident.

use sqlx::any::AnyRow;
use sqlx::{AnyPool, Row};

use super::model::{Addressing, Owner, OwnerKind, State, Target};

pub type Result<T> = std::result::Result<T, sqlx::Error>;

const TARGET_COLUMNS: &str = "id, owner_kind, owner_name, name, endpoint, region, bucket, prefix, addressing, \
     access_key_hint, state, was_enabled, last_error, verified_at, key_gen, version, created, updated";

/// How many verifications a target may run per window.
pub const VERIFY_LIMIT: i64 = 6;
/// The verification rate window, in seconds.
pub const VERIFY_WINDOW: i64 = 3600;

fn corrupt(what: &str) -> sqlx::Error {
    sqlx::Error::Decode(format!("stored {what} is invalid").into())
}

pub(crate) fn target_of(r: &AnyRow) -> Result<Target> {
    let kind: String = r.try_get("owner_kind")?;
    let name: String = r.try_get("owner_name")?;
    let addressing: String = r.try_get("addressing")?;
    let state: String = r.try_get("state")?;
    let was_enabled: i64 = r.try_get("was_enabled")?;
    let key_gen: i64 = r.try_get("key_gen")?;
    Ok(Target {
        id: r.try_get("id")?,
        owner: Owner {
            kind: OwnerKind::parse(&kind).ok_or_else(|| corrupt("owner kind"))?,
            name,
        },
        name: r.try_get("name")?,
        endpoint: r.try_get("endpoint")?,
        region: r.try_get("region")?,
        bucket: r.try_get("bucket")?,
        prefix: r.try_get("prefix")?,
        addressing: Addressing::parse(&addressing).ok_or_else(|| corrupt("addressing"))?,
        access_key_hint: r.try_get("access_key_hint")?,
        state: State::parse(&state).ok_or_else(|| corrupt("state"))?,
        was_enabled: was_enabled != 0,
        last_error: r.try_get("last_error")?,
        verified_at: r.try_get("verified_at")?,
        key_gen: u32::try_from(key_gen).map_err(|_| corrupt("key generation"))?,
        version: r.try_get("version")?,
        created: r.try_get("created")?,
        updated: r.try_get("updated")?,
    })
}

/// A migration to queue.
#[derive(Clone, Copy)]
pub struct NewMigration<'a> {
    pub id: &'a str,
    pub repo_id: i64,
    pub repo_name: &'a str,
    pub from: &'a str,
    pub to: &'a str,
    pub delete_source: bool,
}

/// Fields of a target to insert.
pub struct NewTarget<'a> {
    pub id: &'a str,
    pub owner: &'a Owner,
    pub name: &'a str,
    pub endpoint: &'a str,
    pub region: &'a str,
    pub bucket: &'a str,
    pub prefix: &'a str,
    pub addressing: Addressing,
    pub hint: &'a str,
    pub sealed: &'a [u8],
    pub key_gen: u32,
    pub now: i64,
}

/// A repository's route as stored.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RouteRow {
    pub repo_id: i64,
    /// `""`: the instance default.
    pub target_id: String,
    /// `None`: no other place; `Some("")`: the instance default.
    pub fallback: Option<String>,
    pub mirror_to: Option<String>,
    pub version: i64,
}

/// A migration row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MigrationRow {
    pub id: String,
    pub repo_id: i64,
    pub repo_name: String,
    pub from_target: String,
    pub to_target: String,
    pub state: String,
    pub delete_source: bool,
    pub objects_total: i64,
    pub objects_copied: i64,
    pub bytes_copied: i64,
    pub cursor: String,
    pub cancel: bool,
    pub switched: bool,
    pub mirror_since: i64,
    pub error: String,
    /// Milliseconds, so rows made in the same second still have an order.
    pub created_ms: i64,
    pub updated: i64,
}

const MIGRATION_COLUMNS: &str = "id, repo_id, repo_name, from_target, to_target, state, delete_source, objects_total, \
     objects_copied, bytes_copied, cursor, cancel, switched, mirror_since, error, created_ms, updated";

fn migration_of(r: &AnyRow) -> Result<MigrationRow> {
    let flag = |c: &str| -> Result<bool> { Ok(r.try_get::<i64, _>(c)? != 0) };
    Ok(MigrationRow {
        id: r.try_get("id")?,
        repo_id: r.try_get("repo_id")?,
        repo_name: r.try_get("repo_name")?,
        from_target: r.try_get("from_target")?,
        to_target: r.try_get("to_target")?,
        state: r.try_get("state")?,
        delete_source: flag("delete_source")?,
        objects_total: r.try_get("objects_total")?,
        objects_copied: r.try_get("objects_copied")?,
        bytes_copied: r.try_get("bytes_copied")?,
        cursor: r.try_get("cursor")?,
        cancel: flag("cancel")?,
        switched: flag("switched")?,
        mirror_since: r.try_get("mirror_since")?,
        error: r.try_get("error")?,
        created_ms: r.try_get("created_ms")?,
        updated: r.try_get("updated")?,
    })
}

/// The storage tables.
#[derive(Clone)]
pub struct Db {
    pub pool: AnyPool,
}

impl Db {
    pub fn new(pool: AnyPool) -> Self {
        Self { pool }
    }

    // ------------------------------------------------------------------ targets

    /// Inserts a target in state `pending`. `Ok(false)` when the owner already has that name.
    pub async fn insert_target(&self, t: &NewTarget<'_>) -> Result<bool> {
        let r = sqlx::query(
            "INSERT INTO storage_targets (id, owner_kind, owner_name, name, endpoint, region, bucket, prefix, \
             addressing, access_key_hint, sealed_credentials, key_gen, state, created, updated) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, 'pending', $13, $13)",
        )
        .bind(t.id)
        .bind(t.owner.kind.as_str())
        .bind(&t.owner.name)
        .bind(t.name)
        .bind(t.endpoint)
        .bind(t.region)
        .bind(t.bucket)
        .bind(t.prefix)
        .bind(t.addressing.as_str())
        .bind(t.hint)
        .bind(t.sealed.to_vec())
        .bind(i64::from(t.key_gen))
        .bind(t.now)
        .execute(&self.pool)
        .await;
        match r {
            Ok(_) => Ok(true),
            Err(sqlx::Error::Database(d)) if d.is_unique_violation() => Ok(false),
            Err(e) => Err(e),
        }
    }

    pub async fn count_targets(&self, owner: &Owner) -> Result<i64> {
        sqlx::query_scalar("SELECT COUNT(*) FROM storage_targets WHERE owner_kind = $1 AND owner_name = $2")
            .bind(owner.kind.as_str())
            .bind(&owner.name)
            .fetch_one(&self.pool)
            .await
    }

    pub async fn target(&self, id: &str) -> Result<Option<Target>> {
        let sql = format!("SELECT {TARGET_COLUMNS} FROM storage_targets WHERE id = $1");
        sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(id)
            .fetch_optional(&self.pool)
            .await?
            .map(|r| target_of(&r))
            .transpose()
    }

    pub async fn targets_of(&self, owner: &Owner) -> Result<Vec<Target>> {
        let sql = format!(
            "SELECT {TARGET_COLUMNS} FROM storage_targets WHERE owner_kind = $1 AND owner_name = $2 ORDER BY name"
        );
        sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(owner.kind.as_str())
            .bind(&owner.name)
            .fetch_all(&self.pool)
            .await?
            .iter()
            .map(target_of)
            .collect()
    }

    /// Targets in one of `states` (the health checker's work list).
    pub async fn targets_in(&self, state: State, limit: i64) -> Result<Vec<Target>> {
        let sql = format!(
            "SELECT {TARGET_COLUMNS} FROM storage_targets WHERE state = $1 ORDER BY updated LIMIT $2"
        );
        sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(state.as_str())
            .bind(limit)
            .fetch_all(&self.pool)
            .await?
            .iter()
            .map(target_of)
            .collect()
    }

    /// The sealed credentials and the generation they were sealed under.
    pub async fn sealed(&self, id: &str) -> Result<Option<Vec<u8>>> {
        sqlx::query_scalar("SELECT sealed_credentials FROM storage_targets WHERE id = $1")
            .bind(id)
            .fetch_optional(&self.pool)
            .await
    }

    /// Replaces the descriptive fields, and the credentials when `sealed` is given, moving the
    /// target to `pending`. `Ok(false)` when `version` is stale or the name is taken.
    #[allow(clippy::too_many_arguments)]
    pub async fn update_target(
        &self,
        old: &Target,
        new: &Target,
        sealed: Option<(&[u8], u32, &str)>,
        reset_to_pending: bool,
        now: i64,
    ) -> Result<bool> {
        let state = if reset_to_pending { State::Pending } else { new.state };
        let hint = sealed.map_or(new.access_key_hint.as_str(), |s| s.2);
        let r = sqlx::query(
            "UPDATE storage_targets SET name = $1, endpoint = $2, region = $3, bucket = $4, prefix = $5, \
             addressing = $6, access_key_hint = $7, state = $8, last_error = $9, verified_at = $10, \
             was_enabled = $14, version = version + 1, updated = $11 WHERE id = $12 AND version = $13",
        )
        .bind(&new.name)
        .bind(&new.endpoint)
        .bind(&new.region)
        .bind(&new.bucket)
        .bind(&new.prefix)
        .bind(new.addressing.as_str())
        .bind(hint)
        .bind(state.as_str())
        .bind(if reset_to_pending { "" } else { new.last_error.as_str() })
        .bind(if reset_to_pending { 0 } else { new.verified_at })
        .bind(now)
        .bind(&old.id)
        .bind(old.version)
        .bind(i64::from(new.was_enabled))
        .execute(&self.pool)
        .await;
        let done = match r {
            Ok(r) => r.rows_affected() == 1,
            Err(sqlx::Error::Database(d)) if d.is_unique_violation() => return Ok(false),
            Err(e) => return Err(e),
        };
        if done && let Some((blob, key_gen, _)) = sealed {
            sqlx::query("UPDATE storage_targets SET sealed_credentials = $1, key_gen = $2 WHERE id = $3")
                .bind(blob.to_vec())
                .bind(i64::from(key_gen))
                .bind(&old.id)
                .execute(&self.pool)
                .await?;
        }
        Ok(done)
    }

    /// Sets the lifecycle state. The version is bumped so cached live targets are rebuilt.
    pub async fn set_state(
        &self,
        id: &str,
        state: State,
        was_enabled: Option<bool>,
        last_error: &str,
        verified_at: Option<i64>,
        now: i64,
    ) -> Result<()> {
        sqlx::query(
            "UPDATE storage_targets SET state = $1, \
             was_enabled = CASE WHEN CAST($2 AS BIGINT) < 0 THEN was_enabled ELSE CAST($2 AS BIGINT) END, \
             last_error = $3, verified_at = CASE WHEN CAST($4 AS BIGINT) < 0 THEN verified_at ELSE CAST($4 AS BIGINT) END, \
             version = version + 1, updated = $5 WHERE id = $6",
        )
        .bind(state.as_str())
        .bind(was_enabled.map_or(-1_i64, i64::from))
        .bind(last_error)
        .bind(verified_at.unwrap_or(-1))
        .bind(now)
        .bind(id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Notes a health result without changing the version: `last_error` only.
    pub async fn set_last_error(&self, id: &str, last_error: &str, now: i64) -> Result<()> {
        sqlx::query("UPDATE storage_targets SET last_error = $1, updated = $2 WHERE id = $3")
            .bind(last_error)
            .bind(now)
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn delete_target(&self, id: &str) -> Result<bool> {
        let r = sqlx::query("DELETE FROM storage_targets WHERE id = $1")
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(r.rows_affected() == 1)
    }

    /// Takes one verification slot of the target's hourly allowance. `false`: none left.
    pub async fn take_verify_slot(&self, id: &str, now: i64) -> Result<bool> {
        let reset = sqlx::query(
            "UPDATE storage_targets SET verify_window = $1, verify_count = 1 WHERE id = $2 AND verify_window <= $3",
        )
        .bind(now)
        .bind(id)
        .bind(now - VERIFY_WINDOW)
        .execute(&self.pool)
        .await?;
        if reset.rows_affected() == 1 {
            return Ok(true);
        }
        let bumped = sqlx::query(
            "UPDATE storage_targets SET verify_count = verify_count + 1 WHERE id = $1 AND verify_count < $2",
        )
        .bind(id)
        .bind(VERIFY_LIMIT)
        .execute(&self.pool)
        .await?;
        Ok(bumped.rows_affected() == 1)
    }

    /// Repositories whose route names the target as primary, fallback or mirror.
    pub async fn repos_using(&self, target_id: &str) -> Result<i64> {
        sqlx::query_scalar(
            "SELECT COUNT(*) FROM repo_storage WHERE target_id = $1 OR fallback = $1 OR mirror_to = $1",
        )
        .bind(target_id)
        .fetch_one(&self.pool)
        .await
    }

    /// Repositories assigned (as primary) to the target.
    pub async fn repos_assigned(&self, target_id: &str) -> Result<i64> {
        sqlx::query_scalar("SELECT COUNT(*) FROM repo_storage WHERE target_id = $1")
            .bind(target_id)
            .fetch_one(&self.pool)
            .await
    }

    /// Migrations of one repository that are not over.
    pub async fn count_active_for_repo(&self, repo_id: i64) -> Result<i64> {
        sqlx::query_scalar(
            "SELECT COUNT(*) FROM storage_migrations WHERE repo_id = $1              AND state IN ('queued', 'copying', 'verifying', 'switching')",
        )
        .bind(repo_id)
        .fetch_one(&self.pool)
        .await
    }

    /// Whether a migration that is not over involves the target.
    pub async fn active_migrations_on(&self, target_id: &str) -> Result<i64> {
        sqlx::query_scalar(
            "SELECT COUNT(*) FROM storage_migrations WHERE (from_target = $1 OR to_target = $1) \
             AND state IN ('queued', 'copying', 'verifying', 'switching')",
        )
        .bind(target_id)
        .fetch_one(&self.pool)
        .await
    }

    // ------------------------------------------------------------------ routes

    pub async fn route(&self, repo_id: i64) -> Result<Option<RouteRow>> {
        let r = sqlx::query("SELECT repo_id, target_id, fallback, mirror_to, version FROM repo_storage WHERE repo_id = $1")
            .bind(repo_id)
            .fetch_optional(&self.pool)
            .await?;
        r.map(|r| route_of(&r)).transpose()
    }

    /// `(namespace, target_id)` of every routed repository, including those deleted but not yet
    /// dropped, so a drop still finds its objects.
    pub async fn route_table(&self) -> Result<Vec<(Vec<u8>, RouteRow)>> {
        let rows = sqlx::query(
            "SELECT r.ns AS ns, s.repo_id, s.target_id, s.fallback, s.mirror_to, s.version \
             FROM repo_storage s JOIN repos r ON r.id = s.repo_id \
             UNION ALL \
             SELECT d.ns AS ns, s.repo_id, s.target_id, s.fallback, s.mirror_to, s.version \
             FROM repo_storage s JOIN repo_drops d ON d.repo_id = s.repo_id",
        )
        .fetch_all(&self.pool)
        .await?;
        rows.iter()
            .map(|r| Ok((r.try_get::<Vec<u8>, _>("ns")?, route_of(r)?)))
            .collect()
    }

    /// Removes the routes of repositories that are gone for good (no row, no pending drop).
    pub async fn sweep_routes(&self) -> Result<u64> {
        let r = sqlx::query(
            "DELETE FROM repo_storage WHERE NOT EXISTS (SELECT 1 FROM repos WHERE repos.id = repo_storage.repo_id) \
             AND NOT EXISTS (SELECT 1 FROM repo_drops WHERE repo_drops.repo_id = repo_storage.repo_id)",
        )
        .execute(&self.pool)
        .await?;
        Ok(r.rows_affected())
    }

    /// Replaces a repository's route. `expect_version` is the version read before (`None` when
    /// there was no row). `Ok(false)` when it changed meanwhile.
    pub async fn put_route(
        &self,
        repo_id: i64,
        target_id: &str,
        fallback: Option<&str>,
        mirror_to: Option<&str>,
        expect_version: Option<i64>,
        now: i64,
    ) -> Result<bool> {
        match expect_version {
            None => {
                let r = sqlx::query(
                    "INSERT INTO repo_storage (repo_id, target_id, fallback, mirror_to, version, updated) \
                     VALUES ($1, $2, $3, $4, 1, $5)",
                )
                .bind(repo_id)
                .bind(target_id)
                .bind(fallback)
                .bind(mirror_to)
                .bind(now)
                .execute(&self.pool)
                .await;
                match r {
                    Ok(_) => Ok(true),
                    Err(sqlx::Error::Database(d)) if d.is_unique_violation() => Ok(false),
                    Err(e) => Err(e),
                }
            }
            Some(v) => {
                let r = sqlx::query(
                    "UPDATE repo_storage SET target_id = $1, fallback = $2, mirror_to = $3, \
                     version = version + 1, updated = $4 WHERE repo_id = $5 AND version = $6",
                )
                .bind(target_id)
                .bind(fallback)
                .bind(mirror_to)
                .bind(now)
                .bind(repo_id)
                .bind(v)
                .execute(&self.pool)
                .await?;
                Ok(r.rows_affected() == 1)
            }
        }
    }

    /// Removes a repository's route (back to the instance default with nothing elsewhere).
    pub async fn clear_route(&self, repo_id: i64) -> Result<()> {
        sqlx::query("DELETE FROM repo_storage WHERE repo_id = $1")
            .bind(repo_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// The `repos` row of `name`: `(id, ns)`.
    pub async fn repo(&self, name: &str) -> Result<Option<(i64, Vec<u8>)>> {
        let r = sqlx::query("SELECT id, ns FROM repos WHERE name = $1")
            .bind(name)
            .fetch_optional(&self.pool)
            .await?;
        r.map(|r| Ok((r.try_get("id")?, r.try_get("ns")?))).transpose()
    }

    // ------------------------------------------------------------------ migrations

    /// Inserts a queued migration. `Ok(false)` when the repository already has an active one.
    pub async fn insert_migration(&self, m: &NewMigration<'_>, now_ms: i64) -> Result<bool> {
        let NewMigration { id, repo_id, repo_name, from, to, delete_source } = *m;
        let r = sqlx::query(
            "INSERT INTO storage_migrations (id, repo_id, repo_name, from_target, to_target, state, delete_source, created_ms, updated) \
             VALUES ($1, $2, $3, $4, $5, 'queued', $6, $7, $8)",
        )
        .bind(id)
        .bind(repo_id)
        .bind(repo_name)
        .bind(from)
        .bind(to)
        .bind(i64::from(delete_source))
        .bind(now_ms)
        .bind(now_ms / 1000)
        .execute(&self.pool)
        .await;
        match r {
            Ok(_) => Ok(true),
            Err(sqlx::Error::Database(d)) if d.is_unique_violation() => Ok(false),
            Err(e) => Err(e),
        }
    }

    pub async fn migration(&self, id: &str) -> Result<Option<MigrationRow>> {
        let sql = format!("SELECT {MIGRATION_COLUMNS} FROM storage_migrations WHERE id = $1");
        sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(id)
            .fetch_optional(&self.pool)
            .await?
            .map(|r| migration_of(&r))
            .transpose()
    }

    /// Newest first, `after` is the `(created, id)` of the last row of the previous page.
    pub async fn migrations_of(
        &self,
        repo_id: i64,
        after: Option<(i64, &str)>,
        limit: i64,
    ) -> Result<Vec<MigrationRow>> {
        let (created, id) = after.unwrap_or((i64::MAX, "~"));
        let sql = format!(
            "SELECT {MIGRATION_COLUMNS} FROM storage_migrations WHERE repo_id = $1 \
             AND (created_ms < $2 OR (created_ms = $2 AND id < $3)) ORDER BY created_ms DESC, id DESC LIMIT $4"
        );
        sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(repo_id)
            .bind(created)
            .bind(id)
            .bind(limit)
            .fetch_all(&self.pool)
            .await?
            .iter()
            .map(migration_of)
            .collect()
    }

    /// Migrations nobody holds a lease on, oldest first.
    pub async fn runnable_migrations(&self, now: i64, limit: i64) -> Result<Vec<MigrationRow>> {
        let sql = format!(
            "SELECT {MIGRATION_COLUMNS} FROM storage_migrations WHERE state IN ('queued', 'copying', 'verifying', 'switching') \
             AND lease_until <= $1 ORDER BY created_ms LIMIT $2"
        );
        sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(now)
            .bind(limit)
            .fetch_all(&self.pool)
            .await?
            .iter()
            .map(migration_of)
            .collect()
    }

    /// Takes (or renews) the lease on a migration. `false`: another worker holds it.
    pub async fn claim_migration(&self, id: &str, held_until: i64, now: i64, renew: bool) -> Result<bool> {
        let r = if renew {
            // Renewal: only while we still hold it; `held_until` of the previous claim is not
            // needed because the worker is the only one that renews.
            sqlx::query("UPDATE storage_migrations SET lease_until = $1 WHERE id = $2 AND lease_until > $3")
                .bind(held_until)
                .bind(id)
                .bind(now)
                .execute(&self.pool)
                .await?
        } else {
            sqlx::query("UPDATE storage_migrations SET lease_until = $1 WHERE id = $2 AND lease_until <= $3")
                .bind(held_until)
                .bind(id)
                .bind(now)
                .execute(&self.pool)
                .await?
        };
        Ok(r.rows_affected() == 1)
    }

    pub async fn release_migration(&self, id: &str) -> Result<()> {
        sqlx::query("UPDATE storage_migrations SET lease_until = 0 WHERE id = $1")
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn migration_state(&self, id: &str, state: &str, error: &str, now: i64) -> Result<()> {
        sqlx::query("UPDATE storage_migrations SET state = $1, error = $2, updated = $3 WHERE id = $4")
            .bind(state)
            .bind(error)
            .bind(now)
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn migration_progress(
        &self,
        id: &str,
        total: Option<i64>,
        copied: i64,
        bytes: i64,
        cursor: &str,
        now: i64,
    ) -> Result<()> {
        sqlx::query(
            "UPDATE storage_migrations SET objects_total = CASE WHEN CAST($1 AS BIGINT) < 0 THEN objects_total ELSE CAST($1 AS BIGINT) END, \
             objects_copied = $2, bytes_copied = $3, cursor = $4, updated = $5 WHERE id = $6",
        )
        .bind(total.unwrap_or(-1))
        .bind(copied)
        .bind(bytes)
        .bind(cursor)
        .bind(now)
        .bind(id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn migration_mark(&self, id: &str, column: Mark, now: i64) -> Result<()> {
        let sql = match column {
            Mark::Cancel => "UPDATE storage_migrations SET cancel = 1, updated = $1 WHERE id = $2",
            Mark::Switched => "UPDATE storage_migrations SET switched = 1, updated = $1 WHERE id = $2",
            Mark::MirrorSince => "UPDATE storage_migrations SET mirror_since = $1, updated = $1 WHERE id = $2",
        };
        sqlx::query(sql).bind(now).bind(id).execute(&self.pool).await?;
        Ok(())
    }

    pub async fn count_active_migrations(&self) -> Result<i64> {
        sqlx::query_scalar(
            "SELECT COUNT(*) FROM storage_migrations WHERE state IN ('queued', 'copying', 'verifying', 'switching')",
        )
        .fetch_one(&self.pool)
        .await
    }

    /// Counts sealed credentials per key generation, for the vault.
    pub async fn sealed_counts(&self) -> Result<Vec<(i64, i64)>> {
        let rows = sqlx::query("SELECT key_gen, COUNT(*) AS n FROM storage_targets GROUP BY key_gen")
            .fetch_all(&self.pool)
            .await?;
        rows.iter()
            .map(|r| Ok((r.try_get("key_gen")?, r.try_get("n")?)))
            .collect()
    }

    /// Targets sealed under a generation other than `target`, with their owner scope.
    pub async fn not_under(&self, target: u32, limit: i64) -> Result<Vec<(String, String, Vec<u8>, i64)>> {
        let rows = sqlx::query(
            "SELECT id, owner_kind, owner_name, sealed_credentials, key_gen FROM storage_targets \
             WHERE key_gen <> $1 ORDER BY id LIMIT $2",
        )
        .bind(i64::from(target))
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        rows.iter()
            .map(|r| {
                let kind: String = r.try_get("owner_kind")?;
                let name: String = r.try_get("owner_name")?;
                Ok((
                    r.try_get("id")?,
                    format!("{kind}:{name}"),
                    r.try_get("sealed_credentials")?,
                    r.try_get("key_gen")?,
                ))
            })
            .collect()
    }

    /// Replaces a sealed value only if it still holds `old_gen`.
    pub async fn replace_sealed(&self, id: &str, blob: &[u8], new_gen: u32, old_gen: i64) -> Result<bool> {
        let r = sqlx::query(
            "UPDATE storage_targets SET sealed_credentials = $1, key_gen = $2 WHERE id = $3 AND key_gen = $4",
        )
        .bind(blob.to_vec())
        .bind(i64::from(new_gen))
        .bind(id)
        .bind(old_gen)
        .execute(&self.pool)
        .await?;
        Ok(r.rows_affected() == 1)
    }
}

/// Which flag of a migration to set.
#[derive(Clone, Copy, Debug)]
pub enum Mark {
    Cancel,
    Switched,
    MirrorSince,
}

fn route_of(r: &AnyRow) -> Result<RouteRow> {
    Ok(RouteRow {
        repo_id: r.try_get("repo_id")?,
        target_id: r.try_get("target_id")?,
        fallback: r.try_get("fallback")?,
        mirror_to: r.try_get("mirror_to")?,
        version: r.try_get("version")?,
    })
}
