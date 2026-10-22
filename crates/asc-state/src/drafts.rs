//! `draft_heads`, `cloud_queue`, `cloud_state` and `cloud_base`: branchless drafts and the
//! commit-cloud queue (SPEC §21.2.2).
//!
//! Maintenance methods take the caller's [`Tx`], so the rows change in the same transaction as the
//! ref and op-log write (C04 sinks). Which heads a `hide` reaches, and which commits became
//! immutable, is decided by the caller (B04); this module only keeps the rows.

use asc_core::change::ChangeId;
use asc_core::id::ChangesetId;
use asc_store::refs::RepoKey;
use sqlx::Row;

use crate::state::{Error, StateDb, Tx};

/// Default `asc.cloud.queueMax` (SPEC §21.3.2).
pub const QUEUE_MAX: usize = 10_000;
/// Most ids one maintenance or read call takes.
pub const MAX_BATCH: usize = 10_000;
/// Longest `last_error` kept, in bytes.
pub const MAX_ERROR_LEN: usize = 1024;

/// Failures from the draft tables.
#[derive(Debug, thiserror::Error)]
pub enum DraftsError {
    #[error(transparent)]
    State(#[from] Error),
    #[error("too many {0}")]
    TooMany(&'static str),
}

impl From<sqlx::Error> for DraftsError {
    fn from(e: sqlx::Error) -> Self {
        Self::State(e.into())
    }
}

pub type Result<T, E = DraftsError> = std::result::Result<T, E>;

/// Whether a new head entered the upload queue.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Enqueued {
    Queued,
    /// The queue holds `queue_max` heads; the head stays local (`queue full`).
    Full,
}

/// `cloud_queue.state`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QueueState {
    Pending,
    Failed,
    TooLarge,
}

impl QueueState {
    fn to_i64(self) -> i64 {
        match self {
            Self::Pending => 0,
            Self::Failed => 1,
            Self::TooLarge => 2,
        }
    }

    fn from_i64(v: i64) -> Result<Self> {
        Ok(match v {
            0 => Self::Pending,
            1 => Self::Failed,
            2 => Self::TooLarge,
            _ => return Err(Error::Corrupt("cloud_queue.state").into()),
        })
    }
}

/// One `draft_heads` row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DraftHead {
    pub cs: ChangesetId,
    pub change: ChangeId,
    pub added: i64,
    pub hidden_at: Option<i64>,
}

/// One `cloud_queue` row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QueueRow {
    pub cs: ChangesetId,
    pub enq: i64,
    pub attempts: i64,
    pub next_at: i64,
    pub state: QueueState,
    pub last_error: Option<String>,
}

/// The `cloud_state` row of a repository.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CloudState {
    pub ws_name: String,
    pub version: i64,
    pub last_sync: i64,
}

fn fixed<const N: usize>(b: Vec<u8>, what: &'static str) -> Result<[u8; N]> {
    b.try_into().map_err(|_| Error::Corrupt(what).into())
}

fn bytes(cs: ChangesetId) -> Vec<u8> {
    cs.as_bytes().to_vec()
}

fn check_batch(n: usize, what: &'static str) -> Result<()> {
    if n > MAX_BATCH {
        return Err(DraftsError::TooMany(what));
    }
    Ok(())
}

fn limit(n: usize) -> i64 {
    n.min(MAX_BATCH) as i64
}

fn head_of(r: &sqlx::any::AnyRow) -> Result<DraftHead> {
    Ok(DraftHead {
        cs: ChangesetId::from_bytes(fixed(r.try_get("cs")?, "draft_heads.cs")?),
        change: ChangeId::from_bytes(fixed(r.try_get("change")?, "draft_heads.change")?),
        added: r.try_get("added")?,
        hidden_at: r.try_get("hidden_at")?,
    })
}

fn queue_of(r: &sqlx::any::AnyRow) -> Result<QueueRow> {
    Ok(QueueRow {
        cs: ChangesetId::from_bytes(fixed(r.try_get("cs")?, "cloud_queue.cs")?),
        enq: r.try_get("enq")?,
        attempts: r.try_get("attempts")?,
        next_at: r.try_get("next_at")?,
        state: QueueState::from_i64(r.try_get("state")?)?,
        last_error: r.try_get("last_error")?,
    })
}

/// The draft tables of one repository.
#[derive(Clone, Copy, Debug)]
pub struct Drafts {
    repo: RepoKey,
}

impl Drafts {
    pub fn new(repo: RepoKey) -> Self {
        Self { repo }
    }

    // ---- maintenance (caller's transaction) ----

    /// New changeset `cs` (commit, merge, cherry-pick, `asc create`): it becomes a head, its parents
    /// stop being heads, and queued parents that have not started are dropped (uploading `cs`
    /// uploads them).
    pub async fn new_changeset(
        &self,
        tx: &mut Tx,
        cs: ChangesetId,
        change: ChangeId,
        parents: &[ChangesetId],
        now: i64,
        queue_max: usize,
    ) -> Result<Enqueued> {
        check_batch(parents.len(), "parents")?;
        for p in parents {
            self.delete_head(tx, *p).await?;
            sqlx::query(
                "DELETE FROM cloud_queue WHERE repo_id = ? AND cs = ? AND state = 0 AND attempts = 0",
            )
            .bind(self.repo.0)
            .bind(bytes(*p))
            .execute(&mut **tx)
            .await?;
        }
        self.insert_head(tx, cs, change, now).await?;
        self.enqueue(tx, cs, now, queue_max).await
    }

    /// Rewrite `old` to `new` (amend, rebase, restack, absorb): `old` leaves the heads and the
    /// queue, `new` is handled as a new changeset.
    pub async fn rewrite(
        &self,
        tx: &mut Tx,
        old: ChangesetId,
        (new, change): (ChangesetId, ChangeId),
        parents: &[ChangesetId],
        now: i64,
        queue_max: usize,
    ) -> Result<Enqueued> {
        self.delete_head(tx, old).await?;
        self.delete_queued(tx, old).await?;
        self.new_changeset(tx, new, change, parents, now, queue_max)
            .await
    }

    /// Commits that became immutable (a fetch moved a trunk over them, or a land): drop their head
    /// and queue rows.
    pub async fn immutable(&self, tx: &mut Tx, css: &[ChangesetId]) -> Result<()> {
        check_batch(css.len(), "commits")?;
        for cs in css {
            self.delete_head(tx, *cs).await?;
            self.delete_queued(tx, *cs).await?;
        }
        Ok(())
    }

    /// `asc hide`: `heads` are the visible heads that have the hidden revision as an ancestor.
    /// `expose` is the revision's parent when it is a draft with no other visible child; it
    /// becomes a visible head (and is queued, since its upload was coalesced into the hidden
    /// stack's).
    pub async fn hide(
        &self,
        tx: &mut Tx,
        heads: &[ChangesetId],
        expose: Option<(ChangesetId, ChangeId)>,
        now: i64,
        queue_max: usize,
    ) -> Result<()> {
        check_batch(heads.len(), "heads")?;
        for cs in heads {
            sqlx::query(
                "UPDATE draft_heads SET hidden_at = ? WHERE repo_id = ? AND cs = ? AND hidden_at IS NULL",
            )
            .bind(now)
            .bind(self.repo.0)
            .bind(bytes(*cs))
            .execute(&mut **tx)
            .await?;
            self.delete_queued(tx, *cs).await?;
        }
        if let Some((cs, change)) = expose {
            self.insert_head(tx, cs, change, now).await?;
            self.enqueue(tx, cs, now, queue_max).await?;
        }
        Ok(())
    }

    /// `asc unhide`: clear `hidden_at`, or insert the id as a head, and enqueue it.
    pub async fn unhide(
        &self,
        tx: &mut Tx,
        cs: ChangesetId,
        change: ChangeId,
        now: i64,
        queue_max: usize,
    ) -> Result<Enqueued> {
        let done =
            sqlx::query("UPDATE draft_heads SET hidden_at = NULL WHERE repo_id = ? AND cs = ?")
                .bind(self.repo.0)
                .bind(bytes(cs))
                .execute(&mut **tx)
                .await?;
        if done.rows_affected() == 0 {
            self.insert_head(tx, cs, change, now).await?;
        }
        self.enqueue(tx, cs, now, queue_max).await
    }

    async fn delete_head(&self, tx: &mut Tx, cs: ChangesetId) -> Result<()> {
        sqlx::query("DELETE FROM draft_heads WHERE repo_id = ? AND cs = ?")
            .bind(self.repo.0)
            .bind(bytes(cs))
            .execute(&mut **tx)
            .await?;
        Ok(())
    }

    async fn delete_queued(&self, tx: &mut Tx, cs: ChangesetId) -> Result<()> {
        sqlx::query("DELETE FROM cloud_queue WHERE repo_id = ? AND cs = ?")
            .bind(self.repo.0)
            .bind(bytes(cs))
            .execute(&mut **tx)
            .await?;
        Ok(())
    }

    async fn insert_head(
        &self,
        tx: &mut Tx,
        cs: ChangesetId,
        change: ChangeId,
        now: i64,
    ) -> Result<()> {
        sqlx::query(
            "INSERT OR IGNORE INTO draft_heads (repo_id, cs, change, added, hidden_at) VALUES (?, ?, ?, ?, NULL)",
        )
        .bind(self.repo.0)
        .bind(bytes(cs))
        .bind(change.as_bytes().to_vec())
        .bind(now)
        .execute(&mut **tx)
        .await?;
        Ok(())
    }

    /// Upserts a pending row, unless the queue is full and `cs` is not already in it.
    async fn enqueue(
        &self,
        tx: &mut Tx,
        cs: ChangesetId,
        now: i64,
        queue_max: usize,
    ) -> Result<Enqueued> {
        let queued: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM cloud_queue WHERE repo_id = ? AND cs = ?")
                .bind(self.repo.0)
                .bind(bytes(cs))
                .fetch_one(&mut **tx)
                .await?;
        if queued == 0 {
            let len: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM cloud_queue WHERE repo_id = ?")
                .bind(self.repo.0)
                .fetch_one(&mut **tx)
                .await?;
            if len >= queue_max as i64 {
                return Ok(Enqueued::Full);
            }
        }
        sqlx::query(
            "INSERT INTO cloud_queue (repo_id, cs, enq, attempts, next_at, state, last_error) \
             VALUES (?, ?, ?, 0, 0, 0, NULL) \
             ON CONFLICT (repo_id, cs) DO UPDATE SET enq = excluded.enq, attempts = 0, \
             next_at = 0, state = 0, last_error = NULL",
        )
        .bind(self.repo.0)
        .bind(bytes(cs))
        .bind(now)
        .execute(&mut **tx)
        .await?;
        Ok(Enqueued::Queued)
    }

    // ---- reads and uploader bookkeeping ----

    /// Visible heads (`hidden_at IS NULL`), oldest first, at most `n`.
    pub async fn visible(&self, db: &StateDb, n: usize) -> Result<Vec<DraftHead>> {
        let rows = sqlx::query(
            "SELECT cs, change, added, hidden_at FROM draft_heads \
             WHERE repo_id = ? AND hidden_at IS NULL ORDER BY added, cs LIMIT ?",
        )
        .bind(self.repo.0)
        .bind(limit(n))
        .fetch_all(db.pool())
        .await?;
        rows.iter().map(head_of).collect()
    }

    /// Hidden heads, most recently hidden first, at most `n`.
    pub async fn hidden(&self, db: &StateDb, n: usize) -> Result<Vec<DraftHead>> {
        let rows = sqlx::query(
            "SELECT cs, change, added, hidden_at FROM draft_heads \
             WHERE repo_id = ? AND hidden_at IS NOT NULL ORDER BY hidden_at DESC, cs LIMIT ?",
        )
        .bind(self.repo.0)
        .bind(limit(n))
        .fetch_all(db.pool())
        .await?;
        rows.iter().map(head_of).collect()
    }

    /// Pending queue rows due at `now`, oldest first, at most `n`.
    pub async fn due(&self, db: &StateDb, now: i64, n: usize) -> Result<Vec<QueueRow>> {
        let rows = sqlx::query(
            "SELECT cs, enq, attempts, next_at, state, last_error FROM cloud_queue \
             WHERE repo_id = ? AND state = 0 AND next_at <= ? ORDER BY enq, cs LIMIT ?",
        )
        .bind(self.repo.0)
        .bind(now)
        .bind(limit(n))
        .fetch_all(db.pool())
        .await?;
        rows.iter().map(queue_of).collect()
    }

    /// Number of queue rows (any state).
    pub async fn queue_len(&self, db: &StateDb) -> Result<usize> {
        let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM cloud_queue WHERE repo_id = ?")
            .bind(self.repo.0)
            .fetch_one(db.pool())
            .await?;
        Ok(usize::try_from(n).unwrap_or(0))
    }

    /// An upload attempt failed: count it and set the row's state, retry time and error.
    pub async fn queue_failed(
        &self,
        db: &StateDb,
        cs: ChangesetId,
        state: QueueState,
        next_at: i64,
        error: &str,
    ) -> Result<()> {
        let mut end = error.len().min(MAX_ERROR_LEN);
        while !error.is_char_boundary(end) {
            end -= 1;
        }
        sqlx::query(
            "UPDATE cloud_queue SET attempts = attempts + 1, state = ?, next_at = ?, last_error = ? \
             WHERE repo_id = ? AND cs = ?",
        )
        .bind(state.to_i64())
        .bind(next_at)
        .bind(&error[..end])
        .bind(self.repo.0)
        .bind(bytes(cs))
        .execute(db.pool())
        .await?;
        Ok(())
    }

    /// The upload finished: drop the row.
    pub async fn queue_done(&self, db: &StateDb, cs: ChangesetId) -> Result<()> {
        sqlx::query("DELETE FROM cloud_queue WHERE repo_id = ? AND cs = ?")
            .bind(self.repo.0)
            .bind(bytes(cs))
            .execute(db.pool())
            .await?;
        Ok(())
    }

    pub async fn cloud_state(&self, db: &StateDb) -> Result<Option<CloudState>> {
        let row =
            sqlx::query("SELECT ws_name, version, last_sync FROM cloud_state WHERE repo_id = ?")
                .bind(self.repo.0)
                .fetch_optional(db.pool())
                .await?;
        row.map(|r| {
            Ok(CloudState {
                ws_name: r.try_get("ws_name")?,
                version: r.try_get("version")?,
                last_sync: r.try_get("last_sync")?,
            })
        })
        .transpose()
    }

    pub async fn set_cloud_state(&self, db: &StateDb, s: &CloudState) -> Result<()> {
        sqlx::query(
            "INSERT INTO cloud_state (repo_id, ws_name, version, last_sync) VALUES (?, ?, ?, ?) \
             ON CONFLICT (repo_id) DO UPDATE SET ws_name = excluded.ws_name, \
             version = excluded.version, last_sync = excluded.last_sync",
        )
        .bind(self.repo.0)
        .bind(&s.ws_name)
        .bind(s.version)
        .bind(s.last_sync)
        .execute(db.pool())
        .await?;
        Ok(())
    }

    /// The head set at the last successful sync.
    pub async fn base(&self, db: &StateDb) -> Result<Vec<ChangesetId>> {
        let rows = sqlx::query("SELECT cs FROM cloud_base WHERE repo_id = ? ORDER BY cs LIMIT ?")
            .bind(self.repo.0)
            .bind(limit(MAX_BATCH))
            .fetch_all(db.pool())
            .await?;
        rows.iter()
            .map(|r| {
                Ok(ChangesetId::from_bytes(fixed(
                    r.try_get("cs")?,
                    "cloud_base.cs",
                )?))
            })
            .collect()
    }

    /// Replaces the base with `heads`, atomically.
    pub async fn set_base(&self, db: &StateDb, heads: &[ChangesetId]) -> Result<()> {
        check_batch(heads.len(), "base heads")?;
        let mut tx = db.begin().await?;
        sqlx::query("DELETE FROM cloud_base WHERE repo_id = ?")
            .bind(self.repo.0)
            .execute(&mut *tx)
            .await?;
        for cs in heads {
            sqlx::query("INSERT OR IGNORE INTO cloud_base (repo_id, cs) VALUES (?, ?)")
                .bind(self.repo.0)
                .bind(bytes(*cs))
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests;
