//! Stat times of virtual files (SPEC §10.1). A placeholder reports its path's `entry_times` value
//! if one exists, else the workspace's clone time. A switch that changes paths takes
//! `T = max(floor_seconds(now), previous epoch_time + 1)`, so times only move forward and a
//! changed file is always newer than build outputs made before the switch.

use std::time::{SystemTime, UNIX_EPOCH};

use asc_core::path::RepoPath;
use asc_state::state::tables::entry_times;
use asc_state::state::workspaces::Workspace;
use asc_state::{Error as StateError, StateDb, WorkspaceId};

use crate::plain::Result;

/// `entry_times` rows above which a clean switch to a new tree rewrites every placeholder and
/// clears the table (SPEC §10.1).
pub const COMPACT_THRESHOLD: u64 = 50_000;

/// Whole seconds since the Unix epoch; a clock before 1970 reads as 0.
pub fn floor_seconds(t: SystemTime) -> i64 {
    t.duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

/// The time of the next epoch after one stamped `prev_epoch_time`, given the clock `now`.
pub fn next_time(prev_epoch_time: i64, now: i64) -> i64 {
    now.max(prev_epoch_time.saturating_add(1))
}

/// A new epoch recorded by [`record_switch`] or [`compact`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Switched {
    pub epoch: i64,
    /// `T`: the time of every path the switch changed.
    pub time: i64,
}

/// The time a placeholder at `path` reports.
pub async fn entry_time(db: &StateDb, ws: &Workspace, path: &RepoPath) -> Result<i64> {
    Ok(entry_times::get(db, ws.ws.id, path.as_bytes())
        .await?
        .unwrap_or(ws.created))
}

/// [`entry_time`] for several paths, in order.
pub async fn entry_times_of(db: &StateDb, ws: &Workspace, paths: &[RepoPath]) -> Result<Vec<i64>> {
    let mut out = Vec::with_capacity(paths.len());
    for p in paths {
        out.push(entry_time(db, ws, p).await?);
    }
    Ok(out)
}

/// Stamps a switch: bumps the epoch, sets the epoch time to `T` and records `T` for every changed
/// path, in one transaction. `now` is the clock in whole seconds.
pub async fn record_switch(
    db: &StateDb,
    ws: WorkspaceId,
    changed: &[RepoPath],
    now: i64,
) -> Result<Switched> {
    let mut tx = db.begin().await?;
    let (epoch, time) = bump(&mut tx, ws, now).await?;
    for p in changed {
        sqlx::query(
            "INSERT INTO entry_times (ws_id, path, time) VALUES (?, ?, ?) \
             ON CONFLICT (ws_id, path) DO UPDATE SET time = excluded.time",
        )
        .bind(ws.to_string())
        .bind(p.as_bytes())
        .bind(time)
        .execute(&mut *tx)
        .await
        .map_err(StateError::from)?;
    }
    tx.commit().await.map_err(StateError::from)?;
    Ok(Switched { epoch, time })
}

async fn bump(tx: &mut asc_state::state::Tx, ws: WorkspaceId, now: i64) -> Result<(i64, i64)> {
    let row: Option<(i64, i64)> =
        sqlx::query_as("SELECT epoch, epoch_time FROM workspaces WHERE id = ?")
            .bind(ws.to_string())
            .fetch_optional(&mut **tx)
            .await
            .map_err(StateError::from)?;
    let (epoch, prev) = row.ok_or(StateError::NotFound("workspace"))?;
    let (epoch, time) = (epoch.saturating_add(1), next_time(prev, now));
    sqlx::query("UPDATE workspaces SET epoch = ?, epoch_time = ? WHERE id = ?")
        .bind(epoch)
        .bind(time)
        .bind(ws.to_string())
        .execute(&mut **tx)
        .await
        .map_err(StateError::from)?;
    Ok((epoch, time))
}

/// Number of `entry_times` rows of `ws`.
pub async fn count(db: &StateDb, ws: WorkspaceId) -> Result<u64> {
    let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM entry_times WHERE ws_id = ?")
        .bind(ws.to_string())
        .fetch_one(db.pool())
        .await
        .map_err(StateError::from)?;
    Ok(u64::try_from(n).unwrap_or(0))
}

/// Whether a clean switch to a new tree should compact now.
pub async fn needs_compaction(db: &StateDb, ws: WorkspaceId) -> Result<bool> {
    Ok(count(db, ws).await? > COMPACT_THRESHOLD)
}

/// Compaction: a new epoch with time `T` and an empty `entry_times`. The caller rewrites every
/// placeholder with `T` (the clone-time fallback no longer applies once it has), so call this only
/// for a clean switch where that rewrite follows.
pub async fn compact(db: &StateDb, ws: WorkspaceId, now: i64) -> Result<Switched> {
    let mut tx = db.begin().await?;
    let (epoch, time) = bump(&mut tx, ws, now).await?;
    sqlx::query("DELETE FROM entry_times WHERE ws_id = ?")
        .bind(ws.to_string())
        .execute(&mut *tx)
        .await
        .map_err(StateError::from)?;
    tx.commit().await.map_err(StateError::from)?;
    Ok(Switched { epoch, time })
}

#[cfg(test)]
mod tests {
    use asc_state::state::workspaces;

    use super::*;
    use crate::journal::fixtures;

    fn p(s: &str) -> RepoPath {
        RepoPath::new(s).unwrap()
    }

    #[test]
    fn next_time_never_repeats_or_goes_back() {
        assert_eq!(next_time(100, 200), 200);
        assert_eq!(next_time(100, 100), 101);
        assert_eq!(next_time(100, 50), 101);
        assert_eq!(next_time(i64::MAX, 5), i64::MAX);
        assert_eq!(floor_seconds(UNIX_EPOCH), 0);
    }

    #[tokio::test]
    async fn times_strictly_increase_across_switches() {
        let (_d, db, ws) = fixtures::db().await;
        let w0 = workspaces::get(&db, ws).await.unwrap().unwrap();
        // Unchanged placeholders report the clone time.
        assert_eq!(entry_time(&db, &w0, &p("a")).await.unwrap(), w0.created);

        let now = w0.epoch_time; // a clock that does not advance between switches
        let s1 = record_switch(&db, ws, &[p("a"), p("d/b")], now)
            .await
            .unwrap();
        let s2 = record_switch(&db, ws, &[p("a")], now).await.unwrap();
        let s3 = record_switch(&db, ws, &[], now - 1000).await.unwrap();
        assert!(s1.time > w0.epoch_time && s2.time > s1.time && s3.time > s2.time);
        assert_eq!(
            (s1.epoch, s2.epoch, s3.epoch),
            (w0.epoch + 1, w0.epoch + 2, w0.epoch + 3)
        );

        let w = workspaces::get(&db, ws).await.unwrap().unwrap();
        assert_eq!((w.epoch, w.epoch_time), (s3.epoch, s3.time));
        assert_eq!(entry_time(&db, &w, &p("a")).await.unwrap(), s2.time);
        assert_eq!(entry_time(&db, &w, &p("d/b")).await.unwrap(), s1.time);
        assert_eq!(
            entry_time(&db, &w, &p("untouched")).await.unwrap(),
            w.created
        );
        assert_eq!(
            entry_times_of(&db, &w, &[p("a"), p("d/b"), p("z")])
                .await
                .unwrap(),
            [s2.time, s1.time, w.created]
        );
    }

    #[tokio::test]
    async fn compaction_clears_rows_and_keeps_time_moving() {
        let (_d, db, ws) = fixtures::db().await;
        let t0 = workspaces::get(&db, ws).await.unwrap().unwrap().epoch_time;
        let s = record_switch(&db, ws, &[p("a"), p("b")], t0).await.unwrap();
        assert_eq!(count(&db, ws).await.unwrap(), 2);
        assert!(!needs_compaction(&db, ws).await.unwrap());
        let c = compact(&db, ws, t0).await.unwrap();
        assert!(c.time > s.time && c.epoch == s.epoch + 1);
        assert_eq!(count(&db, ws).await.unwrap(), 0);
    }

    #[tokio::test]
    async fn unknown_workspace_is_an_error() {
        let (_d, db, _ws) = fixtures::db().await;
        assert!(
            record_switch(&db, WorkspaceId::new(), &[], 1)
                .await
                .is_err()
        );
    }
}
