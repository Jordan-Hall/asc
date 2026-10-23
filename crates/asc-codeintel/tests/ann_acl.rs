//! I10: no out-of-scope chunk takes a result slot at any `k`, namespace A never reads B's
//! segments, generations are never mixed, and the path rule follows SPEC §23.3.

use asc_codeintel::vectors::*;
use roaring::RoaringBitmap;

const DIMS: usize = 16;

fn model(g: u32) -> ModelRef {
    ModelRef {
        model_gen: g,
        model_id: format!("m@abc/i8/calib@{g}/1"),
        calib: [g as u8; 32],
    }
}

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn vec(&mut self) -> Vec<i8> {
        (0..DIMS)
            .map(|_| ((self.next() % 255) as i64 - 127) as i8)
            .collect()
    }
}

/// `docs` documents with `per` chunks each; also returns the vector of chunk (0, 0).
fn segment(m: &ModelRef, docs: u32, per: u32, seed: u64) -> (VectorSegment, Vec<i8>) {
    let mut rng = Rng(seed);
    let mut b = SegmentBuilder::new(m.clone(), DIMS).unwrap();
    let mut first = Vec::new();
    for d in 0..docs {
        for c in 0..per {
            let v = rng.vec();
            if d == 0 && c == 0 {
                first = v.clone();
            }
            b.add(chunk_key(d, c), &v).unwrap();
        }
    }
    (b.finish().unwrap(), first)
}

fn sel(kept: &RoaringBitmap, per: u32, docs: u32) -> Selectivity {
    Selectivity {
        kept_chunks: kept.len() * u64::from(per),
        ns_chunks: u64::from(docs) * u64::from(per),
    }
}

#[test]
fn no_out_of_scope_chunk_at_any_k() {
    let m = model(1);
    let (seg, dup) = segment(&m, 200, 5, 7);
    // Denied doc 0 holds an exact duplicate of the query; keep the odd docs only.
    let kept: RoaringBitmap = (1..200).filter(|d| d % 2 == 1).collect();
    for k in [1usize, 2, 3, 10, 50, 100, 500, 1000] {
        let out = search(&[&seg], &m, &dup, k, &kept, sel(&kept, 5, 200)).unwrap();
        assert_eq!(out.hits.len(), k.min(kept.len() as usize * 5));
        for h in &out.hits {
            assert!(
                kept.contains(doc_ord_of(h.key)),
                "k={k} leaked {:#x}",
                h.key
            );
        }
    }
}

#[test]
fn brute_force_is_exact_and_sorted() {
    let m = model(1);
    let (seg, q) = segment(&m, 50, 4, 11);
    let kept: RoaringBitmap = (0..50).collect();
    let out = search(&[&seg], &m, &q, 10, &kept, sel(&kept, 4, 50)).unwrap();
    assert_eq!(out.path, SearchPath::Brute);
    assert_eq!(out.hits[0].key, chunk_key(0, 0));
    assert!(out.hits.windows(2).all(|w| w[0].score >= w[1].score));
}

#[test]
fn sparse_and_dense_iteration_agree() {
    let m = model(1);
    let (seg, q) = segment(&m, 400, 3, 5);
    let sparse: RoaringBitmap = [3u32, 77, 301].into_iter().collect();
    let a = seg.brute_force(&q, 5, &sparse).unwrap();
    assert_eq!(a.len(), 5);
    assert!(a.iter().all(|h| sparse.contains(doc_ord_of(h.key))));
    let dense: RoaringBitmap = (0..400).collect();
    let b = seg.brute_force(&q, 5, &dense).unwrap();
    assert_eq!(b[0].key, chunk_key(0, 0));
}

#[test]
fn generations_are_never_mixed() {
    let (seg, q) = segment(&model(1), 10, 2, 3);
    let kept: RoaringBitmap = (0..10).collect();
    let s = sel(&kept, 2, 10);
    assert!(matches!(
        search(&[&seg], &model(2), &q, 3, &kept, s),
        Err(VectorError::Generation(_))
    ));
    let mut other = model(1);
    other.calib = [9; 32];
    assert!(search(&[&seg], &other, &q, 3, &kept, s).is_err());
    let (seg2, _) = segment(&model(2), 10, 2, 3);
    assert!(search(&[&seg, &seg2], &model(1), &q, 3, &kept, s).is_err());
}

#[test]
fn merged_segments_dedupe_and_stay_in_scope() {
    let m = model(1);
    let (a, q) = segment(&m, 20, 2, 1);
    let (b, _) = segment(&m, 20, 2, 1);
    let kept: RoaringBitmap = (5..20).collect();
    let out = search(&[&a, &b], &m, &q, 100, &kept, sel(&kept, 2, 20)).unwrap();
    let mut keys: Vec<u64> = out.hits.iter().map(|h| h.key).collect();
    let n = keys.len();
    keys.sort_unstable();
    keys.dedup();
    assert_eq!(keys.len(), n);
    assert_eq!(n, 30);
}

#[test]
fn path_rule_follows_the_spec() {
    // 1% of a 3M-chunk namespace: brute force.
    assert_eq!(
        plan(
            10,
            Selectivity {
                kept_chunks: 30_000,
                ns_chunks: 3_000_000
            }
        ),
        SearchPath::Brute
    );
    // At most 20,000 kept: brute force whatever the selectivity.
    assert_eq!(
        plan(
            10,
            Selectivity {
                kept_chunks: 20_000,
                ns_chunks: 30_000
            }
        ),
        SearchPath::Brute
    );
    // 30% of 3M: HNSW, ef = ceil(4k / s), bound = 16 ef.
    let thirty = Selectivity {
        kept_chunks: 900_000,
        ns_chunks: 3_000_000,
    };
    assert_eq!(
        plan(10, thirty),
        SearchPath::Hnsw {
            ef_search: 134,
            visit_bound: 16 * 134
        }
    );
    assert_eq!(
        plan(1, thirty),
        SearchPath::Hnsw {
            ef_search: 64,
            visit_bound: 1024
        }
    );
    assert_eq!(
        plan(1000, thirty),
        SearchPath::Hnsw {
            ef_search: 4096,
            visit_bound: 16 * 4096
        }
    );
    // Just under 3% brute, exactly 3% HNSW.
    let ns = 10_000_000;
    assert_eq!(
        plan(
            10,
            Selectivity {
                kept_chunks: 299_999,
                ns_chunks: ns
            }
        ),
        SearchPath::Brute
    );
    assert!(matches!(
        plan(
            10,
            Selectivity {
                kept_chunks: 300_000,
                ns_chunks: ns
            }
        ),
        SearchPath::Hnsw { .. }
    ));
}

#[test]
fn rejects_bad_arguments() {
    let m = model(1);
    let (seg, q) = segment(&m, 4, 1, 2);
    let kept: RoaringBitmap = (0..4).collect();
    let s = sel(&kept, 1, 4);
    assert!(search(&[&seg], &m, &q, 0, &kept, s).is_err());
    assert!(search(&[&seg], &m, &q, MAX_K + 1, &kept, s).is_err());
    assert!(search(&[&seg], &m, &q[..3], 1, &kept, s).is_err());
    let mut b = SegmentBuilder::new(m.clone(), DIMS).unwrap();
    b.add(1, &q).unwrap();
    assert!(b.add(1, &q).is_err());
    assert!(b.add(2, &q[..2]).is_err());
    assert!(SegmentBuilder::new(m, MAX_DIMS + 1).is_err());
}

#[test]
fn segment_codec_roundtrip_and_tamper() {
    let m = model(3);
    let (seg, q) = segment(&m, 30, 3, 9);
    let (hash, bytes) = seg.encode().unwrap();
    let back = VectorSegment::decode(&bytes, &hash).unwrap();
    assert_eq!(back.model(), &m);
    let kept: RoaringBitmap = (0..30).collect();
    let s = sel(&kept, 3, 30);
    assert_eq!(
        search(&[&seg], &m, &q, 5, &kept, s).unwrap().hits,
        search(&[&back], &m, &q, 5, &kept, s).unwrap().hits
    );
    let mut bad = bytes.clone();
    let last = bad.len() - 1;
    bad[last] ^= 1;
    assert!(matches!(
        VectorSegment::decode(&bad, &hash),
        Err(VectorError::HashMismatch)
    ));
    // Hash-correct but structurally bad objects must fail without allocating or panicking.
    for cut in [0usize, 3, 10, 40, bytes.len() - 1] {
        let t = &bytes[..cut];
        let h = *blake3::hash(t).as_bytes();
        assert!(VectorSegment::decode(t, &h).is_err());
    }
    let mut huge = bytes.clone();
    huge[16..20].copy_from_slice(&u32::MAX.to_le_bytes());
    let h = *blake3::hash(&huge).as_bytes();
    assert!(VectorSegment::decode(&huge, &h).is_err());
}

#[test]
fn local_b1_rescoring_finds_the_query_and_respects_scope() {
    let m = model(1);
    let (seg, q) = segment(&m, 300, 2, 21);
    let local = LocalVectors::from_segment(&seg);
    let kept: RoaringBitmap = (0..300).filter(|d| d % 3 != 0 || *d == 0).collect();
    let hits = local.search(&m, &q, 10, &kept).unwrap();
    assert_eq!(hits[0].key, chunk_key(0, 0));
    assert!(hits.iter().all(|h| kept.contains(doc_ord_of(h.key))));
    let denied: RoaringBitmap = (1..300).collect();
    let hits = local.search(&m, &q, 1000, &denied).unwrap();
    assert!(hits.iter().all(|h| doc_ord_of(h.key) != 0));
    assert!(local.search(&model(2), &q, 3, &kept).is_err());
}

#[test]
fn local_file_header_gates_the_generation() {
    let m = model(1);
    let (seg, _) = segment(&m, 20, 2, 4);
    let bytes = LocalVectors::from_segment(&seg).encode().unwrap();
    assert!(LocalVectors::decode(&bytes, &m).unwrap().is_some());
    assert!(LocalVectors::decode(&bytes, &model(2)).unwrap().is_none());
    let mut bad = bytes.clone();
    bad[60] ^= 0xff;
    assert!(LocalVectors::decode(&bad, &m).is_err());
    assert!(LocalVectors::decode(&bytes[..10], &m).is_err());
}

#[test]
fn stale_generations_are_dropped_not_migrated() {
    let root = tempfile::tempdir().unwrap();
    for g in [1u32, 2, 3] {
        let d = generation_dir(root.path(), g);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("v.ascvl"), b"x").unwrap();
    }
    std::fs::create_dir_all(root.path().join("vec").join("notes")).unwrap();
    assert_eq!(drop_stale_generations(root.path(), 3).unwrap(), 2);
    assert!(generation_dir(root.path(), 3).join("v.ascvl").exists());
    assert!(!generation_dir(root.path(), 1).exists());
    assert!(root.path().join("vec").join("notes").exists());
    assert_eq!(drop_stale_generations(root.path(), 3).unwrap(), 0);
    let empty = tempfile::tempdir().unwrap();
    assert_eq!(drop_stale_generations(empty.path(), 1).unwrap(), 0);
}

#[test]
fn load_local_reads_and_gates() {
    let root = tempfile::tempdir().unwrap();
    let m = model(1);
    let (seg, _) = segment(&m, 5, 2, 8);
    let dir = generation_dir(root.path(), 1);
    std::fs::create_dir_all(&dir).unwrap();
    let f = dir.join("v.ascvl");
    assert!(load_local(root.path(), &f, &m).unwrap().is_none());
    std::fs::write(&f, LocalVectors::from_segment(&seg).encode().unwrap()).unwrap();
    assert!(load_local(root.path(), &f, &m).unwrap().is_some());
    assert!(load_local(root.path(), &f, &model(2)).unwrap().is_none());
    assert!(load_local(root.path(), &std::env::temp_dir(), &m).is_err());
}

#[cfg(feature = "ann")]
mod graph {
    use super::*;

    #[test]
    fn hnsw_filtered_search_never_leaks_or_exceeds_the_bound() {
        let m = model(1);
        let docs = 3000u32;
        let (seg, q) = segment(&m, docs, 10, 99);
        assert!(seg.has_graph());
        // 30% kept, the doc holding the query's duplicate denied.
        let kept: RoaringBitmap = (1..docs).filter(|d| d % 10 < 3).collect();
        // Claim a large namespace so the plan picks HNSW.
        let s = Selectivity {
            kept_chunks: kept.len() * 1000,
            ns_chunks: u64::from(docs) * 3333,
        };
        for k in [1usize, 10, 100, 1000] {
            let SearchPath::Hnsw { visit_bound, .. } = plan(k, s) else {
                panic!("expected hnsw")
            };
            let out = search(&[&seg], &m, &q, k, &kept, s).unwrap();
            assert!(
                out.max_visited <= visit_bound + 1,
                "visited {}",
                out.max_visited
            );
            assert!(out.hits.iter().all(|h| kept.contains(doc_ord_of(h.key))));
        }
    }

    #[test]
    fn graph_survives_the_codec() {
        let m = model(1);
        let (seg, q) = segment(&m, 200, 5, 5);
        let (h, b) = seg.encode().unwrap();
        let back = VectorSegment::decode(&b, &h).unwrap();
        assert!(back.has_graph());
        let kept: RoaringBitmap = (1..200).collect();
        let s = Selectivity {
            kept_chunks: 5_000_000,
            ns_chunks: 6_000_000,
        };
        let out = search(&[&back], &m, &q, 10, &kept, s).unwrap();
        assert!(out.hits.iter().all(|h| doc_ord_of(h.key) != 0));
    }
}
