//! Fixtures shared by the B10 tests: a repository with a chain of five indexed drafts.

use std::sync::Arc;

use asc_core::change::{ChangeId, ChangeMeta, RewriteOp};
use asc_core::id::{ChangesetId, RefTarget};
use asc_core::path::RefName;
use asc_store::refs::{Actor, Expect, RefTxn, RefUpdate, Refs};
use uuid::Uuid;

use crate::changes::index;
use crate::graph_sql::{self, NewNode};
use crate::ingest::testutil::{commit, registry, repo};
use crate::registry::{Registry, RepoEntry};

pub(crate) struct Fx {
    pub reg: Arc<Registry>,
    pub e: Arc<RepoEntry>,
    /// `css[i]` has parent `css[i - 1]`, change `cid(i + 1)` and generation `i + 1`.
    pub css: Vec<ChangesetId>,
}

pub(crate) fn cid(n: u8) -> ChangeId {
    ChangeId::from_bytes([n; 16])
}

/// A repository with five chained drafts, each with a `changes` row and a `cs_graph` row, and a
/// file of its own (`f` = `[i]`).
pub(crate) async fn fx() -> Fx {
    let reg = registry().await;
    let e = repo(&reg).await;
    let idx = index::ChangeIndex::new(reg.clone());
    let mut css: Vec<ChangesetId> = Vec::new();
    for i in 1..=5u8 {
        let parents: Vec<ChangesetId> = css.last().copied().into_iter().collect();
        let cs = commit(&e, &parents, &[("d/f", &[i, i, i])]).await;
        let mut tx = reg.refs().begin().await.unwrap();
        idx.write_meta(
            &mut tx,
            e.key,
            &ChangeMeta::new(cs, cid(i), vec![], RewriteOp::New),
        )
        .await
        .unwrap();
        graph_sql::insert_nodes(
            &mut tx,
            e.ns,
            &[NewNode {
                id: cs,
                generation: u32::from(i),
                parents: &parents,
            }],
        )
        .await
        .unwrap();
        tx.commit().await.unwrap();
        css.push(cs);
    }
    Fx { reg, e, css }
}

/// Adds a head of `owner`'s workspace `ws` (created on demand) and counts it.
pub(crate) async fn add_head(
    f: &Fx,
    owner: i64,
    ws: &str,
    cs: ChangesetId,
    added: i64,
    hidden_at: Option<i64>,
) {
    let ns = f.e.ns.as_bytes().to_vec();
    // A changeset with no `changes` row (a solo commit in a test) gets an unrelated change.
    let change: Vec<u8> = sqlx::query_scalar("SELECT change FROM changes WHERE ns = $1 AND cs = $2")
        .bind(ns.clone())
        .bind(cs.as_bytes().to_vec())
        .fetch_optional(f.reg.pool())
        .await
        .unwrap()
        .unwrap_or_else(|| vec![0xee; 16]);
    sqlx::query(
        "INSERT INTO cloud_ws (ns, owner, name, version, updated, heads) VALUES ($1, $2, $3, 1, 0, 0) \
         ON CONFLICT (ns, owner, name) DO NOTHING",
    )
    .bind(ns.clone())
    .bind(owner)
    .bind(ws)
    .execute(f.reg.pool())
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO cloud_heads (ns, owner, name, cs, change, labels, added, hidden_at) \
         VALUES ($1, $2, $3, $4, $5, '[]', $6, $7)",
    )
    .bind(ns.clone())
    .bind(owner)
    .bind(ws)
    .bind(cs.as_bytes().to_vec())
    .bind(change)
    .bind(added)
    .bind(hidden_at)
    .execute(f.reg.pool())
    .await
    .unwrap();
    sqlx::query("UPDATE cloud_ws SET heads = heads + 1 WHERE ns = $1 AND owner = $2 AND name = $3")
        .bind(ns)
        .bind(owner)
        .bind(ws)
        .execute(f.reg.pool())
        .await
        .unwrap();
}

/// A `change_versions` row: the change was submitted.
pub(crate) async fn submit(f: &Fx, change: ChangeId, cs: ChangesetId) {
    sqlx::query(
        "INSERT INTO change_versions (ns, change, v, cs, parent_change, at, actor, op_id) \
         VALUES ($1, $2, 1, $3, NULL, 0, 'a', $4)",
    )
    .bind(f.e.ns.as_bytes().to_vec())
    .bind(change.as_bytes().to_vec())
    .bind(cs.as_bytes().to_vec())
    .bind(vec![0u8; 16])
    .execute(f.reg.pool())
    .await
    .unwrap();
}

/// Puts `cs` on the mainline of `refs/heads/master` at `pos`.
pub(crate) async fn land(f: &Fx, cs: ChangesetId, pos: i64) {
    sqlx::query("INSERT INTO mainline (ns, ref, pos, cs) VALUES ($1, 'refs/heads/master', $2, $3)")
        .bind(f.e.ns.as_bytes().to_vec())
        .bind(pos)
        .bind(cs.as_bytes().to_vec())
        .execute(f.reg.pool())
        .await
        .unwrap();
}

pub(crate) async fn set_ref(f: &Fx, name: &str, to: Option<ChangesetId>) {
    let rn = RefName::new(name.as_bytes().to_vec()).unwrap();
    let cur = f.reg.refs().get(f.e.key, &rn).await.unwrap();
    let txn = RefTxn {
        updates: vec![RefUpdate {
            name: rn,
            expect: cur.map_or(Expect::Absent, |c| Expect::Value(c.target)),
            new: to.map(RefTarget::Changeset),
            expires_at: None,
        }],
        actor: Actor {
            id: "t".into(),
            via: "test".into(),
        },
        reason: "test".into(),
        op_id: Uuid::new_v4(),
    };
    f.reg.refs().update(f.e.key, &txn).await.unwrap();
}

pub(crate) async fn count(f: &Fx, sql: &str) -> i64 {
    sqlx::query_scalar(sqlx::AssertSqlSafe(sql.to_owned()))
        .fetch_one(f.reg.pool())
        .await
        .unwrap()
}
