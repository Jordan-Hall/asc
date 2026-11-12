//! Packed objects: small objects live many to a file (docs/design/PACKED-OBJECTS.md).
//!
//! A [`PackLayer`] sits inside [`BucketObjects`](crate::bucket::BucketObjects). Reads ask the
//! layer's [`PackIndex`] where a key is packed and fall back to the loose object; writes may go
//! to a new pack; GC deletes forget a key; a [`job`] packs old loose objects, merges small packs,
//! compacts packs GC has hollowed out and removes what nothing references.
//!
//! Two indexes implement [`PackIndex`]: SQL rows in the server's database (in `asc-serve`), and
//! [`BucketPackIndex`], a sorted `.idx` file per pack plus a manifest, for bucket mode where
//! there is no database. [`MemPackIndex`] serves tests.

use std::sync::Arc;
use std::time::Duration;

use crate::key::{KeyClass, ObjKey};

pub mod bucket_index;
pub mod cache;
pub mod format;
pub mod index;
pub mod job;
pub mod layer;
#[cfg(test)]
mod tests;

pub use bucket_index::BucketPackIndex;
pub use cache::PackCache;
pub use format::{PackEntry, PackId};
pub use index::{MemPackIndex, PackDetail, PackIndex, PackInfo, PackLoc, PackState};
pub use layer::PackLayer;

/// Tuning for writing and maintaining packs. Reading needs none of it.
#[derive(Clone, Debug)]
pub struct PackConfig {
    /// Master switch for everything that writes a pack. Reading packs is always on.
    pub enabled: bool,
    /// Largest stored value (`codec ‖ data`) that is packed.
    pub max_object: usize,
    /// Largest pack file.
    pub max_size: u64,
    /// Most objects in one pack.
    pub max_objects: usize,
    /// A loose object younger than this is left alone (a push may still be using it).
    pub min_age: Duration,
    /// A repository with fewer old loose small objects than this is left alone by the job.
    pub min_loose: usize,
    /// Packs smaller than this are candidates for merging.
    pub merge_below: u64,
    /// Merge when at least this many packs are candidates.
    pub merge_count: usize,
    /// A pack whose live bytes are below this share of its bytes is compacted.
    pub compact_below: f64,
    /// A retired pack's file is deleted this long after retirement.
    pub retire_age: Duration,
    /// A pack file no index knows is deleted when older than this.
    pub orphan_age: Duration,
    /// A bulk write (`put_many`) of fewer small objects than this stays loose.
    pub min_batch: usize,
    /// How long a "no packs here" answer is trusted. Also how long the first pack of a
    /// namespace keeps its loose copies, so a reader that has not noticed it still finds them.
    pub negative_ttl: Duration,
    /// Train a zstd dictionary for each pack written, and keep it when it saves more than it
    /// costs (format version 2). `None` writes version 1 packs only.
    pub dict: Option<format::DictOpts>,
    /// Pack derived values too (in packs of their own when first packed). Off keeps them loose;
    /// derived values already packed stay readable.
    pub derived: bool,
}

/// The store feature a writer that packs derived values records (a SQL `store_features` row, or
/// the bucket manifest's `features`), so builds that cannot read those entries refuse the
/// namespace instead of misreading it.
pub const FEATURE_DERIVED: &str = "pack-derived";
/// The store feature a writer that may write version 2 packs (with a dictionary) records.
pub const FEATURE_DICT: &str = "pack2";

impl PackConfig {
    /// The store features a writer with this configuration records.
    pub fn features(&self) -> Vec<&'static str> {
        let mut out = Vec::new();
        if self.enabled && self.dict.is_some() {
            out.push(FEATURE_DICT);
        }
        if self.enabled && self.derived {
            out.push(FEATURE_DERIVED);
        }
        out
    }

    /// Whether a writer with this configuration puts `k` in a pack.
    pub fn packs(&self, k: &ObjKey) -> bool {
        match k.class() {
            KeyClass::Derived => self.derived && k.derived_prefix().is_some(),
            c => c.packable(),
        }
    }
}

impl Default for PackConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            max_object: 64 * 1024,
            max_size: 64 << 20,
            max_objects: 100_000,
            min_age: Duration::from_secs(10 * 60),
            min_loose: 2_000,
            merge_below: 4 << 20,
            merge_count: 8,
            compact_below: 0.5,
            retire_age: Duration::from_secs(24 * 3600),
            orphan_age: Duration::from_secs(3600),
            min_batch: 16,
            negative_ttl: Duration::from_secs(5),
            dict: Some(format::DictOpts::default()),
            derived: true,
        }
    }
}

/// Seconds since the epoch, for index rows.
pub(crate) fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

/// A layer over `index` with `cfg`.
pub fn layer(index: Arc<dyn PackIndex>, cfg: PackConfig) -> Arc<PackLayer> {
    Arc::new(PackLayer::new(index, cfg))
}
