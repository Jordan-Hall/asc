//! The local operation log (SPEC §20.3): one bounded, redacted row per `op_id`.

use asc_core::id::ChangesetId;
use asc_store::refs::RepoKey;
use sqlx::Row;
use sqlx::any::AnyRow;
use uuid::Uuid;

use crate::home::WorkspaceId;
use crate::state::{Error, Result, StateDb, Tx, fixed, now};

mod redact;

pub use redact::redact_argv;

/// Default `asc.opLog.maxDays`.
pub const DEFAULT_MAX_DAYS: u32 = 90;
/// Default `asc.opLog.maxOps`.
pub const DEFAULT_MAX_OPS: u32 = 1_000;
/// Largest page [`list`] returns.
pub const MAX_LIST: u32 = 10_000;

/// The id shared by one command's `ref_log` rows and its op-log row.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct OpId(pub Uuid);

impl OpId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for OpId {
    fn default() -> Self {
        Self::new()
    }
}

/// Position of an op in the log; increases with every recorded op.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Seq(pub i64);

/// `asc.opLog.maxDays` and `asc.opLog.maxOps`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    pub max_days: u32,
    pub max_ops: u32,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_days: DEFAULT_MAX_DAYS,
            max_ops: DEFAULT_MAX_OPS,
        }
    }
}

/// An op to record. `argv` is redacted on the way in; callers pass the raw arguments.
#[derive(Clone, Debug)]
pub struct NewOp {
    pub op_id: OpId,
    pub ws: WorkspaceId,
    pub argv: Vec<String>,
    pub kind: String,
    pub head_before: Option<ChangesetId>,
    pub head_after: Option<ChangesetId>,
    pub wc_snapshot: Option<ChangesetId>,
    pub undoes: Option<Seq>,
}

/// A stored op.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Op {
    pub seq: Seq,
    pub op_id: OpId,
    pub ws: WorkspaceId,
    pub at: i64,
    pub argv: String,
    pub kind: String,
    pub head_before: Option<ChangesetId>,
    pub head_after: Option<ChangesetId>,
    pub wc_snapshot: Option<ChangesetId>,
    pub undoes: Option<Seq>,
}

const COLS: &str =
    "seq, op_id, ws_id, at, argv, kind, head_before, head_after, wc_snapshot, undoes_seq";

/// Records `op` inside the caller's transaction, then prunes to `limits`. Returns its sequence.
pub async fn record_tx(tx: &mut Tx, repo: RepoKey, op: &NewOp, limits: Limits) -> Result<Seq> {
    let at = now();
    let seq: i64 = sqlx::query_scalar(
        "INSERT INTO op_log (repo_id, op_id, ws_id, at, argv, kind, head_before, head_after, \
         wc_snapshot, undoes_seq) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?) RETURNING seq",
    )
    .bind(repo.0)
    .bind(op.op_id.0.as_bytes().to_vec())
    .bind(op.ws.to_string())
    .bind(at)
    .bind(redact_argv(&op.argv))
    .bind(op.kind.clone())
    .bind(op.head_before.map(|c| c.as_bytes().to_vec()))
    .bind(op.head_after.map(|c| c.as_bytes().to_vec()))
    .bind(op.wc_snapshot.map(|c| c.as_bytes().to_vec()))
    .bind(op.undoes.map(|s| s.0))
    .fetch_one(&mut **tx)
    .await?;
    prune_tx(tx, repo, limits, at).await?;
    Ok(Seq(seq))
}

/// Records `op` in its own transaction.
pub async fn record(db: &StateDb, repo: RepoKey, op: &NewOp, limits: Limits) -> Result<Seq> {
    let mut tx = db.begin().await?;
    let seq = record_tx(&mut tx, repo, op, limits).await?;
    tx.commit().await?;
    Ok(seq)
}

/// Deletes ops older than `max_days` (relative to `at`) and ops beyond the newest `max_ops`.
/// Returns the number removed. `max_ops` is at least 1 so the op just recorded survives.
pub async fn prune_tx(tx: &mut Tx, repo: RepoKey, limits: Limits, at: i64) -> Result<u64> {
    let cutoff = at.saturating_sub(i64::from(limits.max_days).saturating_mul(86_400));
    let keep = i64::from(limits.max_ops.max(1));
    let by_age = sqlx::query("DELETE FROM op_log WHERE repo_id = ? AND at < ?")
        .bind(repo.0)
        .bind(cutoff)
        .execute(&mut **tx)
        .await?
        .rows_affected();
    let by_count = sqlx::query(
        "DELETE FROM op_log WHERE repo_id = ? AND seq <= COALESCE((SELECT seq FROM op_log \
         WHERE repo_id = ? ORDER BY seq DESC LIMIT 1 OFFSET ?), 0)",
    )
    .bind(repo.0)
    .bind(repo.0)
    .bind(keep)
    .execute(&mut **tx)
    .await?
    .rows_affected();
    Ok(by_age + by_count)
}

/// Prunes in its own transaction.
pub async fn prune(db: &StateDb, repo: RepoKey, limits: Limits) -> Result<u64> {
    let mut tx = db.begin().await?;
    let n = prune_tx(&mut tx, repo, limits, now()).await?;
    tx.commit().await?;
    Ok(n)
}

/// The newest `limit` ops (capped at [`MAX_LIST`]), newest first.
pub async fn list(db: &StateDb, repo: RepoKey, limit: u32) -> Result<Vec<Op>> {
    let rows = sqlx::query(sqlx::AssertSqlSafe(format!(
        "SELECT {COLS} FROM op_log WHERE repo_id = ? ORDER BY seq DESC LIMIT ?"
    )))
    .bind(repo.0)
    .bind(i64::from(limit.min(MAX_LIST)))
    .fetch_all(db.pool())
    .await?;
    rows.iter().map(decode).collect()
}

/// One op by sequence.
pub async fn get(db: &StateDb, repo: RepoKey, seq: Seq) -> Result<Option<Op>> {
    let row = sqlx::query(sqlx::AssertSqlSafe(format!(
        "SELECT {COLS} FROM op_log WHERE repo_id = ? AND seq = ?"
    )))
    .bind(repo.0)
    .bind(seq.0)
    .fetch_optional(db.pool())
    .await?;
    row.as_ref().map(decode).transpose()
}

/// One op by its `op_id`.
pub async fn by_op_id(db: &StateDb, repo: RepoKey, id: OpId) -> Result<Option<Op>> {
    let row = sqlx::query(sqlx::AssertSqlSafe(format!(
        "SELECT {COLS} FROM op_log WHERE repo_id = ? AND op_id = ?"
    )))
    .bind(repo.0)
    .bind(id.0.as_bytes().to_vec())
    .fetch_optional(db.pool())
    .await?;
    row.as_ref().map(decode).transpose()
}

/// Every snapshot still referenced by a kept op (`asc gc` keeps these).
pub async fn live_snapshots(db: &StateDb, repo: RepoKey) -> Result<Vec<ChangesetId>> {
    let rows: Vec<Vec<u8>> = sqlx::query_scalar(
        "SELECT DISTINCT wc_snapshot FROM op_log WHERE repo_id = ? AND wc_snapshot IS NOT NULL",
    )
    .bind(repo.0)
    .fetch_all(db.pool())
    .await?;
    rows.into_iter()
        .map(|b| cs(b, "op_log.wc_snapshot"))
        .collect()
}

fn cs(b: Vec<u8>, what: &'static str) -> Result<ChangesetId> {
    fixed(b, what).map(ChangesetId::from_bytes)
}

fn opt_cs(row: &AnyRow, col: &str, what: &'static str) -> Result<Option<ChangesetId>> {
    row.try_get::<Option<Vec<u8>>, _>(col)?
        .map(|b| cs(b, what))
        .transpose()
}

fn decode(row: &AnyRow) -> Result<Op> {
    let id: Vec<u8> = row.try_get("op_id")?;
    let ws: String = row.try_get("ws_id")?;
    Ok(Op {
        seq: Seq(row.try_get("seq")?),
        op_id: OpId(Uuid::from_slice(&id).map_err(|_| Error::Corrupt("op_log.op_id"))?),
        ws: ws.parse().map_err(|_| Error::Corrupt("op_log.ws_id"))?,
        at: row.try_get("at")?,
        argv: row.try_get("argv")?,
        kind: row.try_get("kind")?,
        head_before: opt_cs(row, "head_before", "op_log.head_before")?,
        head_after: opt_cs(row, "head_after", "op_log.head_after")?,
        wc_snapshot: opt_cs(row, "wc_snapshot", "op_log.wc_snapshot")?,
        undoes: row.try_get::<Option<i64>, _>("undoes_seq")?.map(Seq),
    })
}

#[cfg(test)]
mod tests;
