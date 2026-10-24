//! Garbage collection of one repository namespace (SPEC §3.6, unit O01).
//!
//! A [`Collector`] takes a *mark* (the set of keys reachable from the roots) and then *sweeps*:
//! an object is deleted only when it is older than `min_age` (14 days) and was unreachable in
//! this mark and in an earlier complete mark taken at least `mark_gap` (24 h) before. Objects an
//! operator purged (`GcRootSource::purged`) skip the age rule and the earlier mark but must be
//! unreachable from *strict* roots (refs, ref-log targets, extra roots), not merely from a
//! recently ingested changeset.
//!
//! Sweeps are fenced: before each batch of deletes the sweeper enters a [`Fence`] (the server's
//! exclusive GC lock, or the bucket's `gc/freeze` key), re-marks from the current refs and from
//! changesets ingested since the mark started, drops newly reachable keys from the batch, and
//! only then deletes. Marks live in a [`MarkStore`]: SQL on the server (`gc_marks`), the bucket
//! under `gc/<run>/` in bucket mode ([`BucketGc`]).
//!
//! All stored bytes are untrusted: objects are read through [`Objects::get`] (verified, size
//! capped), a corrupt object aborts the run, and the number of marks held is bounded by
//! `max_marks` (the run fails closed instead of exhausting memory).

use std::collections::HashMap;
use std::fmt::{self, Write as _};
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use bytes::Bytes;
use asc_core::change::ChangeMeta;
use asc_core::codec::decode_envelope;
use asc_core::git::EXTRA_GIT_COMMIT;
use asc_core::id::{ChangesetId, FileId, GitOid, RefTarget, TreeId};
use asc_core::object::{Change, Changeset, ChunkList, Kind, TagRecord, Tree};
use futures::{StreamExt, TryStreamExt, stream};
use object_store::path::Path;
use object_store::{ObjectStore, ObjectStoreExt, PutMode, PutOptions, PutPayload, UpdateVersion};
use uuid::Uuid;

use crate::bucket::BucketObjects;
use crate::bucketrefs::{BucketRefEntry, BucketRefs};
use crate::key::{KeyClass, KeyPrefix, ObjKey};
use crate::objects::Objects;
use crate::refs::{RefError, Refs, RepoKey};

/// Classes the sweep may delete. `Derived` values are recomputable and are never collected.
pub const SWEEP_CLASSES: [KeyClass; 8] = [
    KeyClass::File,
    KeyClass::ChunkList,
    KeyClass::Chunk,
    KeyClass::Tree,
    KeyClass::Changeset,
    KeyClass::Tag,
    KeyClass::GitTreeOverride,
    KeyClass::ChangeMeta,
];

/// Files above this size are chunked and have an `l/` key.
const CHUNK_THRESHOLD: u64 = 4 * 1024 * 1024;
/// Objects decoded concurrently while marking.
const FETCH_CONCURRENCY: usize = 16;
/// Objects fetched per marking round.
const FETCH_WINDOW: usize = 256;
/// Keys per stored mark part.
const PART_KEYS: usize = 65_536;
/// Largest mark part read back from a bucket.
const MAX_PART_BYTES: u64 = 16 * 1024 * 1024;
/// Longest `gc/lock` or `gc/freeze` body read.
const MAX_LOCK_BYTES: u64 = 256;
/// Listed or deleted objects between lease checks.
const KEEPALIVE_EVERY: usize = 256;
/// A stale run directory listing is never longer than this many entries.
const MAX_RUNS: usize = 4096;

/// Why a collection failed.
#[derive(Debug, thiserror::Error)]
pub enum GcError {
    #[error("object store: {0}")]
    Objects(#[from] crate::objects::Error),
    #[error("refs: {0}")]
    Refs(String),
    #[error("marks: {0}")]
    Marks(String),
    #[error("cannot read {key}: {why}")]
    Undecodable { key: ObjKey, why: String },
    #[error("more than {0} marks; raise asc.gc.maxMarks or collect a smaller repository")]
    TooManyMarks(usize),
    /// Another collection holds the run lock.
    #[error("garbage collection is already running: {0}")]
    Busy(String),
    #[error("{0}")]
    Other(String),
}

impl From<RefError> for GcError {
    fn from(e: RefError) -> Self {
        Self::Refs(e.to_string())
    }
}

impl From<object_store::Error> for GcError {
    fn from(e: object_store::Error) -> Self {
        Self::Marks(e.to_string())
    }
}

pub type Result<T, E = GcError> = std::result::Result<T, E>;

/// Knobs of one collection. [`GcConfig::default`] is the SPEC §3.6 policy.
#[derive(Clone, Debug)]
pub struct GcConfig {
    /// Objects younger than this are never deleted, and changesets younger than this are roots.
    pub min_age: Duration,
    /// The least time between the two marks a sweep needs.
    pub mark_gap: Duration,
    /// Ref-log targets newer than this are roots (`asc.gc.pitrDays`).
    pub pitr: Duration,
    /// Candidates re-marked and deleted per fenced batch.
    pub batch: usize,
    /// Most keys held in memory for one mark.
    pub max_marks: usize,
    /// Bucket mode: how long `gc/freeze` stands before the sweep re-marks.
    pub freeze_wait: Duration,
    /// Bucket mode: the `gc/lock` lease.
    pub lease: Duration,
    /// Keys named in a [`SweepReport`].
    pub report_limit: usize,
}

const DAY: Duration = Duration::from_secs(24 * 3600);

impl Default for GcConfig {
    fn default() -> Self {
        Self {
            min_age: 14 * DAY,
            mark_gap: DAY,
            pitr: 14 * DAY,
            batch: 5000,
            max_marks: 16_000_000,
            freeze_wait: Duration::from_secs(600),
            lease: Duration::from_secs(6 * 3600),
            report_limit: 20,
        }
    }
}

// ------------------------------------------------------------------------------ extension points

/// Refs as roots. `current` is every ref target now; `logged_since` is every target (old and
/// new) the ref log recorded at or after `since` (unix seconds), so that restoring a database
/// backup taken inside the window finds the closure of every ref it held.
#[async_trait]
pub trait RefRoots: Send + Sync {
    async fn current(&self) -> Result<Vec<RefTarget>>;
    async fn logged_since(&self, since: i64) -> Result<Vec<RefTarget>>;
}

/// Extra roots from other units: change versions (C18), commit-cloud heads (B10), unexpired
/// Actions manifests (U75).
#[async_trait]
pub trait GcRootSource: Send + Sync {
    /// Keys that must stay reachable, for the namespace `ns`.
    async fn roots(&self, ns: &[u8]) -> Result<Vec<ObjKey>>;
    /// Keys an operator purged (`gc_purge`). They are swept at the next run when no strict root
    /// reaches them.
    async fn purged(&self, _ns: &[u8]) -> Result<Vec<ObjKey>> {
        Ok(Vec::new())
    }
    /// A sweep that started at `as_of` (unix seconds) finished: every key in `keys` came from
    /// [`purged`](Self::purged) and is now deleted or gone, so the source may forget it. Keys a
    /// strict root still reaches are not listed and stay purged. Entries purged after `as_of`
    /// must be kept.
    async fn purge_swept(&self, _ns: &[u8], _keys: &[ObjKey], _as_of: i64) -> Result<()> {
        Ok(())
    }
}

/// How strongly a key is marked.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tier {
    /// Reached from refs, ref-log targets or extra roots.
    Strict = 0,
    /// Reached only from a changeset ingested within `min_age`.
    Recent = 1,
}

impl Tier {
    fn from_u8(b: u8) -> Option<Self> {
        match b {
            0 => Some(Self::Strict),
            1 => Some(Self::Recent),
            _ => None,
        }
    }
}

/// One stored mark run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MarkRun {
    /// Unix seconds at which the mark started.
    pub run: i64,
    pub complete: bool,
}

/// Persistent marks of one namespace.
#[async_trait]
pub trait MarkStore: Send + Sync {
    async fn runs(&self) -> Result<Vec<MarkRun>>;
    async fn append(&self, run: i64, tier: Tier, keys: &[ObjKey]) -> Result<()>;
    /// Seals `run`: only a sealed run is used as the earlier mark of a sweep.
    async fn complete(&self, run: i64) -> Result<()>;
    /// All marks of `run`; fails with `TooManyMarks` beyond `cap`.
    async fn load(&self, run: i64, cap: usize) -> Result<HashMap<ObjKey, Tier>>;
    async fn drop_run(&self, run: i64) -> Result<()>;
}

/// Entered around each batch of deletes.
#[async_trait]
pub trait Fence: Send + Sync {
    /// Returns once no ref update can be between its ingest and its commit; the guard keeps it so.
    async fn enter(&self) -> Result<Box<dyn Send>>;

    /// Called often during marking and sweeping: extends whatever lease the run holds when it is
    /// due, and fails when the run no longer holds it. Cheap when nothing is due.
    async fn keepalive(&self) -> Result<()> {
        Ok(())
    }

    /// Confirms now, not when due, that the run still holds its exclusion. Called after the
    /// final re-mark and before anything is deleted; an error abandons the batch.
    async fn confirm(&self) -> Result<()> {
        Ok(())
    }
}

/// A fence that fences nothing (dry runs and tests).
pub struct NoFence;

#[async_trait]
impl Fence for NoFence {
    async fn enter(&self) -> Result<Box<dyn Send>> {
        Ok(Box::new(()))
    }
}

/// Told which objects are about to be deleted, inside the fence and before the delete, so that
/// derived rows pointing at them go first. A crash in between leaves an orphan object that the
/// next run sweeps again, never a row pointing at a missing object.
#[async_trait]
pub trait SweepObserver: Send + Sync {
    async fn before_delete(&self, keys: &[ObjKey]) -> Result<()>;
}

// ------------------------------------------------------------------------------ marking

/// Keys seen so far: an optional read-only base (the mark a re-mark extends) plus new keys.
struct Seen<'a> {
    base: Option<&'a HashMap<ObjKey, Tier>>,
    local: HashMap<ObjKey, Tier>,
    cap: usize,
}

impl<'a> Seen<'a> {
    fn new(base: Option<&'a HashMap<ObjKey, Tier>>, cap: usize) -> Self {
        Self {
            base,
            local: HashMap::new(),
            cap,
        }
    }

    /// Whether `k` is already marked at least as strongly as `tier`.
    fn has(&self, k: &ObjKey, tier: Tier) -> bool {
        let sat = |t: Tier| t == Tier::Strict || tier == Tier::Recent;
        self.local.get(k).is_some_and(|&t| sat(t))
            || self.base.is_some_and(|b| b.get(k).is_some_and(|&t| sat(t)))
    }

    /// Marks `k`; true when that changed anything.
    fn add(&mut self, k: ObjKey, tier: Tier) -> Result<bool> {
        if self.has(&k, tier) {
            return Ok(false);
        }
        if self.local.len() >= self.cap && !self.local.contains_key(&k) {
            return Err(GcError::TooManyMarks(self.cap));
        }
        self.local.insert(k, tier);
        Ok(true)
    }
}

fn undecodable(key: &ObjKey, e: impl fmt::Display) -> GcError {
    GcError::Undecodable {
        key: *key,
        why: e.to_string(),
    }
}

fn file_keys(out: &mut Vec<ObjKey>, id: FileId, size: u64) {
    out.push(ObjKey::File(id));
    if size > CHUNK_THRESHOLD {
        out.push(ObjKey::ChunkList(id));
    }
}

/// The `tree <hex>` oid of a raw git commit kept in `git.commit`.
fn raw_commit_tree(raw: &[u8]) -> Option<GitOid> {
    let line = raw.strip_prefix(b"tree ")?;
    let hex = line.get(..40)?;
    let bytes: [u8; 20] = data_encoding::HEXLOWER.decode(hex).ok()?.try_into().ok()?;
    Some(GitOid::from_bytes(bytes))
}

/// The objects a decoded object refers to.
fn children(key: &ObjKey, data: &[u8]) -> Result<Vec<ObjKey>> {
    let mut out = Vec::new();
    match key {
        ObjKey::Changeset(_) => {
            let cs: Changeset = decode_envelope(data).map_err(|e| undecodable(key, e))?;
            out.extend(cs.parents.iter().map(|&p| ObjKey::Changeset(p)));
            out.push(ObjKey::Tree(cs.root));
            for (_, change) in cs.changes.as_slice() {
                if let Change::Write { file, size, .. } = change {
                    file_keys(&mut out, *file, *size);
                }
            }
            if let Some(raw) = cs.extra.get(&EXTRA_GIT_COMMIT.to_vec())
                && let Some(oid) = raw_commit_tree(raw)
            {
                out.push(ObjKey::GitTreeOverride(oid));
            }
        }
        ObjKey::Tree(_) => {
            let tree: Tree = decode_envelope(data).map_err(|e| undecodable(key, e))?;
            for e in tree.entries() {
                match e.kind {
                    Kind::Dir => {
                        out.push(ObjKey::Tree(TreeId::from_bytes(e.id)));
                        out.push(ObjKey::GitTreeOverride(e.git));
                    }
                    Kind::File | Kind::Exec | Kind::Symlink => {
                        file_keys(&mut out, FileId::from_bytes(e.id), e.size);
                    }
                    Kind::Gitlink => {}
                }
            }
        }
        ObjKey::Tag(_) => {
            let tag: TagRecord = decode_envelope(data).map_err(|e| undecodable(key, e))?;
            out.push(ObjKey::Changeset(tag.target));
        }
        ObjKey::ChunkList(_) => {
            let list: ChunkList = decode_envelope(data).map_err(|e| undecodable(key, e))?;
            out.extend(list.chunks.iter().map(|&(c, _)| ObjKey::Chunk(c)));
        }
        _ => {}
    }
    Ok(out)
}

/// Whether reading `key` can name further objects.
fn has_children(key: &ObjKey) -> bool {
    matches!(
        key,
        ObjKey::Changeset(_) | ObjKey::Tree(_) | ObjKey::Tag(_) | ObjKey::ChunkList(_)
    )
}

/// Marks everything reachable from `roots` at `tier`, stopping at keys already seen. `keep` is
/// kept alive once per round of fetches.
async fn walk(
    objects: &dyn Objects,
    keep: &dyn Fence,
    seen: &mut Seen<'_>,
    roots: impl IntoIterator<Item = ObjKey>,
    tier: Tier,
) -> Result<()> {
    let mut frontier = Vec::new();
    for r in roots {
        if seen.add(r, tier)? && has_children(&r) {
            frontier.push(r);
        }
    }
    while !frontier.is_empty() {
        keep.keepalive().await?;
        let take = frontier.split_off(frontier.len().saturating_sub(FETCH_WINDOW));
        let fetched: Vec<(ObjKey, _)> = stream::iter(take)
            .map(|k| async move { (k, objects.get(&k).await) })
            .buffer_unordered(FETCH_CONCURRENCY)
            .collect()
            .await;
        for (k, got) in fetched {
            // A missing object has nothing to follow; a corrupt one fails the run.
            let Some(data) = got? else { continue };
            for child in children(&k, &data)? {
                if seen.add(child, tier)? && has_children(&child) {
                    frontier.push(child);
                }
            }
        }
    }
    Ok(())
}

fn target_key(t: RefTarget) -> ObjKey {
    match t {
        RefTarget::Changeset(c) => ObjKey::Changeset(c),
        RefTarget::Tag(g) => ObjKey::Tag(g),
    }
}

pub(crate) fn unix(t: SystemTime) -> i64 {
    match t.duration_since(UNIX_EPOCH) {
        Ok(d) => i64::try_from(d.as_secs()).unwrap_or(i64::MAX),
        Err(_) => 0,
    }
}

fn at_unix(secs: i64) -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(secs.max(0).unsigned_abs())
}

fn secs(d: Duration) -> i64 {
    i64::try_from(d.as_secs()).unwrap_or(i64::MAX)
}

// ------------------------------------------------------------------------------ reports

/// What a mark found.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MarkReport {
    /// The run id: unix seconds at the start of the mark.
    pub run: i64,
    pub strict: u64,
    pub recent: u64,
}

/// What a sweep did (or, in a dry run, would do).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SweepReport {
    pub dry_run: bool,
    /// No earlier complete mark is old enough; only purged objects can be swept this time.
    pub waiting_for_second_mark: bool,
    pub scanned: u64,
    pub candidates: u64,
    pub deleted: u64,
    /// Candidates a fenced re-mark found reachable again.
    pub rescued: u64,
    pub purged: u64,
    /// Objects that could not be read and were kept.
    pub unreadable: u64,
    /// The first few candidate keys.
    pub sample: Vec<ObjKey>,
}

/// A whole collection.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GcReport {
    pub mark: MarkReport,
    pub sweep: SweepReport,
}

impl fmt::Display for GcReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (m, s) = (&self.mark, &self.sweep);
        writeln!(
            f,
            "mark {}: {} reachable from refs, {} more from recent changesets",
            m.run, m.strict, m.recent
        )?;
        if s.waiting_for_second_mark {
            writeln!(
                f,
                "no earlier mark is 24 hours old yet: nothing is deleted until the next run"
            )?;
        }
        writeln!(f, "scanned {} objects", s.scanned)?;
        let verb = if s.dry_run { "would delete" } else { "deleted" };
        let n = if s.dry_run { s.candidates } else { s.deleted };
        writeln!(f, "{verb} {n} objects ({} purged)", s.purged)?;
        if s.rescued > 0 {
            writeln!(f, "kept {} objects that became reachable again", s.rescued)?;
        }
        if s.unreadable > 0 {
            writeln!(f, "kept {} unreadable change records", s.unreadable)?;
        }
        for k in &s.sample {
            writeln!(f, "  {k}")?;
        }
        if s.candidates > s.sample.len() as u64 {
            writeln!(f, "  ...")?;
        }
        Ok(())
    }
}

// ------------------------------------------------------------------------------ collector

/// One namespace's collection.
pub struct Collector<'a> {
    pub ns: &'a [u8],
    pub objects: &'a dyn Objects,
    pub refs: &'a dyn RefRoots,
    pub extra: &'a [Arc<dyn GcRootSource>],
    pub marks: &'a dyn MarkStore,
    pub observer: Option<&'a dyn SweepObserver>,
    pub cfg: &'a GcConfig,
}

struct Candidate {
    key: ObjKey,
    purged: bool,
    /// For a change record, the changeset it follows.
    follows: Option<ChangesetId>,
}

impl Collector<'_> {
    /// Mark, sweep and prune old marks.
    pub async fn collect(
        &self,
        now: SystemTime,
        dry_run: bool,
        fence: &dyn Fence,
    ) -> Result<GcReport> {
        let mark = self.mark_fenced(now, fence).await?;
        let sweep = self.sweep(&mark, now, dry_run, fence).await?;
        self.prune(mark.run, now).await?;
        Ok(GcReport { mark, sweep })
    }

    async fn strict_roots(&self, pitr_since: i64) -> Result<Vec<ObjKey>> {
        let mut roots: Vec<ObjKey> = self
            .refs
            .current()
            .await?
            .into_iter()
            .map(target_key)
            .collect();
        roots.extend(
            self.refs
                .logged_since(pitr_since)
                .await?
                .into_iter()
                .map(target_key),
        );
        for src in self.extra {
            roots.extend(src.roots(self.ns).await?);
        }
        if roots.len() > self.cfg.max_marks {
            return Err(GcError::TooManyMarks(self.cfg.max_marks));
        }
        Ok(roots)
    }

    /// Changesets and tags stored at or after `since`.
    async fn recent_roots(&self, since: SystemTime, keep: &dyn Fence) -> Result<Vec<ObjKey>> {
        let mut out = Vec::new();
        for class in [KeyClass::Changeset, KeyClass::Tag] {
            let mut s = self.objects.list(KeyPrefix::class(class));
            let mut listed = 0usize;
            while let Some(item) = s.next().await {
                let (key, modified) = item?;
                listed += 1;
                if listed.is_multiple_of(KEEPALIVE_EVERY) {
                    keep.keepalive().await?;
                }
                if modified >= since {
                    if out.len() >= self.cfg.max_marks {
                        return Err(GcError::TooManyMarks(self.cfg.max_marks));
                    }
                    out.push(key);
                }
            }
        }
        Ok(out)
    }

    /// Takes and stores one mark, with nothing to keep alive.
    pub async fn mark(&self, now: SystemTime) -> Result<MarkReport> {
        self.mark_fenced(now, &NoFence).await
    }

    /// Takes and stores one mark, keeping `keep`'s lease alive while it runs.
    pub async fn mark_fenced(&self, now: SystemTime, keep: &dyn Fence) -> Result<MarkReport> {
        let run = unix(now);
        let pitr_since = run.saturating_sub(secs(self.cfg.pitr));
        let mut seen = Seen::new(None, self.cfg.max_marks);
        let strict = self.strict_roots(pitr_since).await?;
        walk(self.objects, keep, &mut seen, strict, Tier::Strict).await?;
        let recent = self.recent_roots(now - self.cfg.min_age, keep).await?;
        walk(self.objects, keep, &mut seen, recent, Tier::Recent).await?;

        let mut report = MarkReport {
            run,
            ..MarkReport::default()
        };
        for tier in [Tier::Strict, Tier::Recent] {
            let keys: Vec<ObjKey> = seen
                .local
                .iter()
                .filter(|&(_, &t)| t == tier)
                .map(|(k, _)| *k)
                .collect();
            match tier {
                Tier::Strict => report.strict = keys.len() as u64,
                Tier::Recent => report.recent = keys.len() as u64,
            }
            for part in keys.chunks(PART_KEYS) {
                keep.keepalive().await?;
                self.marks.append(run, tier, part).await?;
            }
        }
        keep.keepalive().await?;
        self.marks.complete(run).await?;
        Ok(report)
    }

    /// The newest complete run at least `mark_gap` older than `run`.
    async fn previous(&self, run: i64) -> Result<Option<i64>> {
        let limit = run.saturating_sub(secs(self.cfg.mark_gap));
        Ok(self
            .marks
            .runs()
            .await?
            .into_iter()
            .filter(|r| r.complete && r.run <= limit)
            .map(|r| r.run)
            .max())
    }

    /// Deletes what two marks agree is unreachable, in fenced batches.
    pub async fn sweep(
        &self,
        mark: &MarkReport,
        now: SystemTime,
        dry_run: bool,
        fence: &dyn Fence,
    ) -> Result<SweepReport> {
        let cap = self.cfg.max_marks;
        let n = self.marks.load(mark.run, cap).await?;
        let prev = self.previous(mark.run).await?;
        let m = match prev {
            Some(r) => self.marks.load(r, cap).await?,
            None => HashMap::new(),
        };
        let mut purge = std::collections::HashSet::new();
        for src in self.extra {
            purge.extend(src.purged(self.ns).await?);
        }
        // Purged keys that a strict root keeps; the rest are done once the sweep finishes.
        let mut held: std::collections::HashSet<ObjKey> = std::collections::HashSet::new();
        let mut report = SweepReport {
            dry_run,
            waiting_for_second_mark: prev.is_none(),
            ..SweepReport::default()
        };

        let mut batch: Vec<Candidate> = Vec::new();
        for class in SWEEP_CLASSES {
            let mut listing = self.objects.list(KeyPrefix::class(class));
            while let Some(item) = listing.next().await {
                let (key, modified) = item?;
                report.scanned += 1;
                if report.scanned.is_multiple_of(KEEPALIVE_EVERY as u64) {
                    fence.keepalive().await?;
                }
                let purged = purge.contains(&key);
                let old_enough = now
                    .duration_since(modified)
                    .is_ok_and(|a| a >= self.cfg.min_age);
                let mut follows = None;
                if purged {
                    // Purged: no age rule, no earlier mark; only a strict root keeps it.
                    if n.get(&key) == Some(&Tier::Strict) {
                        held.insert(key);
                        continue;
                    }
                } else {
                    if !old_enough || prev.is_none() || n.contains_key(&key) || m.contains_key(&key)
                    {
                        continue;
                    }
                    if let ObjKey::ChangeMeta(_) = key {
                        match self.meta_target(&key).await {
                            Some(cs) => {
                                let k = ObjKey::Changeset(cs);
                                if n.contains_key(&k) || m.contains_key(&k) {
                                    continue;
                                }
                                follows = Some(cs);
                            }
                            None => {
                                report.unreadable += 1;
                                continue;
                            }
                        }
                    }
                }
                report.candidates += 1;
                if purged {
                    report.purged += 1;
                }
                if report.sample.len() < self.cfg.report_limit {
                    report.sample.push(key);
                }
                if dry_run {
                    continue;
                }
                batch.push(Candidate {
                    key,
                    purged,
                    follows,
                });
                if batch.len() >= self.cfg.batch {
                    self.delete_batch(
                        mark,
                        &n,
                        std::mem::take(&mut batch),
                        fence,
                        &mut report,
                        &mut held,
                    )
                    .await?;
                }
            }
        }
        if !batch.is_empty() {
            self.delete_batch(mark, &n, batch, fence, &mut report, &mut held)
                .await?;
        }
        if !dry_run {
            // The sweep finished without error: whatever was purged is deleted, gone or kept
            // by a strict root. The sources forget the first two; the last stay listed.
            let done: Vec<ObjKey> = purge.into_iter().filter(|k| !held.contains(k)).collect();
            if !done.is_empty() {
                for src in self.extra {
                    src.purge_swept(self.ns, &done, mark.run).await?;
                }
            }
        }
        Ok(report)
    }

    /// The changeset a change record describes; `None` when it cannot be read.
    async fn meta_target(&self, key: &ObjKey) -> Option<ChangesetId> {
        let data = self.objects.get(key).await.ok()??;
        let meta: ChangeMeta = decode_envelope(&data).ok()?;
        Some(meta.cs)
    }

    /// One fenced batch: re-mark, drop what became reachable, delete the rest.
    async fn delete_batch(
        &self,
        mark: &MarkReport,
        n: &HashMap<ObjKey, Tier>,
        batch: Vec<Candidate>,
        fence: &dyn Fence,
        report: &mut SweepReport,
        held: &mut std::collections::HashSet<ObjKey>,
    ) -> Result<()> {
        let _guard = fence.enter().await?;
        // Everything below sees refs and storage as no push can change them until the guard drops.
        let mut again = Seen::new(Some(n), self.cfg.max_marks);
        let strict = self.strict_roots(mark.run).await?;
        walk(self.objects, fence, &mut again, strict, Tier::Strict).await?;
        let since = at_unix(mark.run.saturating_sub(5));
        let recent = self.recent_roots(since, fence).await?;
        walk(self.objects, fence, &mut again, recent, Tier::Recent).await?;

        let mut doomed = Vec::with_capacity(batch.len());
        for c in batch {
            let revived = if c.purged {
                again.local.get(&c.key) == Some(&Tier::Strict)
            } else if let Some(cs) = c.follows {
                again.local.contains_key(&ObjKey::Changeset(cs))
            } else {
                again.local.contains_key(&c.key)
            };
            if revived {
                if c.purged {
                    held.insert(c.key);
                }
                report.rescued += 1;
            } else {
                doomed.push(c.key);
            }
        }
        // The re-mark can be long. Nothing is deleted unless the run still holds its exclusion
        // now; a run whose lease was taken over stops here.
        fence.confirm().await?;
        if let Some(o) = self.observer {
            o.before_delete(&doomed).await?;
        }
        for (i, key) in doomed.iter().enumerate() {
            if i % KEEPALIVE_EVERY == KEEPALIVE_EVERY - 1 {
                fence.keepalive().await?;
            }
            self.objects.delete(key).await?;
            report.deleted += 1;
        }
        Ok(())
    }

    /// Drops runs no later sweep can use: incomplete ones, and complete ones superseded by a
    /// newer run that is also at least `mark_gap` old.
    async fn prune(&self, current: i64, now: SystemTime) -> Result<()> {
        let limit = unix(now).saturating_sub(secs(self.cfg.mark_gap));
        let runs = self.marks.runs().await?;
        let newest_old = runs
            .iter()
            .filter(|r| r.complete && r.run <= limit)
            .map(|r| r.run)
            .max();
        for r in runs {
            let keep =
                r.run == current || (r.complete && (r.run > limit || Some(r.run) == newest_old));
            if !keep {
                self.marks.drop_run(r.run).await?;
            }
        }
        Ok(())
    }
}

// ------------------------------------------------------------------------------ bucket mode

fn gc_dir(root: &Path) -> Path {
    root.clone().join("gc")
}

fn run_dir(root: &Path, run: i64) -> Path {
    gc_dir(root).join(format!("{run:020}"))
}

/// Marks stored in the bucket under `gc/<run>/`.
pub struct BucketMarks {
    store: Arc<dyn ObjectStore>,
    root: Path,
}

impl BucketMarks {
    pub fn new(store: Arc<dyn ObjectStore>, root: Path) -> Self {
        Self { store, root }
    }

    async fn exists(&self, p: &Path) -> Result<bool> {
        match self.store.head(p).await {
            Ok(_) => Ok(true),
            Err(object_store::Error::NotFound { .. }) => Ok(false),
            Err(e) => Err(e.into()),
        }
    }
}

#[async_trait]
impl MarkStore for BucketMarks {
    async fn runs(&self) -> Result<Vec<MarkRun>> {
        let listing = self
            .store
            .list_with_delimiter(Some(&gc_dir(&self.root)))
            .await?;
        let mut out = Vec::new();
        for dir in listing.common_prefixes.iter().take(MAX_RUNS) {
            let Some(run) = dir.filename().and_then(|n| n.parse::<i64>().ok()) else {
                continue;
            };
            let complete = self.exists(&dir.clone().join("done")).await?;
            out.push(MarkRun { run, complete });
        }
        Ok(out)
    }

    async fn append(&self, run: i64, tier: Tier, keys: &[ObjKey]) -> Result<()> {
        for part in keys.chunks(PART_KEYS) {
            let mut text = String::new();
            for k in part {
                let _ = writeln!(text, "{}", k.path());
            }
            let name = format!("t{}.{}", tier as u8, Uuid::new_v4().simple());
            self.store
                .put(
                    &run_dir(&self.root, run).join(name),
                    PutPayload::from_bytes(Bytes::from(text)),
                )
                .await?;
        }
        Ok(())
    }

    async fn complete(&self, run: i64) -> Result<()> {
        self.store
            .put(
                &run_dir(&self.root, run).join("done"),
                PutPayload::from_bytes(Bytes::from(run.to_string())),
            )
            .await?;
        Ok(())
    }

    async fn load(&self, run: i64, cap: usize) -> Result<HashMap<ObjKey, Tier>> {
        let dir = run_dir(&self.root, run);
        let metas: Vec<_> = self.store.list(Some(&dir)).try_collect().await?;
        let mut out = HashMap::new();
        for meta in metas {
            let Some(name) = meta.location.filename() else {
                continue;
            };
            let Some(tier) = name
                .strip_prefix('t')
                .and_then(|r| r.split('.').next())
                .and_then(|t| t.parse::<u8>().ok())
                .and_then(Tier::from_u8)
            else {
                continue;
            };
            let got = self.store.get(&meta.location).await?;
            let body = crate::bucket::read_capped(got, MAX_PART_BYTES)
                .await?
                .ok_or_else(|| GcError::Marks(format!("{} is too large", meta.location)))?;
            let text = std::str::from_utf8(&body)
                .map_err(|_| GcError::Marks(format!("{} is not UTF-8", meta.location)))?;
            for line in text.lines() {
                let key = ObjKey::parse(line)
                    .ok_or_else(|| GcError::Marks(format!("bad mark {line:?}")))?;
                if out.len() >= cap && !out.contains_key(&key) {
                    return Err(GcError::TooManyMarks(cap));
                }
                // A key marked in both tiers is strict.
                let slot = out.entry(key).or_insert(tier);
                if tier == Tier::Strict {
                    *slot = Tier::Strict;
                }
            }
        }
        Ok(out)
    }

    async fn drop_run(&self, run: i64) -> Result<()> {
        let dir = run_dir(&self.root, run);
        // `done` first: an interrupted drop leaves an incomplete run, never a truncated complete one.
        match self.store.delete(&dir.clone().join("done")).await {
            Ok(()) | Err(object_store::Error::NotFound { .. }) => {}
            Err(e) => return Err(e.into()),
        }
        let metas: Vec<_> = self.store.list(Some(&dir)).try_collect().await?;
        for meta in metas {
            match self.store.delete(&meta.location).await {
                Ok(()) | Err(object_store::Error::NotFound { .. }) => {}
                Err(e) => return Err(e.into()),
            }
        }
        Ok(())
    }
}

/// Bucket-mode ref roots: current refs from `BucketRefs`, ref-log targets from `rl/`.
pub struct BucketRefRoots {
    refs: BucketRefs,
    store: Arc<dyn ObjectStore>,
    root: Path,
    max_entries: usize,
}

/// Clock skew allowed between a ref-log entry's `at` and the bucket's modification time.
const LOG_SKEW: i64 = 24 * 3600;

impl BucketRefRoots {
    pub fn new(
        refs: BucketRefs,
        store: Arc<dyn ObjectStore>,
        root: Path,
        max_entries: usize,
    ) -> Self {
        Self {
            refs,
            store,
            root,
            max_entries,
        }
    }
}

#[async_trait]
impl RefRoots for BucketRefRoots {
    async fn current(&self) -> Result<Vec<RefTarget>> {
        let all = self.refs.list(RepoKey(0), "").await?;
        Ok(all.into_iter().map(|e| e.target).collect())
    }

    async fn logged_since(&self, since: i64) -> Result<Vec<RefTarget>> {
        let prefix = self.root.clone().join("rl");
        let mut out = Vec::new();
        let mut listing = self.store.list(Some(&prefix));
        let mut read = 0usize;
        while let Some(meta) = listing.try_next().await? {
            if unix(SystemTime::from(meta.last_modified)) < since.saturating_sub(LOG_SKEW) {
                continue;
            }
            read += 1;
            if read > self.max_entries {
                return Err(GcError::TooManyMarks(self.max_entries));
            }
            if meta.size > 4096 {
                return Err(GcError::Marks(format!("{} is too large", meta.location)));
            }
            let body = self.store.get(&meta.location).await?.bytes().await?;
            let entry = BucketRefEntry::decode(&body)
                .map_err(|e| GcError::Marks(format!("{}: {e}", meta.location)))?;
            if entry.at >= since {
                out.extend(entry.old);
                out.extend(entry.new);
            }
        }
        Ok(out)
    }
}

/// Formats unix seconds as `YYYY-MM-DDTHH:MM:SSZ`.
pub fn rfc3339(secs: i64) -> String {
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

/// Unix seconds now. Production reads the run's start plus the time elapsed since, so that
/// lease times stay on the run's own timeline; tests drive it by hand.
pub type Clock = Arc<dyn Fn() -> i64 + Send + Sync>;

fn elapsed_clock(start: i64) -> Clock {
    let began = std::time::Instant::now();
    Arc::new(move || start.saturating_add(i64::try_from(began.elapsed().as_secs()).unwrap_or(0)))
}

/// An object read together with the version a conditional write can name.
struct Held {
    body: Bytes,
    version: UpdateVersion,
}

async fn read_held(store: &dyn ObjectStore, path: &Path) -> Result<Option<Held>> {
    match store.get(path).await {
        Ok(got) => {
            let version = UpdateVersion {
                e_tag: got.meta.e_tag.clone(),
                version: got.meta.version.clone(),
            };
            let body = crate::bucket::read_capped(got, MAX_LOCK_BYTES)
                .await?
                .ok_or_else(|| GcError::Marks(format!("{path} is too large")))?;
            Ok(Some(Held { body, version }))
        }
        Err(object_store::Error::NotFound { .. }) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// How a write conditional on a version ended.
enum Guarded {
    Written,
    /// The object changed (or went) since it was read.
    Changed,
    /// The store has no conditional writes, or gave no version to condition on.
    Unsupported,
}

async fn put_if(
    store: &dyn ObjectStore,
    path: &Path,
    version: &UpdateVersion,
    body: Bytes,
) -> Result<Guarded> {
    if version.e_tag.is_none() && version.version.is_none() {
        return Ok(Guarded::Unsupported);
    }
    let opts = PutOptions {
        mode: PutMode::Update(version.clone()),
        ..PutOptions::default()
    };
    match store
        .put_opts(path, PutPayload::from_bytes(body), opts)
        .await
    {
        Ok(_) => Ok(Guarded::Written),
        Err(
            object_store::Error::Precondition { .. }
            | object_store::Error::NotFound { .. }
            | object_store::Error::AlreadyExists { .. },
        ) => Ok(Guarded::Changed),
        Err(object_store::Error::NotImplemented { .. }) => Ok(Guarded::Unsupported),
        Err(e) => Err(e.into()),
    }
}

/// The `gc/lock` lease. It is created with a create-only put, and taken over and renewed only
/// with a put conditional on the version that was read, so two runs never both believe they hold
/// it. A store without conditional writes cannot take over an expired lease (the operator
/// removes `gc/lock` by hand); it can still renew its own.
struct BucketLock<'a> {
    store: &'a dyn ObjectStore,
    path: Path,
    run: i64,
    /// Tells this run's lock from another run that started in the same second.
    token: String,
    lease: i64,
    clock: Clock,
    last_renewed: AtomicI64,
}

/// A parsed `gc/lock` body: `<run> <expires> [<token>]`.
struct LockBody {
    run: i64,
    expires: i64,
    token: Option<String>,
}

impl BucketLock<'_> {
    fn body(&self, expires: i64) -> Bytes {
        Bytes::from(format!("{} {expires} {}\n", self.run, self.token))
    }

    fn parse(body: &[u8]) -> Option<LockBody> {
        let text = std::str::from_utf8(body).ok()?;
        let mut parts = text.split_whitespace();
        let run = parts.next()?.parse().ok()?;
        let expires = parts.next()?.parse().ok()?;
        let token = parts.next().map(str::to_owned);
        parts.next().is_none().then_some(LockBody {
            run,
            expires,
            token,
        })
    }

    /// The name a freeze carries so its owner can tell it from another run's.
    fn owner(&self) -> String {
        format!("{}-{}", self.run, self.token)
    }

    fn ours(&self, body: &[u8]) -> bool {
        Self::parse(body).is_some_and(|b| b.token.as_deref() == Some(self.token.as_str()))
    }

    async fn create(&self, expires: i64) -> Result<bool> {
        let opts = PutOptions {
            mode: PutMode::Create,
            ..PutOptions::default()
        };
        match self
            .store
            .put_opts(&self.path, PutPayload::from_bytes(self.body(expires)), opts)
            .await
        {
            Ok(_) => Ok(true),
            Err(object_store::Error::AlreadyExists { .. }) => Ok(false),
            Err(e) => Err(e.into()),
        }
    }

    /// Takes the lease, or fails with `Busy` while another run's lease is unexpired.
    async fn acquire<'a>(
        store: &'a dyn ObjectStore,
        root: &Path,
        run: i64,
        now: i64,
        lease: i64,
    ) -> Result<BucketLock<'a>> {
        Self::acquire_with(store, root, run, lease, elapsed_clock(now)).await
    }

    async fn acquire_with<'a>(
        store: &'a dyn ObjectStore,
        root: &Path,
        run: i64,
        lease: i64,
        clock: Clock,
    ) -> Result<BucketLock<'a>> {
        let now = clock();
        let lock = BucketLock {
            store,
            path: gc_dir(root).join("lock"),
            run,
            token: Uuid::new_v4().simple().to_string(),
            lease,
            clock,
            last_renewed: AtomicI64::new(now),
        };
        for _ in 0..2 {
            if lock.create(now.saturating_add(lease)).await? {
                return Ok(lock);
            }
            let Some(held) = read_held(store, &lock.path).await? else {
                continue; // released between the create and the read: try the create again
            };
            if let Some(b) = Self::parse(&held.body)
                && b.expires > now
            {
                return Err(GcError::Busy(format!(
                    "run {} holds gc/lock until {}",
                    b.run,
                    rfc3339(b.expires)
                )));
            }
            // Expired or unreadable: the holder died. Take it over, but only if nobody else has.
            let body = lock.body(now.saturating_add(lease));
            return match put_if(store, &lock.path, &held.version, body).await? {
                Guarded::Written => Ok(lock),
                Guarded::Changed => Err(GcError::Busy(
                    "gc/lock changed while it was being taken".into(),
                )),
                Guarded::Unsupported => Err(GcError::Busy(
                    "gc/lock has expired, but this store has no conditional writes to take it \
                     over safely; remove gc/lock by hand once no collection is running"
                        .into(),
                )),
            };
        }
        Err(GcError::Busy(
            "gc/lock changed while it was being taken".into(),
        ))
    }

    async fn is_ours(&self) -> Result<bool> {
        let held = read_held(self.store, &self.path).await?;
        Ok(held.is_some_and(|h| self.ours(&h.body)))
    }

    /// Confirms the lease is still this run's and extends it from the current clock, with a write
    /// conditional on the version just read. Fails with `Busy` if another run holds it.
    async fn confirm(&self) -> Result<()> {
        let lost = |why: &str| GcError::Busy(format!("gc/lock was lost: {why}"));
        let held = read_held(self.store, &self.path)
            .await?
            .ok_or_else(|| lost("it is gone"))?;
        if !self.ours(&held.body) {
            let other = Self::parse(&held.body).map_or(0, |b| b.run);
            return Err(lost(&format!("run {other} took it over")));
        }
        let now = (self.clock)();
        let body = self.body(now.saturating_add(self.lease));
        match put_if(self.store, &self.path, &held.version, body.clone()).await? {
            Guarded::Written => {}
            Guarded::Changed => return Err(lost("it changed while it was being renewed")),
            // No conditional writes: the lock was just read as ours and, on such a store, nobody
            // can take it over.
            Guarded::Unsupported => {
                self.store
                    .put(&self.path, PutPayload::from_bytes(body))
                    .await?;
            }
        }
        self.last_renewed.store(now, Ordering::Relaxed);
        Ok(())
    }

    /// [`confirm`](Self::confirm), when a sixth of the lease has passed since the last one.
    async fn keepalive(&self) -> Result<()> {
        let due = (self.clock)().saturating_sub(self.last_renewed.load(Ordering::Relaxed))
            >= (self.lease / 6).max(1);
        if due { self.confirm().await } else { Ok(()) }
    }

    async fn release(&self) -> Result<()> {
        if self.is_ours().await? {
            match self.store.delete(&self.path).await {
                Ok(()) | Err(object_store::Error::NotFound { .. }) => {}
                Err(e) => return Err(e.into()),
            }
        }
        Ok(())
    }
}

/// The bucket-mode fence: `gc/freeze` stands while a sweep runs. The first batch creates it and
/// waits `freeze_wait` for pushes that read the key before it appeared; every batch confirms
/// and renews the lock lease. The freeze names its owner, and only its owner removes it.
struct FreezeFence<'a> {
    store: &'a dyn ObjectStore,
    root: &'a Path,
    lock: &'a BucketLock<'a>,
    cfg: &'a GcConfig,
    now: SystemTime,
    frozen: tokio::sync::OnceCell<()>,
}

/// The `run=` owner a freeze body names.
fn freeze_owner(body: &[u8]) -> Option<&str> {
    let text = std::str::from_utf8(body).ok()?;
    text.split_whitespace().find_map(|w| w.strip_prefix("run="))
}

impl FreezeFence<'_> {
    fn freeze_path(&self) -> Path {
        gc_dir(self.root).join("freeze")
    }

    fn freeze_body(&self) -> Bytes {
        Bytes::from(format!(
            "{} run={}",
            rfc3339(unix(self.now)),
            self.lock.owner()
        ))
    }

    /// Removes a freeze left by a run that died. Allowed only while this run holds `gc/lock`,
    /// because only the lock holder creates a freeze.
    async fn lift_stale(&self) -> Result<()> {
        self.lock.confirm().await?;
        match self.store.delete(&self.freeze_path()).await {
            Ok(()) | Err(object_store::Error::NotFound { .. }) => Ok(()),
            Err(e) => Err(e.into()),
        }
    }

    /// Removes this run's freeze. A freeze that names another owner is left alone, and so is
    /// ours once `gc/lock` has passed to another run, whose own start lifts it.
    async fn thaw(&self) -> Result<()> {
        if !self.frozen.initialized() {
            return Ok(());
        }
        let Some(held) = read_held(self.store, &self.freeze_path()).await? else {
            return Ok(());
        };
        if freeze_owner(&held.body) != Some(self.lock.owner().as_str()) {
            return Ok(());
        }
        if !self.lock.is_ours().await? {
            return Ok(());
        }
        match self.store.delete(&self.freeze_path()).await {
            Ok(()) | Err(object_store::Error::NotFound { .. }) => Ok(()),
            Err(e) => Err(e.into()),
        }
    }

    /// Sleeps `freeze_wait` in slices, keeping the lease alive.
    async fn wait(&self) -> Result<()> {
        let slice = Duration::from_secs(u64::try_from((self.lock.lease / 6).max(1)).unwrap_or(1));
        let mut left = self.cfg.freeze_wait;
        while !left.is_zero() {
            let step = left.min(slice);
            tokio::time::sleep(step).await;
            left -= step;
            self.lock.keepalive().await?;
        }
        Ok(())
    }
}

#[async_trait]
impl Fence for FreezeFence<'_> {
    async fn enter(&self) -> Result<Box<dyn Send>> {
        self.lock.confirm().await?;
        self.frozen
            .get_or_try_init(|| async {
                // We hold gc/lock, so a leftover freeze belongs to a dead run.
                self.lift_stale().await?;
                let opts = PutOptions {
                    mode: PutMode::Create,
                    ..PutOptions::default()
                };
                self.store
                    .put_opts(
                        &self.freeze_path(),
                        PutPayload::from_bytes(self.freeze_body()),
                        opts,
                    )
                    .await?;
                self.wait().await
            })
            .await?;
        Ok(Box::new(()))
    }

    async fn keepalive(&self) -> Result<()> {
        self.lock.keepalive().await
    }

    async fn confirm(&self) -> Result<()> {
        self.lock.confirm().await
    }
}

/// Garbage collection of a bucket-mode repository (`asc gc --store <url>`).
pub struct BucketGc {
    store: Arc<dyn ObjectStore>,
    root: Path,
    objects: BucketObjects,
    refs: BucketRefs,
}

impl BucketGc {
    /// Opens `url`; refuses a store without exclusive create (the lock and refs need it).
    pub async fn open(url: &str, credentials: Option<crate::s3::S3Credentials>) -> Result<Self> {
        let parsed = url::Url::parse(url).map_err(|e| GcError::Other(format!("store url: {e}")))?;
        let (store, root): (Arc<dyn ObjectStore>, Path) = if parsed.scheme() == "s3" {
            crate::s3::open_strict(&parsed, credentials)?
        } else {
            let (store, root) = object_store::parse_url(&parsed)?;
            (Arc::from(store), root)
        };
        Self::connect(store, root).await
    }

    pub async fn connect(store: Arc<dyn ObjectStore>, root: Path) -> Result<Self> {
        let refs = BucketRefs::connect(store.clone(), root.clone()).await?;
        Ok(Self {
            objects: BucketObjects::new(store.clone(), root.clone()),
            store,
            root,
            refs,
        })
    }

    /// One collection. `extra` supplies roots and purges; there are none in a plain bucket.
    pub async fn run(
        &self,
        now: SystemTime,
        dry_run: bool,
        cfg: &GcConfig,
        extra: &[Arc<dyn GcRootSource>],
    ) -> Result<GcReport> {
        let run = unix(now);
        let lock = BucketLock::acquire(&*self.store, &self.root, run, run, secs(cfg.lease)).await?;
        let fence = FreezeFence {
            store: &*self.store,
            root: &self.root,
            lock: &lock,
            cfg,
            now,
            frozen: tokio::sync::OnceCell::new(),
        };
        // Holding the lock, a freeze that is still standing belongs to a run that died.
        if let Err(e) = fence.lift_stale().await {
            let _ = lock.release().await;
            return Err(e);
        }
        let marks = BucketMarks::new(self.store.clone(), self.root.clone());
        let roots = BucketRefRoots::new(
            self.refs.clone(),
            self.store.clone(),
            self.root.clone(),
            cfg.max_marks,
        );
        let collector = Collector {
            ns: b"",
            objects: &self.objects,
            refs: &roots,
            extra,
            marks: &marks,
            observer: None,
            cfg,
        };
        let outcome = collector.collect(now, dry_run, &fence).await;
        // Always lift the freeze and the lock, then report the collection's own error first.
        let thawed = fence.thaw().await;
        let released = lock.release().await;
        let report = outcome?;
        thawed?;
        released?;
        Ok(report)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use asc_core::id::ChunkId;
    use asc_core::object::Entry;
    use asc_core::path::Name;
    use asc_core::sig::Signature;
    use object_store::memory::InMemory;

    use super::*;
    use crate::codec::encode_stored;
    use crate::refs::{Actor, Expect, RefTxn, RefUpdate};

    const D15: Duration = Duration::from_secs(15 * 24 * 3600);
    const H25: Duration = Duration::from_secs(25 * 3600);

    struct Env {
        store: Arc<dyn ObjectStore>,
        root: Path,
        objects: BucketObjects,
        refs: BucketRefs,
        gc: BucketGc,
    }

    async fn env() -> Env {
        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let root = Path::from("ns/x");
        let gc = BucketGc::connect(store.clone(), root.clone())
            .await
            .unwrap();
        Env {
            objects: BucketObjects::new(store.clone(), root.clone()),
            refs: BucketRefs::new(store.clone(), root.clone()),
            store,
            root,
            gc,
        }
    }

    fn cfg() -> GcConfig {
        GcConfig {
            freeze_wait: Duration::ZERO,
            ..GcConfig::default()
        }
    }

    fn sig() -> Signature {
        Signature::new(b"A".to_vec(), b"a@x".to_vec(), 1, 0).unwrap()
    }

    /// Stores a changeset holding one file `name` with `body`; returns the ids it uses.
    async fn commit(
        e: &Env,
        parents: &[ChangesetId],
        name: &str,
        body: &[u8],
    ) -> (ChangesetId, FileId, TreeId) {
        use asc_core::codec::Enveloped;
        let fid = FileId::of(body);
        e.objects
            .put(&ObjKey::File(fid), encode_stored(body))
            .await
            .unwrap();
        let entry = Entry {
            name: Name::new(name).unwrap(),
            kind: Kind::File,
            id: *fid.as_bytes(),
            size: body.len() as u64,
            git: GitOid::from_bytes([7; 20]),
        };
        let tree = Tree::new(vec![entry], true).unwrap();
        let tid = tree.id();
        e.objects
            .put(&ObjKey::Tree(tid), encode_stored(&tree.to_envelope()))
            .await
            .unwrap();
        let cs = Changeset {
            parents: parents.to_vec(),
            root: tid,
            author: sig(),
            committer: sig(),
            message: name.as_bytes().to_vec(),
            extra: asc_core::object::SortedMap::new(Vec::new()).unwrap(),
            changes: asc_core::object::SortedMap::new(Vec::new()).unwrap(),
        };
        let id = cs.id();
        e.objects
            .put(&ObjKey::Changeset(id), encode_stored(&cs.to_envelope()))
            .await
            .unwrap();
        (id, fid, tid)
    }

    async fn set_ref(e: &Env, name: &str, to: Option<ChangesetId>) {
        let cur = e.refs.get(RepoKey(0), &name.parse_ref()).await.unwrap();
        let txn = RefTxn {
            updates: vec![RefUpdate {
                name: name.parse_ref(),
                expect: cur.map_or(Expect::Absent, |c| Expect::Value(c.target)),
                new: to.map(RefTarget::Changeset),
                expires_at: None,
            }],
            actor: Actor {
                id: "t".into(),
                via: "test".into(),
            },
            reason: "test".into(),
            op_id: Uuid::new_v4(),
        };
        e.refs.update(RepoKey(0), &txn).await.unwrap();
    }

    trait ParseRef {
        fn parse_ref(&self) -> asc_core::path::RefName;
    }
    impl ParseRef for str {
        fn parse_ref(&self) -> asc_core::path::RefName {
            asc_core::path::RefName::new(self.as_bytes().to_vec()).unwrap()
        }
    }

    async fn has(e: &Env, k: ObjKey) -> bool {
        e.objects.has_many(&[k]).await.unwrap()[0]
    }

    struct Source {
        roots: Mutex<Vec<ObjKey>>,
        purged: Mutex<Vec<ObjKey>>,
        /// The keys `purge_swept` was told about, with the `as_of` of each call.
        swept: Mutex<Vec<(Vec<ObjKey>, i64)>>,
    }

    #[async_trait]
    impl GcRootSource for Source {
        async fn purge_swept(&self, _: &[u8], keys: &[ObjKey], as_of: i64) -> Result<()> {
            self.swept.lock().unwrap().push((keys.to_vec(), as_of));
            Ok(())
        }
        async fn roots(&self, _: &[u8]) -> Result<Vec<ObjKey>> {
            Ok(self.roots.lock().unwrap().clone())
        }
        async fn purged(&self, _: &[u8]) -> Result<Vec<ObjKey>> {
            Ok(self.purged.lock().unwrap().clone())
        }
    }

    #[tokio::test]
    async fn unreachable_old_objects_go_only_after_the_second_mark() {
        let e = env().await;
        let (keep, kf, _) = commit(&e, &[], "keep", b"kept body").await;
        let (lost, lf, lt) = commit(&e, &[], "lost", b"lost body").await;
        set_ref(&e, "refs/heads/master", Some(keep)).await;
        let t1 = SystemTime::now() + D15;

        let first = e.gc.run(t1, false, &cfg(), &[]).await.unwrap();
        assert!(first.sweep.waiting_for_second_mark);
        assert_eq!(first.sweep.deleted, 0);
        assert!(
            has(&e, ObjKey::Changeset(lost)).await,
            "one mark deletes nothing"
        );

        // A second mark less than 24 h later is still not enough.
        let soon =
            e.gc.run(t1 + Duration::from_secs(3600), false, &cfg(), &[])
                .await
                .unwrap();
        assert!(soon.sweep.waiting_for_second_mark);
        assert!(has(&e, ObjKey::Changeset(lost)).await);

        let second = e.gc.run(t1 + H25, false, &cfg(), &[]).await.unwrap();
        assert!(!second.sweep.waiting_for_second_mark);
        assert_eq!(second.sweep.deleted, 3, "{second}");
        for k in [ObjKey::Changeset(lost), ObjKey::File(lf), ObjKey::Tree(lt)] {
            assert!(!has(&e, k).await, "{k} should be swept");
        }
        assert!(has(&e, ObjKey::Changeset(keep)).await);
        assert!(has(&e, ObjKey::File(kf)).await);
        // The lock and the freeze are gone.
        assert!(e.store.head(&Path::from("ns/x/gc/lock")).await.is_err());
        assert!(e.store.head(&Path::from("ns/x/gc/freeze")).await.is_err());
    }

    #[tokio::test]
    async fn young_objects_survive_and_are_roots() {
        let e = env().await;
        let (young, yf, _) = commit(&e, &[], "young", b"young body").await;
        let t1 = SystemTime::now() + Duration::from_secs(3600);
        e.gc.run(t1, false, &cfg(), &[]).await.unwrap();
        let r = e.gc.run(t1 + H25, false, &cfg(), &[]).await.unwrap();
        assert_eq!(r.sweep.deleted, 0);
        assert_eq!(
            r.mark.recent, 3,
            "recent changeset closure: changeset, tree, file"
        );
        assert!(has(&e, ObjKey::Changeset(young)).await);
        assert!(has(&e, ObjKey::File(yf)).await);
    }

    #[tokio::test]
    async fn in_flight_push_across_marks_keeps_re_referenced_objects() {
        let e = env().await;
        let (old, of, _) = commit(&e, &[], "old", b"old body").await;
        let t1 = SystemTime::now() + D15;
        // Mark 1 sees `old` unreachable.
        e.gc.run(t1, false, &cfg(), &[]).await.unwrap();
        // A push that started before mark 1 finishes after it: it re-references `old`.
        set_ref(&e, "refs/heads/master", Some(old)).await;
        let r = e.gc.run(t1 + H25, false, &cfg(), &[]).await.unwrap();
        assert_eq!(r.sweep.deleted, 0, "{r}");
        assert!(has(&e, ObjKey::Changeset(old)).await);
        assert!(has(&e, ObjKey::File(of)).await);
    }

    #[tokio::test]
    async fn sweep_race_push_after_mark2() {
        let e = env().await;
        let (old, of, _) = commit(&e, &[], "old", b"old body").await;
        let t1 = SystemTime::now() + D15;
        e.gc.run(t1, false, &cfg(), &[]).await.unwrap();

        // Mark 2 by hand, so a push can land between it and the sweep.
        let marks = BucketMarks::new(e.store.clone(), e.root.clone());
        let roots = BucketRefRoots::new(e.refs.clone(), e.store.clone(), e.root.clone(), 1000);
        let config = cfg();
        let c = Collector {
            ns: b"",
            objects: &e.objects,
            refs: &roots,
            extra: &[],
            marks: &marks,
            observer: None,
            cfg: &config,
        };
        let t2 = t1 + H25;
        let mark2 = c.mark(t2).await.unwrap();
        assert_eq!(mark2.strict, 0, "mark 2 saw nothing reachable");
        // The push lands: it points a ref at the old changeset, then the sweep runs.
        set_ref(&e, "refs/heads/master", Some(old)).await;
        let swept = c.sweep(&mark2, t2, false, &NoFence).await.unwrap();
        assert_eq!(swept.candidates, 3);
        assert_eq!(swept.rescued, 3, "the fenced re-mark rescues the batch");
        assert_eq!(swept.deleted, 0);
        assert!(has(&e, ObjKey::Changeset(old)).await);
        assert!(has(&e, ObjKey::File(of)).await);
    }

    #[tokio::test]
    async fn pitr_restore_closure() {
        let e = env().await;
        let (a, af, _) = commit(&e, &[], "a", b"body a").await;
        let (b, _, _) = commit(&e, &[a], "b", b"body b").await;
        set_ref(&e, "refs/heads/master", Some(a)).await;
        set_ref(&e, "refs/heads/master", Some(b)).await;
        set_ref(&e, "refs/heads/master", None).await;
        let t1 = SystemTime::now() + Duration::from_secs(3600);
        let mut config = cfg();
        config.min_age = Duration::ZERO;
        config.pitr = 14 * DAY;
        e.gc.run(t1, false, &config, &[]).await.unwrap();
        let r = e.gc.run(t1 + H25, false, &config, &[]).await.unwrap();
        assert_eq!(
            r.sweep.deleted, 0,
            "ref-log targets inside the window are roots: {r}"
        );
        assert!(has(&e, ObjKey::Changeset(a)).await);
        assert!(has(&e, ObjKey::Changeset(b)).await);
        assert!(has(&e, ObjKey::File(af)).await);

        // Once the log entries fall outside the window, they are not.
        config.pitr = Duration::from_secs(3600);
        let t3 = t1 + 3 * H25;
        e.gc.run(t3, false, &config, &[]).await.unwrap();
        let r = e.gc.run(t3 + H25, false, &config, &[]).await.unwrap();
        assert!(r.sweep.deleted >= 4, "{r}");
        assert!(!has(&e, ObjKey::Changeset(a)).await);
    }

    #[tokio::test]
    async fn concurrent_runs_are_refused() {
        let e = env().await;
        let now = SystemTime::now();
        let lock = BucketLock::acquire(&*e.store, &e.root, 5, unix(now), 6 * 3600)
            .await
            .unwrap();
        let err = e.gc.run(now, false, &cfg(), &[]).await.unwrap_err();
        assert!(
            matches!(err, GcError::Busy(ref m) if m.contains("run 5")),
            "{err}"
        );
        lock.release().await.unwrap();
        e.gc.run(now, false, &cfg(), &[]).await.unwrap();
    }

    #[tokio::test]
    async fn an_expired_lease_is_taken_over() {
        let e = env().await;
        let now = SystemTime::now();
        // A dead run's lease that expired long ago.
        BucketLock::acquire(&*e.store, &e.root, 5, 5, 10)
            .await
            .unwrap();
        e.gc.run(now, false, &cfg(), &[]).await.unwrap();
        // And a stale freeze left behind is lifted.
        e.store
            .put(
                &Path::from("ns/x/gc/freeze"),
                PutPayload::from_static(b"old"),
            )
            .await
            .unwrap();
        e.gc.run(now + Duration::from_secs(1), false, &cfg(), &[])
            .await
            .unwrap();
        assert!(e.store.head(&Path::from("ns/x/gc/freeze")).await.is_err());
    }

    #[tokio::test]
    async fn the_freeze_stands_during_the_sweep_and_blocks_a_push() {
        let e = env().await;
        let (old, _, _) = commit(&e, &[], "old", b"old body").await;
        let t1 = SystemTime::now() + D15;
        e.gc.run(t1, false, &cfg(), &[]).await.unwrap();

        // An observer sees the freeze while it is told what will be deleted, and a ref update
        // attempted then is refused before its CAS.
        struct Probe(Arc<InMemoryRefs>);
        struct InMemoryRefs {
            refs: BucketRefs,
            blocked: Mutex<Option<String>>,
        }
        #[async_trait]
        impl SweepObserver for Probe {
            async fn before_delete(&self, _: &[ObjKey]) -> Result<()> {
                let txn = RefTxn {
                    updates: vec![RefUpdate {
                        name: "refs/heads/late".parse_ref(),
                        expect: Expect::Absent,
                        new: None,
                        expires_at: None,
                    }],
                    actor: Actor {
                        id: "t".into(),
                        via: "t".into(),
                    },
                    reason: "late".into(),
                    op_id: Uuid::new_v4(),
                };
                let target = RefTarget::Changeset(ChangesetId::of(b"x"));
                let mut txn = txn;
                txn.updates[0].new = Some(target);
                if let Err(err) = self.0.refs.update(RepoKey(0), &txn).await {
                    *self.0.blocked.lock().unwrap() = Some(err.to_string());
                }
                Ok(())
            }
        }
        let shared = Arc::new(InMemoryRefs {
            refs: e.refs.clone(),
            blocked: Mutex::new(None),
        });
        let probe = Probe(shared.clone());
        let marks = BucketMarks::new(e.store.clone(), e.root.clone());
        let roots = BucketRefRoots::new(e.refs.clone(), e.store.clone(), e.root.clone(), 1000);
        let config = cfg();
        let c = Collector {
            ns: b"",
            objects: &e.objects,
            refs: &roots,
            extra: &[],
            marks: &marks,
            observer: Some(&probe),
            cfg: &config,
        };
        let lock = BucketLock::acquire(&*e.store, &e.root, 9, 9, 3600)
            .await
            .unwrap();
        let fence = FreezeFence {
            store: &*e.store,
            root: &e.root,
            lock: &lock,
            cfg: &config,
            now: t1 + H25,
            frozen: tokio::sync::OnceCell::new(),
        };
        let r = c.collect(t1 + H25, false, &fence).await.unwrap();
        fence.thaw().await.unwrap();
        lock.release().await.unwrap();
        assert_eq!(r.sweep.deleted, 3);
        let blocked = shared.blocked.lock().unwrap().clone().unwrap_or_default();
        assert!(blocked.contains("garbage-collected"), "{blocked}");
        assert!(!has(&e, ObjKey::Changeset(old)).await);
    }

    #[tokio::test]
    async fn dry_run_deletes_nothing_and_lists_candidates() {
        let e = env().await;
        let (lost, _, _) = commit(&e, &[], "lost", b"lost body").await;
        let t1 = SystemTime::now() + D15;
        e.gc.run(t1, false, &cfg(), &[]).await.unwrap();
        let r = e.gc.run(t1 + H25, true, &cfg(), &[]).await.unwrap();
        assert!(r.sweep.dry_run);
        assert_eq!(r.sweep.candidates, 3);
        assert_eq!(r.sweep.deleted, 0);
        assert!(r.sweep.sample.contains(&ObjKey::Changeset(lost)));
        assert!(r.to_string().contains("would delete 3 objects"));
        assert!(has(&e, ObjKey::Changeset(lost)).await);
        // No freeze for a dry run.
        assert!(e.store.head(&Path::from("ns/x/gc/freeze")).await.is_err());
    }

    #[tokio::test]
    async fn extra_roots_keep_objects_and_purged_objects_go_at_once() {
        let e = env().await;
        let (rooted, rf, _) = commit(&e, &[], "rooted", b"rooted body").await;
        let (young_bad, yf, _) = commit(&e, &[], "bad", b"secret body").await;
        let src = Arc::new(Source {
            roots: Mutex::new(vec![ObjKey::Changeset(rooted)]),
            purged: Mutex::new(vec![ObjKey::File(yf)]),
            swept: Mutex::new(Vec::new()),
        });
        let extra: Vec<Arc<dyn GcRootSource>> = vec![src.clone()];
        // Everything is young (no age rule applies to a purge) and there is no earlier mark.
        let now = SystemTime::now() + Duration::from_secs(60);
        let r = e.gc.run(now, false, &cfg(), &extra).await.unwrap();
        assert_eq!(r.sweep.deleted, 1, "{r}");
        assert_eq!(r.sweep.purged, 1);
        assert!(
            !has(&e, ObjKey::File(yf)).await,
            "the purged file is gone at the first run"
        );
        assert!(
            has(&e, ObjKey::Changeset(young_bad)).await,
            "only the purged key goes"
        );
        assert!(has(&e, ObjKey::File(rf)).await);
        {
            // The source is told the deleted key is done, as of the run that deleted it.
            let swept = src.swept.lock().unwrap();
            assert_eq!(swept.len(), 1);
            assert_eq!(swept[0].0, vec![ObjKey::File(yf)]);
            assert_eq!(swept[0].1, r.mark.run);
        }

        // A purged key that a strict root reaches is kept, and the source is not told to forget
        // it; the key already deleted is reported done again (it is gone).
        src.swept.lock().unwrap().clear();
        src.purged.lock().unwrap().push(ObjKey::File(rf));
        let r =
            e.gc.run(now + Duration::from_secs(60), false, &cfg(), &extra)
                .await
                .unwrap();
        assert_eq!(r.sweep.deleted, 0);
        assert!(has(&e, ObjKey::File(rf)).await);
        {
            let swept = src.swept.lock().unwrap();
            assert_eq!(swept.len(), 1);
            assert_eq!(
                swept[0].0,
                vec![ObjKey::File(yf)],
                "the rooted key stays listed"
            );
        }
        // A dry run changes nothing and tells no one.
        src.swept.lock().unwrap().clear();
        e.gc.run(now + Duration::from_secs(120), true, &cfg(), &extra)
            .await
            .unwrap();
        assert!(src.swept.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn change_records_follow_their_changeset() {
        use asc_core::change::{ChangeId, RewriteOp};
        use asc_core::codec::Enveloped;
        let e = env().await;
        let (kept, _, _) = commit(&e, &[], "kept", b"kept body").await;
        let (lost, _, _) = commit(&e, &[], "lost", b"lost body").await;
        let mut metas = Vec::new();
        for (i, cs) in [kept, lost].into_iter().enumerate() {
            let m = ChangeMeta::new(
                cs,
                ChangeId::from_bytes([i as u8; 16]),
                vec![],
                RewriteOp::Import,
            );
            e.objects
                .put(&ObjKey::ChangeMeta(m.id()), encode_stored(&m.to_envelope()))
                .await
                .unwrap();
            metas.push(m.id());
        }
        set_ref(&e, "refs/heads/master", Some(kept)).await;
        let t1 = SystemTime::now() + D15;
        e.gc.run(t1, false, &cfg(), &[]).await.unwrap();
        e.gc.run(t1 + H25, false, &cfg(), &[]).await.unwrap();
        assert!(has(&e, ObjKey::ChangeMeta(metas[0])).await);
        assert!(!has(&e, ObjKey::ChangeMeta(metas[1])).await);
    }

    #[tokio::test]
    async fn a_corrupt_reachable_object_fails_the_run_and_deletes_nothing() {
        let e = env().await;
        let (a, _, tid) = commit(&e, &[], "a", b"body a").await;
        let (lost, _, _) = commit(&e, &[], "lost", b"lost body").await;
        set_ref(&e, "refs/heads/master", Some(a)).await;
        // Overwrite the tree with bytes that do not hash to its id.
        e.store
            .put(
                &Path::from(format!("ns/x/{}", ObjKey::Tree(tid).path())),
                PutPayload::from_bytes(encode_stored(b"garbage")),
            )
            .await
            .unwrap();
        let t1 = SystemTime::now() + D15;
        let err = e.gc.run(t1, false, &cfg(), &[]).await.unwrap_err();
        assert!(matches!(err, GcError::Objects(_)), "{err}");
        assert!(has(&e, ObjKey::Changeset(lost)).await);
        // The failed run released its lock.
        assert!(e.store.head(&Path::from("ns/x/gc/lock")).await.is_err());
    }

    #[tokio::test]
    async fn too_many_marks_fails_closed() {
        let e = env().await;
        let (a, _, _) = commit(&e, &[], "a", b"body a").await;
        set_ref(&e, "refs/heads/master", Some(a)).await;
        let mut config = cfg();
        config.max_marks = 2;
        let err =
            e.gc.run(SystemTime::now(), false, &config, &[])
                .await
                .unwrap_err();
        assert!(matches!(err, GcError::TooManyMarks(2)), "{err}");
    }

    #[tokio::test]
    async fn old_marks_are_pruned() {
        let e = env().await;
        let t = SystemTime::now() + D15;
        for i in 0..5u64 {
            e.gc.run(t + Duration::from_secs(i * 3600 * 13), false, &cfg(), &[])
                .await
                .unwrap();
        }
        let marks = BucketMarks::new(e.store.clone(), e.root.clone());
        let runs = marks.runs().await.unwrap();
        assert!(runs.len() <= 3, "{runs:?}");
        assert!(runs.iter().all(|r| r.complete));
    }

    // ---- lease and freeze ownership

    /// A clock the test moves by hand.
    fn manual(at: i64) -> (Arc<AtomicI64>, Clock) {
        let cell = Arc::new(AtomicI64::new(at));
        let c = cell.clone();
        (cell, Arc::new(move || c.load(Ordering::Relaxed)))
    }

    async fn lock_body(e: &Env) -> String {
        let got = e.store.get(&Path::from("ns/x/gc/lock")).await.unwrap();
        String::from_utf8(got.bytes().await.unwrap().to_vec()).unwrap()
    }

    fn lock_expires(body: &str) -> i64 {
        body.split_whitespace().nth(1).unwrap().parse().unwrap()
    }

    fn freeze_fence<'a>(
        e: &'a Env,
        lock: &'a BucketLock<'a>,
        config: &'a GcConfig,
        now: SystemTime,
    ) -> FreezeFence<'a> {
        FreezeFence {
            store: &*e.store,
            root: &e.root,
            lock,
            cfg: config,
            now,
            frozen: tokio::sync::OnceCell::new(),
        }
    }

    #[tokio::test]
    async fn renewal_extends_from_the_current_clock() {
        let e = env().await;
        let (cell, clock) = manual(1000);
        let lock = BucketLock::acquire_with(&*e.store, &e.root, 1000, 3600, clock)
            .await
            .unwrap();
        assert_eq!(lock_expires(&lock_body(&e).await), 1000 + 3600);
        // The run started at 1000 and has been going for 2000 s.
        cell.store(3000, Ordering::Relaxed);
        let config = cfg();
        let fence = freeze_fence(&e, &lock, &config, at_unix(1000));
        drop(fence.enter().await.unwrap());
        assert_eq!(
            lock_expires(&lock_body(&e).await),
            3000 + 3600,
            "the lease runs from now, not from the start of the run"
        );
        fence.thaw().await.unwrap();
        lock.release().await.unwrap();
    }

    #[tokio::test]
    async fn thaw_leaves_another_runs_freeze_alone() {
        let e = env().await;
        let (_, clock) = manual(1000);
        let lock = BucketLock::acquire_with(&*e.store, &e.root, 1000, 3600, clock)
            .await
            .unwrap();
        let config = cfg();
        let fence = freeze_fence(&e, &lock, &config, at_unix(1000));
        drop(fence.enter().await.unwrap());
        let freeze = Path::from("ns/x/gc/freeze");
        let mine = e.store.get(&freeze).await.unwrap().bytes().await.unwrap();
        assert!(String::from_utf8_lossy(&mine).contains(&format!("run={}", lock.owner())));

        // Another run took the lock and froze the bucket itself.
        let theirs = "2026-01-01T00:00:00Z run=77-other";
        e.store
            .put(&freeze, PutPayload::from_static(theirs.as_bytes()))
            .await
            .unwrap();
        fence.thaw().await.unwrap();
        let left = e.store.get(&freeze).await.unwrap().bytes().await.unwrap();
        assert_eq!(&left[..], theirs.as_bytes(), "not ours: not removed");

        // Our own is removed.
        e.store
            .put(&freeze, PutPayload::from_bytes(mine))
            .await
            .unwrap();
        fence.thaw().await.unwrap();
        assert!(e.store.head(&freeze).await.is_err());
        lock.release().await.unwrap();
    }

    /// Refs that, on their second read (the sweep's re-mark), let another run take the lock.
    struct TakeoverRoots {
        inner: BucketRefRoots,
        calls: AtomicI64,
        store: Arc<dyn ObjectStore>,
        root: Path,
        lease: i64,
        takes_over_at: i64,
    }

    #[async_trait]
    impl RefRoots for TakeoverRoots {
        async fn current(&self) -> Result<Vec<RefTarget>> {
            if self.calls.fetch_add(1, Ordering::Relaxed) == 1 {
                BucketLock::acquire(&*self.store, &self.root, 77, self.takes_over_at, self.lease)
                    .await
                    .unwrap();
            }
            self.inner.current().await
        }
        async fn logged_since(&self, since: i64) -> Result<Vec<RefTarget>> {
            self.inner.logged_since(since).await
        }
    }

    #[tokio::test]
    async fn a_run_whose_lease_was_taken_over_stops_before_deleting() {
        let e = env().await;
        let (old, _, _) = commit(&e, &[], "old", b"old body").await;
        let t1 = SystemTime::now() + D15;
        e.gc.run(t1, false, &cfg(), &[]).await.unwrap();

        let config = cfg();
        let (_, clock) = manual(1000);
        let lock = BucketLock::acquire_with(&*e.store, &e.root, 1000, 3600, clock)
            .await
            .unwrap();
        let t2 = t1 + H25;
        let fence = freeze_fence(&e, &lock, &config, t2);
        let marks = BucketMarks::new(e.store.clone(), e.root.clone());
        let roots = TakeoverRoots {
            inner: BucketRefRoots::new(e.refs.clone(), e.store.clone(), e.root.clone(), 1000),
            calls: AtomicI64::new(0),
            store: e.store.clone(),
            root: e.root.clone(),
            lease: 3600,
            // This run's lease (1000 + 3600) is long expired by then.
            takes_over_at: 1000 + 3600 + 1,
        };
        let c = Collector {
            ns: b"",
            objects: &e.objects,
            refs: &roots,
            extra: &[],
            marks: &marks,
            observer: None,
            cfg: &config,
        };
        let err = c.collect(t2, false, &fence).await.unwrap_err();
        assert!(
            matches!(err, GcError::Busy(ref m) if m.contains("run 77")),
            "{err}"
        );
        assert!(
            has(&e, ObjKey::Changeset(old)).await,
            "the batch was abandoned before any delete"
        );
        // The loser leaves the new holder's lock and nothing else of its own behind.
        fence.thaw().await.unwrap();
        lock.release().await.unwrap();
        assert!(lock_body(&e).await.starts_with("77 "));
    }

    /// Refs that make the clock jump when read, as a long mark does.
    struct SlowRoots {
        inner: BucketRefRoots,
        cell: Arc<AtomicI64>,
    }

    #[async_trait]
    impl RefRoots for SlowRoots {
        async fn current(&self) -> Result<Vec<RefTarget>> {
            self.cell.fetch_add(2000, Ordering::Relaxed);
            self.inner.current().await
        }
        async fn logged_since(&self, since: i64) -> Result<Vec<RefTarget>> {
            self.inner.logged_since(since).await
        }
    }

    #[tokio::test]
    async fn a_long_mark_renews_the_lease() {
        let e = env().await;
        let (a, _, _) = commit(&e, &[], "a", b"body a").await;
        set_ref(&e, "refs/heads/master", Some(a)).await;
        let config = cfg();
        let (cell, clock) = manual(1000);
        let lock = BucketLock::acquire_with(&*e.store, &e.root, 1000, 3600, clock)
            .await
            .unwrap();
        let fence = freeze_fence(&e, &lock, &config, at_unix(1000));
        let marks = BucketMarks::new(e.store.clone(), e.root.clone());
        let roots = SlowRoots {
            inner: BucketRefRoots::new(e.refs.clone(), e.store.clone(), e.root.clone(), 1000),
            cell: cell.clone(),
        };
        let c = Collector {
            ns: b"",
            objects: &e.objects,
            refs: &roots,
            extra: &[],
            marks: &marks,
            observer: None,
            cfg: &config,
        };
        c.mark_fenced(SystemTime::now(), &fence).await.unwrap();
        let expires = lock_expires(&lock_body(&e).await);
        assert!(
            expires >= 3000 + 3600,
            "marking renewed from the clock it had reached: {expires}"
        );
        lock.release().await.unwrap();
    }

    #[tokio::test]
    async fn a_run_from_the_same_second_is_not_mistaken_for_ours() {
        let e = env().await;
        let (_, clock) = manual(1000);
        let first = BucketLock::acquire_with(&*e.store, &e.root, 1000, 3600, clock.clone())
            .await
            .unwrap();
        // The lease runs out; a second run with the same run id takes over.
        let (_, later) = manual(1000 + 3601);
        let second = BucketLock::acquire_with(&*e.store, &e.root, 1000, 3600, later)
            .await
            .unwrap();
        assert!(first.confirm().await.is_err(), "the first run lost it");
        assert!(!first.is_ours().await.unwrap());
        assert!(second.is_ours().await.unwrap());
        second.release().await.unwrap();
    }

    #[test]
    fn rfc3339_matches_known_dates() {
        assert_eq!(rfc3339(0), "1970-01-01T00:00:00Z");
        assert_eq!(rfc3339(1_709_210_096), "2024-02-29T12:34:56Z");
        assert_eq!(rfc3339(4_102_444_799), "2099-12-31T23:59:59Z");
    }

    #[test]
    fn chunk_lists_name_their_chunks() {
        use asc_core::codec::Enveloped;
        let list = ChunkList {
            size: CHUNK_THRESHOLD + 2,
            chunks: vec![(ChunkId::of(b"a"), 4_194_304), (ChunkId::of(b"b"), 2)],
        };
        let key = ObjKey::ChunkList(FileId::of(b"x"));
        let kids = children(&key, &list.to_envelope()).unwrap();
        assert_eq!(
            kids,
            vec![
                ObjKey::Chunk(ChunkId::of(b"a")),
                ObjKey::Chunk(ChunkId::of(b"b"))
            ]
        );
        assert!(children(&key, b"junk").is_err());
    }
}
