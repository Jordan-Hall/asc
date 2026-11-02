//! asc-engine.

use std::sync::Arc;

use asc_core::id::{ChangesetId, TreeId};
use asc_core::object::{Changeset, Tree};
use asc_store::objects::Objects;
use asc_store::refs::{Refs, RepoKey};

pub mod commit;
pub mod derive;
pub mod diff;
pub mod graph;
pub mod graph_skip;
pub mod revspec;
pub mod tree;

/// Number of decoded trees kept in memory per engine.
const TREE_CACHE_ITEMS: usize = 8192;

/// Number of decoded changesets kept in memory per engine.
const CHANGESET_CACHE_ITEMS: usize = 8192;

/// Repository logic over an object store and a ref store (SPEC §4).
#[allow(dead_code)] // `refs` and `repo` are used by later engine modules.
pub struct Engine<O: Objects, R: Refs> {
    pub(crate) objects: Arc<O>,
    pub(crate) refs: Arc<R>,
    pub(crate) repo: RepoKey,
    pub(crate) trees: quick_cache::sync::Cache<TreeId, Arc<Tree>>,
    pub(crate) changesets: quick_cache::sync::Cache<ChangesetId, Arc<Changeset>>,
}

impl<O: Objects, R: Refs> Engine<O, R> {
    /// An engine for one repository namespace.
    pub fn new(objects: Arc<O>, refs: Arc<R>, repo: RepoKey) -> Self {
        Self {
            objects,
            refs,
            repo,
            trees: quick_cache::sync::Cache::new(TREE_CACHE_ITEMS),
            changesets: quick_cache::sync::Cache::new(CHANGESET_CACHE_ITEMS),
        }
    }
}
