//! The server's change index (PLAN C18, SPEC §20.2 and §20.4).
//!
//! * [`index`] holds the `changes` table (`cs -> change, meta`), the `m/` ingest validation (an
//!   [`IngestObserver`](crate::ingest::IngestObserver)), the server-written metas for rewrites,
//!   `landed_as` and the `LogEntry.change`/`meta` enricher.
//! * [`versions`] holds `change_versions`: the version log with the `CHANGE_DIVERGED` lease, and
//!   the [`GcRootSource`](asc_store::gc::GcRootSource) that keeps every published version alive.
//!
//! [`register`] is the unit's one line in `register_all`.

use std::sync::Arc;

use sqlx::Row;

use crate::read::ReadHooks;
use crate::registry::{Extensions, Registry};

pub mod index;
pub mod versions;

/// `$from, $from+1, ...` for `n` placeholders.
pub(crate) fn placeholders(from: usize, n: usize) -> String {
    (0..n)
        .map(|i| format!("${}", from + i))
        .collect::<Vec<_>>()
        .join(", ")
}

/// A fixed-size id from a stored column; `None` when the column is damaged.
pub(crate) fn fixed<const N: usize>(row: &sqlx::any::AnyRow, col: &str) -> Option<[u8; N]> {
    let v: Vec<u8> = row.try_get(col).ok()?;
    v.try_into().ok()
}

/// Registers the ingest observer, the log enricher and the GC root source.
pub fn register(ext: &Extensions, registry: &Arc<Registry>, hooks: &Arc<ReadHooks>) {
    let index = Arc::new(index::ChangeIndex::new(registry.clone()));
    ext.pipeline
        .set_observer(index.clone())
        .unwrap_or_else(|_| panic!("the ingest observer is registered once"));
    hooks.add_enricher(index);
    ext.pipeline
        .gc_roots()
        .register(Arc::new(versions::VersionRoots::new(
            registry.pool().clone(),
        )));
}
