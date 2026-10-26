//! `changes` and `change_preds`: the local change index (SPEC §20.3).

use asc_core::change::{ChangeId, MetaId};
use asc_core::id::ChangesetId;
use asc_engine::change::{
    self, ChangeRow, ChangeRows, MAX_PREDS, MAX_ROWS, rewrite_op_from_i64, rewrite_op_to_i64,
};
use asc_store::refs::RepoKey;
use async_trait::async_trait;
use sqlx::Row;

use crate::state::{Error, Result, StateDb};

/// [`ChangeRows`] over `state.db` for one repository.
#[derive(Clone, Debug)]
pub struct StateChanges {
    db: StateDb,
    repo: RepoKey,
}

impl StateChanges {
    pub fn new(db: StateDb, repo: RepoKey) -> Self {
        Self { db, repo }
    }
}

fn fixed<const N: usize>(b: Vec<u8>, what: &'static str) -> Result<[u8; N]> {
    b.try_into().map_err(|_| Error::Corrupt(what))
}

fn row_of(r: &sqlx::any::AnyRow) -> Result<ChangeRow> {
    let op: i64 = r.try_get("op")?;
    Ok(ChangeRow {
        cs: ChangesetId::from_bytes(fixed(r.try_get("cs")?, "changes.cs")?),
        change: ChangeId::from_bytes(fixed(r.try_get("change")?, "changes.change")?),
        meta: MetaId::from_bytes(fixed(r.try_get("meta")?, "changes.meta")?),
        op: rewrite_op_from_i64(op).ok_or(Error::Corrupt("changes.op"))?,
    })
}

fn limit(n: usize) -> i64 {
    n.min(MAX_ROWS) as i64
}

fn err(e: impl Into<Error>) -> change::Error {
    change::Error::Rows(e.into().to_string())
}

impl StateChanges {
    async fn insert_inner(&self, row: &ChangeRow, preds: &[ChangesetId]) -> Result<bool> {
        let mut tx = self.db.begin().await?;
        let done = sqlx::query(
            "INSERT OR IGNORE INTO changes (repo_id, cs, change, meta, op) VALUES (?, ?, ?, ?, ?)",
        )
        .bind(self.repo.0)
        .bind(row.cs.as_bytes().to_vec())
        .bind(row.change.as_bytes().to_vec())
        .bind(row.meta.as_bytes().to_vec())
        .bind(rewrite_op_to_i64(row.op))
        .execute(&mut *tx)
        .await?;
        if done.rows_affected() == 0 {
            return Ok(false);
        }
        for p in preds {
            sqlx::query("INSERT OR IGNORE INTO change_preds (repo_id, cs, pred) VALUES (?, ?, ?)")
                .bind(self.repo.0)
                .bind(row.cs.as_bytes().to_vec())
                .bind(p.as_bytes().to_vec())
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        Ok(true)
    }

    async fn ids(&self, sql: &'static str, key: ChangesetId, n: i64) -> Result<Vec<ChangesetId>> {
        sqlx::query(sql)
            .bind(self.repo.0)
            .bind(key.as_bytes().to_vec())
            .bind(n)
            .fetch_all(self.db.pool())
            .await?
            .iter()
            .map(|r| {
                Ok(ChangesetId::from_bytes(fixed(
                    r.try_get(0)?,
                    "change_preds",
                )?))
            })
            .collect()
    }
}

/// Most frozen changes one repository may hold (a named limit; `freeze` fails closed past it).
pub const MAX_FROZEN: usize = 100_000;

impl StateChanges {
    /// True when `change` is frozen (SPEC 20.3).
    pub async fn is_frozen(&self, change: ChangeId) -> Result<bool> {
        Ok(
            sqlx::query("SELECT 1 FROM stack_frozen WHERE repo_id = ? AND change = ?")
                .bind(self.repo.0)
                .bind(change.as_bytes().to_vec())
                .fetch_optional(self.db.pool())
                .await?
                .is_some(),
        )
    }

    /// Every frozen change, at most [`MAX_FROZEN`].
    pub async fn frozen_set(&self) -> Result<std::collections::HashSet<ChangeId>> {
        sqlx::query("SELECT change FROM stack_frozen WHERE repo_id = ? LIMIT ?")
            .bind(self.repo.0)
            .bind(MAX_FROZEN as i64)
            .fetch_all(self.db.pool())
            .await?
            .iter()
            .map(|r| {
                Ok(ChangeId::from_bytes(fixed(
                    r.try_get(0)?,
                    "stack_frozen.change",
                )?))
            })
            .collect()
    }

    /// Freezes `changes` in one transaction; returns how many were not frozen yet. Fails when
    /// the table would pass [`MAX_FROZEN`].
    pub async fn freeze(&self, changes: &[ChangeId]) -> Result<usize> {
        let mut tx = self.db.begin().await?;
        let mut added = 0;
        for c in changes {
            let done =
                sqlx::query("INSERT OR IGNORE INTO stack_frozen (repo_id, change) VALUES (?, ?)")
                    .bind(self.repo.0)
                    .bind(c.as_bytes().to_vec())
                    .execute(&mut *tx)
                    .await?;
            added += usize::try_from(done.rows_affected()).unwrap_or(0);
        }
        let held: i64 = sqlx::query("SELECT COUNT(*) FROM stack_frozen WHERE repo_id = ?")
            .bind(self.repo.0)
            .fetch_one(&mut *tx)
            .await?
            .try_get(0)?;
        if usize::try_from(held).unwrap_or(usize::MAX) > MAX_FROZEN {
            return Err(Error::Corrupt("stack_frozen (over the limit)"));
        }
        tx.commit().await?;
        Ok(added)
    }

    /// Unfreezes `change`; true when it was frozen.
    pub async fn unfreeze(&self, change: ChangeId) -> Result<bool> {
        let done = sqlx::query("DELETE FROM stack_frozen WHERE repo_id = ? AND change = ?")
            .bind(self.repo.0)
            .bind(change.as_bytes().to_vec())
            .execute(self.db.pool())
            .await?;
        Ok(done.rows_affected() > 0)
    }
}

#[async_trait]
impl ChangeRows for StateChanges {
    async fn insert(&self, row: &ChangeRow, preds: &[ChangesetId]) -> change::Result<bool> {
        if preds.len() > MAX_PREDS {
            return Err(change::Error::TooManyPreds(preds.len()));
        }
        self.insert_inner(row, preds).await.map_err(err)
    }

    async fn get(&self, cs: ChangesetId) -> change::Result<Option<ChangeRow>> {
        let row =
            sqlx::query("SELECT cs, change, meta, op FROM changes WHERE repo_id = ? AND cs = ?")
                .bind(self.repo.0)
                .bind(cs.as_bytes().to_vec())
                .fetch_optional(self.db.pool())
                .await
                .map_err(err)?;
        row.as_ref().map(row_of).transpose().map_err(err)
    }

    async fn by_change(&self, change: ChangeId, n: usize) -> change::Result<Vec<ChangeRow>> {
        let rows = sqlx::query(
            "SELECT cs, change, meta, op FROM changes WHERE repo_id = ? AND change = ? \
             ORDER BY cs LIMIT ?",
        )
        .bind(self.repo.0)
        .bind(change.as_bytes().to_vec())
        .bind(limit(n))
        .fetch_all(self.db.pool())
        .await
        .map_err(err)?;
        rows.iter().map(row_of).collect::<Result<_>>().map_err(err)
    }

    async fn preds(&self, cs: ChangesetId) -> change::Result<Vec<ChangesetId>> {
        self.ids(
            "SELECT pred FROM change_preds WHERE repo_id = ? AND cs = ? ORDER BY pred LIMIT ?",
            cs,
            MAX_PREDS as i64,
        )
        .await
        .map_err(err)
    }

    async fn successors(&self, pred: ChangesetId, n: usize) -> change::Result<Vec<ChangesetId>> {
        self.ids(
            "SELECT cs FROM change_preds WHERE repo_id = ? AND pred = ? ORDER BY cs LIMIT ?",
            pred,
            limit(n),
        )
        .await
        .map_err(err)
    }
}

#[cfg(test)]
mod tests {
    use asc_core::change::RewriteOp;
    use asc_engine::change::{OpId, Recorder, RewriteSink};
    use uuid::Uuid;

    use super::*;
    use crate::home::{AscHome, RepoId};
    use crate::state::repos::{self, NewRepo, RemoteKind};

    fn cs(n: u8) -> ChangesetId {
        ChangesetId::of(&[n])
    }

    async fn open(dir: &tempfile::TempDir) -> (StateDb, RepoKey) {
        let home = AscHome::open(dir.path().join("home")).unwrap();
        let db = StateDb::open(&home).await.unwrap();
        let repo = repos::insert(
            &db,
            &NewRepo {
                id: RepoId::new(),
                name: "r".into(),
                ns: vec![1],
                remote_url: None,
                remote_kind: RemoteKind::Server,
                default_branch: "master".into(),
                windows_safe: true,
            },
        )
        .await
        .unwrap();
        (db, repo)
    }

    #[tokio::test]
    async fn rows_round_trip_and_first_wins() {
        let dir = tempfile::tempdir().unwrap();
        let (db, repo) = open(&dir).await;
        let rows = StateChanges::new(db, repo);
        let row = ChangeRow {
            cs: cs(1),
            change: ChangeId::from_bytes([7; 16]),
            meta: MetaId::from_bytes([9; 32]),
            op: RewriteOp::Amend,
        };
        assert!(rows.insert(&row, &[cs(3), cs(2)]).await.unwrap());
        let other = ChangeRow {
            change: ChangeId::from_bytes([8; 16]),
            ..row
        };
        assert!(!rows.insert(&other, &[cs(5)]).await.unwrap());
        assert_eq!(rows.get(cs(1)).await.unwrap(), Some(row));
        assert_eq!(rows.get(cs(2)).await.unwrap(), None);
        assert_eq!(rows.preds(cs(1)).await.unwrap().len(), 2);
        assert!(!rows.preds(cs(1)).await.unwrap().contains(&cs(5)));
        assert_eq!(rows.successors(cs(2), 10).await.unwrap(), vec![cs(1)]);
        assert_eq!(rows.by_change(row.change, 10).await.unwrap(), vec![row]);
        assert!(rows.by_change(other.change, 10).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn rows_are_scoped_to_the_repo_and_limited() {
        let dir = tempfile::tempdir().unwrap();
        let (db, repo) = open(&dir).await;
        let change = ChangeId::from_bytes([1; 16]);
        let rows = StateChanges::new(db.clone(), repo);
        for i in 0..3 {
            let row = ChangeRow {
                cs: cs(i),
                change,
                meta: MetaId::from_bytes([i; 32]),
                op: RewriteOp::Rebase,
            };
            rows.insert(&row, &[]).await.unwrap();
        }
        assert_eq!(rows.by_change(change, 2).await.unwrap().len(), 2);
        let elsewhere = StateChanges::new(db, RepoKey(repo.0 + 1));
        assert_eq!(elsewhere.get(cs(0)).await.unwrap(), None);
    }

    #[tokio::test]
    async fn recorder_persists_through_state_db() {
        let dir = tempfile::tempdir().unwrap();
        let (db, repo) = open(&dir).await;
        let rec = Recorder::new(StateChanges::new(db.clone(), repo));
        let op = OpId(Uuid::nil());
        let a = rec.record(cs(1), &[], RewriteOp::New, op).await.unwrap();
        let b = rec
            .record(cs(2), &[cs(1)], RewriteOp::Amend, op)
            .await
            .unwrap();
        assert_eq!(a.change, b.change);
        let reread = StateChanges::new(db, repo);
        let row = reread.get(cs(2)).await.unwrap().unwrap();
        assert_eq!(row.meta, b.id());
        assert_eq!(reread.preds(cs(2)).await.unwrap(), vec![cs(1)]);
    }

    #[tokio::test]
    async fn freezing_is_per_change_idempotent_and_scoped_to_the_repo() {
        let dir = tempfile::tempdir().unwrap();
        let (db, repo) = open(&dir).await;
        let rows = StateChanges::new(db.clone(), repo);
        let (a, b) = (ChangeId::from_bytes([1; 16]), ChangeId::from_bytes([2; 16]));
        assert!(!rows.is_frozen(a).await.unwrap());
        assert_eq!(rows.freeze(&[a, b]).await.unwrap(), 2);
        assert_eq!(rows.freeze(&[a]).await.unwrap(), 0);
        assert!(rows.is_frozen(a).await.unwrap());
        assert_eq!(rows.frozen_set().await.unwrap().len(), 2);
        let elsewhere = StateChanges::new(db, RepoKey(repo.0 + 1));
        assert!(!elsewhere.is_frozen(a).await.unwrap());
        assert!(rows.unfreeze(a).await.unwrap());
        assert!(!rows.unfreeze(a).await.unwrap());
        assert!(!rows.is_frozen(a).await.unwrap());
        assert!(rows.is_frozen(b).await.unwrap());
    }
}
