//! Three-way tree merge, cherry-pick and revert (SPEC §4.4).
//!
//! [`Engine::merge_trees`] merges whole trees with hash pruning: a subtree changed on one side
//! only is reused by id, and file content is read only for paths both sides changed. Text goes
//! through the `gix-merge` builtin driver ([`text`]); merge attributes (SPEC §2.4.1) and
//! configured drivers are looked up through the [`MergeAttributes`] and [`MergeDrivers`] hooks.
//! Rename detection is not part of the tree merge (SPEC §4.4: a union of two change sets).

use std::num::NonZeroU8;
use std::sync::Arc;

use asc_core::codec::{CodecError, Enveloped};
use asc_core::content::{Body, ContentError, ingest};
use asc_core::id::{ChangesetId, FileId, GitOid, TreeId};
use asc_core::object::{Entry, ObjectError};
use asc_core::path::{PathError, RepoPath};
use asc_store::codec::encode_stored;
use asc_store::key::ObjKey;
use asc_store::objects::{self, Objects};
use asc_store::refs::Refs;
use async_trait::async_trait;
use bytes::Bytes;

use crate::Engine;
use crate::{commit, diff, graph};

mod text;
mod tree;

pub use text::is_binary;
pub(crate) use text::merge_text;

#[cfg(test)]
pub(crate) mod testutil;

/// Merge, rebase and land failures.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Store(#[from] objects::Error),
    #[error(transparent)]
    Tree(#[from] crate::tree::Error),
    #[error(transparent)]
    Commit(#[from] commit::Error),
    #[error(transparent)]
    Diff(#[from] diff::Error),
    #[error(transparent)]
    Graph(#[from] graph::Error),
    #[error("stored object does not decode: {0}")]
    Codec(#[from] CodecError),
    #[error(transparent)]
    Object(#[from] ObjectError),
    #[error(transparent)]
    Path(#[from] PathError),
    #[error(transparent)]
    Content(#[from] ContentError),
    #[error("invalid glob '{glob}': {message}")]
    Glob { glob: String, message: String },
    #[error("merge driver '{driver}' failed: {message}")]
    Driver { driver: String, message: String },
    #[error("changeset {0} is a merge; a mainline parent is required")]
    NeedsMainline(ChangesetId),
    #[error("changeset {id} has {parents} parents; mainline {mainline} is out of range")]
    BadMainline {
        id: ChangesetId,
        mainline: usize,
        parents: usize,
    },
    #[error("changeset {0} is not a single-parent changeset and cannot be rebased")]
    NotLinear(ChangesetId),
    #[error("{} path(s) conflict", .0.len())]
    Conflicts(Vec<Conflict>),
    #[error("not rebased: an earlier changeset of the stack failed")]
    Blocked,
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

/// How conflict markers are drawn (`merge.conflictStyle`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ConflictStyle {
    #[default]
    Merge,
    Diff3,
    ZealousDiff3,
}

impl ConflictStyle {
    /// Parses a `merge.conflictStyle` value.
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "merge" => Some(Self::Merge),
            "diff3" => Some(Self::Diff3),
            "zdiff3" => Some(Self::ZealousDiff3),
            _ => None,
        }
    }
}

/// `-X ours` / `-X theirs`: which side wins a text conflict.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Favor {
    #[default]
    None,
    Ours,
    Theirs,
}

/// Names printed on conflict markers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Labels {
    pub base: String,
    pub ours: String,
    pub theirs: String,
}

impl Default for Labels {
    fn default() -> Self {
        Self {
            base: "base".into(),
            ours: "ours".into(),
            theirs: "theirs".into(),
        }
    }
}

/// The `merge` attribute of a path (SPEC §2.4.1).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum MergeAttr {
    /// Not mentioned: the text driver, unless the content is binary.
    #[default]
    Unspecified,
    /// `merge` or `merge=text`: always the text driver.
    Text,
    /// `-merge`: binary handling.
    Unset,
    /// `merge=binary`.
    Binary,
    /// `merge=union`: keep both sides' lines.
    Union,
    /// `merge=ours`: keep our side without conflict.
    Ours,
    /// `merge=<driver>`: a configured `merge.<driver>.driver`.
    Driver(String),
}

/// Merge attribute lookups. The real implementation reads `.ascattributes` / `.gitattributes`.
pub trait MergeAttributes: Send + Sync {
    fn merge_attr(&self, path: &RepoPath) -> MergeAttr;
}

/// No attributes: every path takes the default handling.
pub struct NoMergeAttributes;

impl MergeAttributes for NoMergeAttributes {
    fn merge_attr(&self, _: &RepoPath) -> MergeAttr {
        MergeAttr::Unspecified
    }
}

/// The three versions handed to a configured merge driver.
pub struct DriverRequest<'a> {
    pub path: &'a RepoPath,
    pub base: &'a [u8],
    pub ours: &'a [u8],
    pub theirs: &'a [u8],
    pub marker_size: u8,
    pub labels: &'a Labels,
}

/// A driver's merged content and whether it merged without conflict.
pub struct DriverResult {
    pub content: Vec<u8>,
    pub clean: bool,
}

/// Runs configured `merge.<driver>.driver` commands. The CLI implements it over
/// `asc::child::command()` (SPEC §9.4.1); the engine never spawns processes.
#[async_trait]
pub trait MergeDrivers: Send + Sync {
    /// Merges with `driver`; `None` when no such driver is configured (text merge is used).
    async fn run(&self, driver: &str, req: &DriverRequest<'_>) -> Result<Option<DriverResult>>;
}

/// No drivers configured.
pub struct NoDrivers;

#[async_trait]
impl MergeDrivers for NoDrivers {
    async fn run(&self, _: &str, _: &DriverRequest<'_>) -> Result<Option<DriverResult>> {
        Ok(None)
    }
}

/// Inputs shared by every merge entry point.
#[derive(Clone)]
pub struct MergeOptions {
    pub style: ConflictStyle,
    pub favor: Favor,
    pub marker_size: NonZeroU8,
    pub labels: Labels,
    pub attrs: Arc<dyn MergeAttributes>,
    pub drivers: Arc<dyn MergeDrivers>,
}

impl Default for MergeOptions {
    fn default() -> Self {
        Self {
            style: ConflictStyle::default(),
            favor: Favor::default(),
            marker_size: NonZeroU8::new(7).expect("7 is not zero"),
            labels: Labels::default(),
            attrs: Arc::new(NoMergeAttributes),
            drivers: Arc::new(NoDrivers),
        }
    }
}

/// Why a path could not merge cleanly.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConflictKind {
    /// Both sides changed the file differently.
    Content,
    /// Both sides added the path with different content.
    AddAdd,
    /// One side deleted what the other changed.
    ModifyDelete,
    /// One side has a directory where the other has a file.
    FileDir,
    /// Names that differ only in case were added on both sides.
    CaseFold,
    /// A binary file changed on both sides; ours is kept.
    Binary,
}

/// The base (stage 1), ours (stage 2) and theirs (stage 3) entries of a conflicted path.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Stages {
    pub base: Option<Entry>,
    pub ours: Option<Entry>,
    pub theirs: Option<Entry>,
}

/// One conflicted path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Conflict {
    pub path: RepoPath,
    pub kind: ConflictKind,
    pub stages: Stages,
    /// The stored file holding the merged text with conflict markers (`Content` and `AddAdd`
    /// text conflicts); this is also the file in the merged tree.
    pub marked_id: Option<FileId>,
}

/// A merged tree and its conflicts, sorted by path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MergeOutcome {
    pub root: TreeId,
    pub conflicts: Vec<Conflict>,
}

impl MergeOutcome {
    pub fn is_clean(&self) -> bool {
        self.conflicts.is_empty()
    }
}

/// A stored blob's identity.
pub(crate) struct Blob {
    pub file: FileId,
    pub size: u64,
    pub git: GitOid,
}

impl<O: Objects, R: Refs> Engine<O, R> {
    /// Stores `data` as a file (chunked above 4 MiB) and returns its identity.
    pub(crate) async fn store_blob(&self, data: &[u8]) -> Result<Blob> {
        let got = ingest(std::io::Cursor::new(data))?;
        match got.body {
            Body::Inline(b) => {
                self.objects
                    .put(&ObjKey::File(got.file), encode_stored(&b))
                    .await?;
            }
            Body::Chunked { list, chunks } => {
                for (id, chunk) in chunks {
                    self.objects
                        .put(&ObjKey::Chunk(id), encode_stored(&chunk))
                        .await?;
                }
                self.objects
                    .put(
                        &ObjKey::ChunkList(got.file),
                        encode_stored(&list.to_envelope()),
                    )
                    .await?;
            }
        }
        Ok(Blob {
            file: got.file,
            size: got.size,
            git: got.git,
        })
    }

    /// The bytes of a file entry.
    pub(crate) async fn read_blob(&self, entry: &Entry) -> Result<Bytes> {
        Ok(self
            .read_file(FileId::from_bytes(entry.id), entry.size)
            .await?)
    }

    /// Root of the parent a pick or revert is measured against.
    async fn pick_base(
        &self,
        id: ChangesetId,
        parents: &[ChangesetId],
        mainline: Option<usize>,
    ) -> Result<TreeId> {
        let parent = match (parents.len(), mainline) {
            (0, _) => return Ok(Self::empty_tree_id()),
            (1, None) => parents[0],
            (len, Some(n)) if (1..=len).contains(&n) => parents[n - 1],
            (_, None) => return Err(Error::NeedsMainline(id)),
            (len, Some(n)) => {
                return Err(Error::BadMainline {
                    id,
                    mainline: n,
                    parents: len,
                });
            }
        };
        Ok(self.get_changeset(parent).await?.root)
    }

    /// Replays the change `cs` made onto `onto`: merges `onto`'s root with `cs`'s root over
    /// `cs`'s parent. `mainline` (1-based) selects the parent of a merge changeset.
    ///
    /// The caller writes the changeset (and moves refs) when the outcome is clean.
    pub async fn cherry_pick(
        &self,
        cs: ChangesetId,
        onto: ChangesetId,
        mainline: Option<usize>,
        opts: &MergeOptions,
    ) -> Result<MergeOutcome> {
        let pick = self.get_changeset(cs).await?;
        let base = self.pick_base(cs, &pick.parents, mainline).await?;
        let onto_root = self.get_changeset(onto).await?.root;
        self.merge_trees(base, onto_root, pick.root, opts).await
    }

    /// Undoes the change `cs` made on top of `onto`: the inverse of [`Engine::cherry_pick`].
    pub async fn revert(
        &self,
        cs: ChangesetId,
        onto: ChangesetId,
        mainline: Option<usize>,
        opts: &MergeOptions,
    ) -> Result<MergeOutcome> {
        let undo = self.get_changeset(cs).await?;
        let parent_root = self.pick_base(cs, &undo.parents, mainline).await?;
        let onto_root = self.get_changeset(onto).await?.root;
        self.merge_trees(undo.root, onto_root, parent_root, opts)
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::testutil::{child, commit, files, fixture, read};
    use super::*;

    #[tokio::test]
    async fn cherry_pick_replays_the_change_onto_another_line() {
        let (e, base) = fixture().await;
        let pick = child(&e, base, &[("a", "ONE\n2\n3\n4\n5\n")], Vec::new()).await;
        let onto = child(&e, base, &[("b", "B\n")], Vec::new()).await;
        let got = e
            .cherry_pick(pick, onto, None, &MergeOptions::default())
            .await
            .unwrap();
        assert!(got.is_clean());
        assert_eq!(
            read(&e, got.root, "a").await,
            Some(b"ONE\n2\n3\n4\n5\n".to_vec())
        );
        assert_eq!(read(&e, got.root, "b").await, Some(b"B\n".to_vec()));
    }

    #[tokio::test]
    async fn cherry_pick_reports_conflicts() {
        let (e, base) = fixture().await;
        let pick = child(&e, base, &[("b", "mine\n")], Vec::new()).await;
        let onto = child(&e, base, &[("b", "other\n")], Vec::new()).await;
        let opts = MergeOptions {
            labels: Labels {
                base: "parent".into(),
                ours: "HEAD".into(),
                theirs: "pick".into(),
            },
            ..MergeOptions::default()
        };
        let got = e.cherry_pick(pick, onto, None, &opts).await.unwrap();
        assert_eq!(got.conflicts.len(), 1);
        assert_eq!(
            read(&e, got.root, "b").await,
            Some(b"<<<<<<< HEAD\nother\n=======\nmine\n>>>>>>> pick\n".to_vec())
        );
    }

    #[tokio::test]
    async fn revert_undoes_a_change_under_later_work() {
        let (e, base) = fixture().await;
        let bad = child(&e, base, &[("a", "ONE\n2\n3\n4\n5\n")], Vec::new()).await;
        let later = child(&e, bad, &[("b", "B\n")], Vec::new()).await;
        let got = e
            .revert(bad, later, None, &MergeOptions::default())
            .await
            .unwrap();
        assert!(got.is_clean());
        assert_eq!(
            read(&e, got.root, "a").await,
            Some(b"1\n2\n3\n4\n5\n".to_vec())
        );
        assert_eq!(read(&e, got.root, "b").await, Some(b"B\n".to_vec()));
    }

    #[tokio::test]
    async fn reverting_a_root_changeset_empties_the_tree() {
        let (e, base) = fixture().await;
        let got = e
            .revert(base, base, None, &MergeOptions::default())
            .await
            .unwrap();
        assert!(got.is_clean());
        assert_eq!(
            got.root,
            Engine::<asc_store::bucket::BucketObjects, testutil::NoRefs>::empty_tree_id()
        );
    }

    #[tokio::test]
    async fn merge_changesets_need_a_valid_mainline() {
        let (e, base) = fixture().await;
        let left = child(&e, base, &[("a", "L\n2\n3\n4\n5\n")], Vec::new()).await;
        let right = child(&e, base, &[("b", "R\n")], Vec::new()).await;
        let tree = files(&e, &[("a", "L\n2\n3\n4\n5\n"), ("b", "R\n"), ("c", "c\n")]).await;
        let merged = commit(&e, &[left, right], tree, "merge").await;
        let opts = MergeOptions::default();
        assert!(matches!(
            e.cherry_pick(merged, base, None, &opts).await,
            Err(Error::NeedsMainline(_))
        ));
        assert!(matches!(
            e.cherry_pick(merged, base, Some(3), &opts).await,
            Err(Error::BadMainline { mainline: 3, .. })
        ));
        // Relative to the first parent, the merge brought in `b`.
        let got = e.cherry_pick(merged, base, Some(1), &opts).await.unwrap();
        assert_eq!(read(&e, got.root, "b").await, Some(b"R\n".to_vec()));
    }
}
