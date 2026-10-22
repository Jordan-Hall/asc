//! Derived data: memoised, recomputable values per changeset (SPEC §5).
//!
//! [`Derived`] values depend on the values of the changeset's parents. [`DerivedKeyed`] values
//! depend on a `(changeset, key)` pair and recurse along something other than parents.
//! Both are memoised under [`ObjKey::Derived`]; derivation is pure, so concurrent derivers
//! write identical bytes.

use std::collections::{HashMap, HashSet};

use async_trait::async_trait;
use asc_core::git::GitError;
use asc_core::id::{ChangesetId, GitOid};
use asc_core::object::Changeset;
use asc_store::codec::encode_stored;
use asc_store::key::ObjKey;
use asc_store::objects::{self, Objects};
use asc_store::refs::Refs;

use crate::{Engine, commit, tree};

pub mod generation;
pub mod gitcommit;
pub mod keyed;
pub mod sink;
pub mod skip;
#[cfg(test)]
pub(crate) mod testutil;

pub use generation::Gen;
pub use gitcommit::{GitCommit, index_commit_git};
pub use keyed::DerivedKeyed;
pub use sink::{GitMapRow, GitMapSink, index_tree_git};
pub use skip::Skip;

/// Derivation failures.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Store(#[from] objects::Error),
    #[error(transparent)]
    Commit(#[from] commit::Error),
    #[error(transparent)]
    Tree(#[from] tree::Error),
    #[error(transparent)]
    Git(#[from] GitError),
    #[error("stored {name} value does not decode")]
    BadValue { name: &'static str },
    #[error("git map sink: {0}")]
    Sink(String),
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

/// A value derived from a changeset and the values of its parents.
#[async_trait]
pub trait Derived: Sized + Send + Sync {
    /// Short stable name, e.g. `gen`.
    const NAME: &'static str;
    /// Bumping the version abandons old memos.
    const VERSION: u32;
    /// Must exist before any ref may point at the changeset.
    const EAGER: bool;

    /// Computes the value. `parents` holds the parents' values in parent order.
    async fn derive<O: Objects, R: Refs>(
        e: &Engine<O, R>,
        cs: &Changeset,
        id: ChangesetId,
        parents: &[Self],
    ) -> Result<Self>;

    fn encode(&self) -> Vec<u8>;
    fn decode(b: &[u8]) -> Result<Self>;
}

fn plain_key<D: Derived>(id: ChangesetId) -> ObjKey {
    ObjKey::Derived {
        name: D::NAME,
        version: D::VERSION,
        id: *id.as_bytes(),
    }
}

/// Reads a memo; a damaged one counts as absent because it is recomputable.
pub(crate) async fn read_memo<O: Objects>(objects: &O, key: &ObjKey) -> Result<Option<Vec<u8>>> {
    match objects.get(key).await {
        Ok(Some(b)) => Ok(Some(b.to_vec())),
        Ok(None) | Err(objects::Error::Corrupt(_) | objects::Error::Codec(..)) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

pub(crate) async fn write_memo<O: Objects>(objects: &O, key: &ObjKey, value: &[u8]) -> Result<()> {
    objects.put(key, encode_stored(value)).await?;
    Ok(())
}

impl<O: Objects, R: Refs> Engine<O, R> {
    /// The memoised value of `D` for `id`, deriving missing ancestors first.
    ///
    /// Iterative: an explicit stack finds underived ancestors and they are derived oldest
    /// first, so history depth never grows the call stack.
    pub async fn get_derived<D: Derived>(&self, id: ChangesetId) -> Result<D> {
        let mut done: HashMap<ChangesetId, D> = HashMap::new();
        let mut absent: HashSet<ChangesetId> = HashSet::new();
        let mut stack = vec![id];
        while let Some(&top) = stack.last() {
            if done.contains_key(&top) {
                stack.pop();
                continue;
            }
            if !absent.contains(&top) {
                if let Some(b) = read_memo(&*self.objects, &plain_key::<D>(top)).await?
                    && let Ok(v) = D::decode(&b)
                {
                    done.insert(top, v);
                    stack.pop();
                    continue;
                }
                absent.insert(top);
            }
            let cs = self.get_changeset(top).await?;
            let before = stack.len();
            stack.extend(cs.parents.iter().rev().filter(|p| !done.contains_key(p)));
            if stack.len() > before {
                continue;
            }
            // Every parent is done. Lend their values to `derive`, then put them back
            // for other children still waiting on the stack.
            let mut values = Vec::with_capacity(cs.parents.len());
            for p in &cs.parents {
                values.push(done.remove(p).ok_or(Error::BadValue { name: D::NAME })?);
            }
            let derived = D::derive(self, &cs, top, &values).await;
            for (p, v) in cs.parents.iter().zip(values) {
                done.insert(*p, v);
            }
            let value = derived?;
            write_memo(&*self.objects, &plain_key::<D>(top), &value.encode()).await?;
            done.insert(top, value);
            stack.pop();
        }
        done.remove(&id).ok_or(Error::BadValue { name: D::NAME })
    }

    /// Derives every eager type for `id`, so a ref may point at it.
    pub async fn warm(&self, id: ChangesetId) -> Result<()> {
        self.get_derived::<Gen>(id).await?;
        self.get_derived::<GitCommit>(id).await?;
        Ok(())
    }

    /// Git commit id of `id`.
    pub async fn git_commit_of(&self, id: ChangesetId) -> Result<GitOid> {
        Ok(self.get_derived::<GitCommit>(id).await?.commit)
    }
}
