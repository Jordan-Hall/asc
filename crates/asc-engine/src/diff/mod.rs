//! Tree and line diff with git-compatible output (SPEC §4.3).
//!
//! [`Engine::tree_changes`] turns `leafdiff` into [`TreeChange`]s, [`rename`] pairs additions with
//! deletions, [`line`] diffs file content, and [`format`] writes the result in git's output forms.
//! Content is fetched through a [`ContentSource`] only for the files that need it, one file at a
//! time, so memory stays bounded by the largest file rather than by the size of the change.

use std::io;

use asc_core::codec::CodecError;
use asc_core::id::{FileId, GitOid, TreeId};
use asc_core::object::{Change, Entry, Kind};
use asc_core::path::RepoPath;
use asc_store::objects::{self, Objects};
use asc_store::refs::Refs;
use futures::stream::{self, StreamExt, TryStreamExt};

use crate::Engine;
use crate::tree::{self, MAX_DIFF_VISITS, Pathspec};

pub mod apply;
pub mod format;
pub mod line;
mod options;
mod patch;
mod patience;
pub mod rename;
mod source;
mod stat;
mod words;
mod xdiff;

pub use line::Algorithm;
pub use options::{ConfigError, DiffOptions, SideKind};
pub use patch::{DiffCtx, write_patch, write_word_diff};
pub use rename::RenameOptions;
pub use source::{Attributes, ContentSource, DiffAttr, MAX_READ_FILE_BYTES, NoAttributes};
pub use stat::{write_numstat, write_shortstat, write_stat};

/// Concurrent old-side lookups in [`Engine::tree_changes`].
const LOOKUP_FANOUT: usize = 16;

/// Diff failures.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Tree(#[from] tree::Error),
    #[error(transparent)]
    Store(#[from] objects::Error),
    #[error("stored object does not decode: {0}")]
    Codec(#[from] CodecError),
    #[error("content of file {0} is missing or incomplete")]
    MissingFile(FileId),
    #[error("file {0} is {1} bytes, over the read limit")]
    FileTooLarge(FileId, u64),
    #[error("write failed: {0}")]
    Io(#[from] io::Error),
    #[error("textconv for '{driver}' failed: {message}")]
    Textconv { driver: String, message: String },
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

/// How a path differs between two trees.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    Added,
    Modified,
    Deleted,
    /// Renamed, with the similarity in percent.
    Renamed(u8),
    /// Copied, with the similarity in percent. Copy detection is not performed yet.
    Copied(u8),
    /// File became symlink or gitlink, or the reverse.
    TypeChanged,
}

impl Status {
    /// The letter of `--name-status` and `--raw`.
    pub fn letter(self) -> char {
        match self {
            Self::Added => 'A',
            Self::Modified => 'M',
            Self::Deleted => 'D',
            Self::Renamed(_) => 'R',
            Self::Copied(_) => 'C',
            Self::TypeChanged => 'T',
        }
    }

    /// Similarity percent of a rename or copy.
    pub fn score(self) -> Option<u8> {
        match self {
            Self::Renamed(s) | Self::Copied(s) => Some(s),
            _ => None,
        }
    }
}

/// One changed leaf path. `path` is the new path (the old one for a deletion); `old_path` differs
/// only for renames and copies.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TreeChange {
    pub path: RepoPath,
    pub old_path: RepoPath,
    pub old: Option<Entry>,
    pub new: Option<Entry>,
    pub status: Status,
}

impl TreeChange {
    /// Mode of the old side, or 0.
    pub fn old_mode(&self) -> u32 {
        self.old.as_ref().map_or(0, |e| mode(e.kind))
    }

    /// Mode of the new side, or 0.
    pub fn new_mode(&self) -> u32 {
        self.new.as_ref().map_or(0, |e| mode(e.kind))
    }
}

/// Git file mode of a leaf kind.
pub fn mode(kind: Kind) -> u32 {
    match kind {
        Kind::File => 0o100644,
        Kind::Exec => 0o100755,
        Kind::Symlink => 0o120000,
        Kind::Gitlink => 0o160000,
        Kind::Dir => 0o040000,
    }
}

/// Symlinks and gitlinks cannot be renamed into or out of regular files.
pub(crate) fn is_gitlink_or_link(kind: Kind) -> bool {
    matches!(kind, Kind::Symlink | Kind::Gitlink)
}

/// Lowercase hex of `oid` cut to `abbrev` digits; the null id when `oid` is `None`.
pub(crate) fn abbrev_oid(oid: Option<&GitOid>, abbrev: usize) -> String {
    let full = oid.map_or_else(|| "0".repeat(40), ToString::to_string);
    full[..abbrev.min(40)].to_string()
}

fn entry_of(path: &RepoPath, change: &Change) -> Option<Entry> {
    let name = path.file_name();
    match change {
        Change::Write {
            kind,
            file,
            size,
            git,
            ..
        } => Some(Entry {
            name,
            kind: *kind,
            id: *file.as_bytes(),
            size: *size,
            git: *git,
        }),
        Change::Gitlink { commit } => Some(Entry {
            name,
            kind: Kind::Gitlink,
            id: [0; 32],
            size: 0,
            git: *commit,
        }),
        Change::Delete => None,
    }
}

impl<O: Objects, R: Refs> Engine<O, R> {
    /// Changed leaves from `a` to `b`, sorted by path, with their old and new entries.
    ///
    /// Only directories on changed paths are read (`leafdiff`), plus one spine lookup per changed
    /// path for the old entry.
    pub async fn tree_changes(
        &self,
        a: TreeId,
        b: TreeId,
        spec: Option<&Pathspec>,
    ) -> Result<Vec<TreeChange>> {
        let (changes, _) = self
            .tree_changes_limited(a, b, spec, MAX_DIFF_VISITS, usize::MAX)
            .await?;
        Ok(changes)
    }

    /// [`tree_changes`](Self::tree_changes) that stops the walk once more than `limit` leaves
    /// are found (so at most `limit + 1` come back), with the entries the walk visited.
    pub async fn tree_changes_limited(
        &self,
        a: TreeId,
        b: TreeId,
        spec: Option<&Pathspec>,
        budget: usize,
        limit: usize,
    ) -> Result<(Vec<TreeChange>, usize)> {
        let (leaves, visits) = self.leafdiff_limited(a, b, spec, budget, limit).await?;
        let found: Vec<Option<TreeChange>> = stream::iter(leaves)
            .map(|(path, change)| async move {
                let old = self.lookup(a, &path).await?.filter(|e| e.kind != Kind::Dir);
                let new = entry_of(&path, &change);
                Ok::<_, Error>(classify(path, old, new))
            })
            .buffered(LOOKUP_FANOUT)
            .try_collect()
            .await?;
        Ok((found.into_iter().flatten().collect(), visits))
    }

    /// [`tree_changes`](Self::tree_changes) followed by rename detection when `opts` asks for it.
    pub async fn diff_trees(
        &self,
        a: TreeId,
        b: TreeId,
        spec: Option<&Pathspec>,
        opts: &DiffOptions,
    ) -> Result<Vec<TreeChange>> {
        let changes = self.tree_changes(a, b, spec).await?;
        match &opts.renames {
            Some(r) => rename::detect(changes, self, r).await,
            None => Ok(changes),
        }
    }
}

fn classify(path: RepoPath, old: Option<Entry>, new: Option<Entry>) -> Option<TreeChange> {
    let status = match (&old, &new) {
        (None, Some(_)) => Status::Added,
        (Some(_), None) => Status::Deleted,
        (Some(o), Some(n)) => {
            let class = |k: Kind| match k {
                Kind::Symlink => 1,
                Kind::Gitlink => 2,
                _ => 0,
            };
            if class(o.kind) == class(n.kind) {
                Status::Modified
            } else {
                Status::TypeChanged
            }
        }
        (None, None) => return None,
    };
    Some(TreeChange {
        old_path: path.clone(),
        path,
        old,
        new,
        status,
    })
}

#[cfg(test)]
mod golden;
#[cfg(test)]
mod tests;
#[cfg(test)]
mod tests_support;
