//! The production `ChangeSource` (`RegistryChanges`): which files an update made new.
//!
//! It walks the changesets the new targets reach and stops at what the refs left (`old`) and at
//! the tips of the other refs, so the answer is the size of the update, not of the history.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::HashSet;
use std::sync::Arc;

use asc_core::git::{ObjectKind, blob_oid};
use asc_core::id::{ChangesetId, FileId, RefTarget};
use asc_core::object::{Change, Kind};
use asc_core::path::{RefName, RepoPath};
use asc_core::sig::Signature;
use asc_engine::commit::{NewChangeset, PathPolicy};
use asc_engine::derive::{GitMapRow, GitMapSink};
use asc_rpc::server::RepoName;
use asc_serve::codeintel::queue::{ChangeSource, EFFECT, Written};
use asc_serve::codeintel::source::{RegistryChanges, enqueue_effect};
use asc_serve::migrate;
use asc_serve::outbox::Job;
use asc_serve::registry::{Registry, RepoEntry};
use asc_store::codec::encode_stored;
use asc_store::key::ObjKey;
use asc_store::objects::Objects;
use asc_store::refs::{Actor, Expect, RefTxn, RefUpdate, Refs};
use sqlx::Row;

fn sig() -> Signature {
    Signature::new(b"A".to_vec(), b"a@x".to_vec(), 1, 0).unwrap()
}

async fn commit(e: &RepoEntry, parent: Option<ChangesetId>, files: &[(&str, &str)]) -> ChangesetId {
    let engine = e.backend.engine();
    let base = match parent {
        Some(p) => engine.get_changeset(p).await.unwrap().root,
        None => asc_engine::Engine::<asc_store::bucket::BucketObjects, asc_store::sqlrefs::SqlRefs>::empty_tree_id(),
    };
    let mut changes: Vec<(RepoPath, Change)> = Vec::new();
    for (p, text) in files {
        let b = text.as_bytes();
        let id = FileId::of(b);
        e.objects
            .put(&ObjKey::File(id), encode_stored(b))
            .await
            .unwrap();
        changes.push((
            RepoPath::new(*p).unwrap(),
            Change::write(Kind::File, id, b.len() as u64, blob_oid(b), None).unwrap(),
        ));
    }
    changes.sort_by(|a, b| a.0.cmp(&b.0));
    let root = engine.apply(base, &changes).await.unwrap();
    let cs = engine
        .create_changeset(
            NewChangeset {
                parents: parent.into_iter().collect(),
                author: sig(),
                committer: sig(),
                message: b"m".to_vec(),
                extra: Vec::new(),
                root,
                copies: Vec::new(),
            },
            PathPolicy::Native { windows_safe: true },
        )
        .await
        .unwrap();
    let git = engine.git_commit_of(cs).await.unwrap();
    e.git_map
        .put(&[GitMapRow {
            git,
            kind: ObjectKind::Commit,
            asc: *cs.as_bytes(),
        }])
        .await
        .unwrap();
    cs
}

async fn set_ref(r: &Registry, e: &RepoEntry, name: &str, cs: ChangesetId) {
    let txn = RefTxn {
        updates: vec![RefUpdate {
            name: RefName::new(name.as_bytes().to_vec()).unwrap(),
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
    r.refs().update(e.key, &txn).await.unwrap();
}

fn wire(cs: ChangesetId) -> Vec<u8> {
    RefTarget::Changeset(cs).to_bytes().to_vec()
}

fn file(text: &str) -> Vec<u8> {
    FileId::of(text.as_bytes()).as_bytes().to_vec()
}

fn set(w: &Written) -> HashSet<Vec<u8>> {
    w.files.iter().cloned().collect()
}

struct Rig {
    registry: Arc<Registry>,
    entry: Arc<RepoEntry>,
    source: RegistryChanges,
    c1: ChangesetId,
    c2: ChangesetId,
    c3: ChangesetId,
}

/// `c1 <- c2 <- c3` on `master`, one new file each.
async fn rig() -> Rig {
    let pool = migrate::connect("sqlite::memory:", 1).await.unwrap();
    migrate::migrate(&pool).await.unwrap();
    let registry = Arc::new(Registry::new(pool, "memory:///").unwrap());
    let entry = registry
        .create(&RepoName::parse("o/app").unwrap(), &Default::default())
        .await
        .unwrap();
    let c1 = commit(&entry, None, &[("a.rs", "fn a() {}\n")]).await;
    let c2 = commit(&entry, Some(c1), &[("b.rs", "fn b() {}\n")]).await;
    let c3 = commit(&entry, Some(c2), &[("c.rs", "fn c() {}\n")]).await;
    set_ref(&registry, &entry, "refs/heads/master", c3).await;
    Rig {
        source: RegistryChanges::new(registry.clone()),
        registry,
        entry,
        c1,
        c2,
        c3,
    }
}

#[tokio::test]
async fn a_fast_forward_yields_only_the_files_of_the_new_changesets() {
    let r = rig().await;
    let w = r
        .source
        .written_since(r.entry.key.0, &[wire(r.c3)], &[wire(r.c2)])
        .await
        .unwrap();
    assert_eq!(w.ns, r.entry.ns.as_bytes().to_vec());
    assert_eq!(set(&w), HashSet::from([file("fn c() {}\n")]));

    // Two changesets are new when the ref moved from c1.
    let w = r
        .source
        .written_since(r.entry.key.0, &[wire(r.c3)], &[wire(r.c1)])
        .await
        .unwrap();
    assert_eq!(
        set(&w),
        HashSet::from([file("fn b() {}\n"), file("fn c() {}\n")])
    );
}

#[tokio::test]
async fn a_new_branch_stops_at_the_tips_of_the_other_refs() {
    let r = rig().await;
    // `topic` forks from master's tip c3 and adds d.rs; master is c3.
    let c4 = commit(&r.entry, Some(r.c3), &[("d.rs", "fn d() {}\n")]).await;
    set_ref(&r.registry, &r.entry, "refs/heads/topic", c4).await;
    let w = r
        .source
        .written_since(r.entry.key.0, &[wire(c4)], &[])
        .await
        .unwrap();
    assert_eq!(set(&w), HashSet::from([file("fn d() {}\n")]));

    // With no other ref to stop at, the whole history is new (to this answer).
    let r2 = rig().await;
    let w = r2
        .source
        .written(r2.entry.key.0, &[wire(r2.c3)])
        .await
        .unwrap();
    assert_eq!(
        set(&w),
        HashSet::from([
            file("fn a() {}\n"),
            file("fn b() {}\n"),
            file("fn c() {}\n")
        ])
    );
}

#[tokio::test]
async fn tags_a_missing_repository_and_a_missing_changeset_are_handled() {
    let r = rig().await;
    // A tag target carries no tree to index.
    let tag = RefTarget::Tag(asc_core::id::TagId::from_bytes([7; 32]))
        .to_bytes()
        .to_vec();
    let w = r
        .source
        .written_since(r.entry.key.0, &[tag], &[])
        .await
        .unwrap();
    assert!(w.files.is_empty());
    // A repository that is gone has nothing to index and does not block its outbox rows.
    let w = r.source.written(9_999, &[wire(r.c3)]).await.unwrap();
    assert!(w.files.is_empty() && w.ns.is_empty());
    // A new target the store does not hold is an error, so the row is retried.
    let ghost = ChangesetId::from_bytes([9; 32]);
    let e = r
        .source
        .written_since(r.entry.key.0, &[wire(ghost)], &[])
        .await
        .unwrap_err();
    assert!(e.0.contains("changeset"), "{e}");
}

#[tokio::test]
async fn the_effect_queues_the_new_files_once() {
    let r = rig().await;
    let effect = enqueue_effect(&r.registry);
    assert_eq!(effect.kind(), EFFECT);
    let job = Job {
        id: 1,
        repo_id: r.entry.key.0,
        op_id: vec![1],
        doc: serde_json::json!({
            "effects": ["post_receive", EFFECT],
            "refs": [{
                "name": "refs/heads/master",
                "old": hex(&wire(r.c2)),
                "new": hex(&wire(r.c3)),
            }],
        }),
    };
    for _ in 0..2 {
        // Delivery is at least once; the second run adds nothing.
        effect.run(&job).await.unwrap();
        let n: i64 = sqlx::query("SELECT CAST(COUNT(*) AS BIGINT) AS n FROM ci_jobs WHERE ns = $1")
            .bind(r.entry.ns.as_bytes().to_vec())
            .fetch_one(r.registry.pool())
            .await
            .unwrap()
            .try_get("n")
            .unwrap();
        assert_eq!(n, 1, "only c.rs was new");
    }
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}
