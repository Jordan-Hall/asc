//! Hybrid ranking for `Semantic` in `hybrid` mode (SPEC §23.3, I11): tantivy BM25 over the
//! `chunks` index, the vector list and exact `sym_def` hits on query identifiers, fused with
//! reciprocal rank fusion (k = 60). A cross-encoder rerank is optional and off by default.
//! Every list and every query is bounded; ids are untrusted opaque chunk keys.

use std::collections::{HashMap, HashSet};

use roaring::RoaringBitmap;
use tantivy::collector::TopDocs;
use tantivy::query::{BooleanQuery, BoostQuery, Occur, Query, TermQuery};
use tantivy::schema::{Field, IndexRecordOption};
use tantivy::{Searcher, Term};

use crate::index::schema::{ChunksSchema, MAX_TOKEN_LEN};
use crate::vectors::{chunk_key, doc_ord_of};

/// RRF constant (Cormack et al. 2009).
pub const RRF_K: u32 = 60;
/// BM25 field weights over `chunks` (SPEC §23.3).
pub const BOOST_SYMBOL: f32 = 3.0;
pub const BOOST_IDENT: f32 = 1.0;
pub const BOOST_DOC: f32 = 1.0;
pub const BOOST_PATH_TOK: f32 = 2.0;
/// Most query tokens used for BM25 and identifier lookup.
pub const MAX_QUERY_TOKENS: usize = 32;
/// Longest query text read, in bytes.
pub const MAX_QUERY_BYTES: usize = 4096;
/// Longest candidate list accepted per source and most results fetched from BM25.
pub const MAX_LIST: usize = 10_000;
/// Most fused results returned.
pub const MAX_RESULTS: usize = 1_000;

#[derive(Debug, thiserror::Error)]
pub enum HybridError {
    #[error("tantivy: {0}")]
    Tantivy(#[from] tantivy::TantivyError),
    #[error("reranker: {0}")]
    Rerank(String),
}

pub type Result<T, E = HybridError> = std::result::Result<T, E>;

/// One fused result.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Fused {
    pub key: u64,
    pub score: f64,
}

/// Reciprocal rank fusion. Each list is best first; a key's score is the sum over lists of
/// `1 / (RRF_K + rank)` with rank starting at 1. A key repeated inside a list counts once, at its
/// first position. Ties break on the lower key. Lists are truncated to [`MAX_LIST`].
pub fn rrf(lists: &[&[u64]], limit: usize) -> Vec<Fused> {
    let mut score: HashMap<u64, f64> = HashMap::new();
    for list in lists {
        let mut seen = HashSet::new();
        let mut rank = 0u32;
        for &key in list.iter().take(MAX_LIST) {
            if !seen.insert(key) {
                continue;
            }
            rank += 1;
            *score.entry(key).or_insert(0.0) += 1.0 / f64::from(RRF_K + rank);
        }
    }
    let mut out: Vec<Fused> = score
        .into_iter()
        .map(|(key, score)| Fused { key, score })
        .collect();
    out.sort_by(|a, b| b.score.total_cmp(&a.score).then(a.key.cmp(&b.key)));
    out.truncate(limit.min(MAX_RESULTS));
    out
}

/// Identifier-like tokens of a query: runs of alphanumerics and `_`, in order, deduplicated,
/// at most [`MAX_QUERY_TOKENS`], each at most [`MAX_TOKEN_LEN`] bytes. Case is kept (for
/// `sym_def` lookups); BM25 lowercases.
pub fn query_identifiers(query: &str) -> Vec<String> {
    let q = clip(query);
    let mut out: Vec<String> = Vec::new();
    for t in q.split(|c: char| !(c.is_alphanumeric() || c == '_')) {
        if t.is_empty() || t.len() > MAX_TOKEN_LEN || out.iter().any(|o| o == t) {
            continue;
        }
        out.push(t.to_owned());
        if out.len() == MAX_QUERY_TOKENS {
            break;
        }
    }
    out
}

fn clip(s: &str) -> &str {
    let mut n = s.len().min(MAX_QUERY_BYTES);
    while !s.is_char_boundary(n) {
        n -= 1;
    }
    &s[..n]
}

/// The weighted BM25 query: every token against `symbol`, `ident`, `doc` and, when the schema
/// has it, `path_tok`, each field boosted.
pub fn bm25_query(schema: &ChunksSchema, query: &str) -> Option<Box<dyn Query>> {
    let tokens = query_identifiers(query);
    if tokens.is_empty() {
        return None;
    }
    let mut fields: Vec<(Field, f32)> = vec![
        (schema.symbol, BOOST_SYMBOL),
        (schema.ident, BOOST_IDENT),
        (schema.doc, BOOST_DOC),
    ];
    if let Ok(f) = schema.schema.get_field("path_tok") {
        fields.push((f, BOOST_PATH_TOK));
    }
    let mut clauses: Vec<(Occur, Box<dyn Query>)> = Vec::new();
    for t in &tokens {
        let lower = t.to_lowercase();
        for &(f, boost) in &fields {
            let tq = TermQuery::new(
                Term::from_field_text(f, &lower),
                IndexRecordOption::WithFreqs,
            );
            clauses.push((
                Occur::Should,
                Box::new(BoostQuery::new(Box::new(tq), boost)),
            ));
        }
    }
    Some(Box::new(BooleanQuery::new(clauses)))
}

/// BM25 list over the searchers (one per segment index), best first, as chunk keys. Chunks whose
/// doc ord is not in `kept` are dropped (`None` keeps all). At most `limit` (capped at
/// [`MAX_LIST`]) per searcher are read.
pub fn bm25(
    searchers: &[Searcher],
    schema: &ChunksSchema,
    query: &str,
    kept: Option<&RoaringBitmap>,
    limit: usize,
) -> Result<Vec<u64>> {
    let Some(q) = bm25_query(schema, query) else {
        return Ok(Vec::new());
    };
    let limit = limit.clamp(1, MAX_LIST);
    let mut scored: Vec<(f32, u64)> = Vec::new();
    for s in searchers {
        for (score, addr) in s.search(&q, &TopDocs::with_limit(limit).order_by_score())? {
            let sr = s.segment_reader(addr.segment_ord);
            let ff = sr.fast_fields();
            let (Ok(ord), Ok(chunk)) = (ff.u64("ord"), ff.u64("chunk")) else {
                continue;
            };
            let (Some(o), Some(c)) = (ord.first(addr.doc_id), chunk.first(addr.doc_id)) else {
                continue;
            };
            let (Ok(o), Ok(c)) = (u32::try_from(o), u32::try_from(c)) else {
                continue;
            };
            if kept.is_some_and(|k| !k.contains(o)) {
                continue;
            }
            scored.push((score, chunk_key(o, c)));
        }
    }
    scored.sort_by(|a, b| b.0.total_cmp(&a.0).then(a.1.cmp(&b.1)));
    scored.truncate(limit);
    Ok(scored.into_iter().map(|(_, k)| k).collect())
}

/// Cross-encoder reranker (`bge-reranker-base`, SPEC §23.3). Returns one relevance score per
/// candidate, in candidate order.
pub trait Reranker: Send + Sync {
    fn score(&self, query: &str, candidates: &[u64]) -> Result<Vec<f32>>;
}

/// Hybrid options. `rerank` is off by default.
#[derive(Debug, Clone, Copy)]
pub struct HybridOptions {
    pub k: usize,
    pub rerank: bool,
}

impl Default for HybridOptions {
    fn default() -> Self {
        Self {
            k: 10,
            rerank: false,
        }
    }
}

/// Fuses the BM25, vector and symbol lists. With `opts.rerank` and a reranker the fused
/// candidates are reordered by reranker score (ties keep fused order); otherwise the RRF order
/// stands. A reranker length mismatch is an error. Results whose doc ord is not in `kept` are
/// removed (`None` keeps all) as a final defence.
pub fn hybrid(
    query: &str,
    bm25_list: &[u64],
    vector_list: &[u64],
    sym_list: &[u64],
    kept: Option<&RoaringBitmap>,
    opts: HybridOptions,
    reranker: Option<&dyn Reranker>,
) -> Result<Vec<Fused>> {
    let k = opts.k.clamp(1, MAX_RESULTS);
    let rr = if opts.rerank { reranker } else { None };
    let pool = if rr.is_some() {
        (k * 4).min(MAX_RESULTS)
    } else {
        k
    };
    let mut fused = rrf(&[bm25_list, vector_list, sym_list], MAX_RESULTS);
    if let Some(kp) = kept {
        fused.retain(|f| kp.contains(doc_ord_of(f.key)));
    }
    fused.truncate(pool);
    if let Some(r) = rr {
        let keys: Vec<u64> = fused.iter().map(|f| f.key).collect();
        let s = r.score(clip(query), &keys)?;
        if s.len() != keys.len() {
            return Err(HybridError::Rerank("score count mismatch".into()));
        }
        let mut idx: Vec<usize> = (0..fused.len()).collect();
        idx.sort_by(|&a, &b| s[b].total_cmp(&s[a]).then(a.cmp(&b)));
        fused = idx
            .into_iter()
            .map(|i| Fused {
                key: fused[i].key,
                score: f64::from(s[i]),
            })
            .collect();
    }
    fused.truncate(k);
    Ok(fused)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Rev;
    impl Reranker for Rev {
        fn score(&self, _: &str, c: &[u64]) -> Result<Vec<f32>> {
            Ok((0..c.len()).map(|i| i as f32).collect())
        }
    }

    fn keys(f: &[Fused]) -> Vec<u64> {
        f.iter().map(|x| x.key).collect()
    }

    #[test]
    fn three_list_fixture_fuses_in_expected_order() {
        let bm = [1u64, 2, 3];
        let vec = [2u64, 3, 4];
        let sym = [3u64, 9];
        let f = rrf(&[&bm, &vec, &sym], 10);
        // 3: 1/63+1/62+1/61 ; 2: 1/62+1/61 ; 1: 1/61 ; 9: 1/62 ; 4: 1/63
        assert_eq!(keys(&f), vec![3, 2, 1, 9, 4]);
        let want = 1.0 / 63.0 + 1.0 / 62.0 + 1.0 / 61.0;
        assert!((f[0].score - want).abs() < 1e-12);
    }

    #[test]
    fn duplicates_count_once_and_ties_break_low() {
        let f = rrf(&[&[5, 5, 5], &[7]], 10);
        assert_eq!(keys(&f), vec![5, 7]);
        assert!((f[0].score - 1.0 / 61.0).abs() < 1e-12);
        assert_eq!(rrf(&[&[9], &[3]], 10)[0].key, 3);
    }

    #[test]
    fn rerank_off_by_default() {
        assert!(!HybridOptions::default().rerank);
        let r = hybrid(
            "q",
            &[1, 2],
            &[2, 1, 3],
            &[],
            None,
            HybridOptions::default(),
            Some(&Rev),
        )
        .unwrap();
        assert_eq!(keys(&r), keys(&rrf(&[&[1, 2], &[2, 1, 3]], 10)));
    }

    #[test]
    fn rerank_reorders_when_enabled() {
        let o = HybridOptions { k: 3, rerank: true };
        let r = hybrid("q", &[1, 2, 3], &[], &[], None, o, Some(&Rev)).unwrap();
        assert_eq!(keys(&r), vec![3, 2, 1]);
        let r = hybrid("q", &[1, 2, 3], &[], &[], None, o, None).unwrap();
        assert_eq!(keys(&r), vec![1, 2, 3]);
    }

    #[test]
    fn kept_filters_and_limits_bound() {
        let mut kept = RoaringBitmap::new();
        kept.insert(1);
        let a = chunk_key(1, 0);
        let b = chunk_key(2, 0);
        let o = HybridOptions::default();
        let r = hybrid("q", &[b, a], &[], &[], Some(&kept), o, None).unwrap();
        assert_eq!(keys(&r), vec![a]);
        let big: Vec<u64> = (0..50_000).collect();
        assert!(rrf(&[&big], usize::MAX).len() <= MAX_RESULTS);
    }

    #[test]
    fn identifiers_are_bounded() {
        assert_eq!(
            query_identifiers("find parseUrl, parseUrl in a_b"),
            vec!["find", "parseUrl", "in", "a_b"]
        );
        let long = "x ".repeat(10_000);
        assert!(query_identifiers(&long).len() <= MAX_QUERY_TOKENS);
        assert!(query_identifiers(&"y".repeat(100)).is_empty());
    }

    #[test]
    fn bm25_over_chunks_index_weights_symbol() {
        use tantivy::{Index, doc};
        let cs = ChunksSchema::build();
        let idx = Index::create_in_ram(cs.schema.clone());
        crate::index::schema::register_tokenizers(idx.tokenizers());
        let mut w = idx.writer_with_num_threads(1, 15_000_000).unwrap();
        w.add_document(doc!(cs.ord => 1u64, cs.chunk => 0u64, cs.ident => "widget"))
            .unwrap();
        w.add_document(doc!(cs.ord => 2u64, cs.chunk => 3u64, cs.symbol => "widget"))
            .unwrap();
        w.add_document(doc!(cs.ord => 3u64, cs.chunk => 0u64, cs.ident => "other"))
            .unwrap();
        w.commit().unwrap();
        let s = idx.reader().unwrap().searcher();
        let l = bm25(std::slice::from_ref(&s), &cs, "Widget", None, 10).unwrap();
        assert_eq!(l, vec![chunk_key(2, 3), chunk_key(1, 0)]);
        let mut kept = RoaringBitmap::new();
        kept.insert(1);
        let l = bm25(&[s], &cs, "widget", Some(&kept), 10).unwrap();
        assert_eq!(l, vec![chunk_key(1, 0)]);
    }
}
