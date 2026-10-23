//! Model generations and vector-segment state (SPEC §23.5, "Model changes").
//!
//! A model id is `name@<sha256>/<quantisation>/calib@<digest>/<chunker_ver>`. Any change to it,
//! a new calibration included, creates `model_gen + 1`. Queries keep using the serving
//! generation until the building one reaches coverage >= 0.99 on every land-branch tip; then
//! `model_gen` flips once, atomically (a CAS on the `ci_manifest` row). The old vectors are
//! deleted 7 days later. Vectors of two generations are never compared, and the query-embedding
//! cache is keyed by the generation, so it cannot return an embedding of another one.
//!
//! The state is one `ci_manifest` row `(ns, 'vec')`: `model_gen` is the serving generation and
//! `segments` holds a JSON [`VecState`]. Every change is a CAS on `generation`.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};

use asc_codeintel::index::IndexError;
use asc_codeintel::index::segments::{Hash, get_verified, hex};
use asc_codeintel::vectors::{VectorError, VectorSegment};
use asc_store::codec::encode_stored;
use asc_store::key::ObjKey;
use asc_store::objects::Objects;
use roaring::{RoaringBitmap, RoaringTreemap};
use serde::{Deserialize, Serialize};
use sqlx::{AnyPool, Row};

/// `ci_manifest.idx` of the vector state.
pub const IDX_VEC: &str = "vec";
/// Coverage (in percent) the building generation needs on every land tip before it serves.
pub const FLIP_PERCENT: u128 = 99;
/// Old generations are deleted this long after they stop serving.
pub const RETAIN_MS: u64 = 7 * 24 * 3600 * 1000;
/// Dropped vector segments stay this long, like the other index objects.
pub const DROP_GRACE_MS: u64 = 10 * 60 * 1000;
/// CAS attempts before a state change gives up.
pub const MAX_CAS: u32 = 16;
/// Most live vector segments a state may name.
pub const MAX_VEC_SEGMENTS: usize = 4096;
/// Most models a state remembers.
pub const MAX_MODELS: usize = 64;
/// Largest state blob read from the database.
const MAX_STATE_BYTES: usize = 16 << 20;
/// Largest vector segment object read back.
pub const MAX_VEC_OBJECT: u64 = 1 << 30;
/// Ords per SQL `IN` list.
const IN_CHUNK: usize = 400;
/// Query embeddings kept in the cache, in bytes.
pub const LRU_BYTES: usize = 64 << 20;
/// Longest query text the cache keys.
pub const MAX_QUERY_BYTES: usize = 64 * 1024;

/// Why a code-intelligence worker operation failed.
#[derive(Debug, thiserror::Error)]
pub enum CiError {
    #[error("database: {0}")]
    Sql(#[from] sqlx::Error),
    #[error("index: {0}")]
    Index(#[from] IndexError),
    #[error("vectors: {0}")]
    Vector(#[from] VectorError),
    #[error("queue: {0}")]
    Queue(#[from] super::queue::QueueError),
    #[error("store: {0}")]
    Store(String),
    #[error("state is corrupt: {0}")]
    Corrupt(&'static str),
    #[error("invalid: {0}")]
    Invalid(&'static str),
    #[error("state is contended; gave up after {0} attempts")]
    Contended(u32),
}

pub type Result<T, E = CiError> = std::result::Result<T, E>;

pub(crate) fn store_err(e: impl std::fmt::Display) -> CiError {
    CiError::Store(e.to_string())
}

// ------------------------------------------------------------------ the shared state row

/// One row of `ci_manifest`: `(generation, model_gen, blob)`.
pub(crate) async fn load_row(
    pool: &AnyPool,
    ns: &[u8],
    idx: &str,
) -> Result<Option<(i64, i64, Vec<u8>)>> {
    let row = sqlx::query(
        "SELECT CAST(generation AS BIGINT) AS generation, CAST(model_gen AS BIGINT) AS model_gen, \
                segments FROM ci_manifest WHERE ns = $1 AND idx = $2",
    )
    .bind(ns.to_vec())
    .bind(idx.to_owned())
    .fetch_optional(pool)
    .await?;
    let Some(r) = row else { return Ok(None) };
    let blob: Vec<u8> = r.try_get("segments")?;
    if blob.len() > MAX_STATE_BYTES {
        return Err(CiError::Corrupt("state row too large"));
    }
    Ok(Some((
        r.try_get("generation")?,
        r.try_get("model_gen")?,
        blob,
    )))
}

/// Writes generation `expected + 1` when the row still holds `expected` (0 = no row). True when
/// this call won.
pub(crate) async fn cas_row(
    pool: &AnyPool,
    ns: &[u8],
    idx: &str,
    expected: i64,
    model_gen: i64,
    blob: &[u8],
) -> Result<bool> {
    if blob.len() > MAX_STATE_BYTES {
        return Err(CiError::Corrupt("state row too large"));
    }
    if expected == 0 {
        let n = sqlx::query(
            "INSERT INTO ci_manifest (ns, idx, generation, segments, model_gen) \
             VALUES ($1, $2, 1, $3, $4) ON CONFLICT (ns, idx) DO NOTHING",
        )
        .bind(ns.to_vec())
        .bind(idx.to_owned())
        .bind(blob.to_vec())
        .bind(model_gen)
        .execute(pool)
        .await?
        .rows_affected();
        return Ok(n == 1);
    }
    let n = sqlx::query(
        "UPDATE ci_manifest SET generation = $1, segments = $2, model_gen = $3 \
         WHERE ns = $4 AND idx = $5 AND generation = $6",
    )
    .bind(expected + 1)
    .bind(blob.to_vec())
    .bind(model_gen)
    .bind(ns.to_vec())
    .bind(idx.to_owned())
    .bind(expected)
    .execute(pool)
    .await?
    .rows_affected();
    Ok(n == 1)
}

// ------------------------------------------------------------------ the vector state

/// The model behind a generation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelEntry {
    pub model_gen: u32,
    pub model_id: String,
    pub calib: String,
}

/// A live vector segment.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct VecSeg {
    pub model_gen: u32,
    /// blake3 of the encoded segment, lower-case hex.
    pub hash: String,
    pub vectors: u64,
    pub bytes: u64,
}

/// A segment a state stopped naming; deleted after [`DROP_GRACE_MS`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct VecDrop {
    pub model_gen: u32,
    pub hash: String,
    pub at_ms: u64,
}

/// A generation that stopped serving; its vectors are deleted [`RETAIN_MS`] later.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Retired {
    pub model_gen: u32,
    pub at_ms: u64,
}

/// The vector side of `ci_manifest`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct VecState {
    pub models: Vec<ModelEntry>,
    pub building: Option<u32>,
    pub segments: Vec<VecSeg>,
    pub dropped: Vec<VecDrop>,
    pub retired: Vec<Retired>,
}

/// A loaded row: the CAS token, the serving generation (0 = none yet) and the state.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct VecRow {
    pub generation: i64,
    pub serving: u32,
    pub state: VecState,
}

/// What a model is, apart from its generation number.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModelSpec {
    pub model_id: String,
    pub calib: [u8; 32],
    pub dims: usize,
}

impl ModelSpec {
    pub fn model_ref(&self, model_gen: u32) -> asc_codeintel::vectors::ModelRef {
        asc_codeintel::vectors::ModelRef {
            model_gen,
            model_id: self.model_id.clone(),
            calib: self.calib,
        }
    }
}

/// Coverage of one land tip: chunks that have a vector in the building generation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Coverage {
    pub have: u64,
    pub want: u64,
}

impl Coverage {
    /// True at >= 99%. A tip without chunks is covered.
    pub fn meets(&self) -> bool {
        self.want == 0 || u128::from(self.have) * 100 >= u128::from(self.want) * FLIP_PERCENT
    }

    pub fn ratio(&self) -> f64 {
        if self.want == 0 {
            1.0
        } else {
            self.have as f64 / self.want as f64
        }
    }
}

/// What [`ModelGens::try_flip`] did.
#[derive(Clone, Debug, PartialEq)]
pub enum Flip {
    /// `model_gen` is now `to`; `from` is 0 when nothing served before.
    Flipped { from: u32, to: u32 },
    /// A tip is below 99%; `worst` is the lowest coverage seen.
    NotYet { worst: f64 },
    /// The generation is not the building one (already serving, or abandoned).
    NotBuilding,
}

pub fn parse_hash(s: &str) -> Option<Hash> {
    if s.len() != 64 || !s.is_ascii() {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, b) in out.iter_mut().enumerate() {
        *b = u8::from_str_radix(&s[2 * i..2 * i + 2], 16).ok()?;
    }
    Some(out)
}

/// Key of a vector segment object, scoped by repository namespace and generation.
pub fn vseg_key(ns: &[u8], model_gen: u32, h: Hash) -> ObjKey {
    let mut k = blake3::Hasher::new_derive_key("asc v1 ci vseg");
    k.update(&(ns.len() as u64).to_le_bytes());
    k.update(ns);
    k.update(&model_gen.to_le_bytes());
    k.update(&h);
    ObjKey::Derived {
        name: "vseg",
        version: 1,
        id: *k.finalize().as_bytes(),
    }
}

/// Vector-segment state of every namespace, over the database and the object store.
#[derive(Clone)]
pub struct ModelGens {
    pool: AnyPool,
    objects: Arc<dyn Objects>,
}

impl ModelGens {
    pub fn new(pool: AnyPool, objects: Arc<dyn Objects>) -> Self {
        Self { pool, objects }
    }

    pub fn objects(&self) -> &Arc<dyn Objects> {
        &self.objects
    }

    /// The current row (generation 0 with an empty state when none exists).
    pub async fn load(&self, ns: &[u8]) -> Result<VecRow> {
        let Some((generation, serving, blob)) = load_row(&self.pool, ns, IDX_VEC).await? else {
            return Ok(VecRow::default());
        };
        let state: VecState =
            serde_json::from_slice(&blob).map_err(|_| CiError::Corrupt("vector state"))?;
        let serving = u32::try_from(serving).map_err(|_| CiError::Corrupt("model_gen"))?;
        Ok(VecRow {
            generation,
            serving,
            state,
        })
    }

    /// Applies `f` under a CAS loop. `f` returns false to leave the row alone. Returns the row
    /// as written (or as read, when `f` declined).
    pub async fn update(
        &self,
        ns: &[u8],
        mut f: impl FnMut(&mut VecRow) -> bool,
    ) -> Result<VecRow> {
        for _ in 0..MAX_CAS {
            let mut row = self.load(ns).await?;
            if !f(&mut row) {
                return Ok(row);
            }
            if row.state.segments.len() > MAX_VEC_SEGMENTS || row.state.models.len() > MAX_MODELS {
                return Err(CiError::Corrupt("state limits"));
            }
            let blob = serde_json::to_vec(&row.state).map_err(|_| CiError::Corrupt("encode"))?;
            if cas_row(
                &self.pool,
                ns,
                IDX_VEC,
                row.generation,
                i64::from(row.serving),
                &blob,
            )
            .await?
            {
                row.generation += 1;
                return Ok(row);
            }
        }
        Err(CiError::Contended(MAX_CAS))
    }

    /// Makes sure the generation of `spec` exists, and returns it. The serving generation
    /// answers when it already has this model id; otherwise the model builds as `max + 1`
    /// (a build in progress for another model is abandoned and retired).
    pub async fn ensure_model(&self, ns: &[u8], spec: &ModelSpec, now_ms: u64) -> Result<u32> {
        let mut out = 0;
        self.update(ns, |row| {
            if let Some(m) = row
                .state
                .models
                .iter()
                .find(|m| m.model_id == spec.model_id)
                && (m.model_gen == row.serving || Some(m.model_gen) == row.state.building)
            {
                out = m.model_gen;
                return false;
            }
            let next = row
                .state
                .models
                .iter()
                .map(|m| m.model_gen)
                .max()
                .unwrap_or(0)
                + 1;
            if let Some(old) = row.state.building.take() {
                row.state.retired.push(Retired {
                    model_gen: old,
                    at_ms: now_ms,
                });
            }
            row.state.models.push(ModelEntry {
                model_gen: next,
                model_id: spec.model_id.clone(),
                calib: hex(&spec.calib),
            });
            row.state.building = Some(next);
            out = next;
            true
        })
        .await?;
        Ok(out)
    }

    /// The generation the worker should embed for: the building one, else the serving one.
    pub fn target_gen(row: &VecRow) -> Option<u32> {
        row.state
            .building
            .or((row.serving != 0).then_some(row.serving))
    }

    /// True when `model_gen` still takes vectors.
    pub fn is_live_gen(row: &VecRow, model_gen: u32) -> bool {
        model_gen != 0 && (row.serving == model_gen || row.state.building == Some(model_gen))
    }

    /// Stores a segment object (idempotent) and returns its state entry without naming it.
    pub async fn put_segment(
        &self,
        ns: &[u8],
        model_gen: u32,
        seg: &VectorSegment,
    ) -> Result<VecSeg> {
        let (hash, bytes) = seg.encode()?;
        self.objects
            .put(&vseg_key(ns, model_gen, hash), encode_stored(&bytes))
            .await
            .map_err(store_err)?;
        Ok(VecSeg {
            model_gen,
            hash: hex(&hash),
            vectors: seg.len() as u64,
            bytes: bytes.len() as u64,
        })
    }

    /// Publishes a segment: stores the object, then names it in the state. A generation that
    /// no longer takes vectors drops it.
    pub async fn add_segment(
        &self,
        ns: &[u8],
        model_gen: u32,
        seg: &VectorSegment,
    ) -> Result<VecSeg> {
        let entry = self.put_segment(ns, model_gen, seg).await?;
        let e = entry.clone();
        self.update(ns, |row| {
            if !Self::is_live_gen(row, model_gen) {
                return false;
            }
            if row.state.segments.iter().any(|s| s.hash == e.hash) {
                return false;
            }
            row.state.segments.push(e.clone());
            true
        })
        .await?;
        Ok(entry)
    }

    /// Reads and verifies one segment.
    pub async fn read_segment(&self, ns: &[u8], seg: &VecSeg) -> Result<VectorSegment> {
        let hash = parse_hash(&seg.hash).ok_or(CiError::Corrupt("segment hash"))?;
        let bytes = get_verified(
            &*self.objects,
            vseg_key(ns, seg.model_gen, hash),
            hash,
            MAX_VEC_OBJECT,
        )
        .await?;
        Ok(VectorSegment::decode(&bytes, &hash)?)
    }

    /// Swaps `remove` for `add` (a merge). False when an input is gone (someone else merged).
    pub async fn replace_segments(
        &self,
        ns: &[u8],
        remove: &[VecSeg],
        add: VecSeg,
        now_ms: u64,
    ) -> Result<bool> {
        let mut applied = false;
        self.update(ns, |row| {
            applied = false;
            if !remove
                .iter()
                .all(|r| row.state.segments.iter().any(|s| s.hash == r.hash))
            {
                return false;
            }
            row.state
                .segments
                .retain(|s| !remove.iter().any(|r| r.hash == s.hash));
            if !row.state.segments.iter().any(|s| s.hash == add.hash) {
                row.state.segments.push(add.clone());
            }
            for r in remove {
                if r.hash != add.hash {
                    row.state.dropped.push(VecDrop {
                        model_gen: r.model_gen,
                        hash: r.hash.clone(),
                        at_ms: now_ms,
                    });
                }
            }
            applied = true;
            true
        })
        .await?;
        Ok(applied)
    }

    /// Chunk keys (`chunk_key(doc_ord, chunk)`) that `ords` own, from `ci_chunks`.
    pub async fn chunk_keys(&self, ns: &[u8], ords: &RoaringBitmap) -> Result<RoaringTreemap> {
        let mut out = RoaringTreemap::new();
        let all: Vec<u32> = ords.iter().collect();
        for part in all.chunks(IN_CHUNK) {
            let marks: Vec<String> = (0..part.len()).map(|i| format!("${}", i + 2)).collect();
            let sql = format!(
                "SELECT CAST(chunk_ord AS BIGINT) AS chunk_ord FROM ci_chunks \
                 WHERE ns = $1 AND doc_ord IN ({})",
                marks.join(", ")
            );
            let mut q = sqlx::query(sqlx::AssertSqlSafe(sql)).bind(ns.to_vec());
            for o in part {
                q = q.bind(i64::from(*o));
            }
            for r in q.fetch_all(&self.pool).await? {
                let k: i64 = r.try_get("chunk_ord")?;
                out.insert(u64::try_from(k).map_err(|_| CiError::Corrupt("chunk_ord"))?);
            }
        }
        Ok(out)
    }

    /// The chunk keys that have a vector in `model_gen`.
    pub async fn embedded_keys(&self, ns: &[u8], model_gen: u32) -> Result<RoaringTreemap> {
        let row = self.load(ns).await?;
        let mut out = RoaringTreemap::new();
        for s in row
            .state
            .segments
            .iter()
            .filter(|s| s.model_gen == model_gen)
        {
            for k in self.read_segment(ns, s).await?.keys() {
                out.insert(*k);
            }
        }
        Ok(out)
    }

    /// Coverage of each tip (the ords its scope holds) in `model_gen`.
    pub async fn coverage(
        &self,
        ns: &[u8],
        model_gen: u32,
        tips: &[RoaringBitmap],
    ) -> Result<Vec<Coverage>> {
        let have = self.embedded_keys(ns, model_gen).await?;
        let mut out = Vec::with_capacity(tips.len());
        for t in tips {
            let want = self.chunk_keys(ns, t).await?;
            out.push(Coverage {
                have: (&want & &have).len(),
                want: want.len(),
            });
        }
        Ok(out)
    }

    /// Embed jobs for the chunks of `ords` that have no vector in `model_gen` yet (the chunks
    /// reachable from warmed heads, at backfill priority when a new generation starts).
    pub async fn enqueue_missing(
        &self,
        queue: &super::queue::Queue,
        ns: &[u8],
        model_gen: u32,
        ords: &RoaringBitmap,
        priority: super::queue::Priority,
        now_secs: i64,
    ) -> Result<u64> {
        let have = self.embedded_keys(ns, model_gen).await?;
        let want = self.chunk_keys(ns, ords).await?;
        let mut jobs = Vec::new();
        for key in want.iter().filter(|k| !have.contains(*k)) {
            let row = sqlx::query("SELECT text_id FROM ci_chunks WHERE ns = $1 AND chunk_ord = $2")
                .bind(ns.to_vec())
                .bind(key as i64)
                .fetch_optional(&self.pool)
                .await?;
            let Some(row) = row else { continue };
            let text: Vec<u8> = row.try_get("text_id")?;
            jobs.push(super::worker::embed_job(
                &text,
                asc_codeintel::vectors::doc_ord_of(key),
                asc_codeintel::vectors::chunk_of(key),
                model_gen,
                priority,
            )?);
        }
        let mut n = 0;
        for part in jobs.chunks(super::queue::MAX_ENQUEUE) {
            n += queue.enqueue(ns, part, now_secs).await?;
        }
        Ok(n)
    }

    /// Flips `model_gen` to `target` when every land tip meets coverage. `tips` are the ords of
    /// the land-branch tips' scopes; with none, nothing flips. Flips once: a second call finds
    /// `target` no longer building.
    pub async fn try_flip(
        &self,
        ns: &[u8],
        target: u32,
        tips: &[RoaringBitmap],
        now_ms: u64,
    ) -> Result<Flip> {
        let row = self.load(ns).await?;
        if row.state.building != Some(target) {
            return Ok(Flip::NotBuilding);
        }
        if tips.is_empty() {
            return Ok(Flip::NotYet { worst: 0.0 });
        }
        let cov = self.coverage(ns, target, tips).await?;
        if let Some(w) = cov
            .iter()
            .filter(|c| !c.meets())
            .map(Coverage::ratio)
            .min_by(f64::total_cmp)
        {
            return Ok(Flip::NotYet { worst: w });
        }
        let mut from = 0;
        let mut flipped = false;
        self.update(ns, |row| {
            flipped = false;
            if row.state.building != Some(target) {
                return false;
            }
            from = row.serving;
            if from != 0 {
                row.state.retired.push(Retired {
                    model_gen: from,
                    at_ms: now_ms,
                });
            }
            row.serving = target;
            row.state.building = None;
            flipped = true;
            true
        })
        .await?;
        Ok(if flipped {
            Flip::Flipped { from, to: target }
        } else {
            Flip::NotBuilding
        })
    }

    /// Deletes dropped segments after the grace, and the vectors of generations that stopped
    /// serving at least [`RETAIN_MS`] ago. Returns the number of objects deleted.
    pub async fn gc(&self, ns: &[u8], now_ms: u64) -> Result<usize> {
        let mut doomed: Vec<(u32, String)> = Vec::new();
        self.update(ns, |row| {
            doomed.clear();
            let old: Vec<u32> = row
                .state
                .retired
                .iter()
                .filter(|r| r.at_ms.saturating_add(RETAIN_MS) <= now_ms)
                .map(|r| r.model_gen)
                .filter(|g| *g != row.serving && Some(*g) != row.state.building)
                .collect();
            for d in &row.state.dropped {
                if d.at_ms.saturating_add(DROP_GRACE_MS) <= now_ms {
                    doomed.push((d.model_gen, d.hash.clone()));
                }
            }
            for s in &row.state.segments {
                if old.contains(&s.model_gen) {
                    doomed.push((s.model_gen, s.hash.clone()));
                }
            }
            if doomed.is_empty() && old.is_empty() {
                return false;
            }
            row.state
                .dropped
                .retain(|d| d.at_ms.saturating_add(DROP_GRACE_MS) > now_ms);
            row.state.segments.retain(|s| !old.contains(&s.model_gen));
            row.state.retired.retain(|r| !old.contains(&r.model_gen));
            true
        })
        .await?;
        let mut n = 0;
        for (g, h) in doomed {
            if let Some(hash) = parse_hash(&h) {
                self.objects
                    .delete(&vseg_key(ns, g, hash))
                    .await
                    .map_err(store_err)?;
                n += 1;
            }
        }
        Ok(n)
    }

    /// The tombstoned ords (flag bit set by the sweep).
    pub async fn tombstones(&self, ns: &[u8]) -> Result<RoaringBitmap> {
        super::sweep::tombstoned(&self.pool, ns).await
    }
}

// ------------------------------------------------------------------ the query-embedding cache

type LruKey = (u32, [u8; 32], [u8; 32]);

struct LruInner {
    map: HashMap<LruKey, (Arc<Vec<f32>>, u64)>,
    order: BTreeMap<u64, LruKey>,
    seq: u64,
    bytes: usize,
}

/// The query-embedding LRU: keyed by `(model_gen, calibration digest, blake3(text))`, bounded
/// in bytes. An entry of one generation can never answer a lookup of another.
pub struct QueryLru {
    inner: Mutex<LruInner>,
    cap: usize,
}

fn entry_bytes(v: &[f32]) -> usize {
    v.len() * 4 + 160
}

impl QueryLru {
    pub fn new(cap_bytes: usize) -> Self {
        Self {
            inner: Mutex::new(LruInner {
                map: HashMap::new(),
                order: BTreeMap::new(),
                seq: 0,
                bytes: 0,
            }),
            cap: cap_bytes,
        }
    }

    fn key(model_gen: u32, calib: &[u8; 32], text: &str) -> LruKey {
        (model_gen, *calib, *blake3::hash(text.as_bytes()).as_bytes())
    }

    pub fn get(&self, model_gen: u32, calib: &[u8; 32], text: &str) -> Option<Arc<Vec<f32>>> {
        let k = Self::key(model_gen, calib, text);
        let mut g = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        let g = &mut *g;
        g.seq += 1;
        let seq = g.seq;
        let (v, old) = g.map.get_mut(&k).map(|e| (e.0.clone(), e.1))?;
        g.order.remove(&old);
        g.order.insert(seq, k);
        if let Some(e) = g.map.get_mut(&k) {
            e.1 = seq;
        }
        Some(v)
    }

    /// Stores an embedding. Texts over [`MAX_QUERY_BYTES`] and entries over the whole budget
    /// are not cached.
    pub fn put(&self, model_gen: u32, calib: &[u8; 32], text: &str, v: Vec<f32>) {
        if text.len() > MAX_QUERY_BYTES || entry_bytes(&v) > self.cap {
            return;
        }
        let k = Self::key(model_gen, calib, text);
        let add = entry_bytes(&v);
        let mut g = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        let g = &mut *g;
        if let Some((old_v, old_seq)) = g.map.remove(&k) {
            g.order.remove(&old_seq);
            g.bytes -= entry_bytes(&old_v);
        }
        g.seq += 1;
        let seq = g.seq;
        g.map.insert(k, (Arc::new(v), seq));
        g.order.insert(seq, k);
        g.bytes += add;
        while g.bytes > self.cap {
            let Some((_, oldest)) = g.order.pop_first() else {
                break;
            };
            if let Some((v, _)) = g.map.remove(&oldest) {
                g.bytes -= entry_bytes(&v);
            }
        }
    }

    pub fn len(&self) -> usize {
        self.inner
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .map
            .len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use asc_codeintel::vectors::{SegmentBuilder, chunk_key};
    use asc_store::bucket::BucketObjects;

    use super::*;
    use crate::migrate;

    pub(crate) async fn pool() -> AnyPool {
        let p = migrate::connect("sqlite::memory:", 1).await.unwrap();
        migrate::migrate(&p).await.unwrap();
        p
    }

    fn gens(pool: AnyPool) -> ModelGens {
        ModelGens::new(pool, Arc::new(BucketObjects::memory()))
    }

    fn spec(id: &str, calib: u8) -> ModelSpec {
        ModelSpec {
            model_id: id.to_owned(),
            calib: [calib; 32],
            dims: 4,
        }
    }

    async fn insert_chunks(pool: &AnyPool, ns: &[u8], doc: u32, n: u32) {
        for c in 0..n {
            sqlx::query(
                "INSERT INTO ci_chunks (ns, chunk_ord, doc_ord, lang, start_line, end_line, text_id) \
                 VALUES ($1, $2, $3, 'rust', 1, 2, $4)",
            )
            .bind(ns.to_vec())
            .bind(chunk_key(doc, c) as i64)
            .bind(i64::from(doc))
            .bind(vec![c as u8; 32])
            .execute(pool)
            .await
            .unwrap();
        }
    }

    async fn segment(g: &ModelGens, ns: &[u8], s: &ModelSpec, gen_: u32, keys: &[u64]) {
        let mut b = SegmentBuilder::new(s.model_ref(gen_), s.dims).unwrap();
        for k in keys {
            b.add(*k, &[1, 2, 3, 4]).unwrap();
        }
        g.add_segment(ns, gen_, &b.finish().unwrap()).await.unwrap();
    }

    #[tokio::test]
    async fn a_model_change_serves_the_old_generation_until_coverage_then_flips_once() {
        let pool = pool().await;
        let g = gens(pool.clone());
        let ns = b"ns";
        let a = spec("m@aaa/i8/calib@1/1", 1);
        let gen1 = g.ensure_model(ns, &a, 0).await.unwrap();
        assert_eq!(gen1, 1);
        // The same model again keeps the generation.
        assert_eq!(g.ensure_model(ns, &a, 0).await.unwrap(), 1);
        insert_chunks(&pool, ns, 0, 50).await;
        insert_chunks(&pool, ns, 1, 50).await;
        let tip: RoaringBitmap = [0u32, 1].into_iter().collect();
        let keys = |doc: u32, n: u32| (0..n).map(move |c| chunk_key(doc, c)).collect::<Vec<_>>();
        segment(&g, ns, &a, 1, &[keys(0, 50), keys(1, 50)].concat()).await;
        assert_eq!(
            g.try_flip(ns, 1, std::slice::from_ref(&tip), 1)
                .await
                .unwrap(),
            Flip::Flipped { from: 0, to: 1 }
        );
        assert_eq!(g.load(ns).await.unwrap().serving, 1);

        // A new calibration of the same model is a new generation; generation 1 keeps serving.
        let b = spec("m@aaa/i8/calib@2/1", 2);
        assert_eq!(g.ensure_model(ns, &b, 10).await.unwrap(), 2);
        let row = g.load(ns).await.unwrap();
        assert_eq!((row.serving, row.state.building), (1, Some(2)));

        // 98 of 100 chunks: below 99%, nothing flips.
        let mut have = keys(0, 50);
        have.extend(keys(1, 48));
        segment(&g, ns, &b, 2, &have).await;
        match g
            .try_flip(ns, 2, std::slice::from_ref(&tip), 20)
            .await
            .unwrap()
        {
            Flip::NotYet { worst } => assert!((worst - 0.98).abs() < 1e-9),
            other => panic!("flipped early: {other:?}"),
        }
        assert_eq!(g.load(ns).await.unwrap().serving, 1);

        // 99 of 100: flips, once.
        segment(&g, ns, &b, 2, &[chunk_key(1, 48)]).await;
        assert_eq!(
            g.try_flip(ns, 2, std::slice::from_ref(&tip), 30)
                .await
                .unwrap(),
            Flip::Flipped { from: 1, to: 2 }
        );
        assert_eq!(
            g.try_flip(ns, 2, std::slice::from_ref(&tip), 31)
                .await
                .unwrap(),
            Flip::NotBuilding
        );
        let row = g.load(ns).await.unwrap();
        assert_eq!((row.serving, row.state.building), (2, None));
        assert_eq!(
            row.state.retired,
            [Retired {
                model_gen: 1,
                at_ms: 30
            }]
        );
    }

    #[tokio::test]
    async fn every_land_tip_must_meet_coverage() {
        let pool = pool().await;
        let g = gens(pool.clone());
        let ns = b"ns";
        let a = spec("m@aaa/i8/calib@1/1", 1);
        g.ensure_model(ns, &a, 0).await.unwrap();
        insert_chunks(&pool, ns, 0, 10).await;
        insert_chunks(&pool, ns, 1, 10).await;
        segment(
            &g,
            ns,
            &a,
            1,
            &(0..10).map(|c| chunk_key(0, c)).collect::<Vec<_>>(),
        )
        .await;
        let t0: RoaringBitmap = [0u32].into_iter().collect();
        let t1: RoaringBitmap = [1u32].into_iter().collect();
        assert!(matches!(
            g.try_flip(ns, 1, &[t0.clone(), t1], 1).await.unwrap(),
            Flip::NotYet { .. }
        ));
        assert!(matches!(
            g.try_flip(ns, 1, &[], 1).await.unwrap(),
            Flip::NotYet { .. }
        ));
        assert!(matches!(
            g.try_flip(ns, 1, &[t0], 1).await.unwrap(),
            Flip::Flipped { .. }
        ));
    }

    #[tokio::test]
    async fn old_generations_are_deleted_after_seven_days() {
        let pool = pool().await;
        let g = gens(pool.clone());
        let ns = b"ns";
        let a = spec("m@aaa/i8/calib@1/1", 1);
        g.ensure_model(ns, &a, 0).await.unwrap();
        segment(&g, ns, &a, 1, &[chunk_key(0, 0)]).await;
        let tip: RoaringBitmap = RoaringBitmap::new();
        insert_chunks(&pool, ns, 0, 1).await;
        let tip: RoaringBitmap = tip.into_iter().chain([0]).collect();
        g.try_flip(ns, 1, std::slice::from_ref(&tip), 0)
            .await
            .unwrap();
        let b = spec("m@bbb/i8/calib@1/1", 3);
        g.ensure_model(ns, &b, 5).await.unwrap();
        segment(&g, ns, &b, 2, &[chunk_key(0, 0)]).await;
        g.try_flip(ns, 2, std::slice::from_ref(&tip), 100)
            .await
            .unwrap();
        // Not yet: 7 days have not passed since generation 1 stopped serving.
        assert_eq!(g.gc(ns, 100 + RETAIN_MS - 1).await.unwrap(), 0);
        assert_eq!(g.load(ns).await.unwrap().state.segments.len(), 2);
        assert_eq!(g.gc(ns, 100 + RETAIN_MS).await.unwrap(), 1);
        let row = g.load(ns).await.unwrap();
        assert!(row.state.segments.iter().all(|s| s.model_gen == 2));
        assert!(row.state.retired.is_empty());
    }

    #[test]
    fn the_query_cache_never_answers_across_generations_or_calibrations() {
        let lru = QueryLru::new(LRU_BYTES);
        let (c1, c2) = ([1u8; 32], [2u8; 32]);
        lru.put(1, &c1, "find parser", vec![1.0, 2.0]);
        assert_eq!(
            lru.get(1, &c1, "find parser").unwrap().as_slice(),
            [1.0, 2.0]
        );
        assert!(lru.get(2, &c1, "find parser").is_none());
        assert!(lru.get(1, &c2, "find parser").is_none());
        assert!(lru.get(1, &c1, "find parsers").is_none());
        lru.put(2, &c1, "find parser", vec![9.0]);
        assert_eq!(lru.get(2, &c1, "find parser").unwrap().as_slice(), [9.0]);
        assert_eq!(
            lru.get(1, &c1, "find parser").unwrap().as_slice(),
            [1.0, 2.0]
        );
    }

    #[test]
    fn the_query_cache_evicts_the_least_recently_used_within_its_budget() {
        let lru = QueryLru::new(entry_bytes(&[0.0; 4]) * 2);
        let c = [0u8; 32];
        lru.put(1, &c, "a", vec![0.0; 4]);
        lru.put(1, &c, "b", vec![0.0; 4]);
        assert!(lru.get(1, &c, "a").is_some()); // a is now newer than b
        lru.put(1, &c, "c", vec![0.0; 4]);
        assert_eq!(lru.len(), 2);
        assert!(lru.get(1, &c, "b").is_none());
        assert!(lru.get(1, &c, "a").is_some());
        // Larger than the budget: not cached.
        lru.put(1, &c, "huge", vec![0.0; 100]);
        assert!(lru.get(1, &c, "huge").is_none());
    }

    #[test]
    fn coverage_is_99_percent() {
        assert!(
            Coverage {
                have: 99,
                want: 100
            }
            .meets()
        );
        assert!(
            !Coverage {
                have: 98,
                want: 100
            }
            .meets()
        );
        assert!(Coverage { have: 0, want: 0 }.meets());
        assert!(
            Coverage {
                have: 990,
                want: 1000
            }
            .meets()
        );
    }
}
