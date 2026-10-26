//! The indexing workers `asc serve` starts (PLAN I13, SPEC §23.5).
//!
//! A repository gets a commit on `master`; the `codeintel.enqueue` outbox effect (the same
//! `EnqueueEffect` the server registers) queues the commit's files; the supervisor that
//! `serve()` starts through `codeintel::runner::start` leases them, indexes them from the
//! repository's own namespace and commits the segments. Stopping the supervisor commits what
//! it had buffered.

use std::sync::Arc;
use std::time::{Duration, Instant};

use asc_codeintel::index::manifest::ManifestStore;
use asc_codeintel::index::schema::IndexKind;
use asc_core::git::{ObjectKind, blob_oid};
use asc_core::id::{ChangesetId, FileId};
use asc_core::object::{Change, Kind};
use asc_core::path::{RefName, RepoPath};
use asc_core::sig::Signature;
use asc_engine::commit::{NewChangeset, PathPolicy};
use asc_engine::derive::{GitMapRow, GitMapSink};
use asc_rpc::server::RepoName;
use asc_serve::codeintel::queue::{ChangeSource, EFFECT, EnqueueEffect, Written};
use asc_serve::codeintel::runner::{self, Config};
use asc_serve::codeintel::worker::SqlManifest;
use asc_serve::migrate;
use asc_serve::outbox::{Effect, EffectError, Job};
use asc_serve::registry::{Registry, RepoEntry};
use asc_store::codec::encode_stored;
use asc_store::key::ObjKey;
use asc_store::objects::Objects;
use asc_store::refs::{Actor, Expect, RefTxn, RefUpdate, Refs};
use sqlx::{AnyPool, Row};

const LIB: &str = "/// Adds numbers.\npub fn add_numbers(a: u32, b: u32) -> u32 {\n    a + b\n}\n";
const MAIN: &str = "fn main() {\n    println!(\"{}\", add_numbers(1, 2));\n}\n";

fn sig() -> Signature {
    Signature::new(b"A".to_vec(), b"a@x".to_vec(), 1, 0).unwrap()
}

/// Commits `files` as the root changeset of `e`; returns it and the files' ids.
async fn commit(e: &RepoEntry, files: &[(&str, &str)]) -> (ChangesetId, Vec<FileId>) {
    let engine = e.backend.engine();
    let mut changes: Vec<(RepoPath, Change)> = Vec::new();
    let mut ids = Vec::new();
    for (p, text) in files {
        let b = text.as_bytes();
        let id = FileId::of(b);
        ids.push(id);
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
    let base = asc_engine::Engine::<asc_store::bucket::BucketObjects, asc_store::sqlrefs::SqlRefs>::empty_tree_id();
    let root = engine.apply(base, &changes).await.unwrap();
    let cs = engine
        .create_changeset(
            NewChangeset {
                parents: Vec::new(),
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
    (cs, ids)
}

async fn push_master(r: &Registry, e: &RepoEntry, cs: ChangesetId) {
    let txn = RefTxn {
        updates: vec![RefUpdate {
            name: RefName::new(b"refs/heads/master".to_vec()).unwrap(),
            expect: Expect::Any,
            new: Some(asc_core::id::RefTarget::Changeset(cs)),
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

/// What the update's changesets write: the files of the commit.
struct Pushed {
    ns: Vec<u8>,
    files: Vec<Vec<u8>>,
}

#[async_trait::async_trait]
impl ChangeSource for Pushed {
    async fn written(&self, _repo_id: i64, _targets: &[Vec<u8>]) -> Result<Written, EffectError> {
        Ok(Written {
            ns: self.ns.clone(),
            files: self.files.clone(),
        })
    }
}

struct Rig {
    pool: AnyPool,
    registry: Arc<Registry>,
    entry: Arc<RepoEntry>,
    files: Vec<FileId>,
    dir: tempfile::TempDir,
}

/// A repository with one pushed commit, whose files the outbox effect has queued.
async fn rig() -> Rig {
    let pool = migrate::connect("sqlite::memory:", 1).await.unwrap();
    migrate::migrate(&pool).await.unwrap();
    let registry = Arc::new(Registry::new(pool.clone(), "memory:///").unwrap());
    let entry = registry
        .create(&RepoName::parse("o/app").unwrap(), &Default::default())
        .await
        .unwrap();
    let (cs, files) = commit(&entry, &[("src/lib.rs", LIB), ("src/main.rs", MAIN)]).await;
    push_master(&registry, &entry, cs).await;

    let effect = EnqueueEffect::new(
        pool.clone(),
        Arc::new(Pushed {
            ns: entry.ns.as_bytes().to_vec(),
            files: files.iter().map(|f| f.as_bytes().to_vec()).collect(),
        }),
    );
    assert_eq!(effect.kind(), EFFECT);
    let new: String = std::iter::once(1u8)
        .chain(*cs.as_bytes())
        .map(|b| format!("{b:02x}"))
        .collect();
    effect
        .run(&Job {
            id: 1,
            repo_id: entry.key.0,
            op_id: vec![1],
            doc: serde_json::json!({
                "effects": [EFFECT],
                "refs": [{ "name": "refs/heads/master", "new": new }],
            }),
        })
        .await
        .unwrap();
    Rig {
        pool,
        registry,
        entry,
        files,
        dir: tempfile::tempdir().unwrap(),
    }
}

fn config(r: &Rig) -> Config {
    Config {
        workers: 2,
        heap: 16 << 20,
        work_dir: r.dir.path().join("ci-work"),
        holder: "test-node".into(),
    }
}

async fn count(pool: &AnyPool, sql: &'static str, ns: &[u8]) -> i64 {
    sqlx::query(sql)
        .bind(ns.to_vec())
        .fetch_one(pool)
        .await
        .unwrap()
        .try_get::<i64, _>("n")
        .unwrap()
}

async fn jobs(r: &Rig) -> i64 {
    count(
        &r.pool,
        "SELECT CAST(COUNT(*) AS BIGINT) AS n FROM ci_jobs WHERE ns = $1",
        r.entry.ns.as_bytes(),
    )
    .await
}

async fn leased(r: &Rig) -> i64 {
    count(
        &r.pool,
        "SELECT CAST(COUNT(*) AS BIGINT) AS n FROM ci_jobs WHERE ns = $1 AND state = 'leased'",
        r.entry.ns.as_bytes(),
    )
    .await
}

async fn docs(r: &Rig) -> i64 {
    count(
        &r.pool,
        "SELECT CAST(COUNT(*) AS BIGINT) AS n FROM ci_docs WHERE ns = $1",
        r.entry.ns.as_bytes(),
    )
    .await
}

/// Segments the namespace's files index has committed.
async fn segments(r: &Rig) -> usize {
    let nsn: String = r
        .entry
        .ns
        .as_bytes()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    SqlManifest::new(r.pool.clone())
        .load(&nsn, IndexKind::Files)
        .await
        .unwrap()
        .segments
        .len()
}

async fn until<F, Fut>(what: &str, mut f: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let end = Instant::now() + Duration::from_secs(60);
    while !f().await {
        assert!(Instant::now() < end, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test]
async fn a_pushed_commit_is_indexed_by_the_started_workers() {
    let r = rig().await;
    assert_eq!(jobs(&r).await, 2, "the effect queued both files");
    assert_eq!(segments(&r).await, 0);

    let workers = runner::start(r.registry.clone(), config(&r), None).unwrap();
    until("the files to be indexed", || async {
        jobs(&r).await == 0 && segments(&r).await > 0
    })
    .await;
    assert_eq!(docs(&r).await, r.files.len() as i64);

    // Stopping returns promptly and leaves the committed index in place.
    tokio::time::timeout(Duration::from_secs(30), workers.stop())
        .await
        .expect("stop");
    assert_eq!(jobs(&r).await, 0);
    assert!(segments(&r).await > 0);
}

#[tokio::test]
async fn stopping_commits_what_the_workers_buffered() {
    let r = rig().await;
    let workers = runner::start(r.registry.clone(), config(&r), None).unwrap();
    // Staged: leased and analysed, waiting for the 2 s commit rule.
    until("the jobs to be staged", || async { leased(&r).await > 0 }).await;
    workers.stop().await;
    assert_eq!(jobs(&r).await, 0, "no job is left leased");
    assert!(
        segments(&r).await > 0,
        "the staged documents were committed"
    );
    assert_eq!(docs(&r).await, r.files.len() as i64);
}
