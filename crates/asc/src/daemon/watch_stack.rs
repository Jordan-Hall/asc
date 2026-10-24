//! `Watch KIND_STACK` for the daemon (PLAN C49, SPEC §20.6): a push event when a change is
//! rewritten or restacked, so an IDE refreshes its stack view without polling `Stacks`.
//!
//! The source is the local change index (`changes`, SPEC §20.3): every rewrite the stack verbs
//! record adds a row, and a row's `rowid` is the cursor. A watcher is already authorised for the
//! workspace (`plugin_api::mounted`), and only rows of that workspace's repository are read. The
//! rows read per poll are capped; a larger burst is one `RESYNC` rather than an unbounded stream.
//! A creation (`New`) or an import adds a draft the `HEAD` and `REF` events already report, so
//! those two ops send no event.

use std::collections::HashSet;

use bytes::Bytes;
use asc_core::change::RewriteOp;
use asc_engine::change::rewrite_op_from_i64;
use asc_state::state::StateDb;
use asc_store::refs::RepoKey;
use sqlx::Row as _;

/// Most `changes` rows one poll reads. A burst past this sends one `RESYNC`.
pub(super) const MAX_STACK_ROWS: i64 = 256;

/// What one poll of the change index found.
pub(super) struct Polled {
    /// The cursor to continue from.
    pub next: i64,
    /// More rows than [`MAX_STACK_ROWS`] arrived: the watcher must re-read everything.
    pub overflow: bool,
    /// One entry per rewritten change, oldest first: the change id and the op's name.
    pub events: Vec<(Bytes, &'static str)>,
}

/// The newest cursor of `repo` (`0` for an empty index).
pub(super) async fn max_rowid(db: &StateDb, repo: RepoKey) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar::<_, i64>("SELECT COALESCE(MAX(rowid), 0) FROM changes WHERE repo_id = ?")
        .bind(repo.0)
        .fetch_one(db.pool())
        .await
}

fn op_name(op: RewriteOp) -> Option<&'static str> {
    match op {
        RewriteOp::New | RewriteOp::Import => None,
        RewriteOp::Amend => Some("amend"),
        RewriteOp::Rebase => Some("rebase"),
        RewriteOp::Absorb => Some("absorb"),
        RewriteOp::Split => Some("split"),
        RewriteOp::Fold => Some("fold"),
        RewriteOp::Squash => Some("squash"),
        RewriteOp::Land => Some("land"),
        RewriteOp::Restack => Some("restack"),
    }
}

/// The rewrites recorded for `repo` after cursor `after`.
pub(super) async fn poll(db: &StateDb, repo: RepoKey, after: i64) -> Result<Polled, sqlx::Error> {
    let rows = sqlx::query(
        "SELECT rowid AS rid, change, op FROM changes WHERE repo_id = ? AND rowid > ? \
         ORDER BY rowid LIMIT ?",
    )
    .bind(repo.0)
    .bind(after)
    .bind(MAX_STACK_ROWS + 1)
    .fetch_all(db.pool())
    .await?;
    if rows.len() as i64 > MAX_STACK_ROWS {
        return Ok(Polled {
            next: max_rowid(db, repo).await?,
            overflow: true,
            events: Vec::new(),
        });
    }
    let mut next = after;
    let mut seen = HashSet::new();
    let mut events = Vec::new();
    for r in &rows {
        next = r.try_get::<i64, _>("rid")?;
        let change: Vec<u8> = r.try_get("change")?;
        // A damaged row (no such op, a short id) is skipped; it can only hide an event.
        let Some(name) = rewrite_op_from_i64(r.try_get("op")?).and_then(op_name) else {
            continue;
        };
        if change.len() == 16 && seen.insert(change.clone()) {
            events.push((Bytes::from(change), name));
        }
    }
    Ok(Polled {
        next,
        overflow: false,
        events,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use asc_core::change::{ChangeId, MetaId};
    use asc_core::id::ChangesetId;
    use asc_engine::change::{ChangeRow, ChangeRows as _};
    use asc_state::changes::StateChanges;
    use asc_state::home::{AscHome, RepoId};
    use asc_state::state::repos::{self, NewRepo, RemoteKind};

    async fn repo(db: &StateDb, name: &str) -> RepoKey {
        repos::insert(
            db,
            &NewRepo {
                id: RepoId::new(),
                name: name.into(),
                ns: vec![1],
                remote_url: None,
                remote_kind: RemoteKind::Server,
                default_branch: "master".into(),
                windows_safe: true,
            },
        )
        .await
        .unwrap()
    }

    async fn rig() -> (tempfile::TempDir, StateDb, RepoKey, StateChanges) {
        let dir = tempfile::tempdir().unwrap();
        let home = AscHome::open(dir.path().join("home")).unwrap();
        let db = StateDb::open(&home).await.unwrap();
        let key = repo(&db, "r").await;
        let rows = StateChanges::new(db.clone(), key);
        (dir, db, key, rows)
    }

    async fn put(rows: &StateChanges, n: u8, change: u8, op: RewriteOp) {
        let row = ChangeRow {
            cs: ChangesetId::from_bytes([n; 32]),
            change: ChangeId::from_bytes([change; 16]),
            meta: MetaId::from_bytes([n; 32]),
            op,
        };
        rows.insert(&row, &[]).await.unwrap();
    }

    #[tokio::test]
    async fn rewrites_are_events_and_creations_are_not() {
        let (_d, db, key, rows) = rig().await;
        let start = max_rowid(&db, key).await.unwrap();
        put(&rows, 1, 1, RewriteOp::New).await;
        put(&rows, 2, 1, RewriteOp::Amend).await;
        put(&rows, 3, 2, RewriteOp::Restack).await;
        put(&rows, 4, 1, RewriteOp::Rebase).await;
        let got = poll(&db, key, start).await.unwrap();
        assert!(!got.overflow);
        let names: Vec<_> = got.events.iter().map(|e| (e.0[0], e.1)).collect();
        assert_eq!(names, vec![(1, "amend"), (2, "restack")]);
        assert_eq!(poll(&db, key, got.next).await.unwrap().events.len(), 0);
    }

    #[tokio::test]
    async fn another_repository_is_never_read() {
        let (_d, db, _key, rows) = rig().await;
        put(&rows, 1, 1, RewriteOp::Amend).await;
        let elsewhere = repo(&db, "other").await;
        let other = poll(&db, elsewhere, 0).await.unwrap();
        assert!(other.events.is_empty());
        assert_eq!(other.next, 0);
    }

    #[tokio::test]
    async fn a_burst_past_the_cap_is_one_resync() {
        let (_d, db, key, rows) = rig().await;
        for i in 0..=MAX_STACK_ROWS {
            let n = u8::try_from(i % 200).unwrap();
            let row = ChangeRow {
                cs: ChangesetId::of(&i.to_be_bytes()),
                change: ChangeId::from_bytes([1; 16]),
                meta: MetaId::from_bytes([n; 32]),
                op: RewriteOp::Amend,
            };
            rows.insert(&row, &[]).await.unwrap();
        }
        let got = poll(&db, key, 0).await.unwrap();
        assert!(got.overflow && got.events.is_empty());
        assert_eq!(got.next, max_rowid(&db, key).await.unwrap());
    }
}
