//! Fixtures shared by the overlap and mergeability tests.

use std::sync::Arc;

use asc_core::change::{ChangeId, ChangeMeta, RewriteOp};
use asc_core::id::ChangesetId;
use asc_core::id::RefTarget;
use asc_store::refs::{Actor, Expect, RefTxn, RefUpdate, Refs};
use sqlx::AnyConnection;

use super::index::ChangeIndex;
use super::rpc::Reader;
use super::versions;
use crate::idmode::{Eng, IdCodec};
use crate::ingest::testutil::{commit, registry, repo};
use crate::registry::{Registry, RepoEntry};

pub struct Fx {
    pub reg: Arc<Registry>,
    pub e: Arc<RepoEntry>,
    /// The shared base commit (not a change).
    pub base: ChangesetId,
    /// The heads of changes 1.. in order.
    pub css: Vec<ChangesetId>,
    ids: IdCodec,
    engine: Arc<Eng>,
}

pub fn cid(n: u8) -> ChangeId {
    ChangeId::from_bytes([n; 16])
}

pub async fn set_ref(f: &Fx, name: &str, cs: ChangesetId) {
    let txn = RefTxn {
        updates: vec![RefUpdate {
            name: asc_core::path::RefName::new(name.as_bytes()).unwrap(),
            expect: Expect::Any,
            new: Some(RefTarget::Changeset(cs)),
            expires_at: None,
        }],
        actor: Actor {
            id: "t".into(),
            via: "test".into(),
        },
        reason: "test".into(),
        op_id: uuid::Uuid::new_v4(),
    };
    f.reg.refs().update(f.e.key, &txn).await.unwrap();
}

impl Fx {
    pub fn reader<'a>(&'a self, conn: &'a mut AnyConnection) -> Reader<'a> {
        Reader {
            conn,
            ids: &self.ids,
            engine: &self.engine,
            ns: self.e.ns,
            land_ref: "refs/heads/master".into(),
            heads: Default::default(),
        }
    }

    /// A commit on top of `parent` that writes `files` (a trunk move).
    pub async fn trunk_commit(&self, parent: ChangesetId, files: &[(&str, &[u8])]) -> ChangesetId {
        commit(&self.e, &[parent], files).await
    }

    /// Registers `cs` as version `v` of change `n` (parent change `parent`), with its meta.
    async fn publish(&self, n: u8, cs: ChangesetId, parent: Option<u8>, preds: Vec<ChangesetId>) {
        let idx = ChangeIndex::new(self.reg.clone());
        let mut tx = self.reg.refs().begin().await.unwrap();
        let op = if preds.is_empty() {
            RewriteOp::New
        } else {
            RewriteOp::Amend
        };
        idx.write_meta(&mut tx, self.e.key, &ChangeMeta::new(cs, cid(n), preds, op))
            .await
            .unwrap();
        versions::append(
            &mut tx,
            self.e.ns,
            &versions::NewVersion {
                change: cid(n),
                cs,
                parent_change: parent.map(cid),
                at: 10 + i64::from(n),
                actor: "alice",
                op_id: b"",
                force: false,
            },
        )
        .await
        .unwrap();
        tx.commit().await.unwrap();
    }

    /// Publishes a newer head for change `n` (an amend of its current head).
    pub async fn new_head_for(&self, n: u8) {
        let old = self.css[usize::from(n) - 1];
        let parent = self.engine.get_changeset(old).await.unwrap().parents[0];
        let new = commit(&self.e, &[parent], &[("amended", b"x")]).await;
        let parent_change = match n {
            1 => None,
            2 | 4 => Some(1),
            _ => Some(2),
        };
        self.publish(n, new, parent_change, vec![old]).await;
    }
}

/// Changes 1 <- 2 <- 3 and a fork 2 <- 4 on a base commit; 1 writes "a", 2 "b", 3 "c", 4 "d".
pub async fn stacked() -> Fx {
    fx_with(4).await
}

/// [`stacked`] plus unrelated single-changeset changes up to number `total`.
pub async fn fx_with(total: u8) -> Fx {
    let reg = registry().await;
    let e = repo(&reg).await;
    let base = commit(&e, &[], &[("base", b"base")]).await;
    let c1 = commit(&e, &[base], &[("a", b"a")]).await;
    let c2 = commit(&e, &[c1], &[("b", b"b")]).await;
    let c3 = commit(&e, &[c2], &[("c", b"c")]).await;
    let c4 = commit(&e, &[c2], &[("d", b"d")]).await;
    let mut css = vec![c1, c2, c3, c4];
    for n in 5..=total {
        css.push(commit(&e, &[base], &[(&format!("u{n}"), b"u")]).await);
    }
    let ids = IdCodec::new(asc_rpc::IdMode::Asc, &reg, &e);
    let engine = e.backend.engine().clone();
    let f = Fx {
        reg,
        e,
        base,
        css: css.clone(),
        ids,
        engine,
    };
    for (i, parent) in [(1u8, None), (2, Some(1u8)), (3, Some(2)), (4, Some(2))] {
        f.publish(i, css[usize::from(i) - 1], parent, vec![]).await;
    }
    for n in 5..=total {
        f.publish(n, css[usize::from(n) - 1], None, vec![]).await;
    }
    f
}
