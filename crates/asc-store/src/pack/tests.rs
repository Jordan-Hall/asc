//! Behaviour of packed objects end to end, on an in-memory bucket.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime};

use asc_core::id::{ChangesetId, FileId, TreeId};
use async_trait::async_trait;
use bytes::Bytes;
use futures::TryStreamExt;
use object_store::memory::InMemory;
use object_store::path::Path;
use object_store::{ObjectStore, ObjectStoreExt, PutPayload};

use super::format::{self, PackEntry, PackId};
use super::index::{MemPackIndex, PackDetail, PackIndex, PackInfo, PackLoc, PackState};
use super::layer::PackWriteGate;
use super::{BucketPackIndex, PackConfig, PackLayer};
use crate::bucket::BucketObjects;
use crate::codec::encode_stored;
use crate::gc::{Fence, NoFence};
use crate::key::{KeyClass, KeyPrefix, ObjKey};
use crate::objects::{Error, Objects, Result};

fn cfg() -> PackConfig {
    PackConfig {
        enabled: true,
        min_age: Duration::from_secs(60),
        min_loose: 1,
        min_batch: 4,
        negative_ttl: Duration::ZERO,
        merge_count: 4,
        orphan_age: Duration::from_secs(3600),
        retire_age: Duration::from_secs(3600),
        ..PackConfig::default()
    }
}

struct Env {
    store: Arc<dyn ObjectStore>,
    objects: BucketObjects,
    index: Arc<dyn PackIndex>,
}

fn env_with(index: Arc<dyn PackIndex>, cfg: PackConfig) -> Env {
    let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
    let layer = Arc::new(PackLayer::new(index.clone(), cfg));
    Env {
        objects: BucketObjects::new(store.clone(), Path::from("ns/0001")).with_packs(layer),
        store,
        index,
    }
}

fn env() -> Env {
    env_with(Arc::new(MemPackIndex::new()), cfg())
}

fn obj(n: u32) -> (ObjKey, Bytes) {
    let data = format!("object number {n}");
    (
        ObjKey::File(FileId::of(data.as_bytes())),
        encode_stored(data.as_bytes()),
    )
}

fn many(range: std::ops::Range<u32>) -> Vec<(ObjKey, Bytes)> {
    range.map(obj).collect()
}

fn later() -> SystemTime {
    SystemTime::now() + Duration::from_secs(7200)
}

async fn put_loose(e: &Env, items: &[(ObjKey, Bytes)]) {
    for (k, v) in items {
        e.objects.put(k, v.clone()).await.unwrap();
    }
}

async fn loose_exists(e: &Env, k: &ObjKey) -> bool {
    let p = Path::from(format!("ns/0001/{}", k.path()));
    e.store.head(&p).await.is_ok()
}

async fn all_readable(e: &Env, items: &[(ObjKey, Bytes)]) {
    for (k, v) in items {
        assert_eq!(
            e.objects.get_stored(k).await.unwrap().as_ref(),
            Some(v),
            "{k}"
        );
        assert!(e.objects.get(k).await.unwrap().is_some(), "{k}");
    }
    let keys: Vec<ObjKey> = items.iter().map(|(k, _)| *k).collect();
    assert!(e.objects.has_many(&keys).await.unwrap().iter().all(|&b| b));
}

async fn pack_files(e: &Env) -> Vec<String> {
    let mut v: Vec<String> = e
        .store
        .list(Some(&Path::from("ns/0001/pack")))
        .map_ok(|m| m.location.to_string())
        .try_collect()
        .await
        .unwrap();
    v.sort();
    v
}

// ---------------------------------------------------------------------------- reading

#[tokio::test]
async fn reads_work_from_loose_only_pack_only_and_both() {
    let e = env();
    let loose = many(0..3);
    let packed = many(10..30);
    let both = many(40..44);
    put_loose(&e, &loose).await;
    put_loose(&e, &both).await;
    e.objects.put_many(packed.clone()).await.unwrap();
    // `both` is also packed: write a pack by hand, leaving the loose copies.
    let l = e.objects.pack_layer().unwrap().clone();
    e.objects.write_pack(&l, both.clone()).await.unwrap();

    all_readable(&e, &loose).await;
    all_readable(&e, &packed).await;
    all_readable(&e, &both).await;
    assert!(!loose_exists(&e, &packed[0].0).await, "put_many packed it");
    let absent = obj(999).0;
    assert_eq!(
        e.objects
            .has_many(&[loose[0].0, packed[0].0, absent])
            .await
            .unwrap(),
        [true, true, false]
    );
    assert_eq!(e.objects.get(&absent).await.unwrap(), None);
}

#[tokio::test]
async fn get_range_slices_a_packed_file() {
    let e = env();
    let data = b"0123456789abcdef".to_vec();
    let k = ObjKey::File(FileId::of(&data));
    let mut items = many(0..8);
    items.push((k, encode_stored(&data)));
    e.objects.put_many(items).await.unwrap();
    assert_eq!(
        &e.objects.get_range(&k, 4..8).await.unwrap().unwrap()[..],
        b"4567"
    );
    assert_eq!(
        &e.objects.get_range(&k, 14..99).await.unwrap().unwrap()[..],
        b"ef"
    );
    let tree = ObjKey::Tree(TreeId::of(b"t"));
    assert!(matches!(
        e.objects.get_range(&tree, 0..1).await,
        Err(Error::RangeUnsupported(_))
    ));
}

#[tokio::test]
async fn every_packable_class_round_trips_through_a_pack() {
    let e = env();
    let mut items = vec![
        (ObjKey::Tree(TreeId::of(b"tree")), encode_stored(b"tree")),
        (
            ObjKey::Changeset(ChangesetId::of(b"cs")),
            encode_stored(b"cs"),
        ),
    ];
    items.extend(many(0..6));
    e.objects.put_many(items.clone()).await.unwrap();
    all_readable(&e, &items).await;
    assert_eq!(pack_files(&e).await.len(), 1);
    assert!(!loose_exists(&e, &items[0].0).await);
}

#[tokio::test]
async fn small_batches_big_values_and_a_disabled_writer_stay_loose() {
    // Fewer than min_batch small values.
    let e = env();
    let few = many(0..3);
    e.objects.put_many(few.clone()).await.unwrap();
    assert!(pack_files(&e).await.is_empty());
    all_readable(&e, &few).await;

    // A value over max_object stays loose beside packed neighbours.
    let big_data = vec![7u8; 100 * 1024];
    let mut rng = 1u64;
    let noisy: Vec<u8> = big_data
        .iter()
        .map(|_| {
            rng = rng
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (rng >> 33) as u8
        })
        .collect();
    let big = (ObjKey::File(FileId::of(&noisy)), encode_stored(&noisy));
    assert!(big.1.len() > 64 * 1024);
    let mut items = many(10..20);
    items.push(big.clone());
    e.objects.put_many(items).await.unwrap();
    assert!(loose_exists(&e, &big.0).await);
    assert_eq!(pack_files(&e).await.len(), 1);

    // A writer that is off writes loose.
    let off = env_with(
        Arc::new(MemPackIndex::new()),
        PackConfig {
            enabled: false,
            ..cfg()
        },
    );
    off.objects.put_many(many(0..20)).await.unwrap();
    assert!(pack_files(&off).await.is_empty());
    all_readable(&off, &many(0..20)).await;
}

#[tokio::test]
async fn put_of_a_packed_object_writes_nothing_and_put_many_skips_it() {
    let e = env();
    let items = many(0..10);
    e.objects.put_many(items.clone()).await.unwrap();
    let before = pack_files(&e).await;
    for (k, v) in &items {
        e.objects.put(k, v.clone()).await.unwrap();
        assert!(!loose_exists(&e, k).await);
    }
    e.objects.put_many(items.clone()).await.unwrap();
    assert_eq!(
        pack_files(&e).await,
        before,
        "no second pack for the same objects"
    );
}

#[tokio::test]
async fn a_damaged_packed_copy_yields_to_a_whole_loose_copy_and_put_repairs_it() {
    let e = env();
    let items = many(0..8);
    e.objects.put_many(items.clone()).await.unwrap();
    // Overwrite the pack with the same length of other bytes.
    let file = pack_files(&e).await.remove(0);
    let path = Path::from(file.as_str());
    let good = e.store.get(&path).await.unwrap().bytes().await.unwrap();
    let mut bad = good.to_vec();
    for b in &mut bad[16..] {
        *b ^= 0x5a;
    }
    e.store.put(&path, PutPayload::from(bad)).await.unwrap();
    let (k, v) = items[3].clone();
    assert!(
        e.objects.get(&k).await.is_err(),
        "a damaged copy is not returned"
    );
    // put notices the packed copy is bad, drops its row and writes the loose copy.
    e.objects.put(&k, v.clone()).await.unwrap();
    assert!(loose_exists(&e, &k).await);
    assert_eq!(e.objects.get_stored(&k).await.unwrap().unwrap(), v);
    assert!(e.objects.get(&k).await.unwrap().is_some());
}

#[tokio::test]
async fn a_missing_or_short_pack_falls_back_to_loose_and_otherwise_reports_missing() {
    let e = env();
    let items = many(0..8);
    put_loose(&e, &items).await;
    let l = e.objects.pack_layer().unwrap().clone();
    e.objects.write_pack(&l, items.clone()).await.unwrap();
    let file = pack_files(&e).await.remove(0);
    let path = Path::from(file.as_str());
    let good = e.store.get(&path).await.unwrap().bytes().await.unwrap();

    // Short: the loose copy answers.
    e.store
        .put(&path, PutPayload::from(good.slice(..good.len() / 2)))
        .await
        .unwrap();
    all_readable(&e, &items).await;
    // Missing: same.
    e.store.delete(&path).await.unwrap();
    all_readable(&e, &items).await;

    // Without a loose copy the object is missing, not an error.
    let only = env();
    let packed = many(0..8);
    only.objects.put_many(packed.clone()).await.unwrap();
    let file = pack_files(&only).await.remove(0);
    only.store.delete(&Path::from(file.as_str())).await.unwrap();
    assert_eq!(only.objects.get(&packed[0].0).await.unwrap(), None);
    assert_eq!(only.objects.get_stored(&packed[0].0).await.unwrap(), None);
}

#[tokio::test]
async fn delete_forgets_a_packed_key_and_list_shows_packed_keys() {
    let e = env();
    let items = many(0..8);
    e.objects.put_many(items.clone()).await.unwrap();
    let listed: Vec<ObjKey> = e
        .objects
        .list(KeyPrefix::class(KeyClass::File))
        .map_ok(|(k, _)| k)
        .try_collect()
        .await
        .unwrap();
    assert_eq!(listed.len(), 8);
    assert!(
        e.objects
            .list(KeyPrefix::class(KeyClass::Chunk))
            .try_collect::<Vec<_>>()
            .await
            .unwrap()
            .is_empty()
    );
    e.objects.delete(&items[0].0).await.unwrap();
    assert_eq!(e.objects.get(&items[0].0).await.unwrap(), None);
    assert_eq!(e.objects.has_many(&[items[0].0]).await.unwrap(), [false]);
    all_readable(&e, &items[1..]).await;
    // Deleting twice, and deleting what was never there, is fine.
    e.objects.delete(&items[0].0).await.unwrap();
    e.objects.delete(&obj(500).0).await.unwrap();
}

// ---------------------------------------------------------------------------- the job

#[tokio::test]
async fn the_job_packs_old_loose_objects_and_keeps_young_ones() {
    let e = env();
    let items = many(0..40);
    put_loose(&e, &items).await;
    // Everything is young.
    let rep = e
        .objects
        .pack_run(&NoFence, SystemTime::now(), false)
        .await
        .unwrap();
    assert_eq!(rep.packed, 0);
    assert!(loose_exists(&e, &items[0].0).await);
    // Old enough.
    let rep = e.objects.pack_run(&NoFence, later(), false).await.unwrap();
    assert_eq!(rep.packed, 40);
    assert_eq!(rep.loose_deleted, 40);
    for (k, _) in &items {
        assert!(!loose_exists(&e, k).await);
    }
    all_readable(&e, &items).await;
    // A second run has nothing to do.
    let rep = e.objects.pack_run(&NoFence, later(), false).await.unwrap();
    assert_eq!((rep.packed, rep.packs_written), (0, 0));
}

#[tokio::test]
async fn a_repository_below_min_loose_is_left_alone_unless_forced() {
    let e = env_with(
        Arc::new(MemPackIndex::new()),
        PackConfig {
            min_loose: 100,
            ..cfg()
        },
    );
    let items = many(0..10);
    put_loose(&e, &items).await;
    let rep = e.objects.pack_run(&NoFence, later(), false).await.unwrap();
    assert!(rep.below_threshold);
    assert_eq!(rep.packed, 0);
    let rep = e.objects.pack_run(&NoFence, later(), true).await.unwrap();
    assert_eq!(rep.packed, 10);
    all_readable(&e, &items).await;
}

#[tokio::test]
async fn packs_split_at_the_object_and_size_limits() {
    let e = env_with(
        Arc::new(MemPackIndex::new()),
        PackConfig {
            max_objects: 10,
            ..cfg()
        },
    );
    let items = many(0..25);
    put_loose(&e, &items).await;
    let rep = e.objects.pack_run(&NoFence, later(), false).await.unwrap();
    assert_eq!(rep.packs_written, 3);
    all_readable(&e, &items).await;
}

/// Refuses to commit: the writer died after the pack file was durable.
struct DiesBeforeCommit {
    inner: MemPackIndex,
    armed: AtomicBool,
}

#[async_trait]
impl PackIndex for DiesBeforeCommit {
    async fn any_live(&self) -> Result<bool> {
        self.inner.any_live().await
    }
    async fn locate(&self, k: &[ObjKey]) -> Result<Vec<Option<PackLoc>>> {
        self.inner.locate(k).await
    }
    async fn commit(&self, n: &PackInfo, e: &[PackEntry], r: &[PackId]) -> Result<u64> {
        if self.armed.load(Ordering::SeqCst) {
            return Err(Error::Index("killed".into()));
        }
        self.inner.commit(n, e, r).await
    }
    async fn retire(&self, i: &[PackId], n: i64) -> Result<()> {
        self.inner.retire(i, n).await
    }
    async fn forget(&self, k: &[ObjKey]) -> Result<u64> {
        self.inner.forget(k).await
    }
    async fn packs(&self) -> Result<Vec<PackDetail>> {
        self.inner.packs().await
    }
    async fn rows_of(&self, i: PackId) -> Result<Vec<PackEntry>> {
        self.inner.rows_of(i).await
    }
    async fn remove(&self, i: PackId) -> Result<()> {
        self.inner.remove(i).await
    }
    async fn list(
        &self,
        c: Option<KeyClass>,
        a: Option<(u8, [u8; 32])>,
        l: usize,
    ) -> Result<Vec<(ObjKey, i64)>> {
        self.inner.list(c, a, l).await
    }
    async fn rebuild(&self, p: &[(PackInfo, Vec<PackEntry>)]) -> Result<()> {
        self.inner.rebuild(p).await
    }
}

/// Refuses to let loose copies be deleted: the writer died after the commit.
struct DiesInFence;

#[async_trait]
impl Fence for DiesInFence {
    async fn enter(&self) -> crate::gc::Result<Box<dyn Send>> {
        Err(crate::gc::GcError::Marks("killed".into()))
    }
}

#[tokio::test]
async fn a_crash_before_the_commit_leaves_the_loose_objects_and_an_orphan_that_is_swept() {
    let idx = Arc::new(DiesBeforeCommit {
        inner: MemPackIndex::new(),
        armed: AtomicBool::new(true),
    });
    let e = env_with(idx.clone(), cfg());
    let items = many(0..20);
    put_loose(&e, &items).await;
    assert!(e.objects.pack_run(&NoFence, later(), false).await.is_err());
    // Nothing is lost: every object is still loose and readable; the pack file is orphaned.
    all_readable(&e, &items).await;
    assert_eq!(pack_files(&e).await.len(), 1);
    assert!(!e.index.any_live().await.unwrap());
    // A young orphan is left (its writer may be about to commit); an old one is removed.
    idx.armed.store(false, Ordering::SeqCst);
    let rep = e
        .objects
        .pack_run(
            &NoFence,
            SystemTime::now() + Duration::from_secs(120),
            false,
        )
        .await
        .unwrap();
    assert_eq!(rep.orphans, 0);
    let rep = e.objects.pack_run(&NoFence, later(), false).await.unwrap();
    assert!(rep.orphans >= 1);
    all_readable(&e, &items).await;
}

#[tokio::test]
async fn a_crash_between_the_commit_and_the_loose_delete_leaves_both_copies_readable() {
    let e = env();
    let items = many(0..20);
    put_loose(&e, &items).await;
    assert!(
        e.objects
            .pack_run(&DiesInFence, later(), false)
            .await
            .is_err()
    );
    assert!(loose_exists(&e, &items[0].0).await, "loose copies remain");
    assert!(e.index.any_live().await.unwrap(), "the pack is live");
    all_readable(&e, &items).await;
    // The next run deletes the loose copies without writing the pack again.
    let rep = e.objects.pack_run(&NoFence, later(), false).await.unwrap();
    assert_eq!(rep.loose_deleted, 20);
    assert!(!loose_exists(&e, &items[0].0).await);
    all_readable(&e, &items).await;
}

#[tokio::test]
async fn two_jobs_at_once_lose_nothing() {
    let e = env();
    let items = many(0..60);
    put_loose(&e, &items).await;
    let (a, b) = tokio::join!(
        e.objects.pack_run(&NoFence, later(), false),
        e.objects.pack_run(&NoFence, later(), false)
    );
    a.unwrap();
    b.unwrap();
    all_readable(&e, &items).await;
    // Two packs hold the same objects; one row each, and a later run keeps them readable.
    e.objects.pack_run(&NoFence, later(), false).await.unwrap();
    all_readable(&e, &items).await;
}

#[tokio::test]
async fn small_packs_merge_into_one_and_stay_readable() {
    let e = env();
    let mut all = Vec::new();
    for round in 0..5u32 {
        let items = many(round * 100..round * 100 + 6);
        e.objects.put_many(items.clone()).await.unwrap();
        all.extend(items);
    }
    assert_eq!(pack_files(&e).await.len(), 5);
    let rep = e.objects.pack_run(&NoFence, later(), false).await.unwrap();
    assert_eq!(rep.merged, 5);
    all_readable(&e, &all).await;
    let live: Vec<_> = e
        .index
        .packs()
        .await
        .unwrap()
        .into_iter()
        .filter(|d| d.info.state == PackState::Live)
        .collect();
    assert_eq!(live.len(), 1);
    assert_eq!(live[0].live_objects, 30);
    // The merged-away packs stay for retire_age, then go.
    assert_eq!(pack_files(&e).await.len(), 6);
    let rep = e
        .objects
        .pack_run(
            &NoFence,
            SystemTime::now() + Duration::from_secs(3 * 3600),
            false,
        )
        .await
        .unwrap();
    assert_eq!(rep.deleted, 5);
    assert_eq!(pack_files(&e).await.len(), 1);
    all_readable(&e, &all).await;
}

#[tokio::test]
async fn gc_deletes_hollow_a_pack_and_compaction_keeps_only_live_entries() {
    let e = env();
    let items = many(0..20);
    e.objects.put_many(items.clone()).await.unwrap();
    for (k, _) in &items[..15] {
        e.objects.delete(k).await.unwrap();
    }
    let st = e.objects.pack_status().await.unwrap();
    assert_eq!((st.live_packs, st.packed_objects), (1, 5));
    assert!(st.dead_bytes > 0);
    let rep = e.objects.pack_run(&NoFence, later(), false).await.unwrap();
    assert_eq!(rep.compacted, 1);
    all_readable(&e, &items[15..]).await;
    for (k, _) in &items[..15] {
        assert_eq!(e.objects.get(k).await.unwrap(), None);
    }
    let st = e.objects.pack_status().await.unwrap();
    assert_eq!(
        (st.live_packs, st.retired_packs, st.packed_objects),
        (1, 1, 5)
    );
    assert_eq!(st.dead_bytes, 0);
}

#[tokio::test]
async fn a_pack_with_nothing_live_is_retired_and_then_deleted() {
    let e = env();
    let items = many(0..8);
    e.objects.put_many(items.clone()).await.unwrap();
    for (k, _) in &items {
        e.objects.delete(k).await.unwrap();
    }
    let rep = e.objects.pack_run(&NoFence, later(), false).await.unwrap();
    assert_eq!(rep.emptied, 1);
    let rep = e
        .objects
        .pack_run(
            &NoFence,
            SystemTime::now() + Duration::from_secs(3 * 3600),
            false,
        )
        .await
        .unwrap();
    assert_eq!(rep.deleted, 1);
    assert!(pack_files(&e).await.is_empty());
}

#[tokio::test]
async fn purge_compaction_rewrites_a_pack_with_any_dead_entry() {
    let e = env();
    let items = many(0..20);
    e.objects.put_many(items.clone()).await.unwrap();
    e.objects.delete(&items[0].0).await.unwrap();
    // Above the 50% threshold: the ordinary run leaves it.
    let rep = e.objects.pack_run(&NoFence, later(), false).await.unwrap();
    assert_eq!(rep.compacted, 0);
    // A purge wants the bytes gone now.
    let rep = e.objects.pack_compact_all(&NoFence, later()).await.unwrap();
    assert_eq!(rep.compacted, 1);
    all_readable(&e, &items[1..]).await;
}

#[tokio::test]
async fn a_corrupt_pack_is_not_rewritten_and_is_counted() {
    let e = env();
    let items = many(0..20);
    e.objects.put_many(items.clone()).await.unwrap();
    for (k, _) in &items[..15] {
        e.objects.delete(k).await.unwrap();
    }
    let file = pack_files(&e).await.remove(0);
    let path = Path::from(file.as_str());
    let mut bytes = e
        .store
        .get(&path)
        .await
        .unwrap()
        .bytes()
        .await
        .unwrap()
        .to_vec();
    bytes[20] ^= 1;
    e.store.put(&path, PutPayload::from(bytes)).await.unwrap();
    let rep = e.objects.pack_run(&NoFence, later(), false).await.unwrap();
    assert_eq!((rep.compacted, rep.damaged), (0, 1));
    let v = e.objects.pack_verify().await.unwrap();
    assert_eq!(v.packs, 1);
    assert!(!v.problems.is_empty());
}

#[tokio::test]
async fn verify_passes_a_good_repository_and_reindex_recovers_a_lost_index() {
    let e = env();
    let a = many(0..10);
    let b = many(100..110);
    e.objects.put_many(a.clone()).await.unwrap();
    e.objects.put_many(b.clone()).await.unwrap();
    let v = e.objects.pack_verify().await.unwrap();
    assert!(v.problems.is_empty(), "{:?}", v.problems);
    assert_eq!((v.packs, v.objects), (2, 20));

    // Lose the index entirely; the objects read as missing until it is rebuilt.
    let fresh = env_with(Arc::new(MemPackIndex::new()), cfg());
    let lost = BucketObjects::new(e.store.clone(), Path::from("ns/0001"))
        .with_packs(fresh.objects.pack_layer().unwrap().clone());
    assert_eq!(lost.get(&a[0].0).await.unwrap(), None);
    let rep = lost.pack_reindex().await.unwrap();
    assert_eq!((rep.packs, rep.objects), (2, 20));
    for (k, v) in a.iter().chain(&b) {
        assert_eq!(lost.get_stored(k).await.unwrap().as_ref(), Some(v));
    }
    let v = lost.pack_verify().await.unwrap();
    assert!(v.problems.is_empty(), "{:?}", v.problems);
}

struct Gate(AtomicBool);

#[async_trait]
impl PackWriteGate for Gate {
    async fn before_write(&self) -> Result<()> {
        if self.0.load(Ordering::SeqCst) {
            Ok(())
        } else {
            Err(Error::Index("read-only".into()))
        }
    }
}

#[tokio::test]
async fn the_write_gate_can_refuse_a_pack_and_the_objects_are_not_lost() {
    let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
    let gate = Arc::new(Gate(AtomicBool::new(false)));
    let layer =
        Arc::new(PackLayer::new(Arc::new(MemPackIndex::new()), cfg()).with_gate(gate.clone()));
    let objects = BucketObjects::new(store, Path::default()).with_packs(layer);
    let items = many(0..10);
    assert!(objects.put_many(items.clone()).await.is_err());
    gate.0.store(true, Ordering::SeqCst);
    objects.put_many(items.clone()).await.unwrap();
    for (k, v) in &items {
        assert_eq!(objects.get_stored(k).await.unwrap().as_ref(), Some(v));
    }
}

// ----------------------------------------------------------------------- bucket mode

#[tokio::test]
async fn bucket_mode_packs_with_idx_files_and_a_manifest() {
    let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
    let root = Path::from("ns/0001");
    let mk = || {
        let index = Arc::new(BucketPackIndex::new(store.clone(), root.clone()));
        let layer = Arc::new(PackLayer::new(index, cfg()));
        BucketObjects::new(store.clone(), root.clone()).with_packs(layer)
    };
    let (a, b) = (mk(), mk());
    let first = many(0..10);
    a.put_many(first.clone()).await.unwrap();
    // A second server with its own caches finds the first one's objects.
    for (k, v) in &first {
        assert_eq!(b.get_stored(k).await.unwrap().as_ref(), Some(v));
    }
    let files: Vec<String> = store
        .list(Some(&Path::from("ns/0001/pack")))
        .map_ok(|m| m.location.to_string())
        .try_collect()
        .await
        .unwrap();
    assert!(files.iter().any(|f| f.ends_with(".idx")));
    assert!(files.iter().any(|f| f.ends_with("MANIFEST")));

    // Deleting through one is seen by a server that starts afterwards.
    a.delete(&first[0].0).await.unwrap();
    assert_eq!(mk().get(&first[0].0).await.unwrap(), None);
    assert!(mk().get(&first[1].0).await.unwrap().is_some());

    // The job works the same way: pack loose, merge, compact.
    let loose = many(50..70);
    for (k, v) in &loose {
        a.put(k, v.clone()).await.unwrap();
    }
    let rep = a.pack_run(&NoFence, later(), false).await.unwrap();
    assert_eq!(rep.packed, 20);
    for (k, v) in &loose {
        assert_eq!(mk().get_stored(k).await.unwrap().as_ref(), Some(v));
    }
    for (k, _) in &loose[..15] {
        a.delete(k).await.unwrap();
    }
    let rep = a.pack_run(&NoFence, later(), false).await.unwrap();
    assert!(rep.compacted + rep.merged > 0);
    for (k, v) in &loose[15..] {
        assert_eq!(mk().get_stored(k).await.unwrap().as_ref(), Some(v));
    }
    let v = a.pack_verify().await.unwrap();
    assert!(v.problems.is_empty(), "{:?}", v.problems);
}

#[tokio::test]
async fn the_pack_format_constants_match_what_the_layer_reads() {
    // An index row's offset points at the stored bytes, after the entry header.
    let (k, v) = obj(1);
    let built = format::build(PackId::random(), [(k, v.clone())]).unwrap();
    let e = built.entries[0];
    assert_eq!(
        e.offset as usize,
        format::HEADER_LEN + format::ENTRY_HEADER_LEN
    );
    assert_eq!(format::slice(&built.bytes, &e).unwrap(), v);
}

#[tokio::test]
async fn bulk_writes_wait_for_the_namespace_to_have_known_packs_for_the_ttl() {
    let ttl = Duration::from_millis(300);
    let e = env_with(
        Arc::new(MemPackIndex::new()),
        PackConfig {
            negative_ttl: ttl,
            ..cfg()
        },
    );
    // No pack yet: a bulk write stays loose, so a reader that has not noticed packs finds it.
    let first = many(0..10);
    e.objects.put_many(first.clone()).await.unwrap();
    assert!(pack_files(&e).await.is_empty());
    assert!(loose_exists(&e, &first[0].0).await);

    // The job's first pack keeps the loose copies until readers elsewhere would have noticed.
    let started = std::time::Instant::now();
    let rep = e.objects.pack_run(&NoFence, later(), false).await.unwrap();
    assert_eq!(rep.packed, 10);
    assert!(
        started.elapsed() >= Duration::from_millis(250),
        "{:?}",
        started.elapsed()
    );
    all_readable(&e, &first).await;

    // Known for the ttl now: bulk writes pack directly.
    tokio::time::sleep(ttl).await;
    let more = many(100..110);
    e.objects.put_many(more.clone()).await.unwrap();
    assert!(!loose_exists(&e, &more[0].0).await);
    all_readable(&e, &more).await;
}
