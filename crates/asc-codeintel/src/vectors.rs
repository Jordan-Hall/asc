//! Vector segments (SPEC §23.2.4, §23.3 semantic row, §23.7, CI-D5): i8 vectors per namespace,
//! `filtered_search` with the predicate `doc_ord(chunk) ∈ kept`, exact brute force for small
//! scopes, the b1 local format with i8 rescoring and model generations.
//!
//! The HNSW graph (usearch, M = 16, ef_construction 128) sits behind the cargo feature `ann`.
//! Without it every scope takes the exact path, which uses the same predicate, so the ACL
//! guarantee is identical. The usearch crate has a safe API, so this module needs no `unsafe`.
//!
//! Everything decoded here is untrusted: sizes are checked against the input length before any
//! allocation, the blake3 name of a segment is verified, and a query never compares vectors of
//! two model generations.

use std::cmp::{Ordering, Reverse};
use std::collections::{BTreeMap, BinaryHeap};
use std::path::{Component, Path, PathBuf};

use roaring::RoaringBitmap;

/// Content hash (blake3) naming an object under `d/civec.v1/`.
pub type Hash = [u8; 32];

pub const MAGIC: [u8; 4] = *b"ASVS";
pub const LOCAL_MAGIC: [u8; 4] = *b"ASVL";
pub const FORMAT_VERSION: u32 = 1;
/// Largest vector dimension accepted (matches `asc-embed`).
pub const MAX_DIMS: usize = 4096;
/// Largest number of vectors in one segment.
pub const MAX_VECTORS: usize = 1 << 24;
/// Largest `k` a query may ask for.
pub const MAX_K: usize = 1000;
pub const MAX_MODEL_ID: usize = 1024;
/// Largest segment or local file read from disk or the bucket.
pub const MAX_FILE_BYTES: u64 = 4 << 30;
/// HNSW connectivity.
pub const HNSW_M: usize = 16;
pub const EF_CONSTRUCTION: usize = 128;
/// Brute force when at most this many chunks are kept.
pub const BRUTE_KEPT: u64 = 20_000;
/// Brute-force fallback after the visit bound is hit, when at most this many chunks are kept.
pub const FALLBACK_KEPT: u64 = 200_000;
/// Brute force below this selectivity, in percent.
pub const MIN_SELECTIVITY_PCT: u64 = 3;
pub const EF_MIN: usize = 64;
pub const EF_MAX: usize = 4096;
/// Candidates the b1 prefilter hands to i8 rescoring.
pub const RESCORE_N: usize = 200;

#[derive(Debug, thiserror::Error)]
pub enum VectorError {
    #[error("corrupt vector object: {0}")]
    Corrupt(&'static str),
    #[error("vector object hash mismatch")]
    HashMismatch,
    #[error("vector object too large: {0}")]
    TooLarge(&'static str),
    #[error("invalid argument: {0}")]
    BadArg(&'static str),
    #[error("model generation mismatch: {0}")]
    Generation(&'static str),
    #[error("ann backend: {0}")]
    Ann(String),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

pub type Result<T, E = VectorError> = std::result::Result<T, E>;

/// The embedding model a vector belongs to. Vectors of two generations are never compared.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelRef {
    pub model_gen: u32,
    /// `name@<sha256>/<quantisation>/calib@<digest>/<chunker_ver>` (SPEC §23.5).
    pub model_id: String,
    /// Calibration digest the i8 table was derived with.
    pub calib: [u8; 32],
}

impl ModelRef {
    fn validate(&self) -> Result<()> {
        if self.model_id.is_empty() || self.model_id.len() > MAX_MODEL_ID {
            return Err(VectorError::BadArg("model id length"));
        }
        Ok(())
    }
}

/// Vector key: `(doc ord, chunk index)` so the filter predicate reads the doc ord off the key.
pub fn chunk_key(doc_ord: u32, chunk: u32) -> u64 {
    (u64::from(doc_ord) << 32) | u64::from(chunk)
}

pub fn doc_ord_of(key: u64) -> u32 {
    (key >> 32) as u32
}

pub fn chunk_of(key: u64) -> u32 {
    key as u32
}

// ---------------------------------------------------------------- selectivity plan

/// Counts that drive the path choice. `kept_chunks` is the sum of chunk counts of the docs in
/// `scope ∖ xdocs ∖ secret ∖ deny`; `ns_chunks` is the namespace total (both from `ci_docs`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Selectivity {
    pub kept_chunks: u64,
    pub ns_chunks: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchPath {
    Brute,
    Hnsw {
        ef_search: usize,
        visit_bound: usize,
    },
}

/// Chooses the search path (SPEC §23.3): exact below 3% selectivity or at most 20,000 kept
/// chunks, otherwise HNSW with `ef_search = min(4096, max(64, ceil(4k / s)))` and a visit bound
/// of `16 × ef_search`.
pub fn plan(k: usize, sel: Selectivity) -> SearchPath {
    let kept = sel.kept_chunks;
    let ns = sel.ns_chunks.max(kept).max(1);
    if kept <= BRUTE_KEPT || kept.saturating_mul(100) < ns.saturating_mul(MIN_SELECTIVITY_PCT) {
        return SearchPath::Brute;
    }
    // ceil(4k / s) with s = kept / ns, in integers: ceil(4 k ns / kept).
    let num = (4u128) * (k as u128) * u128::from(ns);
    let ef = num.div_ceil(u128::from(kept));
    let ef_search = ef.clamp(EF_MIN as u128, EF_MAX as u128) as usize;
    SearchPath::Hnsw {
        ef_search,
        visit_bound: 16 * ef_search,
    }
}

// ---------------------------------------------------------------- math

fn dot(a: &[i8], b: &[i8]) -> i32 {
    // dims <= 4096, so |sum| <= 4096 * 127 * 128 fits i32.
    a.iter()
        .zip(b)
        .map(|(&x, &y)| i32::from(x) * i32::from(y))
        .sum()
}

fn norm(a: &[i8]) -> f32 {
    (a.iter().map(|&x| i32::from(x) * i32::from(x)).sum::<i32>() as f32).sqrt()
}

fn cosine(a: &[i8], an: f32, b: &[i8], bn: f32) -> f32 {
    if an == 0.0 || bn == 0.0 {
        return 0.0;
    }
    dot(a, b) as f32 / (an * bn)
}

/// One scored chunk. Higher score is better; ties break on the lower key.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Hit {
    pub key: u64,
    pub score: f32,
}

impl Eq for Hit {}

impl Ord for Hit {
    fn cmp(&self, o: &Self) -> Ordering {
        self.score
            .total_cmp(&o.score)
            .then_with(|| o.key.cmp(&self.key))
    }
}

impl PartialOrd for Hit {
    fn partial_cmp(&self, o: &Self) -> Option<Ordering> {
        Some(self.cmp(o))
    }
}

struct TopK {
    k: usize,
    heap: BinaryHeap<Reverse<Hit>>,
}

impl TopK {
    fn new(k: usize) -> Self {
        Self {
            k,
            heap: BinaryHeap::with_capacity(k.min(4096) + 1),
        }
    }

    fn push(&mut self, h: Hit) {
        if self.heap.len() < self.k {
            self.heap.push(Reverse(h));
        } else if let Some(Reverse(worst)) = self.heap.peek()
            && h > *worst
        {
            self.heap.pop();
            self.heap.push(Reverse(h));
        }
    }

    fn into_sorted(self) -> Vec<Hit> {
        let mut v: Vec<Hit> = self.heap.into_iter().map(|Reverse(h)| h).collect();
        v.sort_by(|a, b| b.cmp(a));
        v
    }
}

// ---------------------------------------------------------------- segment

/// Accumulates vectors for one segment.
pub struct SegmentBuilder {
    model: ModelRef,
    dims: usize,
    rows: BTreeMap<u64, Vec<i8>>,
}

impl SegmentBuilder {
    pub fn new(model: ModelRef, dims: usize) -> Result<Self> {
        model.validate()?;
        if dims == 0 || dims > MAX_DIMS {
            return Err(VectorError::BadArg("dims"));
        }
        Ok(Self {
            model,
            dims,
            rows: BTreeMap::new(),
        })
    }

    pub fn len(&self) -> usize {
        self.rows.len()
    }

    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    /// Adds one chunk vector. A key is added once per segment.
    pub fn add(&mut self, key: u64, v: &[i8]) -> Result<()> {
        if v.len() != self.dims {
            return Err(VectorError::BadArg("vector length"));
        }
        if self.rows.len() >= MAX_VECTORS {
            return Err(VectorError::TooLarge("segment vectors"));
        }
        if self.rows.insert(key, v.to_vec()).is_some() {
            return Err(VectorError::BadArg("duplicate key"));
        }
        Ok(())
    }

    /// Builds the segment. With the `ann` feature the HNSW graph is built too.
    pub fn finish(self) -> Result<VectorSegment> {
        let mut keys = Vec::with_capacity(self.rows.len());
        let mut vecs = Vec::with_capacity(self.rows.len() * self.dims);
        for (k, v) in self.rows {
            keys.push(k);
            vecs.extend_from_slice(&v);
        }
        let mut seg = VectorSegment::assemble(self.model, self.dims, keys, vecs);
        #[cfg(feature = "ann")]
        seg.build_hnsw()?;
        #[cfg(not(feature = "ann"))]
        let _ = &mut seg;
        Ok(seg)
    }
}

/// One immutable vector segment: sorted keys and their i8 vectors, plus the optional graph.
pub struct VectorSegment {
    model: ModelRef,
    dims: usize,
    keys: Vec<u64>,
    vecs: Vec<i8>,
    norms: Vec<f32>,
    #[cfg(feature = "ann")]
    hnsw: Option<ann::Hnsw>,
}

/// Result of a segment or multi-segment search.
#[derive(Debug, Clone, PartialEq)]
pub struct Outcome {
    pub hits: Vec<Hit>,
    pub path: SearchPath,
    /// The visit bound was hit with fewer than `k` results and the scope was too large to
    /// fall back to brute force: the list is partial (`ann_exhausted`).
    pub ann_exhausted: bool,
    /// At least one segment fell back from HNSW to brute force.
    pub fell_back: bool,
    /// Largest number of graph nodes any segment could admit before the visit bound (0 on the
    /// exact path); never more than `16 * ef_search + 1`.
    pub max_visited: usize,
}

impl VectorSegment {
    fn assemble(model: ModelRef, dims: usize, keys: Vec<u64>, vecs: Vec<i8>) -> Self {
        let norms = vecs.chunks_exact(dims).map(norm).collect();
        Self {
            model,
            dims,
            keys,
            vecs,
            norms,
            #[cfg(feature = "ann")]
            hnsw: None,
        }
    }

    pub fn model(&self) -> &ModelRef {
        &self.model
    }

    pub fn dims(&self) -> usize {
        self.dims
    }

    pub fn len(&self) -> usize {
        self.keys.len()
    }

    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }

    pub fn keys(&self) -> &[u64] {
        &self.keys
    }

    pub fn has_graph(&self) -> bool {
        #[cfg(feature = "ann")]
        {
            self.hnsw.is_some()
        }
        #[cfg(not(feature = "ann"))]
        {
            false
        }
    }

    pub fn vector(&self, key: u64) -> Option<&[i8]> {
        let i = self.keys.binary_search(&key).ok()?;
        Some(&self.vecs[i * self.dims..(i + 1) * self.dims])
    }

    /// Serialises the segment and returns its blake3 name.
    pub fn encode(&self) -> Result<(Hash, Vec<u8>)> {
        #[cfg(feature = "ann")]
        let graph: Vec<u8> = match &self.hnsw {
            Some(h) => h.to_bytes()?,
            None => Vec::new(),
        };
        #[cfg(not(feature = "ann"))]
        let graph: Vec<u8> = Vec::new();
        let mut out = Vec::with_capacity(96 + self.keys.len() * (8 + self.dims) + graph.len());
        out.extend_from_slice(&MAGIC);
        put_header(&mut out, &self.model, self.dims, self.keys.len())?;
        out.extend_from_slice(&(graph.len() as u64).to_le_bytes());
        for k in &self.keys {
            out.extend_from_slice(&k.to_le_bytes());
        }
        out.extend(self.vecs.iter().map(|&b| b as u8));
        out.extend_from_slice(&graph);
        let hash = *blake3::hash(&out).as_bytes();
        Ok((hash, out))
    }

    /// Decodes a segment, verifying its blake3 name first.
    pub fn decode(bytes: &[u8], expect: &Hash) -> Result<Self> {
        if bytes.len() as u64 > MAX_FILE_BYTES {
            return Err(VectorError::TooLarge("segment"));
        }
        if blake3::hash(bytes).as_bytes() != expect {
            return Err(VectorError::HashMismatch);
        }
        let mut r = Reader { b: bytes, at: 0 };
        if r.take(4)? != MAGIC {
            return Err(VectorError::Corrupt("magic"));
        }
        let (model, dims, count) = get_header(&mut r)?;
        let graph_len = r.u64()?;
        let key_bytes = r.take(count.checked_mul(8).ok_or(VectorError::Corrupt("count"))?)?;
        let vec_bytes = r.take(
            count
                .checked_mul(dims)
                .ok_or(VectorError::Corrupt("count"))?,
        )?;
        let graph_len = usize::try_from(graph_len).map_err(|_| VectorError::Corrupt("graph"))?;
        let graph = r.take(graph_len)?;
        if r.at != bytes.len() {
            return Err(VectorError::Corrupt("trailing bytes"));
        }
        let keys = read_keys(key_bytes)?;
        let vecs: Vec<i8> = vec_bytes.iter().map(|&b| b as i8).collect();
        let mut seg = Self::assemble(model, dims, keys, vecs);
        if !graph.is_empty() {
            #[cfg(feature = "ann")]
            {
                seg.hnsw = Some(ann::Hnsw::from_bytes(graph, dims, seg.keys.len())?);
            }
            // Without `ann` the graph is ignored and every query takes the exact path.
        }
        let _ = &mut seg;
        Ok(seg)
    }

    #[cfg(feature = "ann")]
    fn build_hnsw(&mut self) -> Result<()> {
        if self.keys.is_empty() {
            return Ok(());
        }
        self.hnsw = Some(ann::Hnsw::build(&self.keys, &self.vecs, self.dims)?);
        Ok(())
    }

    /// Replaces the in-memory graph with one memory-mapped from `path` (usearch `view`).
    /// The file must hold this segment's graph, as written by [`Self::write_graph`].
    #[cfg(feature = "ann")]
    pub fn view_graph(&mut self, path: &Path) -> Result<()> {
        let p = path.to_str().ok_or(VectorError::BadArg("graph path"))?;
        self.hnsw = Some(ann::Hnsw::view(p, self.dims, self.keys.len())?);
        Ok(())
    }

    /// Writes the graph to `path` so a replica can `view` it.
    #[cfg(feature = "ann")]
    pub fn write_graph(&self, path: &Path) -> Result<()> {
        let p = path.to_str().ok_or(VectorError::BadArg("graph path"))?;
        match &self.hnsw {
            Some(h) => h.save(p),
            None => Err(VectorError::BadArg("no graph")),
        }
    }

    fn check_query(&self, q: &[i8], model: &ModelRef) -> Result<()> {
        if &self.model != model {
            return Err(VectorError::Generation("segment and query differ"));
        }
        if q.len() != self.dims {
            return Err(VectorError::BadArg("query length"));
        }
        Ok(())
    }

    /// Exact search over the kept vectors of this segment.
    pub fn brute_force(&self, q: &[i8], k: usize, kept: &RoaringBitmap) -> Result<Vec<Hit>> {
        if q.len() != self.dims {
            return Err(VectorError::BadArg("query length"));
        }
        if k > MAX_K {
            return Err(VectorError::BadArg("k"));
        }
        let qn = norm(q);
        let mut top = TopK::new(k);
        let mut visit = |i: usize| {
            let v = &self.vecs[i * self.dims..(i + 1) * self.dims];
            top.push(Hit {
                key: self.keys[i],
                score: cosine(q, qn, v, self.norms[i]),
            });
        };
        if (kept.len() as usize).saturating_mul(16) < self.keys.len() {
            for ord in kept {
                let lo = self.keys.partition_point(|&x| x < u64::from(ord) << 32);
                let mut i = lo;
                while i < self.keys.len() && doc_ord_of(self.keys[i]) == ord {
                    visit(i);
                    i += 1;
                }
            }
        } else {
            for i in 0..self.keys.len() {
                if kept.contains(doc_ord_of(self.keys[i])) {
                    visit(i);
                }
            }
        }
        Ok(top.into_sorted())
    }

    /// HNSW `filtered_search` with the visit bound. Returns the hits (exactly rescored, and
    /// filtered once more against `kept`), the number of nodes visited and whether the bound
    /// was reached. `None` when this segment has no graph.
    #[cfg(feature = "ann")]
    fn graph_search(
        &self,
        q: &[i8],
        k: usize,
        kept: &RoaringBitmap,
        ef: usize,
        bound: usize,
    ) -> Result<Option<(Vec<Hit>, usize, bool)>> {
        let Some(h) = &self.hnsw else { return Ok(None) };
        let (keys, visited) = h.filtered(q, k, kept, ef, bound)?;
        let qn = norm(q);
        let mut top = TopK::new(k);
        for key in keys {
            if !kept.contains(doc_ord_of(key)) {
                continue;
            }
            if let Ok(i) = self.keys.binary_search(&key) {
                let v = &self.vecs[i * self.dims..(i + 1) * self.dims];
                top.push(Hit {
                    key,
                    score: cosine(q, qn, v, self.norms[i]),
                });
            }
        }
        // usearch cannot abort a traversal from the predicate, so past the bound the predicate
        // rejects every node (no further node can take a result slot) and the count reported
        // is capped at the bound plus the one evaluation that crossed it.
        Ok(Some((
            top.into_sorted(),
            visited.min(bound + 1),
            visited > bound,
        )))
    }
}

/// Searches every segment on its own and merges the lists (SPEC §23.3). All segments must carry
/// the query's model reference, else the call fails: generations are never mixed.
pub fn search(
    segments: &[&VectorSegment],
    model: &ModelRef,
    query: &[i8],
    k: usize,
    kept: &RoaringBitmap,
    sel: Selectivity,
) -> Result<Outcome> {
    if k == 0 || k > MAX_K {
        return Err(VectorError::BadArg("k"));
    }
    for s in segments {
        s.check_query(query, model)?;
    }
    let path = plan(k, sel);
    let mut merged: BTreeMap<u64, f32> = BTreeMap::new();
    let (mut exhausted, mut fell_back, mut max_visited) = (false, false, 0usize);
    for s in segments {
        let hits = match path {
            SearchPath::Brute => s.brute_force(query, k, kept)?,
            #[cfg(feature = "ann")]
            SearchPath::Hnsw {
                ef_search,
                visit_bound,
            } => match s.graph_search(query, k, kept, ef_search, visit_bound)? {
                None => s.brute_force(query, k, kept)?,
                Some((hits, visited, hit_bound)) => {
                    max_visited = max_visited.max(visited);
                    if hit_bound && hits.len() < k {
                        if sel.kept_chunks <= FALLBACK_KEPT {
                            fell_back = true;
                            s.brute_force(query, k, kept)?
                        } else {
                            exhausted = true;
                            hits
                        }
                    } else {
                        hits
                    }
                }
            },
            #[cfg(not(feature = "ann"))]
            SearchPath::Hnsw { .. } => s.brute_force(query, k, kept)?,
        };
        for h in hits {
            merged
                .entry(h.key)
                .and_modify(|x| *x = x.max(h.score))
                .or_insert(h.score);
        }
    }
    let _ = (&mut exhausted, &mut fell_back, &mut max_visited);
    let mut top = TopK::new(k);
    for (key, score) in merged {
        top.push(Hit { key, score });
    }
    Ok(Outcome {
        hits: top.into_sorted(),
        path,
        ann_exhausted: exhausted,
        fell_back,
        max_visited,
    })
}

// ---------------------------------------------------------------- codec helpers

struct Reader<'a> {
    b: &'a [u8],
    at: usize,
}

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        let end = self
            .at
            .checked_add(n)
            .ok_or(VectorError::Corrupt("length"))?;
        let s = self
            .b
            .get(self.at..end)
            .ok_or(VectorError::Corrupt("truncated"))?;
        self.at = end;
        Ok(s)
    }

    fn u32(&mut self) -> Result<u32> {
        let mut a = [0u8; 4];
        a.copy_from_slice(self.take(4)?);
        Ok(u32::from_le_bytes(a))
    }

    fn u64(&mut self) -> Result<u64> {
        let mut a = [0u8; 8];
        a.copy_from_slice(self.take(8)?);
        Ok(u64::from_le_bytes(a))
    }
}

fn put_header(out: &mut Vec<u8>, m: &ModelRef, dims: usize, count: usize) -> Result<()> {
    m.validate()?;
    out.extend_from_slice(&FORMAT_VERSION.to_le_bytes());
    out.extend_from_slice(&m.model_gen.to_le_bytes());
    out.extend_from_slice(&(dims as u32).to_le_bytes());
    out.extend_from_slice(&(count as u32).to_le_bytes());
    out.extend_from_slice(&m.calib);
    out.extend_from_slice(&(m.model_id.len() as u32).to_le_bytes());
    out.extend_from_slice(m.model_id.as_bytes());
    Ok(())
}

fn get_header(r: &mut Reader<'_>) -> Result<(ModelRef, usize, usize)> {
    if r.u32()? != FORMAT_VERSION {
        return Err(VectorError::Corrupt("version"));
    }
    let model_gen = r.u32()?;
    let dims = r.u32()? as usize;
    let count = r.u32()? as usize;
    if dims == 0 || dims > MAX_DIMS {
        return Err(VectorError::Corrupt("dims"));
    }
    if count > MAX_VECTORS {
        return Err(VectorError::TooLarge("segment vectors"));
    }
    let mut calib = [0u8; 32];
    calib.copy_from_slice(r.take(32)?);
    let id_len = r.u32()? as usize;
    if id_len == 0 || id_len > MAX_MODEL_ID {
        return Err(VectorError::Corrupt("model id length"));
    }
    let model_id = std::str::from_utf8(r.take(id_len)?)
        .map_err(|_| VectorError::Corrupt("model id utf8"))?
        .to_owned();
    Ok((
        ModelRef {
            model_gen,
            model_id,
            calib,
        },
        dims,
        count,
    ))
}

fn read_keys(b: &[u8]) -> Result<Vec<u64>> {
    let mut keys = Vec::with_capacity(b.len() / 8);
    let mut prev: Option<u64> = None;
    for c in b.chunks_exact(8) {
        let mut a = [0u8; 8];
        a.copy_from_slice(c);
        let k = u64::from_le_bytes(a);
        if prev.is_some_and(|p| p >= k) {
            return Err(VectorError::Corrupt("keys not strictly ascending"));
        }
        prev = Some(k);
        keys.push(k);
    }
    Ok(keys)
}

// ---------------------------------------------------------------- b1 local format

fn b1_len(dims: usize) -> usize {
    dims.div_ceil(8)
}

/// Sign code of an i8 vector: bit `i` is set when component `i` is not negative.
pub fn b1_code(v: &[i8]) -> Vec<u8> {
    let mut out = vec![0u8; b1_len(v.len())];
    for (i, &x) in v.iter().enumerate() {
        if x >= 0 {
            out[i / 8] |= 1 << (i % 8);
        }
    }
    out
}

fn hamming(a: &[u8], b: &[u8]) -> u32 {
    a.iter().zip(b).map(|(&x, &y)| (x ^ y).count_ones()).sum()
}

/// The daemon's local vector file (SPEC §23.2.4, §23.2.5): b1 codes for the cheap first pass,
/// the i8 vectors for rescoring the top 200, and a header with the model id and calibration
/// digest. A file of another generation is dropped and rebuilt, never migrated.
pub struct LocalVectors {
    model: ModelRef,
    dims: usize,
    keys: Vec<u64>,
    codes: Vec<u8>,
    vecs: Vec<i8>,
    norms: Vec<f32>,
}

impl LocalVectors {
    pub fn from_segment(seg: &VectorSegment) -> Self {
        let mut codes = Vec::with_capacity(seg.keys.len() * b1_len(seg.dims));
        for v in seg.vecs.chunks_exact(seg.dims) {
            codes.extend_from_slice(&b1_code(v));
        }
        Self {
            model: seg.model.clone(),
            dims: seg.dims,
            keys: seg.keys.clone(),
            codes,
            vecs: seg.vecs.clone(),
            norms: seg.norms.clone(),
        }
    }

    pub fn model(&self) -> &ModelRef {
        &self.model
    }

    pub fn len(&self) -> usize {
        self.keys.len()
    }

    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }

    /// File bytes: header, keys, b1 codes, i8 vectors, then a blake3 trailer over all of it.
    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut out =
            Vec::with_capacity(96 + self.keys.len() * (8 + b1_len(self.dims) + self.dims) + 32);
        out.extend_from_slice(&LOCAL_MAGIC);
        put_header(&mut out, &self.model, self.dims, self.keys.len())?;
        for k in &self.keys {
            out.extend_from_slice(&k.to_le_bytes());
        }
        out.extend_from_slice(&self.codes);
        out.extend(self.vecs.iter().map(|&b| b as u8));
        let h = *blake3::hash(&out).as_bytes();
        out.extend_from_slice(&h);
        Ok(out)
    }

    /// Decodes a local file. A header whose model differs from `expect` yields `Ok(None)`: the
    /// caller drops the file and rebuilds. A damaged file is an error.
    pub fn decode(bytes: &[u8], expect: &ModelRef) -> Result<Option<Self>> {
        if bytes.len() as u64 > MAX_FILE_BYTES {
            return Err(VectorError::TooLarge("local vectors"));
        }
        let body_len = bytes
            .len()
            .checked_sub(32)
            .ok_or(VectorError::Corrupt("truncated"))?;
        let (body, trailer) = bytes.split_at(body_len);
        if blake3::hash(body).as_bytes() != trailer {
            return Err(VectorError::HashMismatch);
        }
        let mut r = Reader { b: body, at: 0 };
        if r.take(4)? != LOCAL_MAGIC {
            return Err(VectorError::Corrupt("magic"));
        }
        let (model, dims, count) = get_header(&mut r)?;
        if &model != expect {
            return Ok(None);
        }
        let key_bytes = r.take(count.checked_mul(8).ok_or(VectorError::Corrupt("count"))?)?;
        let code_bytes = r.take(
            count
                .checked_mul(b1_len(dims))
                .ok_or(VectorError::Corrupt("count"))?,
        )?;
        let vec_bytes = r.take(
            count
                .checked_mul(dims)
                .ok_or(VectorError::Corrupt("count"))?,
        )?;
        if r.at != body.len() {
            return Err(VectorError::Corrupt("trailing bytes"));
        }
        let keys = read_keys(key_bytes)?;
        let vecs: Vec<i8> = vec_bytes.iter().map(|&b| b as i8).collect();
        for (v, c) in vecs
            .chunks_exact(dims)
            .zip(code_bytes.chunks_exact(b1_len(dims)))
        {
            if b1_code(v) != c {
                return Err(VectorError::Corrupt("b1 code does not match its vector"));
            }
        }
        let norms = vecs.chunks_exact(dims).map(norm).collect();
        Ok(Some(Self {
            model,
            dims,
            keys,
            codes: code_bytes.to_vec(),
            vecs,
            norms,
        }))
    }

    /// b1 Hamming prefilter over the kept vectors, then i8 cosine rescoring of the best
    /// `max(200, k)` candidates.
    pub fn search(
        &self,
        model: &ModelRef,
        q: &[i8],
        k: usize,
        kept: &RoaringBitmap,
    ) -> Result<Vec<Hit>> {
        if &self.model != model {
            return Err(VectorError::Generation("local file and query differ"));
        }
        if q.len() != self.dims {
            return Err(VectorError::BadArg("query length"));
        }
        if k == 0 || k > MAX_K {
            return Err(VectorError::BadArg("k"));
        }
        let qc = b1_code(q);
        let w = b1_len(self.dims);
        let n = RESCORE_N.max(k);
        // Max-heap on (distance, key): the root is the worst candidate kept so far.
        let mut cand: BinaryHeap<(u32, u64, usize)> = BinaryHeap::with_capacity(n + 1);
        for (i, &key) in self.keys.iter().enumerate() {
            if !kept.contains(doc_ord_of(key)) {
                continue;
            }
            let d = hamming(&qc, &self.codes[i * w..(i + 1) * w]);
            if cand.len() < n {
                cand.push((d, key, i));
            } else if let Some(&(wd, wk, _)) = cand.peek()
                && (d, key) < (wd, wk)
            {
                cand.pop();
                cand.push((d, key, i));
            }
        }
        let qn = norm(q);
        let mut top = TopK::new(k);
        for (_, key, i) in cand {
            let v = &self.vecs[i * self.dims..(i + 1) * self.dims];
            top.push(Hit {
                key,
                score: cosine(q, qn, v, self.norms[i]),
            });
        }
        Ok(top.into_sorted())
    }
}

/// `<root>/vec/<model_gen>/` for the local vectors of one generation.
pub fn generation_dir(root: &Path, model_gen: u32) -> PathBuf {
    root.join("vec").join(model_gen.to_string())
}

/// Rejects any symlink between `root` (exclusive) and `path`, and any `..`.
fn no_symlinks_below(root: &Path, path: &Path) -> Result<()> {
    let rel = path
        .strip_prefix(root)
        .map_err(|_| VectorError::BadArg("path outside root"))?;
    let mut cur = root.to_path_buf();
    for c in rel.components() {
        match c {
            Component::Normal(n) => cur.push(n),
            _ => return Err(VectorError::BadArg("path component")),
        }
        match std::fs::symlink_metadata(&cur) {
            Ok(m) if m.file_type().is_symlink() => {
                return Err(VectorError::BadArg("symlink in path"));
            }
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}

/// Reads a local vector file under `root`. `Ok(None)` when absent or of another generation.
pub fn load_local(root: &Path, file: &Path, expect: &ModelRef) -> Result<Option<LocalVectors>> {
    no_symlinks_below(root, file)?;
    let meta = match std::fs::symlink_metadata(file) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    if !meta.is_file() {
        return Err(VectorError::BadArg("not a regular file"));
    }
    if meta.len() > MAX_FILE_BYTES {
        return Err(VectorError::TooLarge("local vectors"));
    }
    LocalVectors::decode(&std::fs::read(file)?, expect)
}

/// Deletes every `vec/<n>/` directory except the active generation's. Vectors of another
/// generation are dropped, never migrated. Symlinks and non-numeric names are left alone.
/// Returns the number of directories removed.
pub fn drop_stale_generations(root: &Path, active_gen: u32) -> Result<usize> {
    let vec_dir = root.join("vec");
    no_symlinks_below(root, &vec_dir)?;
    let rd = match std::fs::read_dir(&vec_dir) {
        Ok(r) => r,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(e) => return Err(e.into()),
    };
    let active = active_gen.to_string();
    let mut removed = 0;
    for e in rd {
        let e = e?;
        let name = e.file_name();
        let Some(n) = name.to_str() else { continue };
        if n == active || n.is_empty() || !n.bytes().all(|b| b.is_ascii_digit()) {
            continue;
        }
        if e.file_type()?.is_dir() {
            std::fs::remove_dir_all(e.path())?;
            removed += 1;
        }
    }
    Ok(removed)
}

// ---------------------------------------------------------------- usearch backend

#[cfg(feature = "ann")]
mod ann {
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use roaring::RoaringBitmap;
    use usearch::ffi::{IndexOptions, MetricKind, ScalarKind};

    use super::{EF_CONSTRUCTION, EF_MIN, HNSW_M, VectorError, doc_ord_of};

    fn err(e: impl std::fmt::Display) -> VectorError {
        VectorError::Ann(e.to_string())
    }

    fn options(dims: usize, ef: usize) -> IndexOptions {
        IndexOptions {
            dimensions: dims,
            metric: MetricKind::Cos,
            quantization: ScalarKind::I8,
            connectivity: HNSW_M,
            expansion_add: EF_CONSTRUCTION,
            expansion_search: ef,
            multi: false,
        }
    }

    /// usearch index guarded by a mutex because `ef_search` is set per query.
    pub struct Hnsw(Mutex<usearch::Index>);

    impl Hnsw {
        pub fn build(keys: &[u64], vecs: &[i8], dims: usize) -> Result<Self, VectorError> {
            let idx = usearch::new_index(&options(dims, EF_MIN)).map_err(err)?;
            idx.reserve(keys.len()).map_err(err)?;
            for (i, &k) in keys.iter().enumerate() {
                idx.add::<i8>(k, &vecs[i * dims..(i + 1) * dims])
                    .map_err(err)?;
            }
            Ok(Self(Mutex::new(idx)))
        }

        fn checked(idx: usearch::Index, dims: usize, count: usize) -> Result<Self, VectorError> {
            if idx.dimensions() != dims || idx.size() != count || idx.multi() {
                return Err(VectorError::Corrupt("graph does not match segment"));
            }
            Ok(Self(Mutex::new(idx)))
        }

        pub fn from_bytes(b: &[u8], dims: usize, count: usize) -> Result<Self, VectorError> {
            let idx = usearch::new_index(&options(dims, EF_MIN)).map_err(err)?;
            idx.load_from_buffer(b).map_err(err)?;
            Self::checked(idx, dims, count)
        }

        pub fn to_bytes(&self) -> Result<Vec<u8>, VectorError> {
            let idx = self.0.lock().map_err(|_| err("poisoned"))?;
            let mut buf = vec![0u8; idx.serialized_length()];
            idx.save_to_buffer(&mut buf).map_err(err)?;
            Ok(buf)
        }

        pub fn save(&self, path: &str) -> Result<(), VectorError> {
            self.0
                .lock()
                .map_err(|_| err("poisoned"))?
                .save(path)
                .map_err(err)
        }

        pub fn view(path: &str, dims: usize, count: usize) -> Result<Self, VectorError> {
            let idx = usearch::new_index(&options(dims, EF_MIN)).map_err(err)?;
            idx.view(path).map_err(err)?;
            Self::checked(idx, dims, count)
        }

        /// Filtered traversal. The predicate counts every node the search asks about; once the
        /// count passes `bound` it rejects everything, which ends the traversal. Returns the
        /// keys found and the number of nodes visited.
        pub fn filtered(
            &self,
            q: &[i8],
            k: usize,
            kept: &RoaringBitmap,
            ef: usize,
            bound: usize,
        ) -> Result<(Vec<u64>, usize), VectorError> {
            let idx = self.0.lock().map_err(|_| err("poisoned"))?;
            idx.change_expansion_search(ef);
            let visits = AtomicUsize::new(0);
            let m = idx
                .filtered_search::<i8, _>(q, k, |key| {
                    let n = visits.fetch_add(1, Ordering::Relaxed) + 1;
                    n <= bound && kept.contains(doc_ord_of(key))
                })
                .map_err(err)?;
            Ok((m.keys, visits.load(Ordering::Relaxed)))
        }
    }
}
