//! `ci_manifest` (SPEC §23.2.4): the generation CAS that publishes segments, the leader
//! lease, and the `indexed` bitmap object of each generation.
//!
//! The SQL table belongs to the server (I13). This module owns the row's shape, the
//! [`ManifestStore`] contract it must meet, an in-memory implementation, and the leader's
//! [`Publisher`]: commit with re-base, compaction and the delayed sweep of dropped objects.

use std::collections::HashMap;
use std::sync::Mutex;

use async_trait::async_trait;
use asc_store::objects::Objects;
use roaring::RoaringBitmap;
use serde::{Deserialize, Serialize};

use super::schema::IndexKind;
use super::segments::{
    Hash, MAX_PACK_BYTES, SegmentPack, get_indexed, get_verified, idx_key, merge_packs,
    put_indexed, put_pack, seg_key,
};
use super::{IndexError, Result};

/// Dropped objects stay in the bucket this long after the generation that dropped them, so
/// pinned page tokens stay valid.
pub const DROP_GRACE_MS: u64 = 10 * 60 * 1000;
/// Re-bases tried before a commit gives up.
pub const MAX_REBASES: u32 = 16;
/// Most live segments a manifest may name.
pub const MAX_SEGMENTS: usize = 4096;
/// Segments merged at a time.
pub const MERGE_FACTOR: usize = 8;
/// Merge inputs stay below this many bytes in total.
pub const MAX_MERGE_BYTES: u64 = MAX_PACK_BYTES / 2;

/// A live segment.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SegRef {
    pub hash: Hash,
    pub docs: u32,
    pub bytes: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum DropKind {
    Segment,
    Indexed,
}

/// An object a generation stopped naming, with the time of that generation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Dropped {
    pub kind: DropKind,
    pub hash: Hash,
    pub at_ms: u64,
}

/// One `ci_manifest(ns, index, generation, segments, model_gen)` row plus lease columns.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    pub index: IndexKind,
    pub generation: u64,
    pub segments: Vec<SegRef>,
    /// `d/ciidx.v1/<hash>`: ords committed at this generation.
    pub indexed: Option<Hash>,
    pub model_gen: u32,
    pub dropped: Vec<Dropped>,
    pub leader: Option<String>,
    pub lease_until_ms: u64,
}

impl Manifest {
    /// Generation 0: nothing indexed.
    pub fn empty(index: IndexKind) -> Self {
        Manifest {
            index,
            generation: 0,
            segments: Vec::new(),
            indexed: None,
            model_gen: 0,
            dropped: Vec::new(),
            leader: None,
            lease_until_ms: 0,
        }
    }
}

/// Result of a CAS.
#[derive(Debug)]
pub enum Cas {
    Won(Manifest),
    /// Another commit got there first; this is the current row.
    Lost(Manifest),
}

/// The storage contract of `ci_manifest`. An implementation is atomic per `(ns, index)`.
#[async_trait]
pub trait ManifestStore: Send + Sync {
    /// The current row, or generation 0 when none exists.
    async fn load(&self, ns: &str, index: IndexKind) -> Result<Manifest>;

    /// Replaces the row with `next` when its generation is still `expected` and `holder` holds
    /// an unexpired lease at `now_ms`. `next.generation` must be `expected + 1`. The lease
    /// columns of `next` are ignored: only [`lease`](Self::lease) changes them.
    async fn cas(
        &self,
        ns: &str,
        index: IndexKind,
        expected: u64,
        next: Manifest,
        holder: &str,
        now_ms: u64,
    ) -> Result<Cas>;

    /// Takes or renews the leader lease. `Ok(false)` when another holder's lease is live.
    async fn lease(
        &self,
        ns: &str,
        index: IndexKind,
        holder: &str,
        now_ms: u64,
        ttl_ms: u64,
    ) -> Result<bool>;
}

/// An in-process [`ManifestStore`] (tests, SQLite single-node and the local index).
#[derive(Default)]
pub struct MemManifest {
    rows: Mutex<HashMap<(String, IndexKind), Manifest>>,
}

impl MemManifest {
    pub fn new() -> Self {
        Self::default()
    }

    fn rows(&self) -> std::sync::MutexGuard<'_, HashMap<(String, IndexKind), Manifest>> {
        self.rows.lock().unwrap_or_else(|p| p.into_inner())
    }
}

#[async_trait]
impl ManifestStore for MemManifest {
    async fn load(&self, ns: &str, index: IndexKind) -> Result<Manifest> {
        Ok(self
            .rows()
            .get(&(ns.to_owned(), index))
            .cloned()
            .unwrap_or_else(|| Manifest::empty(index)))
    }

    async fn cas(
        &self,
        ns: &str,
        index: IndexKind,
        expected: u64,
        mut next: Manifest,
        holder: &str,
        now_ms: u64,
    ) -> Result<Cas> {
        let mut rows = self.rows();
        let cur = rows
            .entry((ns.to_owned(), index))
            .or_insert_with(|| Manifest::empty(index));
        if cur.leader.as_deref() != Some(holder) || cur.lease_until_ms <= now_ms {
            return Err(IndexError::NotLeader);
        }
        if cur.generation != expected {
            return Ok(Cas::Lost(cur.clone()));
        }
        if next.generation != expected + 1 || next.index != index {
            return Err(IndexError::BadPack("manifest generation"));
        }
        next.leader.clone_from(&cur.leader);
        next.lease_until_ms = cur.lease_until_ms;
        *cur = next.clone();
        Ok(Cas::Won(next))
    }

    async fn lease(
        &self,
        ns: &str,
        index: IndexKind,
        holder: &str,
        now_ms: u64,
        ttl_ms: u64,
    ) -> Result<bool> {
        let mut rows = self.rows();
        let cur = rows
            .entry((ns.to_owned(), index))
            .or_insert_with(|| Manifest::empty(index));
        let free = cur.leader.is_none() || cur.lease_until_ms <= now_ms;
        if free || cur.leader.as_deref() == Some(holder) {
            cur.leader = Some(holder.to_owned());
            cur.lease_until_ms = now_ms.saturating_add(ttl_ms);
            return Ok(true);
        }
        Ok(false)
    }
}

/// Picks the smallest segments to merge: `MERGE_FACTOR` of them once that many exist,
/// keeping the inputs under [`MAX_MERGE_BYTES`]. Indexes into `segs`.
pub fn pick_merge(segs: &[SegRef]) -> Vec<usize> {
    if segs.len() < MERGE_FACTOR {
        return Vec::new();
    }
    let mut order: Vec<usize> = (0..segs.len()).collect();
    order.sort_by_key(|&i| (segs[i].bytes, i));
    let mut picked = Vec::new();
    let mut total = 0u64;
    for i in order.into_iter().take(MERGE_FACTOR) {
        total = total.saturating_add(segs[i].bytes);
        if total > MAX_MERGE_BYTES {
            break;
        }
        picked.push(i);
    }
    if picked.len() < 2 {
        picked.clear();
    }
    picked
}

/// The leader's side: publishes segments, compacts and sweeps.
pub struct Publisher<'a> {
    pub objects: &'a dyn Objects,
    pub manifests: &'a dyn ManifestStore,
    pub ns: &'a str,
    pub kind: IndexKind,
    pub holder: &'a str,
}

impl Publisher<'_> {
    /// Takes or renews the lease.
    pub async fn lease(&self, now_ms: u64, ttl_ms: u64) -> Result<bool> {
        self.manifests
            .lease(self.ns, self.kind, self.holder, now_ms, ttl_ms)
            .await
    }

    /// Publishes whole segments as one new generation. The packs are uploaded first; a lost
    /// CAS re-bases onto the winner's generation (its segments and `indexed` are kept) and
    /// tries again, so nothing a concurrent commit published is lost.
    pub async fn commit(&self, packs: &[SegmentPack], now_ms: u64) -> Result<Manifest> {
        for p in packs {
            if p.kind != self.kind {
                return Err(IndexError::BadPack("kind mismatch"));
            }
            put_pack(self.objects, p).await?;
        }
        let mut added = RoaringBitmap::new();
        for p in packs {
            added |= &p.ords;
        }
        let mut cur = self.manifests.load(self.ns, self.kind).await?;
        for _ in 0..=MAX_REBASES {
            let mut indexed = match cur.indexed {
                Some(h) => get_indexed(self.objects, h).await?,
                None => RoaringBitmap::new(),
            };
            indexed |= &added;
            let new_idx = put_indexed(self.objects, &indexed).await?;
            let mut next = cur.clone();
            next.generation = cur.generation + 1;
            for p in packs {
                if !next.segments.iter().any(|s| s.hash == p.hash) {
                    next.segments.push(SegRef {
                        hash: p.hash,
                        docs: p.docs,
                        bytes: p.bytes,
                    });
                }
            }
            if next.segments.len() > MAX_SEGMENTS {
                return Err(IndexError::Limit("live segments"));
            }
            if let Some(old) = cur.indexed
                && old != new_idx
            {
                next.dropped.push(Dropped {
                    kind: DropKind::Indexed,
                    hash: old,
                    at_ms: now_ms,
                });
            }
            next.indexed = Some(new_idx);
            match self
                .manifests
                .cas(
                    self.ns,
                    self.kind,
                    cur.generation,
                    next,
                    self.holder,
                    now_ms,
                )
                .await?
            {
                Cas::Won(m) => return Ok(m),
                Cas::Lost(m) => cur = m,
            }
        }
        Err(IndexError::Contended(MAX_REBASES))
    }

    /// Merges the smallest segments into one and publishes the result as a new generation
    /// that drops the inputs. `Ok(None)` when there is nothing to merge or the inputs changed
    /// under it.
    pub async fn compact(
        &self,
        work_dir: &std::path::Path,
        now_ms: u64,
    ) -> Result<Option<Manifest>> {
        let mut cur = self.manifests.load(self.ns, self.kind).await?;
        let picks: Vec<SegRef> = pick_merge(&cur.segments)
            .into_iter()
            .map(|i| cur.segments[i].clone())
            .collect();
        if picks.is_empty() {
            return Ok(None);
        }
        let mut inputs = Vec::with_capacity(picks.len());
        for s in &picks {
            inputs.push(get_verified(self.objects, seg_key(s.hash), s.hash, MAX_PACK_BYTES).await?);
        }
        let (kind, dir) = (self.kind, work_dir.to_owned());
        let merged = tokio::task::spawn_blocking(move || merge_packs(kind, &dir, &inputs))
            .await
            .map_err(|e| IndexError::Io(e.to_string()))??;
        let Some(merged) = merged else {
            return Ok(None);
        };
        put_pack(self.objects, &merged).await?;
        for _ in 0..=MAX_REBASES {
            if !picks
                .iter()
                .all(|p| cur.segments.iter().any(|s| s.hash == p.hash))
            {
                return Ok(None);
            }
            let mut next = cur.clone();
            next.generation = cur.generation + 1;
            next.segments
                .retain(|s| !picks.iter().any(|p| p.hash == s.hash));
            next.segments.push(SegRef {
                hash: merged.hash,
                docs: merged.docs,
                bytes: merged.bytes,
            });
            for p in &picks {
                next.dropped.push(Dropped {
                    kind: DropKind::Segment,
                    hash: p.hash,
                    at_ms: now_ms,
                });
            }
            match self
                .manifests
                .cas(
                    self.ns,
                    self.kind,
                    cur.generation,
                    next,
                    self.holder,
                    now_ms,
                )
                .await?
            {
                Cas::Won(m) => return Ok(Some(m)),
                Cas::Lost(m) => cur = m,
            }
        }
        Err(IndexError::Contended(MAX_REBASES))
    }

    /// Deletes objects dropped at least [`DROP_GRACE_MS`] ago and removes them from the
    /// manifest. Returns how many were deleted.
    pub async fn sweep(&self, now_ms: u64) -> Result<usize> {
        let mut cur = self.manifests.load(self.ns, self.kind).await?;
        for _ in 0..=MAX_REBASES {
            let ripe: Vec<Dropped> = cur
                .dropped
                .iter()
                .filter(|d| d.at_ms.saturating_add(DROP_GRACE_MS) <= now_ms)
                .cloned()
                .collect();
            if ripe.is_empty() {
                return Ok(0);
            }
            for d in &ripe {
                let key = match d.kind {
                    DropKind::Segment => seg_key(d.hash),
                    DropKind::Indexed => idx_key(d.hash),
                };
                // A live reference must never be deleted, whatever the list says.
                let live =
                    cur.segments.iter().any(|s| s.hash == d.hash) || cur.indexed == Some(d.hash);
                if !live {
                    self.objects
                        .delete(&key)
                        .await
                        .map_err(|e| IndexError::Store(e.to_string()))?;
                }
            }
            let mut next = cur.clone();
            next.generation = cur.generation + 1;
            next.dropped.retain(|d| !ripe.contains(d));
            match self
                .manifests
                .cas(
                    self.ns,
                    self.kind,
                    cur.generation,
                    next,
                    self.holder,
                    now_ms,
                )
                .await?
            {
                Cas::Won(_) => return Ok(ripe.len()),
                Cas::Lost(m) => cur = m,
            }
        }
        Err(IndexError::Contended(MAX_REBASES))
    }
}
