use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use async_trait::async_trait;
use asc_store::bucket::BucketObjects;
use asc_store::codec::encode_stored;
use asc_store::objects::Objects;
use tantivy::query::{BooleanQuery, Query, QueryParser, TermQuery};
use tantivy::schema::{IndexRecordOption, Term};

use super::manifest::{
    Cas, DROP_GRACE_MS, Manifest, ManifestStore, MemManifest, Publisher, SegRef,
};
use super::schema::{ChunksSchema, FilesSchema, IndexKind};
use super::segments::{
    ChunkDoc, FileDoc, Replica, SegmentPack, SegmentWriter, WRITER_HEAP, put_pack, seg_key,
    unpack_and_open,
};
use super::*;
use crate::grams::{distinct_trigrams, gram};
use crate::lang::Lang;

const NS: &str = "ns1";
const T0: u64 = 1_000_000;

fn file_pack(work: &std::path::Path, files: &[(u32, &str, &str)]) -> SegmentPack {
    let mut w = SegmentWriter::new(IndexKind::Files, work, WRITER_HEAP).unwrap();
    for &(ord, text, sym) in files {
        let grams = distinct_trigrams(text.as_bytes(), 1 << 20).unwrap();
        w.add_file(&FileDoc {
            ord,
            grams: &grams,
            sym_def: &[sym],
            sym_ref: &[],
            ident: text,
            doc: "",
            path: "src/lib.rs",
            langs: &[Lang::Rust],
        })
        .unwrap();
    }
    w.finish().unwrap().unwrap()
}

fn tri_query(s: &str) -> Box<dyn Query> {
    let f = FilesSchema::build();
    let terms: Vec<Box<dyn Query>> = distinct_trigrams(s.as_bytes(), 100)
        .unwrap()
        .into_iter()
        .map(|g| {
            Box::new(TermQuery::new(
                Term::from_field_u64(f.tri, u64::from(g)),
                IndexRecordOption::Basic,
            )) as Box<dyn Query>
        })
        .collect();
    Box::new(BooleanQuery::intersection(terms))
}

fn setup() -> (tempfile::TempDir, BucketObjects, MemManifest) {
    (
        tempfile::tempdir().unwrap(),
        BucketObjects::memory(),
        MemManifest::new(),
    )
}

fn publisher<'a>(o: &'a BucketObjects, m: &'a MemManifest, holder: &'a str) -> Publisher<'a> {
    Publisher {
        objects: o,
        manifests: m,
        ns: NS,
        kind: IndexKind::Files,
        holder,
    }
}

fn ords(s: &segments::Snapshot, q: &dyn Query) -> Vec<u32> {
    s.ords_for_query(q).unwrap().iter().collect()
}

#[tokio::test]
async fn publish_sync_and_search_by_trigram() {
    let (dir, objs, mans) = setup();
    let p = publisher(&objs, &mans, "a");
    assert!(p.lease(T0, 60_000).await.unwrap());
    let pack = file_pack(
        dir.path(),
        &[
            (1, "fn parse_widget()", "parse_widget"),
            (2, "struct Gadget", "Gadget"),
        ],
    );
    let m = p.commit(&[pack], T0).await.unwrap();
    assert_eq!(m.generation, 1);
    let rep = Replica::new(dir.path().join("ci"));
    let snap = rep.snapshot(&objs, NS, &m).await.unwrap();
    assert_eq!(ords(&snap, &*tri_query("widget")), vec![1]);
    assert_eq!(ords(&snap, &*tri_query("GADGET")), vec![2]);
    assert!(ords(&snap, &*tri_query("absent")).is_empty());
    let f = FilesSchema::build();
    let def = TermQuery::new(
        Term::from_field_text(f.sym_def, "Gadget"),
        IndexRecordOption::Basic,
    );
    assert_eq!(ords(&snap, &def), vec![2]);
    let lang = TermQuery::new(
        Term::from_field_text(f.lang, "rust"),
        IndexRecordOption::Basic,
    );
    assert_eq!(ords(&snap, &lang), vec![1, 2]);
    // A second sync reuses the local copy.
    let again = rep.snapshot(&objs, NS, &m).await.unwrap();
    assert_eq!(again.visible_ords().unwrap().len(), 2);
}

#[tokio::test]
async fn chunks_index_scores_with_bm25() {
    let dir = tempfile::tempdir().unwrap();
    let mut w = SegmentWriter::new(IndexKind::Chunks, dir.path(), WRITER_HEAP).unwrap();
    for (ord, chunk, ident) in [
        (1, 0, "parseWidget tokenizer"),
        (1, 1, "other"),
        (2, 0, "widget widget"),
    ] {
        w.add_chunk(&ChunkDoc {
            ord,
            chunk,
            ident,
            doc: "",
            symbol: "",
        })
        .unwrap();
    }
    let pack = w.finish().unwrap().unwrap();
    assert_eq!(pack.docs, 3);
    let raw = asc_store::codec::decode_stored_capped(&pack.stored, 1 << 28).unwrap();
    let seg = unpack_and_open(IndexKind::Chunks, pack.hash, &raw, &dir.path().join("o")).unwrap();
    let cs = ChunksSchema::build();
    let qp = QueryParser::for_index(&seg.index, vec![cs.ident, cs.doc, cs.symbol]);
    let q = qp.parse_query("widget").unwrap();
    let hits = seg
        .reader
        .searcher()
        .search(
            &q,
            &tantivy::collector::TopDocs::with_limit(5).order_by_score(),
        )
        .unwrap();
    assert_eq!(
        hits.len(),
        1,
        "camelCase is not split; only the whole word matches"
    );
}

/// A store whose first CAS is preceded by a rival commit of another writer.
struct Racing {
    inner: MemManifest,
    raced: AtomicBool,
    objects: Arc<BucketObjects>,
}

#[async_trait]
impl ManifestStore for Racing {
    async fn load(&self, ns: &str, i: IndexKind) -> Result<Manifest> {
        self.inner.load(ns, i).await
    }
    async fn cas(
        &self,
        ns: &str,
        i: IndexKind,
        expected: u64,
        next: Manifest,
        holder: &str,
        now: u64,
    ) -> Result<Cas> {
        if !self.raced.swap(true, Ordering::SeqCst) {
            let p = Publisher {
                objects: &*self.objects,
                manifests: &self.inner,
                ns,
                kind: i,
                holder,
            };
            let dir = tempfile::tempdir().unwrap();
            let rival = file_pack(dir.path(), &[(9, "rival content", "rival")]);
            p.commit(&[rival], now).await.unwrap();
        }
        self.inner.cas(ns, i, expected, next, holder, now).await
    }
    async fn lease(
        &self,
        ns: &str,
        i: IndexKind,
        holder: &str,
        now: u64,
        ttl: u64,
    ) -> Result<bool> {
        self.inner.lease(ns, i, holder, now, ttl).await
    }
}

#[tokio::test]
async fn cas_loser_rebases_and_keeps_the_winner() {
    let dir = tempfile::tempdir().unwrap();
    let objs = Arc::new(BucketObjects::memory());
    let mans = Racing {
        inner: MemManifest::new(),
        raced: AtomicBool::new(false),
        objects: objs.clone(),
    };
    let p = Publisher {
        objects: &*objs,
        manifests: &mans,
        ns: NS,
        kind: IndexKind::Files,
        holder: "a",
    };
    assert!(p.lease(T0, 60_000).await.unwrap());
    let mine = file_pack(dir.path(), &[(1, "mine mine", "mine")]);
    let m = p.commit(&[mine], T0).await.unwrap();
    assert_eq!(
        m.generation, 2,
        "the rival won generation 1; the loser re-based onto it"
    );
    assert_eq!(m.segments.len(), 2);
    let rep = Replica::new(dir.path().join("ci"));
    let snap = rep.snapshot(&*objs, NS, &m).await.unwrap();
    assert_eq!(
        snap.visible_ords().unwrap().iter().collect::<Vec<_>>(),
        vec![1, 9]
    );
}

#[tokio::test]
async fn reader_never_sees_a_half_indexed_file() {
    let (dir, objs, mans) = setup();
    let p = publisher(&objs, &mans, "a");
    p.lease(T0, 60_000).await.unwrap();
    let first = file_pack(dir.path(), &[(1, "alpha beta", "alpha")]);
    let m1 = p.commit(&[first], T0).await.unwrap();
    // The next segment is uploaded but its generation is never published.
    let second = file_pack(dir.path(), &[(2, "gamma delta", "gamma")]);
    put_pack(&objs, NS, &second).await.unwrap();
    let rep = Replica::new(dir.path().join("ci"));
    let cur = mans.load(NS, IndexKind::Files).await.unwrap();
    assert_eq!(cur, m1);
    let snap = rep.snapshot(&objs, NS, &cur).await.unwrap();
    assert!(ords(&snap, &*tri_query("gamma")).is_empty());
    // A segment named by a manifest whose `indexed` lacks its ords stays invisible.
    let mut forged = cur.clone();
    forged.segments.push(SegRef {
        hash: second.hash,
        docs: second.docs,
        bytes: second.bytes,
    });
    let snap = rep.snapshot(&objs, NS, &forged).await.unwrap();
    assert!(ords(&snap, &*tri_query("gamma")).is_empty());
    assert_eq!(snap.visible_ords().unwrap().len(), 1);
}

#[tokio::test]
async fn only_the_lease_holder_commits() {
    let (dir, objs, mans) = setup();
    let a = publisher(&objs, &mans, "a");
    let b = publisher(&objs, &mans, "b");
    assert!(a.lease(T0, 1000).await.unwrap());
    assert!(!b.lease(T0 + 10, 1000).await.unwrap());
    let pack = file_pack(dir.path(), &[(1, "abc def", "abc")]);
    assert!(matches!(
        b.commit(&[pack], T0 + 10).await,
        Err(IndexError::NotLeader)
    ));
    let pack = file_pack(dir.path(), &[(1, "abc def", "abc")]);
    assert!(
        matches!(
            a.commit(&[pack], T0 + 5000).await,
            Err(IndexError::NotLeader)
        ),
        "expired lease"
    );
    assert!(
        b.lease(T0 + 5000, 1000).await.unwrap(),
        "expired lease is taken over"
    );
}

#[tokio::test]
async fn damaged_or_forged_packs_are_refused() {
    let (dir, objs, mans) = setup();
    let p = publisher(&objs, &mans, "a");
    p.lease(T0, 60_000).await.unwrap();
    let pack = file_pack(dir.path(), &[(1, "abc def", "abc")]);
    let hash = pack.hash;
    let m = p.commit(&[pack], T0).await.unwrap();
    let rep = Replica::new(dir.path().join("ci"));
    // Wrong bytes under the right key.
    objs.delete(&seg_key(NS, IndexKind::Files, hash))
        .await
        .unwrap();
    objs.put(
        &seg_key(NS, IndexKind::Files, hash),
        encode_stored(b"CISGgarbage"),
    )
    .await
    .unwrap();
    assert!(matches!(
        rep.snapshot(&objs, NS, &m).await,
        Err(IndexError::HashMismatch(_))
    ));
    // Missing object.
    objs.delete(&seg_key(NS, IndexKind::Files, hash))
        .await
        .unwrap();
    assert!(matches!(
        rep.snapshot(&objs, NS, &m).await,
        Err(IndexError::Missing(_))
    ));
    // Packs that hash right but are malformed, including a path-escaping file name.
    let cases: [&[u8]; 4] = [
        b"",
        b"CISG",
        b"CISG\x01\x00\x01\x01\x00",
        b"CISG\x01\x00\x01\x01\x00\x04../x\x00\x00\x00\x00\x00\x00\x00\x00",
    ];
    for (i, raw) in cases.iter().enumerate() {
        let h = *blake3::hash(raw).as_bytes();
        let r = unpack_and_open(IndexKind::Files, h, raw, &dir.path().join(format!("x{i}")));
        assert!(matches!(r, Err(IndexError::BadPack(_))), "{raw:?}");
    }
}

#[tokio::test]
async fn duplicate_ord_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let mut w = SegmentWriter::new(IndexKind::Files, dir.path(), WRITER_HEAP).unwrap();
    let d = FileDoc {
        ord: 5,
        grams: &[gram(b'a', b'b', b'c')],
        sym_def: &[],
        sym_ref: &[],
        ident: "",
        doc: "",
        path: "a",
        langs: &[],
    };
    w.add_file(&d).unwrap();
    assert!(matches!(w.add_file(&d), Err(IndexError::DuplicateOrd(5))));
    assert_eq!(w.docs(), 1);
    let empty = SegmentWriter::new(IndexKind::Files, dir.path(), WRITER_HEAP).unwrap();
    assert!(empty.finish().unwrap().is_none());
}

#[tokio::test]
async fn compaction_merges_and_the_sweep_waits_ten_minutes() {
    let (dir, objs, mans) = setup();
    let p = publisher(&objs, &mans, "a");
    p.lease(T0, 3_600_000).await.unwrap();
    let mut first_hash = None;
    for i in 0..8u32 {
        let text = format!("content number {i} uniqueword{i}");
        let pack = file_pack(dir.path(), &[(i + 1, &text, "sym")]);
        first_hash.get_or_insert(pack.hash);
        p.commit(&[pack], T0).await.unwrap();
    }
    let m = mans.load(NS, IndexKind::Files).await.unwrap();
    assert_eq!(m.segments.len(), 8);
    let merged = p.compact(dir.path(), T0 + 1).await.unwrap().unwrap();
    assert_eq!(merged.segments.len(), 1);
    assert_eq!(merged.segments[0].docs, 8);
    let rep = Replica::new(dir.path().join("ci"));
    let snap = rep.snapshot(&objs, NS, &merged).await.unwrap();
    assert_eq!(snap.visible_ords().unwrap().len(), 8);
    assert_eq!(ords(&snap, &*tri_query("uniqueword3")), vec![4]);
    // Before the grace period nothing is deleted, so a pinned older generation still opens.
    assert_eq!(p.sweep(T0 + DROP_GRACE_MS - 1).await.unwrap(), 0);
    let old = rep.snapshot(&objs, NS, &m).await.unwrap();
    assert_eq!(old.visible_ords().unwrap().len(), 8);
    assert!(p.sweep(T0 + 1 + DROP_GRACE_MS).await.unwrap() >= 8);
    let h = first_hash.unwrap();
    assert!(
        objs.get_stored(&seg_key(NS, IndexKind::Files, h))
            .await
            .unwrap()
            .is_none()
    );
    let after = mans.load(NS, IndexKind::Files).await.unwrap();
    assert!(after.dropped.is_empty());
    let fresh = Replica::new(dir.path().join("ci2"));
    let again = fresh.snapshot(&objs, NS, &after).await.unwrap();
    assert_eq!(again.visible_ords().unwrap().len(), 8);
}

#[tokio::test]
async fn replica_rejects_bad_namespace_names() {
    let (dir, objs, _) = setup();
    let rep = Replica::new(dir.path().join("ci"));
    let m = Manifest::empty(IndexKind::Files);
    for ns in ["", "..", "a/b", "a\\b", "x\0"] {
        assert!(rep.snapshot(&objs, ns, &m).await.is_err(), "{ns:?}");
    }
}

#[cfg(unix)]
#[tokio::test]
async fn symlinked_replica_root_is_refused() {
    let (dir, objs, _) = setup();
    let real = dir.path().join("real");
    std::fs::create_dir(&real).unwrap();
    std::os::unix::fs::symlink(&real, dir.path().join("ci")).unwrap();
    let rep = Replica::new(dir.path().join("ci"));
    let r = rep
        .snapshot(&objs, NS, &Manifest::empty(IndexKind::Files))
        .await;
    assert!(matches!(r, Err(IndexError::Symlink(_))));
}

fn dup(p: &SegmentPack) -> SegmentPack {
    SegmentPack {
        hash: p.hash,
        stored: p.stored.clone(),
        kind: p.kind,
        docs: p.docs,
        bytes: p.bytes,
        ords: p.ords.clone(),
    }
}

fn publisher_of<'a>(
    o: &'a BucketObjects,
    m: &'a dyn ManifestStore,
    ns: &'a str,
    kind: IndexKind,
) -> Publisher<'a> {
    Publisher {
        objects: o,
        manifests: m,
        ns,
        kind,
        holder: "a",
    }
}

/// Eight one-document packs; the first one is returned a second time for sharing.
fn eight_packs(work: &std::path::Path) -> Vec<SegmentPack> {
    (0..8u32)
        .map(|i| {
            let text = format!("content number {i} uniqueword{i}");
            file_pack(work, &[(i + 1, &text, "sym")])
        })
        .collect()
}

async fn compact_and_sweep(p: &Publisher<'_>, work: &std::path::Path) {
    assert!(p.compact(work, T0 + 1).await.unwrap().is_some());
    assert!(p.sweep(T0 + 1 + DROP_GRACE_MS).await.unwrap() >= 8);
}

#[tokio::test]
async fn compaction_in_one_namespace_leaves_an_identical_object_of_another_readable() {
    let (dir, objs, mans) = setup();
    let a = publisher_of(&objs, &mans, "tenant-a", IndexKind::Files);
    let b = publisher_of(&objs, &mans, "tenant-b", IndexKind::Files);
    a.lease(T0, 3_600_000).await.unwrap();
    b.lease(T0, 3_600_000).await.unwrap();
    let packs = eight_packs(dir.path());
    // Namespace b indexes byte-identical content: same pack, same `indexed` bitmap.
    let mb = b.commit(&[dup(&packs[0])], T0).await.unwrap();
    for p in &packs {
        a.commit(&[dup(p)], T0).await.unwrap();
    }
    compact_and_sweep(&a, dir.path()).await;
    let rep = Replica::new(dir.path().join("ci"));
    let snap = rep.snapshot(&objs, "tenant-b", &mb).await.unwrap();
    assert_eq!(
        snap.visible_ords().unwrap().iter().collect::<Vec<_>>(),
        vec![1]
    );
    let ma = mans.load("tenant-a", IndexKind::Files).await.unwrap();
    let snap = rep.snapshot(&objs, "tenant-a", &ma).await.unwrap();
    assert_eq!(snap.visible_ords().unwrap().len(), 8);
}

#[tokio::test]
async fn compaction_of_one_kind_leaves_an_identical_bitmap_of_the_other_readable() {
    let (dir, objs, mans) = setup();
    let files = publisher_of(&objs, &mans, NS, IndexKind::Files);
    let chunks = publisher_of(&objs, &mans, NS, IndexKind::Chunks);
    files.lease(T0, 3_600_000).await.unwrap();
    chunks.lease(T0, 3_600_000).await.unwrap();
    let mut w = SegmentWriter::new(IndexKind::Chunks, dir.path(), WRITER_HEAP).unwrap();
    w.add_chunk(&ChunkDoc {
        ord: 1,
        chunk: 0,
        ident: "alpha",
        doc: "",
        symbol: "",
    })
    .unwrap();
    let mc = chunks
        .commit(&[w.finish().unwrap().unwrap()], T0)
        .await
        .unwrap();
    for p in eight_packs(dir.path()) {
        files.commit(&[p], T0).await.unwrap();
    }
    compact_and_sweep(&files, dir.path()).await;
    let rep = Replica::new(dir.path().join("ci"));
    let snap = rep.snapshot(&objs, NS, &mc).await.unwrap();
    assert_eq!(snap.visible_ords().unwrap().len(), 1);
}

#[test]
fn object_keys_are_scoped_to_the_index() {
    let h = [7u8; 32];
    let k = seg_key("a", IndexKind::Files, h);
    assert_ne!(k, seg_key("b", IndexKind::Files, h));
    assert_ne!(k, seg_key("a", IndexKind::Chunks, h));
    assert_ne!(k, seg_key("ab", IndexKind::Files, h));
    assert_ne!(
        segments::idx_key("a", IndexKind::Files, h),
        seg_key("a", IndexKind::Files, h)
    );
    assert!(k.path().starts_with("d/ciseg.v2/"));
}

/// Returns a stale row from `load` and publishes `pack` just after it, as a rival writer
/// would between the sweep's read and its CAS.
struct Resurrect {
    inner: MemManifest,
    pack: std::sync::Mutex<Option<SegmentPack>>,
    objects: Arc<BucketObjects>,
}

#[async_trait]
impl ManifestStore for Resurrect {
    async fn load(&self, ns: &str, i: IndexKind) -> Result<Manifest> {
        let stale = self.inner.load(ns, i).await?;
        let rival = self.pack.lock().unwrap().take();
        if let Some(pack) = rival {
            let p = Publisher {
                objects: &*self.objects,
                manifests: &self.inner,
                ns,
                kind: i,
                holder: "a",
            };
            p.commit(&[pack], T0 + 2).await.unwrap();
        }
        Ok(stale)
    }
    async fn cas(
        &self,
        ns: &str,
        i: IndexKind,
        expected: u64,
        next: Manifest,
        holder: &str,
        now: u64,
    ) -> Result<Cas> {
        self.inner.cas(ns, i, expected, next, holder, now).await
    }
    async fn lease(
        &self,
        ns: &str,
        i: IndexKind,
        holder: &str,
        now: u64,
        ttl: u64,
    ) -> Result<bool> {
        self.inner.lease(ns, i, holder, now, ttl).await
    }
}

#[tokio::test]
async fn sweep_does_not_delete_an_object_a_concurrent_commit_just_referenced() {
    let dir = tempfile::tempdir().unwrap();
    let objs = Arc::new(BucketObjects::memory());
    let mans = Resurrect {
        inner: MemManifest::new(),
        pack: std::sync::Mutex::new(None),
        objects: objs.clone(),
    };
    let p = publisher_of(&objs, &mans, NS, IndexKind::Files);
    p.lease(T0, 3_600_000).await.unwrap();
    let packs = eight_packs(dir.path());
    for pk in &packs {
        p.commit(&[dup(pk)], T0).await.unwrap();
    }
    p.compact(dir.path(), T0 + 1).await.unwrap().unwrap();
    // The dropped first pack is published again right after the sweep reads the manifest.
    *mans.pack.lock().unwrap() = Some(dup(&packs[0]));
    p.sweep(T0 + 1 + DROP_GRACE_MS).await.unwrap();
    let cur = mans.load(NS, IndexKind::Files).await.unwrap();
    assert!(cur.segments.iter().any(|s| s.hash == packs[0].hash));
    let rep = Replica::new(dir.path().join("ci"));
    let snap = rep.snapshot(&*objs, NS, &cur).await.unwrap();
    assert_eq!(snap.visible_ords().unwrap().len(), 8);
}
