//! asc-engine.

use std::sync::Arc;

use asc_core::id::{ChangesetId, TreeId};
use asc_core::object::{Changeset, Tree};
use asc_store::objects::Objects;
use asc_store::refs::{Refs, RepoKey};

pub mod blame;
pub mod branchless;
pub mod change;
pub mod classify;
pub mod commit;
pub mod commit_graph;
pub mod derive;
pub mod diff;
pub mod graph;
pub mod graph_skip;
pub mod history;
pub mod interdiff;
pub mod land;
pub mod merge;
pub mod projects;
pub mod rebase;
pub mod render;
pub mod revspec;
pub mod stack;
pub mod sync;
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
    pub(crate) projects: projects::State,
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
            projects: projects::State::new(),
        }
    }

    /// An engine for the same refs and repository over other objects, with empty caches. A bulk
    /// writer uses it to read back through its own write buffer.
    pub fn rebound<P: Objects>(&self, objects: Arc<P>) -> Engine<P, R> {
        Engine::new(objects, self.refs.clone(), self.repo)
    }
}
