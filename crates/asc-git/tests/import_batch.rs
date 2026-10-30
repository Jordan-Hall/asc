//! Import batching: the same bytes and rows as a commit-at-a-time import, in far fewer writes,
//! with objects always durable before the rows that name them.

#[path = "fixtures/mod.rs"]
mod fixtures;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use asc_core::git::ObjectKind;
use asc_core::id::GitOid;
use asc_engine::Engine;
use asc_engine::derive::{self, GitMapRow, GitMapSink};
use asc_git::import::{ImportMap, ImportPolicy, Importer};
use asc_git::odb::Odb;
use asc_store::bucket::BucketObjects;
use asc_store::key::{KeyPrefix, ObjKey};
use asc_store::objects::{self, Objects};
use async_trait::async_trait;
use bytes::Bytes;
use fixtures::{Killer, MemMap, NoRefs, Repo, dump_store, repo_key, small_fixture};
use futures::stream::BoxStream;

/// Counts how a store is written.
struct CountingObjects {
    inner: Arc<BucketObjects>,
    puts: AtomicUsize,
    many: AtomicUsize,
}

#[async_trait]
impl Objects for CountingObjects {
    async fn get(&self, key: &ObjKey) -> objects::Result<Option<Bytes>> {
        self.inner.get(key).await
    }
    async fn get_stored(&self, key: &ObjKey) -> objects::Result<Option<Bytes>> {
        self.inner.get_stored(key).await
    }
    async fn get_range(
        &self,
        key: &ObjKey,
        r: std::ops::Range<u64>,
    ) -> objects::Result<Option<Bytes>> {
        self.inner.get_range(key, r).await
    }
    async fn put(&self, key: &ObjKey, stored: Bytes) -> objects::Result<()> {
        self.puts.fetch_add(1, Ordering::SeqCst);
        self.inner.put(key, stored).await
    }
    async fn put_many(&self, items: Vec<(ObjKey, Bytes)>) -> objects::Result<()> {
        self.many.fetch_add(1, Ordering::SeqCst);
        self.inner.put_many(items).await
    }
    async fn has_many(&self, keys: &[ObjKey]) -> objects::Result<Vec<bool>> {
        self.inner.has_many(keys).await
    }
    async fn delete(&self, key: &ObjKey) -> objects::Result<()> {
        self.inner.delete(key).await
    }
    fn list(
        &self,
        prefix: KeyPrefix,
    ) -> BoxStream<'_, objects::Result<(ObjKey, std::time::SystemTime)>> {
        self.inner.list(prefix)
    }
}

/// A map that counts calls and can refuse the first one.
struct CountingMap {
    inner: MemMap,
    calls: AtomicUsize,
    rows: AtomicUsize,
    refuse: bool,
}

#[async_trait]
impl GitMapSink for CountingMap {
    async fn put(&self, rows: &[GitMapRow]) -> derive::Result<()> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.rows.fetch_add(rows.len(), Ordering::SeqCst);
        if self.refuse {
            return Err(derive::Error::Sink("refused".into()));
        }
        self.inner.put(rows).await
    }
}

#[async_trait]
impl ImportMap for CountingMap {
    async fn get(&self, git: GitOid, kind: ObjectKind) -> derive::Result<Option<[u8; 32]>> {
        self.inner.get(git, kind).await
    }
}

struct Run {
    store: Arc<BucketObjects>,
    objects: Arc<CountingObjects>,
    map: CountingMap,
}

fn run_env(refuse: bool) -> Run {
    let store = Arc::new(BucketObjects::memory());
    Run {
        objects: Arc::new(CountingObjects {
            inner: store.clone(),
            puts: AtomicUsize::new(0),
            many: AtomicUsize::new(0),
        }),
        store,
        map: CountingMap {
            inner: MemMap::new(Arc::new(Killer::new(usize::MAX))),
            calls: AtomicUsize::new(0),
            rows: AtomicUsize::new(0),
            refuse,
        },
    }
}

async fn import(
    repo: &Repo,
    run: &Run,
    batch_commits: usize,
) -> Result<asc_git::import::ImportReport, asc_git::import::ImportError> {
    let engine = Engine::new(run.objects.clone(), Arc::new(NoRefs), repo_key());
    let odb = Odb::open(repo.path()).expect("open odb");
    let policy = ImportPolicy {
        batch_commits,
        ..ImportPolicy::default()
    };
    let mut importer = Importer::new(&engine, run.objects.clone(), &odb, &run.map, policy);
    importer.import_commits(&repo.tips()).await
}

#[tokio::test]
async fn a_batched_import_stores_the_same_bytes_and_rows_in_fewer_writes() {
    let repo = small_fixture();
    let one = run_env(false);
    let many = run_env(false);
    let r1 = import(&repo, &one, 1).await.unwrap();
    let r2 = import(&repo, &many, 500).await.unwrap();

    assert_eq!(
        (r1.commits, r1.blobs, r1.trees),
        (r2.commits, r2.blobs, r2.trees)
    );
    assert_eq!(r1.commits, 10);
    assert_eq!(
        dump_store(&*one.store).await,
        dump_store(&*many.store).await
    );
    assert_eq!(one.map.inner.dump(), many.map.inner.dump());

    // A commit at a time: map calls scale with the history. Batched: one for the commits and
    // at most one more for the tag.
    assert!(one.map.calls.load(Ordering::SeqCst) >= 10);
    assert!(
        many.map.calls.load(Ordering::SeqCst) <= 2,
        "{}",
        many.map.calls.load(Ordering::SeqCst)
    );
    // Each row is sent once however it was batched.
    assert_eq!(
        many.map.rows.load(Ordering::SeqCst),
        many.map.inner.dump().len()
    );

    // Objects reach the store through put_many, never one put at a time.
    assert_eq!(many.objects.puts.load(Ordering::SeqCst), 0);
    assert!(many.objects.many.load(Ordering::SeqCst) >= 1);
    assert!(many.objects.many.load(Ordering::SeqCst) <= one.objects.many.load(Ordering::SeqCst));
}

#[tokio::test]
async fn a_batch_size_that_splits_the_history_changes_nothing() {
    let repo = small_fixture();
    let whole = run_env(false);
    import(&repo, &whole, 500).await.unwrap();
    for n in [2, 3, 7] {
        let split = run_env(false);
        import(&repo, &split, n).await.unwrap();
        assert_eq!(
            dump_store(&*split.store).await,
            dump_store(&*whole.store).await,
            "n={n}"
        );
        assert_eq!(split.map.inner.dump(), whole.map.inner.dump(), "n={n}");
    }
}

#[tokio::test]
async fn objects_are_durable_before_any_row_names_them_and_a_rerun_converges() {
    let repo = small_fixture();
    let clean = run_env(false);
    import(&repo, &clean, 500).await.unwrap();

    // The map refuses its first write: the objects are stored, no row exists.
    let broken = run_env(true);
    assert!(import(&repo, &broken, 500).await.is_err());
    assert!(broken.map.inner.dump().is_empty());
    assert!(!dump_store(&*broken.store).await.is_empty());

    // A rerun over the same store, with a working map, reaches the clean result.
    let rerun = Run {
        store: broken.store.clone(),
        objects: broken.objects.clone(),
        map: CountingMap {
            inner: MemMap::new(Arc::new(Killer::new(usize::MAX))),
            calls: AtomicUsize::new(0),
            rows: AtomicUsize::new(0),
            refuse: false,
        },
    };
    import(&repo, &rerun, 500).await.unwrap();
    assert_eq!(
        dump_store(&*rerun.store).await,
        dump_store(&*clean.store).await
    );
    assert_eq!(rerun.map.inner.dump(), clean.map.inner.dump());
}

#[tokio::test]
async fn a_second_import_finds_the_first_in_the_map_and_writes_nothing() {
    let repo = small_fixture();
    let run = run_env(false);
    import(&repo, &run, 500).await.unwrap();
    let calls = run.map.calls.load(Ordering::SeqCst);
    let again = import(&repo, &run, 500).await.unwrap();
    assert_eq!((again.commits, again.blobs), (0, 0));
    // Only the annotated tag is recorded again, in the one call after the commits.
    assert_eq!(run.map.calls.load(Ordering::SeqCst), calls + 1);
}

#[tokio::test]
async fn an_import_into_a_pack_enabled_store_writes_packs_and_stores_the_same_objects() {
    use asc_store::pack::{MemPackIndex, PackConfig, PackLayer};

    let repo = small_fixture();
    let plain = run_env(false);
    import(&repo, &plain, 500).await.unwrap();

    let layer = PackLayer::new(
        Arc::new(MemPackIndex::new()),
        PackConfig {
            enabled: true,
            min_batch: 4,
            negative_ttl: std::time::Duration::ZERO,
            ..PackConfig::default()
        },
    );
    let store = Arc::new(BucketObjects::memory().with_packs(Arc::new(layer)));
    let packed = Run {
        objects: Arc::new(CountingObjects {
            inner: store.clone(),
            puts: AtomicUsize::new(0),
            many: AtomicUsize::new(0),
        }),
        store: store.clone(),
        map: CountingMap {
            inner: MemMap::new(Arc::new(Killer::new(usize::MAX))),
            calls: AtomicUsize::new(0),
            rows: AtomicUsize::new(0),
            refuse: false,
        },
    };
    // Small batches, so several packs are written along the way.
    let report = import(&repo, &packed, 3).await.unwrap();
    assert_eq!(report.commits, 10);

    // Every object is there with the same bytes, whether it sits in a pack or loose.
    assert_eq!(
        dump_store(&*packed.store).await,
        dump_store(&*plain.store).await
    );
    assert_eq!(packed.map.inner.dump(), plain.map.inner.dump());
    let status = store.pack_status().await.unwrap();
    assert!(status.live_packs >= 2, "{status:?}");
    assert!(status.packed_objects > status.loose_small, "{status:?}");
    // And a second import of the same history writes nothing new.
    let again = import(&repo, &packed, 3).await.unwrap();
    assert_eq!(again.commits, 0);
    assert_eq!(
        store.pack_status().await.unwrap().live_packs,
        status.live_packs
    );
    assert!(store.pack_verify().await.unwrap().problems.is_empty());
}
