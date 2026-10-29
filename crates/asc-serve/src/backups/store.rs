//! The SQL side of backups: run records, the schedule, staged restores.

use sqlx::any::AnyRow;
use sqlx::{AnyPool, Row};

pub const RUNNING: i64 = 1;
pub const SUCCEEDED: i64 = 2;
pub const FAILED: i64 = 3;

pub const TRIG_MANUAL: i64 = 1;
pub const TRIG_SCHEDULE: i64 = 2;

pub const V_NONE: i64 = 0;
pub const V_RUNNING: i64 = 1;
pub const V_OK: i64 = 2;
pub const V_FAILED: i64 = 3;

/// A restore that is being prepared or waits for the next start.
pub const R_PREPARING: i64 = 1;
pub const R_STAGED: i64 = 2;
pub const R_FAILED: i64 = 3;

const COLS: &str = "id, state, trig, target, started, finished, size_bytes, objects, encrypted, verify, verified_at, \
                    note, done_bytes, total_bytes, actor, location, purged, seal";

/// One run.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Run {
    pub id: String,
    pub state: i64,
    pub trig: i64,
    pub target: String,
    pub started: i64,
    pub finished: i64,
    pub size_bytes: i64,
    pub objects: i64,
    pub encrypted: bool,
    pub verify: i64,
    pub verified_at: i64,
    pub note: String,
    pub done_bytes: i64,
    pub total_bytes: i64,
    pub actor: String,
    pub location: String,
    pub purged: bool,
    /// The server's record of what the backup holds (see `place::seal`); empty until it finishes.
    pub seal: String,
}

fn run_of(r: &AnyRow) -> Result<Run, sqlx::Error> {
    Ok(Run {
        id: r.try_get("id")?,
        state: r.try_get("state")?,
        trig: r.try_get("trig")?,
        target: r.try_get("target")?,
        started: r.try_get("started")?,
        finished: r.try_get("finished")?,
        size_bytes: r.try_get("size_bytes")?,
        objects: r.try_get("objects")?,
        encrypted: r.try_get::<i64, _>("encrypted")? != 0,
        verify: r.try_get("verify")?,
        verified_at: r.try_get("verified_at")?,
        note: r.try_get("note")?,
        done_bytes: r.try_get("done_bytes")?,
        total_bytes: r.try_get("total_bytes")?,
        actor: r.try_get("actor")?,
        location: r.try_get("location")?,
        purged: r.try_get::<i64, _>("purged")? != 0,
        seal: r.try_get("seal")?,
    })
}

/// The schedule row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Schedule {
    pub enabled: bool,
    pub interval_hours: u32,
    pub minute_of_day: u32,
    pub target: String,
    pub keep_days: u32,
    pub keep_weeks: u32,
    pub include_vault_key: bool,
    pub include_objects: bool,
    pub updated: i64,
}

impl Default for Schedule {
    fn default() -> Self {
        Self {
            enabled: false,
            interval_hours: 24,
            minute_of_day: 120,
            target: String::new(),
            keep_days: 30,
            keep_weeks: 52,
            include_vault_key: false,
            include_objects: true,
            updated: 0,
        }
    }
}

/// A staged restore.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Restore {
    pub id: String,
    pub backup_id: String,
    pub state: i64,
    pub requested: i64,
    pub actor: String,
    pub note: String,
}

#[derive(Clone)]
pub struct Store {
    pool: AnyPool,
}

impl Store {
    pub fn new(pool: AnyPool) -> Self {
        Self { pool }
    }

    pub async fn insert_run(&self, r: &Run) -> Result<(), sqlx::Error> {
        sqlx::query(
            "INSERT INTO backup_runs (id, state, trig, target, started, actor, location, encrypted) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
        )
        .bind(r.id.clone())
        .bind(r.state)
        .bind(r.trig)
        .bind(r.target.clone())
        .bind(r.started)
        .bind(r.actor.clone())
        .bind(r.location.clone())
        .bind(i64::from(r.encrypted))
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn get_run(&self, id: &str) -> Result<Option<Run>, sqlx::Error> {
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "SELECT {COLS} FROM backup_runs WHERE id = $1"
        )))
        .bind(id.to_owned())
        .fetch_optional(&self.pool)
        .await?
        .as_ref()
        .map(run_of)
        .transpose()
    }

    /// Newest first.
    pub async fn list_runs(&self, limit: i64) -> Result<Vec<Run>, sqlx::Error> {
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "SELECT {COLS} FROM backup_runs ORDER BY started DESC, id DESC LIMIT $1"
        )))
        .bind(limit)
        .fetch_all(&self.pool)
        .await?
        .iter()
        .map(run_of)
        .collect()
    }

    /// Finished, still-held runs, for retention.
    pub async fn held(&self) -> Result<Vec<Run>, sqlx::Error> {
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "SELECT {COLS} FROM backup_runs WHERE purged = 0 AND state <> $1"
        )))
        .bind(RUNNING)
        .fetch_all(&self.pool)
        .await?
        .iter()
        .map(run_of)
        .collect()
    }

    pub async fn running(&self) -> Result<i64, sqlx::Error> {
        sqlx::query_scalar("SELECT CAST(COUNT(*) AS BIGINT) FROM backup_runs WHERE state = $1")
            .bind(RUNNING)
            .fetch_one(&self.pool)
            .await
    }

    /// Runs a dead process left running become failed.
    pub async fn fail_orphans(&self, now: i64) -> Result<u64, sqlx::Error> {
        let r = sqlx::query(
            "UPDATE backup_runs SET state = $1, finished = $2, note = 'interrupted by a restart' WHERE state = $3",
        )
        .bind(FAILED)
        .bind(now)
        .bind(RUNNING)
        .execute(&self.pool)
        .await?;
        let v = sqlx::query("UPDATE backup_runs SET verify = $1, note = 'verification interrupted by a restart' WHERE verify = $2")
            .bind(V_NONE)
            .bind(V_RUNNING)
            .execute(&self.pool)
            .await?;
        Ok(r.rows_affected() + v.rows_affected())
    }

    pub async fn progress(&self, id: &str, done: i64, total: i64) -> Result<(), sqlx::Error> {
        sqlx::query("UPDATE backup_runs SET done_bytes = $2, total_bytes = $3 WHERE id = $1")
            .bind(id.to_owned())
            .bind(done)
            .bind(total)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn finish(
        &self,
        id: &str,
        state: i64,
        now: i64,
        size: i64,
        objects: i64,
        encrypted: bool,
        note: &str,
        seal: &str,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            "UPDATE backup_runs SET state = $2, finished = $3, size_bytes = $4, objects = $5, encrypted = $6, \
             note = $7, seal = $8, done_bytes = CASE WHEN $2 = 2 THEN total_bytes ELSE done_bytes END WHERE id = $1",
        )
        .bind(id.to_owned())
        .bind(state)
        .bind(now)
        .bind(size)
        .bind(objects)
        .bind(i64::from(encrypted))
        .bind(note.to_owned())
        .bind(seal.to_owned())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn set_verify(
        &self,
        id: &str,
        v: i64,
        now: i64,
        note: Option<&str>,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            "UPDATE backup_runs SET verify = $2, verified_at = CASE WHEN $2 = 2 THEN $3 ELSE verified_at END, \
             note = CASE WHEN $4 = '' THEN note ELSE $4 END WHERE id = $1",
        )
        .bind(id.to_owned())
        .bind(v)
        .bind(now)
        .bind(note.unwrap_or("").to_owned())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn mark_purged(&self, id: &str) -> Result<(), sqlx::Error> {
        sqlx::query("UPDATE backup_runs SET purged = 1 WHERE id = $1")
            .bind(id.to_owned())
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Forgets the records of purged backups that finished before `before`.
    pub async fn forget_purged(&self, before: i64) -> Result<(), sqlx::Error> {
        sqlx::query("DELETE FROM backup_runs WHERE purged = 1 AND finished < $1")
            .bind(before)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// The start of the newest scheduled run, 0 when none.
    pub async fn last_scheduled(&self) -> Result<i64, sqlx::Error> {
        sqlx::query_scalar(
            "SELECT CAST(COALESCE(MAX(started), 0) AS BIGINT) FROM backup_runs WHERE trig = $1",
        )
        .bind(TRIG_SCHEDULE)
        .fetch_one(&self.pool)
        .await
    }

    /// `(last success, last failure)`, 0 when none.
    pub async fn last_results(&self) -> Result<(i64, i64), sqlx::Error> {
        let ok: i64 = sqlx::query_scalar(
            "SELECT CAST(COALESCE(MAX(finished), 0) AS BIGINT) FROM backup_runs WHERE state = $1 AND purged = 0",
        )
        .bind(SUCCEEDED)
        .fetch_one(&self.pool)
        .await?;
        let bad: i64 = sqlx::query_scalar(
            "SELECT CAST(COALESCE(MAX(finished), 0) AS BIGINT) FROM backup_runs WHERE state = $1",
        )
        .bind(FAILED)
        .fetch_one(&self.pool)
        .await?;
        Ok((ok, bad))
    }

    pub async fn schedule(&self) -> Result<Schedule, sqlx::Error> {
        let row = sqlx::query(
            "SELECT enabled, interval_hours, minute_of_day, target, keep_days, keep_weeks, include_vault_key, \
             include_objects, updated FROM backup_schedule WHERE id = 1",
        )
        .fetch_optional(&self.pool)
        .await?;
        let Some(r) = row else {
            return Ok(Schedule::default());
        };
        let u = |c: &str| -> Result<u32, sqlx::Error> {
            Ok(u32::try_from(r.try_get::<i64, _>(c)?).unwrap_or(0))
        };
        Ok(Schedule {
            enabled: r.try_get::<i64, _>("enabled")? != 0,
            interval_hours: u("interval_hours")?,
            minute_of_day: u("minute_of_day")?,
            target: r.try_get("target")?,
            keep_days: u("keep_days")?,
            keep_weeks: u("keep_weeks")?,
            include_vault_key: r.try_get::<i64, _>("include_vault_key")? != 0,
            include_objects: r.try_get::<i64, _>("include_objects")? != 0,
            updated: r.try_get("updated")?,
        })
    }

    pub async fn set_schedule(&self, s: &Schedule) -> Result<(), sqlx::Error> {
        sqlx::query("DELETE FROM backup_schedule WHERE id = 1")
            .execute(&self.pool)
            .await?;
        sqlx::query(
            "INSERT INTO backup_schedule (id, enabled, interval_hours, minute_of_day, target, keep_days, keep_weeks, \
             include_vault_key, include_objects, updated) VALUES (1, $1, $2, $3, $4, $5, $6, $7, $8, $9)",
        )
        .bind(i64::from(s.enabled))
        .bind(i64::from(s.interval_hours))
        .bind(i64::from(s.minute_of_day))
        .bind(s.target.clone())
        .bind(i64::from(s.keep_days))
        .bind(i64::from(s.keep_weeks))
        .bind(i64::from(s.include_vault_key))
        .bind(i64::from(s.include_objects))
        .bind(s.updated)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn insert_restore(&self, r: &Restore) -> Result<(), sqlx::Error> {
        sqlx::query(
            "INSERT INTO backup_restores (id, backup_id, state, requested, actor, note) VALUES ($1, $2, $3, $4, $5, $6)",
        )
        .bind(r.id.clone())
        .bind(r.backup_id.clone())
        .bind(r.state)
        .bind(r.requested)
        .bind(r.actor.clone())
        .bind(r.note.clone())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn set_restore(&self, id: &str, state: i64, note: &str) -> Result<(), sqlx::Error> {
        sqlx::query("UPDATE backup_restores SET state = $2, note = $3 WHERE id = $1")
            .bind(id.to_owned())
            .bind(state)
            .bind(note.to_owned())
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// The newest restore that is preparing or staged.
    pub async fn pending_restore(&self) -> Result<Option<Restore>, sqlx::Error> {
        let row = sqlx::query(
            "SELECT id, backup_id, state, requested, actor, note FROM backup_restores \
             WHERE state IN ($1, $2) ORDER BY requested DESC LIMIT 1",
        )
        .bind(R_PREPARING)
        .bind(R_STAGED)
        .fetch_optional(&self.pool)
        .await?;
        row.map(|r| {
            Ok(Restore {
                id: r.try_get("id")?,
                backup_id: r.try_get("backup_id")?,
                state: r.try_get("state")?,
                requested: r.try_get("requested")?,
                actor: r.try_get("actor")?,
                note: r.try_get("note")?,
            })
        })
        .transpose()
    }
}
