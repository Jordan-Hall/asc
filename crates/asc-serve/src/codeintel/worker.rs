//! Indexing workers (SPEC §23.5): file jobs, embed jobs, segment commit and tiered vector
//! merges.
//!
//! * **File job:** read the bytes once (`Objects::get`), verify the `FileId`, apply the skip
//!   rules, scan for secrets, take trigrams, tags (250 ms budget) and chunks, write
//!   `d/tags.v1` and `d/chunks.v1`, add the documents to the in-memory segment writers, and
//!   enqueue one embed job per chunk. The outputs are pure functions of `(FileId, versions)`.
//! * **Segment commit:** every 2 s or 5,000 documents the writers become packs, which the
//!   leader publishes with a CAS on `ci_manifest.generation`. A job completes only after the
//!   commit that made its document visible.
//! * **Embed job:** batches of 32 chunk texts, embedded and quantised to i8, added to a vector
//!   segment of the job's model generation (committed on the same 2 s / 5,000 rule).
//! * **Tiered merges:** only vector segments of one generation and similar size merge, 4 at a
//!   time, with output <= 512 MiB; one merge runs at a time per namespace.
//!
//! Everything read from the object store is verified by hash; everything read from the database
//! is bounded.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use asc_codeintel::chunk::{self, CHUNKER_VERSION, Chunk, ChunkTextId};
use asc_codeintel::doc::{DocLangs, DocRow, Versions};
use asc_codeintel::grams::{self, GRAMS_VER, Gram};
use asc_codeintel::index::IndexError;
use asc_codeintel::index::manifest::{Cas, Dropped, Manifest, ManifestStore, Publisher, SegRef};
use asc_codeintel::index::schema::IndexKind;
use asc_codeintel::index::segments::{
    ChunkDoc, FileDoc, Hash, SegmentWriter, WRITER_HEAP, get_indexed,
};
use asc_codeintel::lang::Lang;
use asc_codeintel::skip::{self, Flags};
use asc_codeintel::tags::{self, Role, Tag};
use asc_codeintel::vectors::{SegmentBuilder, chunk_key, doc_ord_of};
use asc_core::id::FileId;
use asc_store::codec::encode_stored;
use asc_store::key::ObjKey;
use asc_store::objects::Objects;
use roaring::RoaringBitmap;
use serde::{Deserialize, Serialize};
use sqlx::{AnyPool, Row};
use tokio::sync::Mutex as AsyncMutex;

use super::modelgen::{CiError, ModelGens, ModelSpec, Result, VecSeg, store_err};
use super::queue::{Leased, NewJob, Priority, Queue};

/// Job kind for embedding one chunk.
pub const KIND_EMBED: &str = "embed";
/// Chunk texts embedded per model call.
pub const EMBED_BATCH: usize = 32;
/// A segment commits after this long ...
pub const COMMIT_MS: u64 = 2_000;
/// ... or this many documents (or vectors).
pub const COMMIT_DOCS: u32 = 5_000;
/// Seconds a job lease lasts; it must outlive the commit interval.
pub const LEASE_SECS: i64 = 300;
/// Jobs one step leases.
pub const STEP_JOBS: i64 = 64;
/// How long a leader lease lasts.
pub const LEADER_TTL_MS: u64 = 30_000;
/// Version of the `tags.v1` object.
pub const TAGS_VER: u32 = 1;
/// Vector segments merged at a time.
pub const MERGE_WIDTH: usize = 4;
/// No merge output exceeds this (SPEC §23.2.4 share of the memory budget).
pub const MAX_VEC_MERGE_BYTES: u64 = 512 << 20;
/// Largest `tags.v1`, `chunks.v1` or chunk-text object read back.
const MAX_DERIVED: u64 = 64 << 20;
/// A vector segment smaller than this is "tier 0".
const MIN_TIER_BYTES: u64 = 1 << 16;
/// Bytes of an embed job key: text id, doc ord, chunk index.
const EMBED_KEY: usize = 40;
/// Most distinct tag names one file contributes to the symbol terms.
const MAX_SYMBOLS: usize = 100_000;

/// The embedding model a worker runs: identity, calibration and the embed-and-quantise call.
/// The server wires this to `asc-embed`; tests use a fake.
pub trait EmbedModel: Send + Sync {
    fn spec(&self) -> &ModelSpec;
    /// One i8 vector of `spec().dims` per text, in order.
    fn embed(&self, texts: &[String]) -> std::result::Result<Vec<Vec<i8>>, String>;
}

/// Worker settings.
#[derive(Clone, Debug)]
pub struct WorkerConfig {
    /// File jobs analysed at once (`[codeintel] workers`).
    pub workers: usize,
    /// Scratch directory for segment writers; created when missing.
    pub work_dir: PathBuf,
    /// This node's name in the leader lease.
    pub holder: String,
    /// Tantivy writer heap.
    pub heap: usize,
}

impl WorkerConfig {
    pub fn new(work_dir: PathBuf, holder: &str) -> Self {
        Self {
            workers: std::thread::available_parallelism().map_or(2, |n| (n.get() / 2).max(1)),
            work_dir,
            holder: holder.to_owned(),
            heap: WRITER_HEAP,
        }
    }
}

// ------------------------------------------------------------------ names and keys

/// The index name of a repository namespace: lower-case hex, so it is always a valid index
/// namespace. Namespaces are at most 32 bytes.
pub fn ns_name(ns: &[u8]) -> Result<String> {
    if ns.is_empty() || ns.len() > 32 {
        return Err(CiError::Invalid("namespace length"));
    }
    Ok(ns.iter().map(|b| format!("{b:02x}")).collect())
}

fn ns_bytes(name: &str) -> std::result::Result<Vec<u8>, IndexError> {
    if !name.len().is_multiple_of(2) || name.len() > 64 || !name.is_ascii() {
        return Err(IndexError::BadPack("namespace"));
    }
    (0..name.len())
        .step_by(2)
        .map(|i| {
            u8::from_str_radix(&name[i..i + 2], 16).map_err(|_| IndexError::BadPack("namespace"))
        })
        .collect()
}

/// `d/tags.v1/<FileId>`.
pub fn tags_key(f: &FileId) -> ObjKey {
    ObjKey::Derived {
        name: "tags",
        version: TAGS_VER,
        id: *f.as_bytes(),
    }
}

/// `d/chunks.v1/<FileId>`.
pub fn chunks_key(f: &FileId) -> ObjKey {
    ObjKey::Derived {
        name: "chunks",
        version: CHUNKER_VERSION,
        id: *f.as_bytes(),
    }
}

/// The text of a chunk, addressed by its [`ChunkTextId`] so identical chunks share one object.
pub fn ctext_key(id: &ChunkTextId) -> ObjKey {
    ObjKey::Derived {
        name: "ctext",
        version: 1,
        id: id.0,
    }
}

/// The embed job of one chunk position. Key: text id, doc ord (BE), chunk index (BE); version:
/// the model generation. Identical texts at several positions embed once per batch.
pub fn embed_job(
    text_id: &[u8],
    doc_ord: u32,
    chunk: u32,
    model_gen: u32,
    priority: Priority,
) -> Result<NewJob> {
    if text_id.len() != 32 {
        return Err(CiError::Corrupt("text id length"));
    }
    let mut key = Vec::with_capacity(EMBED_KEY);
    key.extend_from_slice(text_id);
    key.extend_from_slice(&doc_ord.to_be_bytes());
    key.extend_from_slice(&chunk.to_be_bytes());
    Ok(NewJob {
        kind: KIND_EMBED.into(),
        key,
        version: i64::from(model_gen),
        priority,
    })
}

fn versions_blob(v: &Versions) -> Vec<u8> {
    let mut out = Vec::with_capacity(12);
    for x in [v.grams, v.tags, v.chunker] {
        out.extend_from_slice(&x.to_le_bytes());
    }
    out
}

/// The versions this build writes.
pub fn current_versions() -> Versions {
    Versions {
        grams: GRAMS_VER,
        tags: TAGS_VER,
        chunker: CHUNKER_VERSION,
    }
}

// ------------------------------------------------------------------ analysis

/// Everything one file contributes, computed from its bytes alone.
#[derive(Debug)]
pub struct Analysis {
    pub flags: Flags,
    pub size: u64,
    pub langs: Vec<Lang>,
    pub grams: Vec<Gram>,
    pub sym_def: Vec<String>,
    pub sym_ref: Vec<String>,
    pub ident: String,
    pub doc: String,
    /// `None` when the file has no grammar, was not indexable, or tags ran out of time.
    pub tags: Option<Vec<Tag>>,
    pub chunks: Vec<Chunk>,
}

fn dedup_names(tags: &[Tag], role: Role) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for t in tags.iter().filter(|t| t.role == role) {
        if out.len() >= MAX_SYMBOLS {
            break;
        }
        if seen.insert(t.name.as_str()) {
            out.push(t.name.clone());
        }
    }
    out
}

/// Skip rules, secret scan, trigrams, tags (250 ms) and chunks of one file. Pure CPU work,
/// bounded by the 1 MiB skip rule. `known_langs` come from the paths that hold the file; with
/// none, a shebang decides.
pub fn analyze(bytes: &[u8], known_langs: &[Lang]) -> Analysis {
    let row = DocRow::from_bytes(0, bytes, current_versions());
    let mut flags = row.flags;
    let mut out = Analysis {
        flags,
        size: row.size,
        langs: known_langs.to_vec(),
        grams: Vec::new(),
        sym_def: Vec::new(),
        sym_ref: Vec::new(),
        ident: String::new(),
        doc: String::new(),
        tags: None,
        chunks: Vec::new(),
    };
    if !flags.indexable() {
        return out;
    }
    match grams::distinct_trigrams(bytes, skip::MAX_GRAMS) {
        Some(g) => out.grams = g,
        None => {
            flags.insert(Flags::TOO_MANY_GRAMS);
            out.flags = flags;
            return out;
        }
    }
    if out.langs.is_empty() {
        let mut l = DocLangs::default();
        l.add_path("", bytes);
        out.langs = l.as_slice().to_vec();
    }
    let primary = out.langs.first().copied();
    if let Some(l) = primary
        && let Ok(t) = tags::extract_within(l, bytes, tags::BUDGET)
    {
        out.sym_def = dedup_names(&t, Role::Def);
        out.sym_ref = dedup_names(&t, Role::Ref);
        out.ident = out
            .sym_def
            .iter()
            .chain(&out.sym_ref)
            .map(String::as_str)
            .collect::<Vec<_>>()
            .join(" ");
        out.doc = t
            .iter()
            .filter(|t| !t.doc.is_empty())
            .map(|t| String::from_utf8_lossy(&t.doc).into_owned())
            .collect::<Vec<_>>()
            .join("\n");
        out.tags = Some(t);
    }
    // A secret file stays searchable by exact match but is never chunked or embedded.
    if !flags.contains(Flags::SECRET)
        && let Ok(c) = chunk::chunk(primary, bytes)
    {
        out.chunks = c;
    }
    out
}

/// The `chunks.v1` rows.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChunkRow {
    pub start: u32,
    pub end: u32,
    pub symbol: String,
    /// `ChunkTextId`, lower-case hex.
    pub text: String,
}

pub fn chunk_rows(chunks: &[Chunk]) -> Vec<ChunkRow> {
    chunk::chunk_map(chunks)
        .into_iter()
        .map(|(start, end, symbol, id)| ChunkRow {
            start,
            end,
            symbol,
            text: asc_codeintel::index::segments::hex(&id.0),
        })
        .collect()
}

/// Reads `d/tags.v1` back.
pub async fn read_tags(objects: &dyn Objects, f: &FileId) -> Result<Option<Vec<Tag>>> {
    read_json(objects, &tags_key(f)).await
}

/// Reads `d/chunks.v1` back.
pub async fn read_chunk_rows(objects: &dyn Objects, f: &FileId) -> Result<Option<Vec<ChunkRow>>> {
    read_json(objects, &chunks_key(f)).await
}

async fn read_json<T: for<'de> Deserialize<'de>>(
    objects: &dyn Objects,
    key: &ObjKey,
) -> Result<Option<T>> {
    let Some(stored) = objects.get_stored(key).await.map_err(store_err)? else {
        return Ok(None);
    };
    let data = asc_store::codec::decode_stored_capped(&stored, MAX_DERIVED)
        .map_err(|_| CiError::Corrupt("derived object"))?;
    serde_json::from_slice(&data)
        .map(Some)
        .map_err(|_| CiError::Corrupt("derived object"))
}

// ------------------------------------------------------------------ SQL manifest

/// `ci_manifest` rows for the `files` and `chunks` indexes, with the leader lease.
pub struct SqlManifest {
    pool: AnyPool,
}

#[derive(Default, Serialize, Deserialize)]
struct ManBlob {
    segments: Vec<SegRef>,
    dropped: Vec<Dropped>,
}

fn sql_err(e: sqlx::Error) -> IndexError {
    IndexError::Store(e.to_string())
}

impl SqlManifest {
    pub fn new(pool: AnyPool) -> Self {
        Self { pool }
    }

    async fn read(&self, ns: &[u8], kind: IndexKind) -> asc_codeintel::index::Result<Manifest> {
        let row = sqlx::query(
            "SELECT CAST(generation AS BIGINT) AS generation, CAST(model_gen AS BIGINT) AS model_gen, \
                    segments, indexed_ref, leader, CAST(lease_until AS BIGINT) AS lease_until \
             FROM ci_manifest WHERE ns = $1 AND idx = $2",
        )
        .bind(ns.to_vec())
        .bind(kind.name().to_owned())
        .fetch_optional(&self.pool)
        .await
        .map_err(sql_err)?;
        let Some(r) = row else {
            return Ok(Manifest::empty(kind));
        };
        let blob: Vec<u8> = r.try_get("segments").map_err(sql_err)?;
        let parsed: ManBlob = if blob.len() > (16 << 20) {
            return Err(IndexError::BadPack("manifest too large"));
        } else {
            serde_json::from_slice(&blob).unwrap_or_default()
        };
        let indexed: Option<Vec<u8>> = r.try_get("indexed_ref").map_err(sql_err)?;
        let indexed = match indexed {
            Some(b) => {
                Some(Hash::try_from(b.as_slice()).map_err(|_| IndexError::BadPack("indexed_ref"))?)
            }
            None => None,
        };
        let generation: i64 = r.try_get("generation").map_err(sql_err)?;
        let model_gen: i64 = r.try_get("model_gen").map_err(sql_err)?;
        let lease: Option<i64> = r.try_get("lease_until").map_err(sql_err)?;
        Ok(Manifest {
            index: kind,
            generation: u64::try_from(generation).map_err(|_| IndexError::BadPack("generation"))?,
            segments: parsed.segments,
            indexed,
            model_gen: u32::try_from(model_gen).map_err(|_| IndexError::BadPack("model_gen"))?,
            dropped: parsed.dropped,
            leader: r.try_get("leader").map_err(sql_err)?,
            lease_until_ms: lease.and_then(|l| u64::try_from(l).ok()).unwrap_or(0),
        })
    }
}

#[async_trait]
impl ManifestStore for SqlManifest {
    async fn load(&self, ns: &str, index: IndexKind) -> asc_codeintel::index::Result<Manifest> {
        self.read(&ns_bytes(ns)?, index).await
    }

    async fn cas(
        &self,
        ns: &str,
        index: IndexKind,
        expected: u64,
        next: Manifest,
        holder: &str,
        now_ms: u64,
    ) -> asc_codeintel::index::Result<Cas> {
        let nsb = ns_bytes(ns)?;
        let blob = serde_json::to_vec(&ManBlob {
            segments: next.segments.clone(),
            dropped: next.dropped.clone(),
        })
        .map_err(|e| IndexError::Io(e.to_string()))?;
        let n = sqlx::query(
            "UPDATE ci_manifest SET generation = $1, segments = $2, model_gen = $3, indexed_ref = $4 \
             WHERE ns = $5 AND idx = $6 AND generation = $7 AND leader = $8 AND lease_until > $9",
        )
        .bind(i64::try_from(expected + 1).map_err(|_| IndexError::BadPack("generation"))?)
        .bind(blob)
        .bind(i64::from(next.model_gen))
        .bind(next.indexed.map(|h| h.to_vec()))
        .bind(nsb.clone())
        .bind(index.name().to_owned())
        .bind(i64::try_from(expected).map_err(|_| IndexError::BadPack("generation"))?)
        .bind(holder.to_owned())
        .bind(i64::try_from(now_ms).unwrap_or(i64::MAX))
        .execute(&self.pool)
        .await
        .map_err(sql_err)?
        .rows_affected();
        let cur = self.read(&nsb, index).await?;
        if n == 1 {
            Ok(Cas::Won(cur))
        } else if cur.generation != expected {
            Ok(Cas::Lost(cur))
        } else {
            Err(IndexError::NotLeader)
        }
    }

    async fn lease(
        &self,
        ns: &str,
        index: IndexKind,
        holder: &str,
        now_ms: u64,
        ttl_ms: u64,
    ) -> asc_codeintel::index::Result<bool> {
        let nsb = ns_bytes(ns)?;
        let now = i64::try_from(now_ms).unwrap_or(i64::MAX);
        let until = now.saturating_add(i64::try_from(ttl_ms).unwrap_or(i64::MAX));
        let empty =
            serde_json::to_vec(&ManBlob::default()).map_err(|e| IndexError::Io(e.to_string()))?;
        let ins = sqlx::query(
            "INSERT INTO ci_manifest (ns, idx, generation, segments, model_gen, leader, lease_until) \
             VALUES ($1, $2, 0, $3, 0, $4, $5) ON CONFLICT (ns, idx) DO NOTHING",
        )
        .bind(nsb.clone())
        .bind(index.name().to_owned())
        .bind(empty)
        .bind(holder.to_owned())
        .bind(until)
        .execute(&self.pool)
        .await
        .map_err(sql_err)?
        .rows_affected();
        if ins == 1 {
            return Ok(true);
        }
        let n = sqlx::query(
            "UPDATE ci_manifest SET leader = $1, lease_until = $2 \
             WHERE ns = $3 AND idx = $4 AND (leader IS NULL OR leader = $1 OR lease_until <= $5)",
        )
        .bind(holder.to_owned())
        .bind(until)
        .bind(nsb)
        .bind(index.name().to_owned())
        .bind(now)
        .execute(&self.pool)
        .await
        .map_err(sql_err)?
        .rows_affected();
        Ok(n == 1)
    }
}

// ------------------------------------------------------------------ the worker

struct PendingFile {
    job: Leased,
    embeds: Vec<NewJob>,
}

struct VecBuf {
    model_gen: u32,
    builder: SegmentBuilder,
    keys: HashSet<u64>,
    jobs: Vec<Leased>,
    since_ms: u64,
}

#[derive(Default)]
struct NsBuf {
    files: Option<SegmentWriter>,
    chunks: Option<SegmentWriter>,
    docs: u32,
    since_ms: u64,
    pending: Vec<PendingFile>,
    vec: Option<VecBuf>,
    /// Ords committed in the files index as of `indexed_gen`.
    indexed: RoaringBitmap,
    indexed_gen: Option<u64>,
}

/// What one [`Worker::step`] did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct StepReport {
    /// This node did not hold the namespace lease; nothing was leased.
    pub not_leader: bool,
    pub leased: usize,
    pub files: usize,
    pub embedded: usize,
    pub skipped: usize,
    pub failed: usize,
    pub committed: bool,
}

/// What one flush published.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FlushReport {
    pub docs: u32,
    pub vectors: usize,
    pub embeds_enqueued: u64,
}

/// What one merge did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MergeReport {
    pub model_gen: u32,
    pub inputs: usize,
    pub vectors: usize,
    pub dropped_tombstoned: usize,
    pub bytes: u64,
}

/// The indexing worker of one server.
pub struct Worker {
    pool: AnyPool,
    queue: Queue,
    objects: Arc<dyn Objects>,
    manifests: Arc<dyn ManifestStore>,
    gens: ModelGens,
    model: Option<Arc<dyn EmbedModel>>,
    cfg: WorkerConfig,
    bufs: std::sync::Mutex<HashMap<Vec<u8>, Arc<AsyncMutex<NsBuf>>>>,
    merging: std::sync::Mutex<HashSet<Vec<u8>>>,
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

fn secs(ms: u64) -> i64 {
    i64::try_from(ms / 1000).unwrap_or(i64::MAX)
}

struct Ctx {
    target_gen: Option<u32>,
    live: super::modelgen::VecRow,
}

impl Worker {
    pub fn new(
        pool: AnyPool,
        objects: Arc<dyn Objects>,
        manifests: Arc<dyn ManifestStore>,
        model: Option<Arc<dyn EmbedModel>>,
        cfg: WorkerConfig,
    ) -> Result<Self> {
        std::fs::create_dir_all(&cfg.work_dir).map_err(store_err)?;
        Ok(Self {
            queue: Queue::new(pool.clone()),
            gens: ModelGens::new(pool.clone(), objects.clone()),
            pool,
            objects,
            manifests,
            model,
            cfg,
            bufs: std::sync::Mutex::new(HashMap::new()),
            merging: std::sync::Mutex::new(HashSet::new()),
        })
    }

    pub fn queue(&self) -> &Queue {
        &self.queue
    }

    pub fn gens(&self) -> &ModelGens {
        &self.gens
    }

    fn buf(&self, ns: &[u8]) -> Arc<AsyncMutex<NsBuf>> {
        let mut m = self.bufs.lock().unwrap_or_else(|p| p.into_inner());
        m.entry(ns.to_vec()).or_default().clone()
    }

    fn publisher<'a>(&'a self, nsn: &'a str, kind: IndexKind) -> Publisher<'a> {
        Publisher {
            objects: &*self.objects,
            manifests: &*self.manifests,
            ns: nsn,
            kind,
            holder: &self.cfg.holder,
        }
    }

    async fn lead(&self, nsn: &str, now_ms: u64) -> Result<bool> {
        for kind in [IndexKind::Files, IndexKind::Chunks] {
            if !self
                .publisher(nsn, kind)
                .lease(now_ms, LEADER_TTL_MS)
                .await?
            {
                return Ok(false);
            }
        }
        Ok(true)
    }

    // -------------------------------------------------------------- the step

    /// Leases a batch of jobs of `ns` and runs them, then commits the segments if they are due.
    /// Only the namespace leader leases: a follower returns at once.
    pub async fn step(&self, ns: &[u8], now_ms: u64) -> Result<StepReport> {
        let nsn = ns_name(ns)?;
        let mut rep = StepReport::default();
        if !self.lead(&nsn, now_ms).await? {
            rep.not_leader = true;
            return Ok(rep);
        }
        let target_gen = match &self.model {
            Some(m) => Some(self.gens.ensure_model(ns, m.spec(), now_ms).await?),
            None => None,
        };
        let ctx = Ctx {
            target_gen,
            live: self.gens.load(ns).await?,
        };
        self.refresh_indexed(ns, &nsn).await?;
        let jobs = self
            .queue
            .lease(ns, STEP_JOBS, LEASE_SECS, secs(now_ms))
            .await?;
        rep.leased = jobs.len();
        let (embeds, others): (Vec<Leased>, Vec<Leased>) =
            jobs.into_iter().partition(|j| j.kind == KIND_EMBED);

        let results: Vec<(Leased, Result<bool>)> = {
            use futures::StreamExt;
            futures::stream::iter(others)
                .map(|job| {
                    let ctx = &ctx;
                    async move {
                        let r = if job.kind == super::queue::KIND_FILE {
                            self.file_job(ns, &job, ctx, now_ms).await
                        } else {
                            Err(CiError::Invalid("unknown job kind"))
                        };
                        (job, r)
                    }
                })
                .buffer_unordered(self.cfg.workers.max(1))
                .collect()
                .await
        };
        for (job, r) in results {
            match r {
                Ok(true) => rep.files += 1,
                Ok(false) => {
                    rep.skipped += 1;
                    self.queue.complete(&job).await?;
                }
                Err(e) => {
                    rep.failed += 1;
                    self.queue.fail(&job, &e.to_string(), secs(now_ms)).await?;
                }
            }
        }
        for batch in embeds.chunks(EMBED_BATCH) {
            let (done, skipped, failed) = self.embed_batch(ns, batch, &ctx, now_ms).await?;
            rep.embedded += done;
            rep.skipped += skipped;
            rep.failed += failed;
        }
        let before = rep.files + rep.embedded;
        if self.flush_if_due(ns, now_ms).await?.is_some() {
            rep.committed = before > 0 || rep.committed;
        }
        Ok(rep)
    }

    async fn refresh_indexed(&self, ns: &[u8], nsn: &str) -> Result<()> {
        let m = self.manifests.load(nsn, IndexKind::Files).await?;
        let buf = self.buf(ns);
        let mut b = buf.lock().await;
        if b.indexed_gen == Some(m.generation) {
            return Ok(());
        }
        b.indexed = match m.indexed {
            Some(h) => get_indexed(&*self.objects, nsn, IndexKind::Files, h).await?,
            None => RoaringBitmap::new(),
        };
        b.indexed_gen = Some(m.generation);
        Ok(())
    }

    // -------------------------------------------------------------- file jobs

    /// Runs one file job up to staging. `Ok(true)`: staged, completes at the next commit.
    /// `Ok(false)`: nothing to do (already indexed); the caller completes it.
    async fn file_job(&self, ns: &[u8], job: &Leased, ctx: &Ctx, now_ms: u64) -> Result<bool> {
        let key: [u8; 32] = job
            .key
            .as_slice()
            .try_into()
            .map_err(|_| CiError::Invalid("file job key"))?;
        let id = FileId::from_bytes(key);
        let existing = self.doc_row(ns, &id).await?;
        if let Some((ord, flags)) = existing {
            let tomb = flags & super::sweep::TOMBSTONE != 0;
            let buf = self.buf(ns);
            if !tomb && buf.lock().await.indexed.contains(ord) {
                return Ok(false);
            }
        }
        let known_langs = match existing {
            Some((ord, _)) => self.doc_langs(ns, ord).await?,
            None => Vec::new(),
        };
        let bytes = match self
            .objects
            .get(&ObjKey::File(id))
            .await
            .map_err(store_err)?
        {
            Some(b) => b,
            None => {
                // A chunked file is over every size limit: record the skip, index nothing.
                let chunked = self
                    .objects
                    .has_many(&[ObjKey::ChunkList(id)])
                    .await
                    .map_err(store_err)?;
                if chunked.first() == Some(&true) {
                    self.reserve_doc(
                        ns,
                        &id,
                        (skip::MAX_BYTES as u64) + 1,
                        Flags::TOO_LARGE.bits(),
                        &[],
                    )
                    .await?;
                    return Ok(false);
                }
                return Err(CiError::Store("file object is missing".into()));
            }
        };
        if FileId::of(&bytes) != id {
            return Err(CiError::Store("file content does not match its id".into()));
        }
        let a = {
            let bytes = bytes.clone();
            let langs = known_langs.clone();
            tokio::task::spawn_blocking(move || analyze(&bytes, &langs))
                .await
                .map_err(store_err)?
        };
        let ord = self
            .reserve_doc(ns, &id, a.size, a.flags.bits(), &a.langs)
            .await?;

        // Derived objects first: they are pure functions of the file, so a retry rewrites the
        // same bytes.
        if let Some(t) = &a.tags {
            let json = serde_json::to_vec(t).map_err(store_err)?;
            self.objects
                .put(&tags_key(&id), encode_stored(&json))
                .await
                .map_err(store_err)?;
        }
        if !a.chunks.is_empty() {
            let json = serde_json::to_vec(&chunk_rows(&a.chunks)).map_err(store_err)?;
            self.objects
                .put(&chunks_key(&id), encode_stored(&json))
                .await
                .map_err(store_err)?;
            self.put_texts(&a.chunks).await?;
        }
        self.replace_chunks(ns, ord, &a).await?;

        let embeds = match ctx.target_gen {
            Some(g) => {
                let mut v = Vec::with_capacity(a.chunks.len());
                for (i, c) in a.chunks.iter().enumerate() {
                    v.push(embed_job(
                        &c.id.0,
                        ord,
                        u32::try_from(i).map_err(|_| CiError::Invalid("chunk index"))?,
                        g,
                        job.priority,
                    )?);
                }
                v
            }
            None => Vec::new(),
        };

        if a.flags.indexable() {
            let buf = self.buf(ns);
            let mut b = buf.lock().await;
            if b.files.is_none() {
                b.files = Some(SegmentWriter::new(
                    IndexKind::Files,
                    &self.cfg.work_dir,
                    self.cfg.heap,
                )?);
                b.chunks = Some(SegmentWriter::new(
                    IndexKind::Chunks,
                    &self.cfg.work_dir,
                    self.cfg.heap,
                )?);
                b.since_ms = now_ms;
            }
            let defs: Vec<&str> = a.sym_def.iter().map(String::as_str).collect();
            let refs: Vec<&str> = a.sym_ref.iter().map(String::as_str).collect();
            let files = b.files.as_mut().ok_or(CiError::Invalid("writer"))?;
            let added = files.add_file(&FileDoc {
                ord,
                grams: &a.grams,
                sym_def: &defs,
                sym_ref: &refs,
                ident: &a.ident,
                doc: &a.doc,
                path: "",
                langs: &a.langs,
            });
            match added {
                Ok(()) => {}
                // Already staged by an earlier lease of this job: just wait for the commit.
                Err(IndexError::DuplicateOrd(_)) => {
                    b.pending.push(PendingFile {
                        job: job.clone(),
                        embeds,
                    });
                    return Ok(true);
                }
                Err(e) => return Err(e.into()),
            }
            let chunks = b.chunks.as_mut().ok_or(CiError::Invalid("writer"))?;
            for (i, c) in a.chunks.iter().enumerate() {
                chunks.add_chunk(&ChunkDoc {
                    ord,
                    chunk: u32::try_from(i).map_err(|_| CiError::Invalid("chunk index"))?,
                    ident: &c.embed_text,
                    doc: "",
                    symbol: &c.symbol,
                })?;
            }
            b.docs += 1 + u32::try_from(a.chunks.len()).unwrap_or(u32::MAX);
            b.pending.push(PendingFile {
                job: job.clone(),
                embeds,
            });
            return Ok(true);
        }
        // Skipped by the rules: the row is the whole result.
        Ok(false)
    }

    async fn doc_row(&self, ns: &[u8], id: &FileId) -> Result<Option<(u32, u8)>> {
        let row = sqlx::query(
            "SELECT CAST(ord AS BIGINT) AS ord, CAST(flags AS BIGINT) AS flags \
             FROM ci_docs WHERE ns = $1 AND file = $2",
        )
        .bind(ns.to_vec())
        .bind(id.as_bytes().to_vec())
        .fetch_optional(&self.pool)
        .await?;
        let Some(r) = row else { return Ok(None) };
        let ord: i64 = r.try_get("ord")?;
        let flags: i64 = r.try_get("flags")?;
        Ok(Some((
            u32::try_from(ord).map_err(|_| CiError::Corrupt("ord"))?,
            u8::try_from(flags).map_err(|_| CiError::Corrupt("flags"))?,
        )))
    }

    async fn doc_langs(&self, ns: &[u8], ord: u32) -> Result<Vec<Lang>> {
        let rows = sqlx::query("SELECT lang FROM ci_doc_langs WHERE ns = $1 AND ord = $2 LIMIT 8")
            .bind(ns.to_vec())
            .bind(i64::from(ord))
            .fetch_all(&self.pool)
            .await?;
        let mut out = Vec::new();
        for r in rows {
            let name: String = r.try_get("lang")?;
            if let Some(l) = Lang::from_name(&name) {
                out.push(l);
            }
        }
        out.sort_unstable();
        Ok(out)
    }

    /// Assigns the next dense ord to `id` (ords are never reused: rows are never deleted) or
    /// refreshes its row, and records its languages. Safe under concurrent callers.
    async fn reserve_doc(
        &self,
        ns: &[u8],
        id: &FileId,
        size: u64,
        flags: u8,
        langs: &[Lang],
    ) -> Result<u32> {
        let versions = versions_blob(&current_versions());
        let size = i64::try_from(size).unwrap_or(i64::MAX);
        for _ in 0..16 {
            if let Some((ord, _)) = self.doc_row(ns, id).await? {
                sqlx::query(
                    "UPDATE ci_docs SET size = $1, flags = $2, versions = $3 \
                     WHERE ns = $4 AND file = $5",
                )
                .bind(size)
                .bind(i64::from(flags))
                .bind(versions.clone())
                .bind(ns.to_vec())
                .bind(id.as_bytes().to_vec())
                .execute(&self.pool)
                .await?;
                self.put_langs(ns, ord, langs).await?;
                return Ok(ord);
            }
            // A concurrent insert of the same ord loses on the unique index; retry.
            let inserted = sqlx::query(
                "INSERT INTO ci_docs (ns, file, ord, size, flags, versions) \
                 VALUES ($1, $2, (SELECT COALESCE(MAX(ord) + 1, 0) FROM ci_docs WHERE ns = $1), $3, $4, $5) \
                 ON CONFLICT DO NOTHING",
            )
            .bind(ns.to_vec())
            .bind(id.as_bytes().to_vec())
            .bind(size)
            .bind(i64::from(flags))
            .bind(versions.clone())
            .execute(&self.pool)
            .await;
            if inserted.is_err() {
                continue;
            }
        }
        Err(CiError::Contended(16))
    }

    async fn put_langs(&self, ns: &[u8], ord: u32, langs: &[Lang]) -> Result<()> {
        for l in langs.iter().take(asc_codeintel::doc::MAX_LANGS) {
            sqlx::query(
                "INSERT INTO ci_doc_langs (ns, ord, lang) VALUES ($1, $2, $3) ON CONFLICT DO NOTHING",
            )
            .bind(ns.to_vec())
            .bind(i64::from(ord))
            .bind(l.name().to_owned())
            .execute(&self.pool)
            .await?;
        }
        Ok(())
    }

    async fn put_texts(&self, chunks: &[Chunk]) -> Result<()> {
        let keys: Vec<ObjKey> = chunks.iter().map(|c| ctext_key(&c.id)).collect();
        let have = self.objects.has_many(&keys).await.map_err(store_err)?;
        for ((c, k), h) in chunks.iter().zip(&keys).zip(have) {
            if !h {
                self.objects
                    .put(k, encode_stored(c.embed_text.as_bytes()))
                    .await
                    .map_err(store_err)?;
            }
        }
        Ok(())
    }

    /// Replaces the `ci_chunks` rows of a doc. The key `chunk_ord` is `chunk_key(doc, index)`.
    async fn replace_chunks(&self, ns: &[u8], ord: u32, a: &Analysis) -> Result<()> {
        let lang = a.langs.first().map_or("", |l| l.name()).to_owned();
        let mut tx = self.pool.begin().await?;
        sqlx::query("DELETE FROM ci_chunks WHERE ns = $1 AND doc_ord = $2")
            .bind(ns.to_vec())
            .bind(i64::from(ord))
            .execute(&mut *tx)
            .await?;
        for (i, c) in a.chunks.iter().enumerate() {
            let idx = u32::try_from(i).map_err(|_| CiError::Invalid("chunk index"))?;
            sqlx::query(
                "INSERT INTO ci_chunks (ns, chunk_ord, doc_ord, lang, start_line, end_line, text_id) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7)",
            )
            .bind(ns.to_vec())
            .bind(i64::try_from(chunk_key(ord, idx)).map_err(|_| CiError::Invalid("chunk key"))?)
            .bind(i64::from(ord))
            .bind(lang.clone())
            .bind(i64::from(c.start_line))
            .bind(i64::from(c.end_line))
            .bind(c.id.0.to_vec())
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    // -------------------------------------------------------------- embed jobs

    /// Embeds up to [`EMBED_BATCH`] leased embed jobs. Returns `(embedded, skipped, failed)`.
    async fn embed_batch(
        &self,
        ns: &[u8],
        jobs: &[Leased],
        ctx: &Ctx,
        now_ms: u64,
    ) -> Result<(usize, usize, usize)> {
        struct Item<'a> {
            job: &'a Leased,
            text_id: [u8; 32],
            ord: u32,
            chunk: u32,
            model_gen: u32,
        }
        let (mut skipped, mut failed) = (0, 0);
        let mut items: Vec<Item<'_>> = Vec::new();
        for job in jobs {
            let parsed = (|| {
                let k: &[u8; EMBED_KEY] = job.key.as_slice().try_into().ok()?;
                let text_id: [u8; 32] = k[..32].try_into().ok()?;
                Some(Item {
                    job,
                    text_id,
                    ord: u32::from_be_bytes(k[32..36].try_into().ok()?),
                    chunk: u32::from_be_bytes(k[36..40].try_into().ok()?),
                    model_gen: u32::try_from(job.version).ok()?,
                })
            })();
            // A malformed key, an abandoned generation or another model's job: drop it.
            let Some(it) = parsed else {
                self.queue.complete(job).await?;
                skipped += 1;
                continue;
            };
            let mine = ctx.target_gen == Some(it.model_gen)
                && super::modelgen::ModelGens::is_live_gen(&ctx.live, it.model_gen);
            if !mine || self.model.is_none() {
                self.queue.complete(job).await?;
                skipped += 1;
                continue;
            }
            let row = sqlx::query("SELECT text_id FROM ci_chunks WHERE ns = $1 AND chunk_ord = $2")
                .bind(ns.to_vec())
                .bind(
                    i64::try_from(chunk_key(it.ord, it.chunk))
                        .map_err(|_| CiError::Invalid("key"))?,
                )
                .fetch_optional(&self.pool)
                .await?;
            let current = match row {
                Some(r) => r.try_get::<Vec<u8>, _>("text_id")?,
                None => Vec::new(),
            };
            if current != it.text_id {
                // The chunk is gone or changed (re-chunked, tombstoned): nothing to embed.
                self.queue.complete(job).await?;
                skipped += 1;
                continue;
            }
            items.push(it);
        }
        let Some(model) = self.model.clone().filter(|_| !items.is_empty()) else {
            return Ok((0, skipped, failed));
        };

        // One text per distinct id in the batch.
        let mut index: HashMap<[u8; 32], usize> = HashMap::new();
        let mut texts: Vec<String> = Vec::new();
        let mut bad: HashSet<[u8; 32]> = HashSet::new();
        for it in &items {
            if index.contains_key(&it.text_id) || bad.contains(&it.text_id) {
                continue;
            }
            match self.read_text(&ChunkTextId(it.text_id)).await {
                Ok(t) => {
                    index.insert(it.text_id, texts.len());
                    texts.push(t);
                }
                Err(_) => {
                    bad.insert(it.text_id);
                }
            }
        }
        let vectors = if texts.is_empty() {
            Vec::new()
        } else {
            let m = model.clone();
            let t = texts.clone();
            let out = tokio::task::spawn_blocking(move || m.embed(&t))
                .await
                .map_err(store_err)?;
            match out {
                Ok(v)
                    if v.len() == texts.len() && v.iter().all(|x| x.len() == model.spec().dims) =>
                {
                    v
                }
                Ok(_) => return Err(CiError::Invalid("embedder returned the wrong shape")),
                Err(e) => {
                    for it in &items {
                        self.queue.fail(it.job, &e, secs(now_ms)).await?;
                        failed += 1;
                    }
                    return Ok((0, skipped, failed));
                }
            }
        };

        let buf = self.buf(ns);
        let mut done = 0;
        let mut b = buf.lock().await;
        for it in items {
            let Some(&i) = index.get(&it.text_id) else {
                self.queue
                    .fail(it.job, "chunk text is missing or corrupt", secs(now_ms))
                    .await?;
                failed += 1;
                continue;
            };
            if b.vec.as_ref().is_some_and(|v| v.model_gen != it.model_gen) {
                drop(b);
                self.flush_vectors(ns, now_ms).await?;
                b = buf.lock().await;
            }
            if b.vec.is_none() {
                b.vec = Some(VecBuf {
                    model_gen: it.model_gen,
                    builder: SegmentBuilder::new(
                        model.spec().model_ref(it.model_gen),
                        model.spec().dims,
                    )?,
                    keys: HashSet::new(),
                    jobs: Vec::new(),
                    since_ms: now_ms,
                });
            }
            let v = b.vec.as_mut().ok_or(CiError::Invalid("vector buffer"))?;
            let key = chunk_key(it.ord, it.chunk);
            if v.keys.insert(key) {
                v.builder.add(key, &vectors[i])?;
            }
            v.jobs.push(it.job.clone());
            done += 1;
        }
        Ok((done, skipped, failed))
    }

    async fn read_text(&self, id: &ChunkTextId) -> Result<String> {
        let stored = self
            .objects
            .get_stored(&ctext_key(id))
            .await
            .map_err(store_err)?
            .ok_or(CiError::Corrupt("chunk text missing"))?;
        let data = asc_store::codec::decode_stored_capped(&stored, MAX_DERIVED)
            .map_err(|_| CiError::Corrupt("chunk text"))?;
        let s = String::from_utf8(data.to_vec()).map_err(|_| CiError::Corrupt("chunk text"))?;
        if ChunkTextId::of(&s) != *id {
            return Err(CiError::Corrupt("chunk text hash"));
        }
        Ok(s)
    }

    // -------------------------------------------------------------- commits

    /// Commits the segments of `ns` when 2 s have passed or 5,000 documents are buffered.
    pub async fn flush_if_due(&self, ns: &[u8], now_ms: u64) -> Result<Option<FlushReport>> {
        let (files_due, vec_due) = {
            let buf = self.buf(ns);
            let b = buf.lock().await;
            let files_due = b.files.is_some()
                && (b.docs >= COMMIT_DOCS || now_ms.saturating_sub(b.since_ms) >= COMMIT_MS);
            let vec_due = b.vec.as_ref().is_some_and(|v| {
                v.builder.len() >= COMMIT_DOCS as usize
                    || now_ms.saturating_sub(v.since_ms) >= COMMIT_MS
            });
            (files_due, vec_due)
        };
        if !files_due && !vec_due {
            return Ok(None);
        }
        let mut rep = FlushReport::default();
        if files_due {
            rep = self.flush(ns, now_ms).await?;
        }
        if vec_due {
            rep.vectors = self.flush_vectors(ns, now_ms).await?;
        }
        Ok(Some(rep))
    }

    /// Commits the buffered documents and vectors now.
    pub async fn flush_all(&self, ns: &[u8], now_ms: u64) -> Result<FlushReport> {
        let mut rep = self.flush(ns, now_ms).await?;
        rep.vectors = self.flush_vectors(ns, now_ms).await?;
        Ok(rep)
    }

    /// Publishes the file and chunk segments: tantivy commit, upload, CAS of the manifest
    /// generation (re-based on a lost race), then the embed jobs and the job completions.
    pub async fn flush(&self, ns: &[u8], now_ms: u64) -> Result<FlushReport> {
        let nsn = ns_name(ns)?;
        let buf = self.buf(ns);
        let mut b = buf.lock().await;
        let (files, chunks) = (b.files.take(), b.chunks.take());
        let pending = std::mem::take(&mut b.pending);
        let docs = std::mem::take(&mut b.docs);
        if files.is_none() && pending.is_empty() {
            return Ok(FlushReport::default());
        }
        let outcome = self.publish(&nsn, files, chunks, now_ms).await;
        if let Err(e) = outcome {
            for p in &pending {
                self.queue
                    .fail(&p.job, &e.to_string(), secs(now_ms))
                    .await?;
            }
            return Err(e);
        }
        let mut enq = 0;
        for p in &pending {
            if !p.embeds.is_empty() {
                for part in p.embeds.chunks(super::queue::MAX_ENQUEUE) {
                    enq += self.queue.enqueue(ns, part, secs(now_ms)).await?;
                }
            }
            self.queue.complete(&p.job).await?;
        }
        let m = self.manifests.load(&nsn, IndexKind::Files).await?;
        b.indexed = match m.indexed {
            Some(h) => get_indexed(&*self.objects, &nsn, IndexKind::Files, h).await?,
            None => RoaringBitmap::new(),
        };
        b.indexed_gen = Some(m.generation);
        Ok(FlushReport {
            docs,
            vectors: 0,
            embeds_enqueued: enq,
        })
    }

    async fn publish(
        &self,
        nsn: &str,
        files: Option<SegmentWriter>,
        chunks: Option<SegmentWriter>,
        now_ms: u64,
    ) -> Result<()> {
        for (kind, w) in [(IndexKind::Files, files), (IndexKind::Chunks, chunks)] {
            let Some(w) = w else { continue };
            let pack = tokio::task::spawn_blocking(move || w.finish())
                .await
                .map_err(store_err)??;
            if let Some(p) = pack {
                self.publisher(nsn, kind).commit(&[p], now_ms).await?;
            }
        }
        Ok(())
    }

    /// Commits the buffered vectors as one segment and completes their jobs.
    pub async fn flush_vectors(&self, ns: &[u8], _now_ms: u64) -> Result<usize> {
        let buf = self.buf(ns);
        let mut b = buf.lock().await;
        let Some(v) = b.vec.take() else { return Ok(0) };
        drop(b);
        let n = v.builder.len();
        if n > 0 {
            let seg = v.builder.finish()?;
            self.gens.add_segment(ns, v.model_gen, &seg).await?;
        }
        for j in &v.jobs {
            self.queue.complete(j).await?;
        }
        Ok(n)
    }

    // -------------------------------------------------------------- tiered merges

    /// Merges one tier of similar-sized vector segments of `ns`, if any. One merge runs at a
    /// time per namespace; a second caller gets `None`.
    pub async fn merge_vectors(&self, ns: &[u8], now_ms: u64) -> Result<Option<MergeReport>> {
        {
            let mut m = self.merging.lock().unwrap_or_else(|p| p.into_inner());
            if !m.insert(ns.to_vec()) {
                return Ok(None);
            }
        }
        let out = self.merge_inner(ns, now_ms).await;
        self.merging
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(ns);
        out
    }

    async fn merge_inner(&self, ns: &[u8], now_ms: u64) -> Result<Option<MergeReport>> {
        let row = self.gens.load(ns).await?;
        let picks: Vec<VecSeg> = pick_vec_merge(&row.state.segments)
            .into_iter()
            .map(|i| row.state.segments[i].clone())
            .collect();
        if picks.is_empty() {
            return Ok(None);
        }
        let model_gen = picks[0].model_gen;
        let tomb = self.gens.tombstones(ns).await?;
        let mut inputs = Vec::with_capacity(picks.len());
        for p in &picks {
            inputs.push(self.gens.read_segment(ns, p).await?);
        }
        let model = inputs[0].model().clone();
        let dims = inputs[0].dims();
        if inputs
            .iter()
            .any(|s| s.dims() != dims || s.model() != &model)
        {
            return Err(CiError::Corrupt("segments of one generation disagree"));
        }
        let mut b = SegmentBuilder::new(model, dims)?;
        let mut seen: HashSet<u64> = HashSet::new();
        let mut dead = 0;
        for s in &inputs {
            for &k in s.keys() {
                if tomb.contains(doc_ord_of(k)) {
                    dead += 1;
                    continue;
                }
                if !seen.insert(k) {
                    continue;
                }
                if let Some(v) = s.vector(k) {
                    b.add(k, v)?;
                }
            }
        }
        let merged = b.finish()?;
        let entry = self.gens.put_segment(ns, model_gen, &merged).await?;
        if entry.bytes > MAX_VEC_MERGE_BYTES {
            return Err(CiError::Invalid("merge output over 512 MiB"));
        }
        let applied = self
            .gens
            .replace_segments(ns, &picks, entry.clone(), now_ms)
            .await?;
        if !applied {
            return Ok(None);
        }
        Ok(Some(MergeReport {
            model_gen,
            inputs: picks.len(),
            vectors: merged.len(),
            dropped_tombstoned: dead,
            bytes: entry.bytes,
        }))
    }

    // -------------------------------------------------------------- the loop

    /// Runs steps until aborted: every namespace with ready jobs, then a short sleep; segments
    /// are committed on the 2 s rule even when the queue is empty.
    pub fn spawn(self: Arc<Self>) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            loop {
                let now = now_ms();
                let mut worked = false;
                let mut nss = self
                    .queue
                    .ready_namespaces(secs(now), 64)
                    .await
                    .unwrap_or_default();
                let buffered: Vec<Vec<u8>> = {
                    let m = self.bufs.lock().unwrap_or_else(|p| p.into_inner());
                    m.keys().cloned().collect()
                };
                for b in buffered {
                    if !nss.contains(&b) {
                        nss.push(b);
                    }
                }
                for ns in nss {
                    match self.step(&ns, now).await {
                        Ok(r) => worked |= r.leased > 0,
                        Err(e) => tracing::warn!(error = %e, "codeintel worker step failed"),
                    }
                    if let Err(e) = self.merge_vectors(&ns, now).await {
                        tracing::warn!(error = %e, "codeintel vector merge failed");
                    }
                }
                if !worked {
                    tokio::time::sleep(Duration::from_millis(250)).await;
                }
            }
        })
    }
}

/// Picks the vector segments to merge: of one generation, in the same size tier (a factor of
/// 4 per tier), [`MERGE_WIDTH`] at a time, with the inputs totalling at most
/// [`MAX_VEC_MERGE_BYTES`] (the output is never larger). Smaller tiers merge first.
pub fn pick_vec_merge(segs: &[VecSeg]) -> Vec<usize> {
    let tier = |b: u64| (b.max(MIN_TIER_BYTES) / MIN_TIER_BYTES).ilog2() / 2;
    let mut groups: std::collections::BTreeMap<(u32, u32), Vec<usize>> = Default::default();
    for (i, s) in segs.iter().enumerate() {
        groups
            .entry((s.model_gen, tier(s.bytes)))
            .or_default()
            .push(i);
    }
    let mut order: Vec<((u32, u32), Vec<usize>)> = groups.into_iter().collect();
    order.sort_by_key(|((g, t), _)| (*t, *g));
    for (_, mut idx) in order {
        if idx.len() < MERGE_WIDTH {
            continue;
        }
        idx.sort_by_key(|&i| (segs[i].bytes, i));
        idx.truncate(MERGE_WIDTH);
        let total: u64 = idx
            .iter()
            .fold(0u64, |a, &i| a.saturating_add(segs[i].bytes));
        if total <= MAX_VEC_MERGE_BYTES {
            idx.sort_unstable();
            return idx;
        }
    }
    Vec::new()
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use asc_codeintel::index::manifest::MemManifest;
    use asc_store::bucket::BucketObjects;

    use super::*;
    use crate::codeintel::modelgen::tests::pool;

    /// Deterministic embedder: counts calls and the largest batch.
    struct Fake {
        spec: ModelSpec,
        calls: AtomicUsize,
        texts: AtomicUsize,
        biggest: AtomicUsize,
    }

    impl Fake {
        fn new(id: &str) -> Arc<Self> {
            Arc::new(Self {
                spec: ModelSpec {
                    model_id: id.to_owned(),
                    calib: [7; 32],
                    dims: 8,
                },
                calls: AtomicUsize::new(0),
                texts: AtomicUsize::new(0),
                biggest: AtomicUsize::new(0),
            })
        }
    }

    impl EmbedModel for Fake {
        fn spec(&self) -> &ModelSpec {
            &self.spec
        }

        fn embed(&self, texts: &[String]) -> std::result::Result<Vec<Vec<i8>>, String> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.texts.fetch_add(texts.len(), Ordering::SeqCst);
            self.biggest.fetch_max(texts.len(), Ordering::SeqCst);
            Ok(texts
                .iter()
                .map(|t| {
                    let h = blake3::hash(t.as_bytes());
                    h.as_bytes()[..8].iter().map(|b| (*b as i8) / 2).collect()
                })
                .collect())
        }
    }

    struct Rig {
        worker: Worker,
        objects: Arc<BucketObjects>,
        model: Arc<Fake>,
        _dir: tempfile::TempDir,
    }

    const NS: &[u8] = b"repo-ns";

    async fn rig() -> Rig {
        let pool = pool().await;
        let objects = Arc::new(BucketObjects::memory());
        let dir = tempfile::tempdir().unwrap();
        let model = Fake::new("fake@abc/i8/calib@7/1");
        let worker = Worker::new(
            pool.clone(),
            objects.clone(),
            Arc::new(SqlManifest::new(pool)),
            Some(model.clone()),
            WorkerConfig::new(dir.path().join("work"), "node-a"),
        )
        .unwrap();
        Rig {
            worker,
            objects,
            model,
            _dir: dir,
        }
    }

    /// Stores a Rust file whose path (and so language) is known, as the enqueue side records.
    async fn put_file(r: &Rig, bytes: &[u8]) -> FileId {
        let id = FileId::of(bytes);
        r.worker
            .reserve_doc(NS, &id, bytes.len() as u64, 0, &[Lang::Rust])
            .await
            .unwrap();
        r.objects
            .put(&ObjKey::File(id), encode_stored(bytes))
            .await
            .unwrap();
        r.worker
            .queue
            .enqueue(NS, &[NewJob::file(id.as_bytes(), Priority::LandTip)], 1)
            .await
            .unwrap();
        id
    }

    const RUST: &str = "/// Adds numbers.\npub fn add_numbers(a: u32, b: u32) -> u32 {\n    a + b\n}\n\npub struct Counter {\n    n: u32,\n}\n\nimpl Counter {\n    pub fn bump(&mut self) {\n        self.n = add_numbers(self.n, 1);\n    }\n}\n";

    #[test]
    fn analysis_applies_skip_rules_secrets_tags_and_chunks() {
        let a = analyze(RUST.as_bytes(), &[Lang::Rust]);
        assert!(a.flags.indexable());
        assert!(!a.grams.is_empty());
        assert!(a.sym_def.iter().any(|s| s == "add_numbers"));
        assert!(a.tags.is_some());
        assert!(!a.chunks.is_empty());

        let bin = analyze(b"\0\0\0binary", &[]);
        assert!(!bin.flags.indexable());
        assert!(bin.grams.is_empty() && bin.chunks.is_empty());

        // Secret: still has grams (exact search) but no chunks, so nothing is embedded.
        let secret = format!(
            "fn main() {{}}\nlet k = \"AKIAIOSFODNN7EXAMPLE\";\n// {}\n",
            "x".repeat(10)
        );
        let s = analyze(secret.as_bytes(), &[Lang::Rust]);
        if s.flags.contains(Flags::SECRET) {
            assert!(!s.grams.is_empty());
            assert!(s.chunks.is_empty());
        }
    }

    #[tokio::test]
    async fn a_file_job_flows_to_a_committed_segment_and_embedded_vectors() {
        let r = rig().await;
        let id = put_file(&r, RUST.as_bytes()).await;

        let rep = r.worker.step(NS, 10_000).await.unwrap();
        assert_eq!((rep.leased, rep.files, rep.failed), (1, 1, 0));
        // Not due yet: the job stays leased and nothing is visible.
        assert!(!rep.committed);
        let nsn = ns_name(NS).unwrap();
        assert_eq!(
            r.worker
                .manifests
                .load(&nsn, IndexKind::Files)
                .await
                .unwrap()
                .generation,
            0
        );

        // 2 s later the commit happens.
        let rep = r.worker.step(NS, 12_000).await.unwrap();
        assert_eq!(rep.leased, 0);
        let fm = r
            .worker
            .manifests
            .load(&nsn, IndexKind::Files)
            .await
            .unwrap();
        let cm = r
            .worker
            .manifests
            .load(&nsn, IndexKind::Chunks)
            .await
            .unwrap();
        assert_eq!((fm.generation, cm.generation), (1, 1));
        let indexed = get_indexed(&*r.objects, &nsn, IndexKind::Files, fm.indexed.unwrap())
            .await
            .unwrap();
        assert!(indexed.contains(0));

        // The row, derived objects and chunk rows exist.
        let (ord, _) = r.worker.doc_row(NS, &id).await.unwrap().unwrap();
        assert_eq!(ord, 0);
        let tags = read_tags(&*r.objects, &id).await.unwrap().unwrap();
        assert!(tags.iter().any(|t| t.name == "add_numbers"));
        let rows = read_chunk_rows(&*r.objects, &id).await.unwrap().unwrap();
        assert!(!rows.is_empty());
        let depth = r.worker.queue.depth().await.unwrap();
        let embeds = depth.iter().find(|d| d.0 == "embed").map_or(0, |d| d.2);
        assert_eq!(embeds as usize, rows.len());
        assert!(depth.iter().all(|d| d.0 != "file"));

        // Embed jobs run, then commit as one vector segment of generation 1.
        let rep = r.worker.step(NS, 20_000).await.unwrap();
        assert_eq!(rep.embedded, rows.len());
        r.worker.flush_all(NS, 20_000).await.unwrap();
        let row = r.worker.gens.load(NS).await.unwrap();
        assert_eq!(row.state.building, Some(1));
        assert_eq!(row.state.segments.len(), 1);
        let seg = r
            .worker
            .gens
            .read_segment(NS, &row.state.segments[0])
            .await
            .unwrap();
        assert_eq!(seg.len(), rows.len());
        assert!(r.worker.queue.depth().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn rerunning_a_finished_job_changes_nothing() {
        let r = rig().await;
        let id = put_file(&r, RUST.as_bytes()).await;
        r.worker.step(NS, 10_000).await.unwrap();
        r.worker.flush(NS, 10_000).await.unwrap();
        let nsn = ns_name(NS).unwrap();
        let g = r
            .worker
            .manifests
            .load(&nsn, IndexKind::Files)
            .await
            .unwrap()
            .generation;
        r.worker
            .queue
            .enqueue(NS, &[NewJob::file(id.as_bytes(), Priority::Backfill)], 11)
            .await
            .unwrap();
        let rep = r.worker.step(NS, 20_000).await.unwrap();
        assert_eq!((rep.files, rep.skipped), (0, 1));
        r.worker.flush(NS, 20_000).await.unwrap();
        assert_eq!(
            r.worker
                .manifests
                .load(&nsn, IndexKind::Files)
                .await
                .unwrap()
                .generation,
            g
        );
    }

    #[tokio::test]
    async fn a_missing_file_object_fails_the_job_for_a_retry() {
        let r = rig().await;
        let id = FileId::of(b"never stored");
        r.worker
            .queue
            .enqueue(NS, &[NewJob::file(id.as_bytes(), Priority::LandTip)], 1)
            .await
            .unwrap();
        let rep = r.worker.step(NS, 10_000).await.unwrap();
        assert_eq!((rep.files, rep.failed), (0, 1));
        assert!(r.worker.doc_row(NS, &id).await.unwrap().is_none());
        let depth = r.worker.queue.depth().await.unwrap();
        assert_eq!(depth, [("file".to_string(), "queued".to_string(), 1)]);
    }

    #[tokio::test]
    async fn embeds_run_in_batches_of_32_and_identical_texts_embed_once() {
        let r = rig().await;
        // Many distinct functions: many chunks; two identical files share chunk texts.
        let mut src = String::new();
        for i in 0..40 {
            src.push_str(&format!("pub fn f{i}() -> u32 {{\n"));
            for j in 0..60 {
                src.push_str(&format!("    let v{j} = {i} * {j} + helper_{j}();\n"));
            }
            src.push_str("    0\n}\n\n");
        }
        let a = put_file(&r, src.as_bytes()).await;
        let b = put_file(&r, format!("// copy\n{src}").as_bytes()).await;
        r.worker.step(NS, 10_000).await.unwrap();
        r.worker.step(NS, 12_000).await.unwrap();
        let chunks_a = read_chunk_rows(&*r.objects, &a)
            .await
            .unwrap()
            .unwrap()
            .len();
        let chunks_b = read_chunk_rows(&*r.objects, &b)
            .await
            .unwrap()
            .unwrap()
            .len();
        assert!(chunks_a + chunks_b > 64, "{chunks_a} + {chunks_b}");
        for t in 0..20 {
            let rep = r.worker.step(NS, 20_000 + t * 3_000).await.unwrap();
            if rep.leased == 0 {
                break;
            }
        }
        r.worker.flush_all(NS, 100_000).await.unwrap();
        assert!(r.model.biggest.load(Ordering::SeqCst) <= EMBED_BATCH);
        assert!(r.model.calls.load(Ordering::SeqCst) >= 2);
        let row = r.worker.gens.load(NS).await.unwrap();
        let vectors: u64 = row.state.segments.iter().map(|s| s.vectors).sum();
        assert_eq!(vectors as usize, chunks_a + chunks_b);
        assert!(r.model.texts.load(Ordering::SeqCst) <= chunks_a + chunks_b);
    }

    #[tokio::test]
    async fn identical_chunk_texts_in_one_batch_are_embedded_once() {
        let r = rig().await;
        let id = put_file(&r, RUST.as_bytes()).await;
        r.worker.step(NS, 10_000).await.unwrap();
        r.worker.step(NS, 12_000).await.unwrap();
        let rows = read_chunk_rows(&*r.objects, &id).await.unwrap().unwrap();
        let n = rows.len();
        // The same chunks again at doc 900.
        sqlx::query(
            "INSERT INTO ci_chunks (ns, chunk_ord, doc_ord, lang, start_line, end_line, text_id)              SELECT ns, chunk_ord + $1, 900, lang, start_line, end_line, text_id              FROM ci_chunks WHERE ns = $2 AND doc_ord = 0",
        )
        .bind((900i64) << 32)
        .bind(NS.to_vec())
        .execute(&r.worker.pool)
        .await
        .unwrap();
        let mut jobs = Vec::new();
        for (i, row) in rows.iter().enumerate() {
            let text = (0..32)
                .map(|k| u8::from_str_radix(&row.text[2 * k..2 * k + 2], 16).unwrap())
                .collect::<Vec<u8>>();
            jobs.push(embed_job(&text, 900, i as u32, 1, Priority::Backfill).unwrap());
        }
        r.worker.queue.enqueue(NS, &jobs, 13).await.unwrap();
        let distinct = rows
            .iter()
            .map(|r| r.text.clone())
            .collect::<HashSet<_>>()
            .len();
        let rep = r.worker.step(NS, 20_000).await.unwrap();
        assert_eq!(rep.embedded, 2 * n);
        assert_eq!(r.model.texts.load(Ordering::SeqCst), distinct);
        r.worker.flush_all(NS, 20_000).await.unwrap();
        let row = r.worker.gens.load(NS).await.unwrap();
        assert_eq!(
            row.state.segments.iter().map(|s| s.vectors).sum::<u64>() as usize,
            2 * n
        );
    }

    #[tokio::test]
    async fn only_the_leader_leases_jobs() {
        let r = rig().await;
        put_file(&r, RUST.as_bytes()).await;
        let nsn = ns_name(NS).unwrap();
        // Another node holds both leases.
        for k in [IndexKind::Files, IndexKind::Chunks] {
            assert!(
                r.worker
                    .manifests
                    .lease(&nsn, k, "node-b", 10_000, 60_000)
                    .await
                    .unwrap()
            );
        }
        let rep = r.worker.step(NS, 11_000).await.unwrap();
        assert!(rep.not_leader);
        assert_eq!(rep.leased, 0);
        // The lease runs out: this node takes over.
        let rep = r.worker.step(NS, 80_000).await.unwrap();
        assert_eq!(rep.files, 1);
    }

    fn seg(gen_: u32, bytes: u64, tag: &str) -> VecSeg {
        VecSeg {
            model_gen: gen_,
            hash: tag.repeat(64)[..64].to_owned(),
            vectors: 1,
            bytes,
        }
    }

    #[test]
    fn tiered_merge_picks_four_similar_segments_of_one_generation() {
        let mb = 1u64 << 20;
        // Four small and one huge: the small ones merge, the huge one never joins.
        let segs = vec![
            seg(1, 8 * mb, "a"),
            seg(1, 9 * mb, "b"),
            seg(1, 8 * mb, "c"),
            seg(1, 10 * mb, "d"),
            seg(1, 400 * mb, "e"),
        ];
        assert_eq!(pick_vec_merge(&segs), [0, 1, 2, 3]);
        // Fewer than four in a tier: nothing.
        assert!(pick_vec_merge(&segs[..3]).is_empty());
        // Generations never mix.
        let mixed = vec![
            seg(1, 8 * mb, "a"),
            seg(2, 8 * mb, "b"),
            seg(1, 8 * mb, "c"),
            seg(2, 8 * mb, "d"),
        ];
        assert!(pick_vec_merge(&mixed).is_empty());
        // Four 200 MiB segments would produce an output over 512 MiB: they do not merge.
        let big: Vec<VecSeg> = (0..4).map(|i| seg(1, 200 * mb, &i.to_string())).collect();
        assert!(pick_vec_merge(&big).is_empty());
        // Dissimilar sizes stay apart.
        let spread = vec![
            seg(1, mb / 8, "a"),
            seg(1, 3 * mb, "b"),
            seg(1, 40 * mb, "c"),
            seg(1, 300 * mb, "d"),
        ];
        assert!(pick_vec_merge(&spread).is_empty());
    }

    #[tokio::test]
    async fn a_merge_combines_segments_dedupes_keys_and_drops_tombstoned_docs() {
        let r = rig().await;
        let spec = r.model.spec.clone();
        r.worker.gens.ensure_model(NS, &spec, 0).await.unwrap();
        // Four tier-0 segments; key (0,0) is in two of them, doc 3 is tombstoned.
        for i in 0..4u32 {
            let mut b = SegmentBuilder::new(spec.model_ref(1), spec.dims).unwrap();
            b.add(chunk_key(i, 0), &[i as i8; 8]).unwrap();
            if i < 2 {
                b.add(chunk_key(0, 0), &[1; 8]).unwrap_or(());
            }
            r.worker
                .gens
                .add_segment(NS, 1, &b.finish().unwrap())
                .await
                .unwrap();
        }
        sqlx::query(
            "INSERT INTO ci_docs (ns, file, ord, size, flags, versions) VALUES ($1, $2, 3, 1, 128, $3)",
        )
        .bind(NS.to_vec())
        .bind(vec![3u8; 32])
        .bind(vec![0u8; 12])
        .execute(&r.worker.pool)
        .await
        .unwrap();

        let rep = r.worker.merge_vectors(NS, 5).await.unwrap().unwrap();
        assert_eq!((rep.model_gen, rep.inputs), (1, 4));
        assert_eq!(rep.dropped_tombstoned, 1);
        assert_eq!(rep.vectors, 3);
        let row = r.worker.gens.load(NS).await.unwrap();
        assert_eq!(row.state.segments.len(), 1);
        assert_eq!(row.state.dropped.len(), 4);
        // Nothing left to merge.
        assert!(r.worker.merge_vectors(NS, 6).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn one_merge_runs_at_a_time_per_namespace() {
        let r = rig().await;
        r.worker.merging.lock().unwrap().insert(NS.to_vec());
        assert!(r.worker.merge_vectors(NS, 1).await.unwrap().is_none());
        r.worker.merging.lock().unwrap().clear();
    }

    #[tokio::test]
    async fn the_sql_manifest_cas_needs_the_lease_and_reports_lost_races() {
        let pool = pool().await;
        let m = SqlManifest::new(pool);
        let ns = "0a0b";
        assert!(
            m.lease(ns, IndexKind::Files, "a", 100, 1_000)
                .await
                .unwrap()
        );
        assert!(
            !m.lease(ns, IndexKind::Files, "b", 200, 1_000)
                .await
                .unwrap()
        );
        assert!(
            m.lease(ns, IndexKind::Files, "a", 300, 1_000)
                .await
                .unwrap()
        );
        let mut next = Manifest::empty(IndexKind::Files);
        next.generation = 1;
        next.indexed = Some([9; 32]);
        match m
            .cas(ns, IndexKind::Files, 0, next.clone(), "a", 400)
            .await
            .unwrap()
        {
            Cas::Won(cur) => {
                assert_eq!(cur.generation, 1);
                assert_eq!(cur.indexed, Some([9; 32]));
            }
            Cas::Lost(_) => panic!("lost"),
        }
        assert!(matches!(
            m.cas(ns, IndexKind::Files, 0, next.clone(), "a", 400)
                .await
                .unwrap(),
            Cas::Lost(_)
        ));
        // After the lease expires, the old holder cannot commit; a new one can.
        next.generation = 2;
        assert!(matches!(
            m.cas(ns, IndexKind::Files, 1, next.clone(), "a", 5_000)
                .await,
            Err(IndexError::NotLeader)
        ));
        assert!(
            m.lease(ns, IndexKind::Files, "b", 5_000, 1_000)
                .await
                .unwrap()
        );
        assert!(matches!(
            m.cas(ns, IndexKind::Files, 1, next, "b", 5_100)
                .await
                .unwrap(),
            Cas::Won(_)
        ));
        let _ = MemManifest::new();
    }
}
