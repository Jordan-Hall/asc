//! The difference between two versions of one change (PLAN C20, SPEC §20.5 `InterDiff`).
//!
//! A version is a changeset; its *patch* is the diff from its first parent's tree to its own. The
//! interdiff of versions `A` and `B` is the diff of the two trees (`A.root` to `B.root`) kept to
//! the paths that either patch touches. Rebasing a change onto a moved trunk changes many paths
//! that neither version's patch names; those are the base's, not the change's, so they are left
//! out. When both versions have the same base the filter keeps everything, because every path
//! that differs is then inside one of the patches.
//!
//! The work is bounded: a patch of more than [`MAX_PATCH_PATHS`] paths is refused, not truncated.

use std::collections::HashSet;

use asc_core::id::{ChangesetId, TreeId};
use asc_core::path::RepoPath;
use asc_store::objects::Objects;
use asc_store::refs::Refs;

use crate::Engine;
use crate::commit;
use crate::diff::{self, DiffOptions, TreeChange, rename};
use crate::tree::{MAX_DIFF_VISITS, Pathspec};

/// Most paths one version's patch may touch before the interdiff is refused.
pub const MAX_PATCH_PATHS: usize = 100_000;

/// Interdiff failures.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Diff(#[from] diff::Error),
    #[error(transparent)]
    Commit(#[from] commit::Error),
    #[error("a version patch touches more than {0} paths")]
    TooLarge(usize),
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

/// One version: its tree and the tree it was made on (its first parent's, or the empty tree).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Side {
    pub root: TreeId,
    pub base: TreeId,
}

impl<O: Objects, R: Refs> Engine<O, R> {
    /// The [`Side`] of the version `cs`.
    pub async fn version_side(&self, cs: ChangesetId) -> Result<Side> {
        let c = self.get_changeset(cs).await?;
        let base = match c.parents.first() {
            Some(&p) => self.get_changeset(p).await?.root,
            None => Self::empty_tree_id(),
        };
        Ok(Side { root: c.root, base })
    }

    /// Paths the patch of `side` touches.
    async fn patch_paths(&self, side: Side, max: usize) -> Result<HashSet<RepoPath>> {
        let (changes, _) = self
            .tree_changes_limited(side.base, side.root, None, MAX_DIFF_VISITS, max)
            .await?;
        if changes.len() > max {
            return Err(Error::TooLarge(max));
        }
        Ok(changes.into_iter().map(|c| c.path).collect())
    }

    /// What changed between versions `from` and `to`, sorted by path. `spec` narrows the paths.
    pub async fn interdiff(
        &self,
        from: Side,
        to: Side,
        spec: Option<&Pathspec>,
        opts: &DiffOptions,
    ) -> Result<Vec<TreeChange>> {
        self.interdiff_limited(from, to, spec, opts, MAX_PATCH_PATHS)
            .await
    }

    async fn interdiff_limited(
        &self,
        from: Side,
        to: Side,
        spec: Option<&Pathspec>,
        opts: &DiffOptions,
        max: usize,
    ) -> Result<Vec<TreeChange>> {
        let mut changes = self.tree_changes(from.root, to.root, spec).await?;
        if from.base != to.base {
            let mut touched = self.patch_paths(from, max).await?;
            touched.extend(self.patch_paths(to, max).await?);
            changes.retain(|c| touched.contains(&c.path));
        }
        match &opts.renames {
            Some(r) => Ok(rename::detect(changes, self, r).await?),
            None => Ok(changes),
        }
    }
}

#[cfg(test)]
mod tests;
