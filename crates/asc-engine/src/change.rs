//! Local change index contract and rewrite recording (SPEC §20.2, §20.3).
//!
//! [`ChangeRows`] is the storage seam: `asc-state` implements it over `state.db`, tests use
//! [`MemRows`]. [`Recorder`] applies the §20.2 rules table to decide whether a rewrite keeps
//! or mints a change id.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Mutex;

use asc_core::change::{ChangeId, ChangeMeta, MetaId, RewriteOp};
use asc_core::id::ChangesetId;
use async_trait::async_trait;
use uuid::Uuid;

/// Most predecessors one changeset may record.
pub const MAX_PREDS: usize = 1024;

/// Most rows one lookup returns.
pub const MAX_ROWS: usize = 4096;

/// Failures from recording or reading the change index.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("change index: {0}")]
    Rows(String),
    #[error("{0} predecessors exceed the limit of {MAX_PREDS}")]
    TooManyPreds(usize),
    #[error("a {0:?} rewrite needs at least one predecessor")]
    NoPreds(RewriteOp),
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Identifies one command invocation (the newtype of `RefTxn.op_id`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct OpId(pub Uuid);

/// One `changes` row: the change and meta a changeset carries.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ChangeRow {
    pub cs: ChangesetId,
    pub change: ChangeId,
    pub meta: MetaId,
    pub op: RewriteOp,
}

/// The local `changes` and `change_preds` tables.
#[async_trait]
pub trait ChangeRows: Send + Sync {
    /// Indexes a changeset. The first row for a changeset wins: returns `false`, changing
    /// nothing, when `row.cs` is already indexed. `preds` is stored only with a new row.
    async fn insert(&self, row: &ChangeRow, preds: &[ChangesetId]) -> Result<bool>;
    async fn get(&self, cs: ChangesetId) -> Result<Option<ChangeRow>>;
    /// Rows of one change, at most `limit` (clamped to [`MAX_ROWS`]).
    async fn by_change(&self, change: ChangeId, limit: usize) -> Result<Vec<ChangeRow>>;
    /// Predecessors of a changeset, sorted.
    async fn preds(&self, cs: ChangesetId) -> Result<Vec<ChangesetId>>;
    /// Changesets that list `pred` as a predecessor, at most `limit` (clamped), sorted.
    async fn successors(&self, pred: ChangesetId, limit: usize) -> Result<Vec<ChangesetId>>;
}

/// Receives one call per changeset a command writes (SPEC §20.3 sink contract).
#[async_trait]
pub trait RewriteSink: Send + Sync {
    /// `preds` is in fold order, oldest first, for `Fold` and `Squash`.
    async fn record(
        &self,
        new: ChangesetId,
        preds: &[ChangesetId],
        kind: RewriteOp,
        op: OpId,
    ) -> Result<ChangeMeta>;
}

/// Reads the discriminant stored in `changes.op`.
pub fn rewrite_op_from_i64(v: i64) -> Option<RewriteOp> {
    Some(match v {
        0 => RewriteOp::New,
        1 => RewriteOp::Amend,
        2 => RewriteOp::Rebase,
        3 => RewriteOp::Absorb,
        4 => RewriteOp::Split,
        5 => RewriteOp::Fold,
        6 => RewriteOp::Squash,
        7 => RewriteOp::Land,
        8 => RewriteOp::Restack,
        9 => RewriteOp::Import,
        _ => return None,
    })
}

/// The stored discriminant of `op`.
pub fn rewrite_op_to_i64(op: RewriteOp) -> i64 {
    op as i64
}

/// A fresh random change id.
pub fn mint_change_id() -> ChangeId {
    ChangeId::from_bytes(*Uuid::new_v4().as_bytes())
}

/// Applies the §20.2 rules table over a [`ChangeRows`].
pub struct Recorder<R: ChangeRows> {
    rows: R,
    mint: fn() -> ChangeId,
}

impl<R: ChangeRows> Recorder<R> {
    pub fn new(rows: R) -> Self {
        Self::with_mint(rows, mint_change_id)
    }

    /// Replaces the id source (tests).
    pub fn with_mint(rows: R, mint: fn() -> ChangeId) -> Self {
        Self { rows, mint }
    }

    /// Records one piece of an `asc split`. The piece that keeps the original message
    /// (`keeps_id`) keeps the change id; every other piece gets a new one.
    pub async fn record_split(
        &self,
        new: ChangesetId,
        old: ChangesetId,
        keeps_id: bool,
    ) -> Result<ChangeMeta> {
        let change = if keeps_id {
            self.inherited(&[old]).await?
        } else {
            None
        };
        self.write(new, &[old], RewriteOp::Split, change).await
    }

    /// The change of the first predecessor, if it has a row.
    async fn inherited(&self, preds: &[ChangesetId]) -> Result<Option<ChangeId>> {
        match preds.first() {
            Some(p) => Ok(self.rows.get(*p).await?.map(|r| r.change)),
            None => Ok(None),
        }
    }

    async fn write(
        &self,
        new: ChangesetId,
        preds: &[ChangesetId],
        kind: RewriteOp,
        change: Option<ChangeId>,
    ) -> Result<ChangeMeta> {
        if preds.len() > MAX_PREDS {
            return Err(Error::TooManyPreds(preds.len()));
        }
        if let Some(existing) = self.rows.get(new).await? {
            // The first meta indexed for a changeset wins (SPEC §20.2).
            let preds = self.rows.preds(new).await?;
            return Ok(ChangeMeta::new(new, existing.change, preds, existing.op));
        }
        let meta = ChangeMeta::new(new, change.unwrap_or_else(self.mint), preds.to_vec(), kind);
        let row = ChangeRow {
            cs: new,
            change: meta.change,
            meta: meta.id(),
            op: kind,
        };
        self.rows.insert(&row, &meta.preds).await?;
        Ok(meta)
    }
}

#[async_trait]
impl<R: ChangeRows> RewriteSink for Recorder<R> {
    async fn record(
        &self,
        new: ChangesetId,
        preds: &[ChangesetId],
        kind: RewriteOp,
        _op: OpId, // the op log row is written by the caller's sink in the same transaction
    ) -> Result<ChangeMeta> {
        let change = match kind {
            // A copy is a new change; an import's id arrives in its own meta (SPEC §20.2);
            // a split piece that keeps the id goes through `record_split`.
            RewriteOp::New | RewriteOp::Import | RewriteOp::Split => None,
            RewriteOp::Amend
            | RewriteOp::Rebase
            | RewriteOp::Absorb
            | RewriteOp::Land
            | RewriteOp::Restack
            | RewriteOp::Fold
            | RewriteOp::Squash => {
                if preds.is_empty() {
                    return Err(Error::NoPreds(kind));
                }
                self.inherited(preds).await?
            }
        };
        self.write(new, preds, kind, change).await
    }
}

/// An in-memory [`ChangeRows`].
#[derive(Default)]
pub struct MemRows {
    inner: Mutex<MemInner>,
}

#[derive(Default)]
struct MemInner {
    rows: BTreeMap<ChangesetId, ChangeRow>,
    preds: BTreeSet<(ChangesetId, ChangesetId)>,
}

impl MemRows {
    fn lock(&self) -> std::sync::MutexGuard<'_, MemInner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }
}

#[async_trait]
impl ChangeRows for MemRows {
    async fn insert(&self, row: &ChangeRow, preds: &[ChangesetId]) -> Result<bool> {
        let mut g = self.lock();
        if g.rows.contains_key(&row.cs) {
            return Ok(false);
        }
        g.rows.insert(row.cs, *row);
        for p in preds {
            g.preds.insert((row.cs, *p));
        }
        Ok(true)
    }
    async fn get(&self, cs: ChangesetId) -> Result<Option<ChangeRow>> {
        Ok(self.lock().rows.get(&cs).copied())
    }
    async fn by_change(&self, change: ChangeId, limit: usize) -> Result<Vec<ChangeRow>> {
        Ok(self
            .lock()
            .rows
            .values()
            .filter(|r| r.change == change)
            .take(limit.min(MAX_ROWS))
            .copied()
            .collect())
    }
    async fn preds(&self, cs: ChangesetId) -> Result<Vec<ChangesetId>> {
        Ok(self
            .lock()
            .preds
            .iter()
            .filter(|(c, _)| *c == cs)
            .map(|(_, p)| *p)
            .collect())
    }
    async fn successors(&self, pred: ChangesetId, limit: usize) -> Result<Vec<ChangesetId>> {
        Ok(self
            .lock()
            .preds
            .iter()
            .filter(|(_, p)| *p == pred)
            .map(|(c, _)| *c)
            .take(limit.min(MAX_ROWS))
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cs(n: u8) -> ChangesetId {
        ChangesetId::of(&[n])
    }

    fn op() -> OpId {
        OpId(Uuid::nil())
    }

    fn rec() -> Recorder<MemRows> {
        Recorder::new(MemRows::default())
    }

    #[tokio::test]
    async fn new_mints_and_amend_keeps() {
        let r = rec();
        let a = r.record(cs(1), &[], RewriteOp::New, op()).await.unwrap();
        let b = r
            .record(cs(2), &[cs(1)], RewriteOp::Amend, op())
            .await
            .unwrap();
        assert_eq!(a.change, b.change);
        assert_eq!(b.preds, vec![cs(1)]);
        let c = r.record(cs(3), &[], RewriteOp::New, op()).await.unwrap();
        assert_ne!(a.change, c.change);
    }

    #[tokio::test]
    async fn rebase_restack_land_absorb_keep() {
        let r = rec();
        let a = r.record(cs(1), &[], RewriteOp::New, op()).await.unwrap();
        for (i, k) in [
            RewriteOp::Rebase,
            RewriteOp::Restack,
            RewriteOp::Land,
            RewriteOp::Absorb,
        ]
        .into_iter()
        .enumerate()
        {
            let n = r.record(cs(10 + i as u8), &[cs(1)], k, op()).await.unwrap();
            assert_eq!(n.change, a.change);
        }
    }

    #[tokio::test]
    async fn fold_takes_the_earliest_change() {
        let r = rec();
        let a = r.record(cs(1), &[], RewriteOp::New, op()).await.unwrap();
        let b = r.record(cs(2), &[], RewriteOp::New, op()).await.unwrap();
        // Fold order, not id order: cs(2) is earliest here.
        let f = r
            .record(cs(3), &[cs(2), cs(1)], RewriteOp::Fold, op())
            .await
            .unwrap();
        assert_eq!(f.change, b.change);
        assert_ne!(f.change, a.change);
        let mut want = vec![cs(1), cs(2)];
        want.sort();
        assert_eq!(f.preds, want);
        let s = r
            .record(cs(4), &[cs(1), cs(2)], RewriteOp::Squash, op())
            .await
            .unwrap();
        assert_eq!(s.change, a.change);
    }

    #[tokio::test]
    async fn split_keeps_one_piece() {
        let r = rec();
        let a = r.record(cs(1), &[], RewriteOp::New, op()).await.unwrap();
        let keep = r.record_split(cs(2), cs(1), true).await.unwrap();
        let other = r.record_split(cs(3), cs(1), false).await.unwrap();
        assert_eq!(keep.change, a.change);
        assert_ne!(other.change, a.change);
        assert_eq!(other.preds, vec![cs(1)]);
        assert_eq!(other.op, RewriteOp::Split);
    }

    #[tokio::test]
    async fn unknown_pred_mints_and_empty_preds_errors() {
        let r = rec();
        let n = r
            .record(cs(2), &[cs(1)], RewriteOp::Rebase, op())
            .await
            .unwrap();
        assert_eq!(n.preds, vec![cs(1)]);
        assert!(matches!(
            r.record(cs(3), &[], RewriteOp::Amend, op()).await,
            Err(Error::NoPreds(RewriteOp::Amend))
        ));
    }

    #[tokio::test]
    async fn first_meta_wins_and_preds_are_bounded() {
        let r = rec();
        let a = r.record(cs(1), &[], RewriteOp::New, op()).await.unwrap();
        let again = r
            .record(cs(1), &[cs(9)], RewriteOp::Amend, op())
            .await
            .unwrap();
        assert_eq!(a, again);
        let many: Vec<_> = (0..=MAX_PREDS as u32)
            .map(|i| ChangesetId::of(&i.to_le_bytes()))
            .collect();
        assert!(matches!(
            r.record(cs(5), &many, RewriteOp::Squash, op()).await,
            Err(Error::TooManyPreds(_))
        ));
    }

    #[test]
    fn op_discriminants_round_trip() {
        for v in 0..=9 {
            assert_eq!(rewrite_op_to_i64(rewrite_op_from_i64(v).unwrap()), v);
        }
        assert!(rewrite_op_from_i64(10).is_none());
    }
}
