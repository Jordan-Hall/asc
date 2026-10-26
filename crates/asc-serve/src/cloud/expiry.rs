//! The commit-cloud expiry worker and the GC roots of cloud heads (SPEC §21.2.4, PLAN B10).
//!
//! Hourly, per repository, in batches of [`BATCH`] rows through the indexes `cloud_heads_hidden`
//! and `cloud_heads_added` (never `changes` or `refs` as a scan):
//!
//! * hidden heads with `hidden_at < now - hidden_grace_days` are deleted; their objects become
//!   collectable by O01's two-mark sweep, and
//! * visible heads with `added < now - abandon_days` and no open PR for any change in their stack
//!   get `hidden_at = now` (and start the grace period).
//!
//! Both change the workspace (`version + 1`, `heads` kept exact), so clients sync the change.
//! Replicas may run at once: every statement re-checks its row's state, so the loser of a race
//! changes nothing.
//!
//! [`CloudRoots`] is the [`GcRootSource`] O01 asks: every `cloud_heads.cs`, visible or hidden
//! within the grace period (expired rows are gone), and the keys of `gc_purge`.
//!
//! Choices where SPEC is silent:
//!
//! * The stack of a head is its first-parent walk through indexed changes, at most
//!   `asc_engine::stack::MAX_STACK` long, ended by a landed change (one on the mainline of the
//!   default branch), a merge, an unindexed changeset or a root. A change has an open PR when a
//!   `change_versions` row names it and it has not landed.
//! * `hidden_grace_days = 0` and `abandon_days = 0` switch the rule off, never "expire at once".
//! * A `gc_purge` row is dropped by the first GC sweep that finishes after it was written
//!   ([`GcRootSource::purge_swept`]), unless a strict root still reaches the object: that row is
//!   kept, so the object goes at the next sweep after the root is gone, and is pruned after
//!   [`PURGE_RETAIN_DAYS`] days at the latest.
//! * Settings are the SPEC defaults until `[cloud]` reaches the server configuration.

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use asc_core::change::ChangeId;
use asc_core::id::ChangesetId;
use asc_store::gc::{GcError, GcRootSource};
use asc_store::key::ObjKey;
use async_trait::async_trait;
use futures::TryStreamExt;
use sqlx::{AnyConnection, AnyPool, Row};
use tokio::task::JoinHandle;

use crate::changes::index;
use crate::registry::Ns;

/// How often the worker runs (SPEC §21.2.4: hourly).
pub const INTERVAL: Duration = Duration::from_secs(60 * 60);
/// Rows read and changed per batch.
pub const BATCH: i64 = 1_000;
/// Batches one repository gets per run and rule; the rest waits for the next run.
pub const MAX_BATCHES: usize = 100;
/// Days a `gc_purge` row is kept at most when a sweep never finishes it (its object is still
/// referenced).
pub const PURGE_RETAIN_DAYS: i64 = 30;
const DAY: i64 = 86_400;
/// Keys per `gc_purge` delete.
const SWEPT_ROWS: usize = 200;

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

/// `[cloud]` expiry settings (SPEC §21.3.2).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Settings {
    /// Days a hidden head is kept (`hidden_grace_days`, 30). 0 disables the deletion.
    pub hidden_grace_days: u32,
    /// Days after which a visible head with no open PR is hidden (`abandon_days`, 180). 0
    /// disables the hiding.
    pub abandon_days: u32,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            hidden_grace_days: 30,
            abandon_days: 180,
        }
    }
}

/// What one run did.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct ExpiryReport {
    /// Hidden heads deleted after their grace period.
    pub deleted: usize,
    /// Stale visible heads hidden.
    pub hidden: usize,
    /// Stale visible heads kept because a change in their stack has an open PR.
    pub kept: usize,
    /// `gc_purge` rows pruned.
    pub pruned: u64,
}

/// The worker.
pub struct CloudExpirer {
    pool: AnyPool,
    settings: Settings,
}

type Head = (i64, String, Vec<u8>);

impl CloudExpirer {
    pub fn new(pool: AnyPool, settings: Settings) -> Self {
        Self { pool, settings }
    }

    /// Runs every [`INTERVAL`] until aborted.
    pub fn spawn_worker(self: Arc<Self>) -> JoinHandle<()> {
        tokio::spawn(async move {
            loop {
                match self.run_once(now()).await {
                    Ok(r) => tracing::info!(
                        event = "cloud.expiry",
                        deleted = r.deleted,
                        hidden = r.hidden,
                        kept = r.kept,
                        pruned = r.pruned
                    ),
                    Err(e) => tracing::warn!(event = "cloud.expiry", error = %e, "run failed"),
                }
                tokio::time::sleep(INTERVAL).await;
            }
        })
    }

    /// One run at time `at`. A repository that fails does not stop the others (the first error
    /// is returned after the pass).
    pub async fn run_once(&self, at: i64) -> Result<ExpiryReport, sqlx::Error> {
        let mut report = ExpiryReport::default();
        let repos = sqlx::query("SELECT ns, default_branch FROM repos ORDER BY id")
            .fetch_all(&self.pool)
            .await?;
        let mut first_err = None;
        for r in &repos {
            let ns: Vec<u8> = r.try_get("ns")?;
            let branch: String = r.try_get("default_branch")?;
            if let Err(e) = self.repo(&ns, &branch, at, &mut report).await {
                tracing::warn!(event = "cloud.expiry", error = %e, "repository failed");
                first_err.get_or_insert(e);
            }
        }
        match first_err {
            Some(e) => Err(e),
            None => Ok(report),
        }
    }

    async fn repo(
        &self,
        ns: &[u8],
        branch: &str,
        at: i64,
        report: &mut ExpiryReport,
    ) -> Result<(), sqlx::Error> {
        if self.settings.hidden_grace_days > 0 {
            let cutoff = at.saturating_sub(i64::from(self.settings.hidden_grace_days) * DAY);
            for _ in 0..MAX_BATCHES {
                let n = self.delete_expired(ns, cutoff, at).await?;
                report.deleted += n.0;
                if n.1 < BATCH {
                    break;
                }
            }
        }
        if self.settings.abandon_days > 0 {
            let cutoff = at.saturating_sub(i64::from(self.settings.abandon_days) * DAY);
            self.hide_stale(ns, branch, cutoff, at, report).await?;
        }
        report.pruned +=
            crate::cloud::purge::prune(&self.pool, ns, at.saturating_sub(PURGE_RETAIN_DAYS * DAY))
                .await?;
        Ok(())
    }

    /// Deletes one batch of hidden heads older than `cutoff`. Returns the rows deleted and the
    /// rows read.
    async fn delete_expired(
        &self,
        ns: &[u8],
        cutoff: i64,
        at: i64,
    ) -> Result<(usize, i64), sqlx::Error> {
        let rows = sqlx::query(
            "SELECT owner, name, cs FROM cloud_heads \
             WHERE ns = $1 AND hidden_at IS NOT NULL AND hidden_at < $2 \
             ORDER BY hidden_at LIMIT $3",
        )
        .bind(ns.to_vec())
        .bind(cutoff)
        .bind(BATCH)
        .fetch_all(&self.pool)
        .await?;
        let read = i64::try_from(rows.len()).unwrap_or(i64::MAX);
        let mut tx = self.pool.begin().await?;
        let mut per_ws: Vec<((i64, String), i64)> = Vec::new();
        let mut deleted = 0usize;
        for r in &rows {
            let (owner, name, cs): Head =
                (r.try_get("owner")?, r.try_get("name")?, r.try_get("cs")?);
            // Re-check the state: a head unhidden since the read stays.
            let n = sqlx::query(
                "DELETE FROM cloud_heads WHERE ns = $1 AND owner = $2 AND name = $3 AND cs = $4 \
                 AND hidden_at IS NOT NULL AND hidden_at < $5",
            )
            .bind(ns.to_vec())
            .bind(owner)
            .bind(name.clone())
            .bind(cs)
            .bind(cutoff)
            .execute(&mut *tx)
            .await?
            .rows_affected();
            if n > 0 {
                deleted += 1;
                bump(&mut per_ws, (owner, name));
            }
        }
        touch_workspaces(&mut tx, ns, &per_ws, at, true).await?;
        tx.commit().await?;
        Ok((deleted, read))
    }

    /// Hides the visible heads added before `cutoff` that have no open PR in their stack.
    async fn hide_stale(
        &self,
        ns: &[u8],
        branch: &str,
        cutoff: i64,
        at: i64,
        report: &mut ExpiryReport,
    ) -> Result<(), sqlx::Error> {
        let land_ref = format!("refs/heads/{branch}");
        let mut after: Option<(i64, i64, String, Vec<u8>)> = None;
        for _ in 0..MAX_BATCHES {
            // Keyset pages: heads kept for an open PR are skipped, not re-read, within a run.
            let rows = match &after {
                None => {
                    sqlx::query(
                        "SELECT owner, name, cs, change, added FROM cloud_heads \
                         WHERE ns = $1 AND hidden_at IS NULL AND added < $2 \
                         ORDER BY added, owner, name, cs LIMIT $3",
                    )
                    .bind(ns.to_vec())
                    .bind(cutoff)
                    .bind(BATCH)
                    .fetch_all(&self.pool)
                    .await?
                }
                Some((added, owner, name, cs)) => {
                    sqlx::query(
                        "SELECT owner, name, cs, change, added FROM cloud_heads \
                         WHERE ns = $1 AND hidden_at IS NULL AND added < $2 \
                         AND (added, owner, name, cs) > ($4, $5, $6, $7) \
                         ORDER BY added, owner, name, cs LIMIT $3",
                    )
                    .bind(ns.to_vec())
                    .bind(cutoff)
                    .bind(BATCH)
                    .bind(*added)
                    .bind(*owner)
                    .bind(name.clone())
                    .bind(cs.clone())
                    .fetch_all(&self.pool)
                    .await?
                }
            };
            let read = rows.len();
            let mut conn = self.pool.acquire().await?;
            let mut to_hide = Vec::new();
            for r in &rows {
                let owner: i64 = r.try_get("owner")?;
                let name: String = r.try_get("name")?;
                let cs: Vec<u8> = r.try_get("cs")?;
                let change: Vec<u8> = r.try_get("change")?;
                let added: i64 = r.try_get("added")?;
                after = Some((added, owner, name.clone(), cs.clone()));
                if open_pr_in_stack(&mut conn, ns, &land_ref, &cs, &change).await? {
                    report.kept += 1;
                } else {
                    to_hide.push((owner, name, cs));
                }
            }
            drop(conn);
            let mut tx = self.pool.begin().await?;
            let mut per_ws: Vec<((i64, String), i64)> = Vec::new();
            for (owner, name, cs) in to_hide {
                let n = sqlx::query(
                    "UPDATE cloud_heads SET hidden_at = $5 \
                     WHERE ns = $1 AND owner = $2 AND name = $3 AND cs = $4 \
                     AND hidden_at IS NULL AND added < $6",
                )
                .bind(ns.to_vec())
                .bind(owner)
                .bind(name.clone())
                .bind(cs)
                .bind(at)
                .bind(cutoff)
                .execute(&mut *tx)
                .await?
                .rows_affected();
                if n > 0 {
                    report.hidden += 1;
                    bump(&mut per_ws, (owner, name));
                }
            }
            touch_workspaces(&mut tx, ns, &per_ws, at, false).await?;
            tx.commit().await?;
            if i64::try_from(read).unwrap_or(i64::MAX) < BATCH {
                break;
            }
        }
        Ok(())
    }
}

fn bump(list: &mut Vec<((i64, String), i64)>, key: (i64, String)) {
    match list.iter_mut().find(|(k, _)| *k == key) {
        Some((_, n)) => *n += 1,
        None => list.push((key, 1)),
    }
}

/// Moves each affected workspace's version (so clients sync) and, when `removed`, its `heads`.
async fn touch_workspaces(
    tx: &mut sqlx::Transaction<'_, sqlx::Any>,
    ns: &[u8],
    list: &[((i64, String), i64)],
    at: i64,
    removed: bool,
) -> Result<(), sqlx::Error> {
    for ((owner, name), n) in list {
        let dec = if removed { *n } else { 0 };
        sqlx::query(
            "UPDATE cloud_ws SET heads = CASE WHEN heads >= $4 THEN heads - $4 ELSE 0 END, \
             version = version + 1, updated = $5 \
             WHERE ns = $1 AND owner = $2 AND name = $3",
        )
        .bind(ns.to_vec())
        .bind(*owner)
        .bind(name.clone())
        .bind(dec)
        .bind(at)
        .execute(&mut **tx)
        .await?;
    }
    Ok(())
}

fn change_id(b: &[u8]) -> Option<ChangeId> {
    <[u8; 16]>::try_from(b).ok().map(ChangeId::from_bytes)
}

/// Whether a change in the stack of the head `cs` (whose change is `change`) has an open PR.
async fn open_pr_in_stack(
    conn: &mut AnyConnection,
    ns: &[u8],
    land_ref: &str,
    cs: &[u8],
    change: &[u8],
) -> Result<bool, sqlx::Error> {
    let ns_id = Ns::from_bytes(ns)
        .ok_or_else(|| sqlx::Error::Decode("namespace is not 16 bytes".into()))?;
    let mut cur_cs = cs.to_vec();
    let mut cur_change = change.to_vec();
    for _ in 0..asc_engine::stack::MAX_STACK {
        let Some(change) = change_id(&cur_change) else {
            // A damaged row: keep the head rather than hide it.
            return Ok(true);
        };
        if index::landed_as(&mut *conn, ns_id, change, land_ref)
            .await
            .map_err(|e| sqlx::Error::Decode(Box::new(e)))?
            .is_some()
        {
            return Ok(false);
        }
        let has_pr =
            sqlx::query("SELECT 1 FROM change_versions WHERE ns = $1 AND change = $2 LIMIT 1")
                .bind(ns.to_vec())
                .bind(cur_change.clone())
                .fetch_optional(&mut *conn)
                .await?
                .is_some();
        if has_pr {
            return Ok(true);
        }
        let node = sqlx::query("SELECT parents FROM cs_graph WHERE ns = $1 AND cs = $2")
            .bind(ns.to_vec())
            .bind(cur_cs.clone())
            .fetch_optional(&mut *conn)
            .await?;
        let Some(node) = node else { return Ok(false) };
        let parents: Vec<u8> = node.try_get("parents")?;
        // A merge or a root ends the stack.
        if parents.len() != 32 {
            return Ok(false);
        }
        let next = sqlx::query("SELECT change FROM changes WHERE ns = $1 AND cs = $2")
            .bind(ns.to_vec())
            .bind(parents.clone())
            .fetch_optional(&mut *conn)
            .await?;
        let Some(next) = next else { return Ok(false) };
        cur_cs = parents;
        cur_change = next.try_get("change")?;
    }
    Ok(false)
}

// ------------------------------------------------------------------------------ GC roots

/// The commit cloud's contribution to garbage collection (SPEC §3.6, §21.2.4).
pub struct CloudRoots {
    pool: AnyPool,
}

impl CloudRoots {
    pub fn new(pool: AnyPool) -> Self {
        Self { pool }
    }
}

fn other(what: &str, e: &dyn std::fmt::Display) -> GcError {
    GcError::Other(format!("{what}: {e}"))
}

#[async_trait]
impl GcRootSource for CloudRoots {
    /// Every `cloud_heads.cs` of the namespace, visible or hidden within the grace period.
    async fn roots(&self, ns: &[u8]) -> Result<Vec<ObjKey>, GcError> {
        let mut rows = sqlx::query("SELECT DISTINCT cs FROM cloud_heads WHERE ns = $1")
            .bind(ns.to_vec())
            .fetch(&self.pool);
        let mut out = Vec::new();
        while let Some(r) = rows
            .try_next()
            .await
            .map_err(|e| other("cloud_heads", &e))?
        {
            let raw: Vec<u8> = r.try_get("cs").map_err(|e| other("cloud_heads", &e))?;
            // A damaged row must not silently drop a root.
            let id = <[u8; 32]>::try_from(raw.as_slice())
                .map(ChangesetId::from_bytes)
                .map_err(|e| other("cloud_heads.cs", &e))?;
            out.push(ObjKey::Changeset(id));
        }
        Ok(out)
    }

    /// The objects of purged drafts.
    async fn purged(&self, ns: &[u8]) -> Result<Vec<ObjKey>, GcError> {
        let rows = sqlx::query("SELECT key FROM gc_purge WHERE ns = $1")
            .bind(ns.to_vec())
            .fetch_all(&self.pool)
            .await
            .map_err(|e| other("gc_purge", &e))?;
        let mut out = Vec::with_capacity(rows.len());
        for r in &rows {
            let key: String = r.try_get("key").map_err(|e| other("gc_purge", &e))?;
            match ObjKey::parse(&key) {
                Some(k) => out.push(k),
                // Not an object path: nothing to sweep, and nothing it could name.
                None => tracing::warn!(event = "gc_purge.bad_key", "skipping an unparsable key"),
            }
        }
        Ok(out)
    }

    /// Forgets the purged keys a finished sweep handled, except rows written after it began.
    async fn purge_swept(&self, ns: &[u8], keys: &[ObjKey], as_of: i64) -> Result<(), GcError> {
        for chunk in keys.chunks(SWEPT_ROWS) {
            let marks = (0..chunk.len())
                .map(|i| format!("${}", i + 3))
                .collect::<Vec<_>>()
                .join(", ");
            let text = format!(
                "DELETE FROM gc_purge WHERE ns = $1 AND purged_at <= $2 AND key IN ({marks})"
            );
            let mut q = sqlx::query(sqlx::AssertSqlSafe(text))
                .bind(ns.to_vec())
                .bind(as_of);
            for k in chunk {
                q = q.bind(k.path());
            }
            q.execute(&self.pool)
                .await
                .map_err(|e| other("gc_purge", &e))?;
        }
        Ok(())
    }
}

#[cfg(test)]
#[path = "expiry_tests.rs"]
mod tests;
