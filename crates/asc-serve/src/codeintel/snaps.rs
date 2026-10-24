//! Pinned index snapshots and cached vector segments for the query side (SPEC §23.2.4,
//! §23.3, PLAN I16).
//!
//! A snapshot is one manifest generation of one index of one namespace, opened from the
//! replica's local segment directory (`Replica::snapshot` downloads and hash-checks what is
//! missing). Snapshots are kept for [`PIN_TTL`], the grace after which the publisher deletes
//! the objects a generation dropped (`DROP_GRACE_MS`), so a page token of an older generation
//! still resumes on the snapshot it was issued against. A token whose generation is no longer
//! pinned is stale.
//!
//! Vector segments are decoded once and kept under a byte budget.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use asc_codeintel::index::IndexError;
use asc_codeintel::index::manifest::{DROP_GRACE_MS, ManifestStore};
use asc_codeintel::index::schema::IndexKind;
use asc_codeintel::index::segments::{Replica, Snapshot};
use asc_codeintel::vectors::VectorSegment;
use asc_store::objects::Objects;
use tokio::sync::Mutex as AsyncMutex;

use super::modelgen::{CiError, ModelGens, VecSeg};

/// How long a snapshot stays pinned after it was opened.
pub const PIN_TTL: Duration = Duration::from_millis(DROP_GRACE_MS);
/// Pinned snapshots kept at once; the oldest go first.
pub const MAX_PINNED: usize = 64;
/// Bytes of decoded vector segments kept.
pub const DEFAULT_VECTOR_BYTES: u64 = 512 << 20;

type Key = (String, &'static str, u64);

struct Pinned {
    snap: Arc<Snapshot>,
    at: Instant,
}

/// The snapshots of this replica.
pub struct Snaps {
    replica: Replica,
    manifests: Arc<dyn ManifestStore>,
    pinned: std::sync::Mutex<HashMap<Key, Pinned>>,
    /// One open at a time: a cold open downloads and unpacks segments.
    opening: AsyncMutex<()>,
}

fn ix(e: IndexError) -> tonic::Status {
    tracing::error!(error = %e, "codeintel index error");
    match e {
        IndexError::Missing(_) | IndexError::HashMismatch(_) | IndexError::BadPack(_) => {
            tonic::Status::data_loss("the index is damaged")
        }
        _ => tonic::Status::internal("internal error"),
    }
}

impl Snaps {
    pub fn new(root: impl Into<PathBuf>, manifests: Arc<dyn ManifestStore>) -> Self {
        Self {
            replica: Replica::new(root),
            manifests,
            pinned: std::sync::Mutex::new(HashMap::new()),
            opening: AsyncMutex::new(()),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<Key, Pinned>> {
        self.pinned.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// The snapshot of generation `generation` if it is still pinned.
    pub fn pinned(&self, ns: &str, kind: IndexKind, generation: u64) -> Option<Arc<Snapshot>> {
        let g = self.lock();
        let p = g.get(&(ns.to_owned(), kind.name(), generation))?;
        (p.at.elapsed() < PIN_TTL).then(|| p.snap.clone())
    }

    /// The snapshot of the manifest's current generation, opened and pinned on a miss.
    pub async fn current(
        &self,
        objects: &dyn Objects,
        ns: &str,
        kind: IndexKind,
    ) -> Result<Arc<Snapshot>, tonic::Status> {
        let m = self.manifests.load(ns, kind).await.map_err(ix)?;
        if let Some(s) = self.pinned(ns, kind, m.generation) {
            return Ok(s);
        }
        let _one = self.opening.lock().await;
        // Another call may have opened it while this one waited.
        if let Some(s) = self.pinned(ns, kind, m.generation) {
            return Ok(s);
        }
        let snap = Arc::new(
            self.replica
                .snapshot(objects, ns, &m)
                .await
                .map_err(ix)?,
        );
        let keep: Vec<_> = {
            let mut g = self.lock();
            let now = Instant::now();
            g.retain(|_, p| now.duration_since(p.at) < PIN_TTL);
            while g.len() >= MAX_PINNED {
                let Some(oldest) = g.iter().min_by_key(|(_, p)| p.at).map(|(k, _)| k.clone())
                else {
                    break;
                };
                g.remove(&oldest);
            }
            g.insert(
                (ns.to_owned(), kind.name(), m.generation),
                Pinned {
                    snap: snap.clone(),
                    at: now,
                },
            );
            g.iter()
                .filter(|(k, _)| k.0 == ns && k.1 == kind.name())
                .flat_map(|(_, p)| p.snap.segments.iter().map(|s| s.hash))
                .collect()
        };
        // Segment directories no pinned snapshot names are removed.
        let replica = self.replica.clone();
        let ns = ns.to_owned();
        let pruned = tokio::task::spawn_blocking(move || replica.prune(&ns, kind, &keep)).await;
        if let Ok(Err(e)) = pruned {
            tracing::warn!(error = %e, "pruning replica segments failed");
        }
        Ok(snap)
    }
}

type VecKey = (Vec<u8>, String);

#[derive(Clone)]
struct VecWeight;

impl quick_cache::Weighter<VecKey, Arc<VectorSegment>> for VecWeight {
    fn weight(&self, key: &VecKey, val: &Arc<VectorSegment>) -> u64 {
        let per = u64::try_from(val.dims()).unwrap_or(u64::MAX).saturating_add(12);
        let n = u64::try_from(val.len()).unwrap_or(u64::MAX);
        n.saturating_mul(per)
            .saturating_add(u64::try_from(key.1.len()).unwrap_or(0))
            .saturating_add(256)
    }
}

/// Decoded vector segments, keyed by namespace and segment hash.
pub struct VecCache {
    cache: quick_cache::sync::Cache<VecKey, Arc<VectorSegment>, VecWeight>,
}

impl VecCache {
    pub fn new(bytes: u64) -> Self {
        let items = usize::try_from(bytes / (1 << 16)).unwrap_or(1 << 12).clamp(16, 1 << 16);
        Self {
            cache: quick_cache::sync::Cache::with_weighter(items, bytes.max(1 << 20), VecWeight),
        }
    }

    /// The segment, read and hash-verified through `gens` on a miss.
    pub async fn get(
        &self,
        gens: &ModelGens,
        ns: &[u8],
        seg: &VecSeg,
    ) -> Result<Arc<VectorSegment>, CiError> {
        let key = (ns.to_vec(), seg.hash.clone());
        if let Some(v) = self.cache.get(&key) {
            return Ok(v);
        }
        let v = Arc::new(gens.read_segment(ns, seg).await?);
        self.cache.insert(key, v.clone());
        Ok(v)
    }
}
