//! Changeset build, validation and reads (SPEC §2.6, §2.8, §4.2).
//!
//! Every check touches only the changed paths and the directories on their spines; whole trees
//! are never materialised and file bytes are streamed one chunk at a time.

use std::collections::HashMap;
use std::sync::Arc;

use asc_core::codec::{CodecError, Enveloped, decode_envelope};
use asc_core::content::{ChunkAssembler, ContentError};
use asc_core::git::{BlobOidHasher, GitError, blob_oid};
use asc_core::id::{ChangesetId, FileId, GitOid, TreeId};
use asc_core::object::{Change, Changeset, ChunkList, Kind, ObjectError, SortedMap, Tree};
use asc_core::path::{Name, PathError, RepoPath, casefold, checkout_safe, windows_valid};
use asc_core::sig::Signature;
use asc_store::codec::encode_stored;
use asc_store::key::ObjKey;
use asc_store::objects::{self, Objects};
use asc_store::refs::Refs;
use futures::stream::{self, StreamExt, TryStreamExt};

use crate::Engine;
use crate::tree;

/// Concurrent file verifications per changeset.
const VERIFY_FANOUT: usize = 16;

/// `extra` keys a changeset may carry.
const EXTRA_COMMIT: &[u8] = b"git.commit";
const EXTRA_GPGSIG: &[u8] = b"git.gpgsig";
const EXTRA_SNAPSHOT: &[u8] = b"asc.snapshot";

/// Changeset build and validation failures.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Store(#[from] objects::Error),
    #[error(transparent)]
    Tree(#[from] tree::Error),
    #[error("stored object does not decode: {0}")]
    Codec(#[from] CodecError),
    #[error(transparent)]
    Object(#[from] ObjectError),
    #[error(transparent)]
    Path(#[from] PathError),
    #[error(transparent)]
    Content(#[from] ContentError),
    #[error(transparent)]
    Git(#[from] GitError),
    #[error("changeset {0} is missing")]
    MissingChangeset(ChangesetId),
    #[error("parent {0} does not exist")]
    MissingParent(ChangesetId),
    #[error("parent {0} is listed twice")]
    DuplicateParent(ChangesetId),
    #[error("changes do not match the tree diff at {0}")]
    ChangesMismatch(RepoPath),
    #[error("file {file} written at {path} does not exist")]
    MissingFile { path: RepoPath, file: FileId },
    #[error("file written at {0} does not match its recorded size or git oid")]
    FileMismatch(RepoPath),
    #[error("copy source of {0} does not exist in the first parent")]
    MissingCopySource(RepoPath),
    #[error("{0} is named as a copy destination but is not a written file")]
    CopyNotWrite(RepoPath),
    #[error("{0} is the destination of two copies")]
    DuplicateCopy(RepoPath),
    #[error("extra key {0:?} is not allowed here")]
    BadExtra(String),
    #[error("extra key {0:?} appears twice")]
    DuplicateExtra(String),
    #[error("{0} is not safe to check out")]
    UnsafePath(RepoPath),
    #[error("{0} has a component that is invalid on Windows")]
    WindowsInvalid(RepoPath),
    #[error("{0} collides in case with a sibling")]
    CaseCollision(RepoPath),
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Which path rules apply to a changeset (SPEC §2.8).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PathPolicy {
    /// Imported git history: no path rules, and `git.commit` extras are accepted.
    Import,
    /// A new native changeset. With `windows_safe`, added or changed paths must be valid on
    /// Windows and must not collide in case with a sibling in the resulting tree.
    Native { windows_safe: bool },
}

/// Inputs of [`Engine::create_changeset`].
#[derive(Clone, Debug)]
pub struct NewChangeset {
    pub parents: Vec<ChangesetId>,
    pub author: Signature,
    pub committer: Signature,
    pub message: Vec<u8>,
    pub extra: Vec<(Vec<u8>, Vec<u8>)>,
    pub root: TreeId,
    /// `(destination, source)` pairs; each destination must be a written file.
    pub copies: Vec<(RepoPath, RepoPath)>,
}

/// A change without its `copy_from` annotation, the part fixed by the two roots.
fn bare(change: &Change) -> Change {
    match change {
        Change::Write {
            kind,
            file,
            size,
            git,
            ..
        } => Change::Write {
            kind: *kind,
            file: *file,
            size: *size,
            git: *git,
            copy_from: None,
        },
        other => other.clone(),
    }
}

/// A directory read during the case-collision check, with its folded-name counts.
struct DirInfo {
    tree: Arc<Tree>,
    folds: HashMap<Vec<u8>, usize>,
}

impl DirInfo {
    fn new(tree: Arc<Tree>) -> Self {
        let mut folds = HashMap::new();
        for e in tree.entries() {
            *folds.entry(casefold(e.name.as_bytes())).or_insert(0) += 1;
        }
        Self { tree, folds }
    }
}

impl<O: Objects, R: Refs> Engine<O, R> {
    /// Reads a changeset through the cache.
    pub async fn get_changeset(&self, id: ChangesetId) -> Result<Arc<Changeset>> {
        if let Some(cs) = self.changesets.get(&id) {
            return Ok(cs);
        }
        let data = self
            .objects
            .get(&ObjKey::Changeset(id))
            .await?
            .ok_or(Error::MissingChangeset(id))?;
        let cs = Arc::new(decode_envelope::<Changeset>(&data)?);
        self.changesets.insert(id, cs.clone());
        Ok(cs)
    }

    /// Builds, validates and stores a changeset, returning its id.
    ///
    /// `changes` is `leafdiff(root of parents[0], root)` with `copy_from` attached from
    /// `copies`. A refused changeset is not stored.
    pub async fn create_changeset(
        &self,
        new: NewChangeset,
        policy: PathPolicy,
    ) -> Result<ChangesetId> {
        let base = self.base_root(&new.parents).await?;
        let mut changes = self.leafdiff(base, new.root, None).await?;
        let mut copied: Vec<&RepoPath> = Vec::new();
        for (dest, src) in &new.copies {
            if copied.contains(&dest) {
                return Err(Error::DuplicateCopy(dest.clone()));
            }
            copied.push(dest);
            let at = changes
                .binary_search_by(|(p, _)| p.cmp(dest))
                .map_err(|_| Error::CopyNotWrite(dest.clone()))?;
            match &mut changes[at].1 {
                Change::Write { copy_from, .. } => *copy_from = Some(src.clone()),
                _ => return Err(Error::CopyNotWrite(dest.clone())),
            }
        }
        let cs = Changeset {
            parents: new.parents,
            root: new.root,
            author: new.author,
            committer: new.committer,
            message: new.message,
            extra: sorted_extra(new.extra)?,
            changes: SortedMap::new(changes)?,
        };
        // The diff was just computed from these roots, so rule 3 holds by construction.
        self.validate_inner(&cs, policy, false).await?;
        let envelope = cs.to_envelope();
        let id = ChangesetId::of(&envelope);
        self.objects
            .put(&ObjKey::Changeset(id), encode_stored(&envelope))
            .await?;
        self.changesets.insert(id, Arc::new(cs));
        Ok(id)
    }

    /// Checks every rule of SPEC §2.6 plus the path rules of `policy` (§2.8).
    ///
    /// Rule 2 (`apply(base, changes) == root`) follows from rule 3, because `apply` inverts
    /// `leafdiff`, so the exact diff comparison covers both.
    pub async fn validate_changeset(&self, cs: &Changeset, policy: PathPolicy) -> Result<()> {
        self.validate_inner(cs, policy, true).await
    }

    /// Root of `parents[0]`, or the empty tree.
    async fn base_root(&self, parents: &[ChangesetId]) -> Result<TreeId> {
        match parents.first() {
            None => Ok(Self::empty_tree_id()),
            Some(&p) => match self.get_changeset(p).await {
                Ok(cs) => Ok(cs.root),
                Err(Error::MissingChangeset(_)) => Err(Error::MissingParent(p)),
                Err(e) => Err(e),
            },
        }
    }

    async fn validate_inner(
        &self,
        cs: &Changeset,
        policy: PathPolicy,
        check_diff: bool,
    ) -> Result<()> {
        // Rule 1.
        for (i, p) in cs.parents.iter().enumerate() {
            if cs.parents[..i].contains(p) {
                return Err(Error::DuplicateParent(*p));
            }
        }
        let keys: Vec<ObjKey> = cs.parents.iter().map(|&p| ObjKey::Changeset(p)).collect();
        let present = self.objects.has_many(&keys).await?;
        if let Some(i) = present.iter().position(|&x| !x) {
            return Err(Error::MissingParent(cs.parents[i]));
        }
        validate_extra(cs, policy)?;
        let base = self.base_root(&cs.parents).await?;

        // Rules 2, 3 and 6: changes are exactly the diff against parents[0].
        if check_diff {
            let expected = self.leafdiff(base, cs.root, None).await?;
            compare_changes(&expected, cs.changes.as_slice())?;
        }

        // Rule 4.
        stream::iter(
            cs.changes
                .as_slice()
                .iter()
                .filter_map(|(path, change)| match change {
                    Change::Write {
                        file, size, git, ..
                    } => Some(self.verify_file(path, *file, *size, *git)),
                    _ => None,
                }),
        )
        .buffer_unordered(VERIFY_FANOUT)
        .try_collect::<()>()
        .await?;

        // Rule 5.
        for (path, change) in cs.changes.as_slice() {
            if let Change::Write {
                copy_from: Some(src),
                ..
            } = change
                && self.lookup(base, src).await?.is_none()
            {
                return Err(Error::MissingCopySource(path.clone()));
            }
        }

        if matches!(policy, PathPolicy::Native { windows_safe: true }) {
            self.check_windows_paths(cs).await?;
        }
        Ok(())
    }

    /// Checks that `file` exists and that its length and git blob oid match, streaming chunks.
    async fn verify_file(
        &self,
        path: &RepoPath,
        file: FileId,
        size: u64,
        git: GitOid,
    ) -> Result<()> {
        let mismatch = || Error::FileMismatch(path.clone());
        let missing = || Error::MissingFile {
            path: path.clone(),
            file,
        };
        if let Some(data) = self.objects.get(&ObjKey::File(file)).await? {
            if data.len() as u64 != size || blob_oid(&data) != git {
                return Err(mismatch());
            }
            return Ok(());
        }
        let list_bytes = self
            .objects
            .get(&ObjKey::ChunkList(file))
            .await?
            .ok_or_else(missing)?;
        let list: ChunkList = decode_envelope(&list_bytes)?;
        if list.size != size {
            return Err(mismatch());
        }
        let ids: Vec<_> = list.chunks.iter().map(|&(id, _)| id).collect();
        let mut assembler = ChunkAssembler::new(list, file)?;
        let mut hasher = BlobOidHasher::new(size);
        for id in ids {
            let chunk = self
                .objects
                .get(&ObjKey::Chunk(id))
                .await?
                .ok_or_else(missing)?;
            assembler.push(&chunk)?;
            hasher.update(&chunk);
        }
        assembler.finish()?;
        if hasher.finalize()? != git {
            return Err(mismatch());
        }
        Ok(())
    }

    /// SPEC §2.8 native rule: each added or changed path is checkout-safe, Windows-valid in
    /// every component, and has no case-fold sibling in the resulting tree.
    async fn check_windows_paths(&self, cs: &Changeset) -> Result<()> {
        let mut dirs: HashMap<Vec<u8>, DirInfo> = HashMap::new();
        for (path, change) in cs.changes.as_slice() {
            if matches!(change, Change::Delete) {
                continue;
            }
            if !checkout_safe(path) {
                return Err(Error::UnsafePath(path.clone()));
            }
            let mut key: Vec<u8> = Vec::new();
            let mut dir_id = cs.root;
            let mut comps = path.components().peekable();
            while let Some(comp) = comps.next() {
                let name = Name::new(comp)?;
                if !windows_valid(&name) {
                    return Err(Error::WindowsInvalid(path.clone()));
                }
                if !dirs.contains_key(&key) {
                    let tree = self.get_tree(dir_id).await?;
                    dirs.insert(key.clone(), DirInfo::new(tree));
                }
                let info = &dirs[&key];
                if info.folds.get(&casefold(comp)).copied().unwrap_or(0) > 1 {
                    return Err(Error::CaseCollision(path.clone()));
                }
                if comps.peek().is_none() {
                    break;
                }
                match info.tree.get(&name) {
                    Some(e) if e.kind == Kind::Dir => dir_id = TreeId::from_bytes(e.id),
                    _ => break,
                }
                if !key.is_empty() {
                    key.push(b'/');
                }
                key.extend_from_slice(comp);
            }
        }
        Ok(())
    }
}

/// Sorts `extra` into its map, refusing duplicate keys.
fn sorted_extra(mut extra: Vec<(Vec<u8>, Vec<u8>)>) -> Result<SortedMap<Vec<u8>, Vec<u8>>> {
    extra.sort_by(|a, b| a.0.cmp(&b.0));
    if let Some(w) = extra.windows(2).find(|w| w[0].0 == w[1].0) {
        return Err(Error::DuplicateExtra(
            String::from_utf8_lossy(&w[0].0).into_owned(),
        ));
    }
    Ok(SortedMap::new(extra)?)
}

/// `extra` keys are limited to the SPEC §2.6 table. `git.commit` is only for git import
/// (CARRY: ingest rejects it on every other path) and `asc.snapshot` must be empty.
fn validate_extra(cs: &Changeset, policy: PathPolicy) -> Result<()> {
    for (key, value) in cs.extra.as_slice() {
        let ok = match key.as_slice() {
            EXTRA_COMMIT => policy == PathPolicy::Import,
            EXTRA_GPGSIG => true,
            EXTRA_SNAPSHOT => value.is_empty(),
            _ => false,
        };
        if !ok {
            return Err(Error::BadExtra(String::from_utf8_lossy(key).into_owned()));
        }
    }
    Ok(())
}

/// Both lists are sorted by path; reports the first path where they differ, ignoring
/// `copy_from`.
fn compare_changes(expected: &[(RepoPath, Change)], got: &[(RepoPath, Change)]) -> Result<()> {
    let (mut i, mut j) = (0, 0);
    loop {
        match (expected.get(i), got.get(j)) {
            (None, None) => return Ok(()),
            (Some((p, _)), None) | (None, Some((p, _))) => {
                return Err(Error::ChangesMismatch(p.clone()));
            }
            (Some((pe, ce)), Some((pg, asc))) => match pe.cmp(pg) {
                std::cmp::Ordering::Less => return Err(Error::ChangesMismatch(pe.clone())),
                std::cmp::Ordering::Greater => return Err(Error::ChangesMismatch(pg.clone())),
                std::cmp::Ordering::Equal => {
                    if bare(ce) != bare(asc) {
                        return Err(Error::ChangesMismatch(pe.clone()));
                    }
                    i += 1;
                    j += 1;
                }
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use async_trait::async_trait;
    use asc_core::content::{Body, ingest};
    use asc_core::git::blob_oid as blob;
    use asc_core::path::RefName;
    use asc_store::bucket::BucketObjects;
    use asc_store::refs::{RefEntry, RefError, RefLogEntry, RefTxn, RepoKey};

    use super::*;

    struct NoRefs;

    #[async_trait]
    impl Refs for NoRefs {
        async fn get(&self, _: RepoKey, _: &RefName) -> asc_store::refs::Result<Option<RefEntry>> {
            Ok(None)
        }
        async fn list(&self, _: RepoKey, _: &str) -> asc_store::refs::Result<Vec<RefEntry>> {
            Ok(Vec::new())
        }
        async fn update(&self, _: RepoKey, _: &RefTxn) -> asc_store::refs::Result<()> {
            Err(RefError::Corrupt("unused".into()))
        }
        async fn log(
            &self,
            _: RepoKey,
            _: &RefName,
            _: u32,
        ) -> asc_store::refs::Result<Vec<RefLogEntry>> {
            Ok(Vec::new())
        }
    }

    type TestEngine = Engine<BucketObjects, NoRefs>;

    fn engine() -> TestEngine {
        Engine::new(
            Arc::new(BucketObjects::memory()),
            Arc::new(NoRefs),
            RepoKey(1),
        )
    }

    fn p(s: &str) -> RepoPath {
        RepoPath::new(s).unwrap()
    }

    fn sig() -> Signature {
        Signature::new(b"A".to_vec(), b"a@x".to_vec(), 1, 0).unwrap()
    }

    const NATIVE: PathPolicy = PathPolicy::Native { windows_safe: true };

    /// Stores `content` as a file and returns the matching `Write`.
    async fn put_file(e: &TestEngine, content: &[u8]) -> Change {
        let got = ingest(Cursor::new(content)).unwrap();
        match got.body {
            Body::Inline(b) => {
                e.objects
                    .put(&ObjKey::File(got.file), encode_stored(&b))
                    .await
                    .unwrap();
            }
            Body::Chunked { list, chunks } => {
                for (id, data) in chunks {
                    e.objects
                        .put(&ObjKey::Chunk(id), encode_stored(&data))
                        .await
                        .unwrap();
                }
                e.objects
                    .put(
                        &ObjKey::ChunkList(got.file),
                        encode_stored(&list.to_envelope()),
                    )
                    .await
                    .unwrap();
            }
        }
        Change::write(Kind::File, got.file, got.size, got.git, None).unwrap()
    }

    async fn root_of(e: &TestEngine, base: TreeId, changes: &[(RepoPath, Change)]) -> TreeId {
        e.apply(base, changes).await.unwrap()
    }

    fn new_cs(parents: Vec<ChangesetId>, root: TreeId) -> NewChangeset {
        NewChangeset {
            parents,
            author: sig(),
            committer: sig(),
            message: b"m".to_vec(),
            extra: Vec::new(),
            root,
            copies: Vec::new(),
        }
    }

    /// A stored root changeset with `a.txt` and `dir/b.txt`.
    async fn base(e: &TestEngine) -> (ChangesetId, TreeId) {
        let a = put_file(e, b"alpha").await;
        let b = put_file(e, b"beta").await;
        let empty = TestEngine::empty_tree_id();
        let root = root_of(e, empty, &[(p("a.txt"), a), (p("dir/b.txt"), b)]).await;
        let id = e
            .create_changeset(new_cs(vec![], root), NATIVE)
            .await
            .unwrap();
        (id, root)
    }

    async fn load(e: &TestEngine, id: ChangesetId) -> Changeset {
        (*e.get_changeset(id).await.unwrap()).clone()
    }

    /// Creates a child of `parent` (whose root is `pr`) applying `ch`.
    async fn child(
        e: &TestEngine,
        parent: ChangesetId,
        pr: TreeId,
        ch: &[(RepoPath, Change)],
    ) -> (Changeset, TreeId) {
        let root = root_of(e, pr, ch).await;
        let id = e
            .create_changeset(new_cs(vec![parent], root), NATIVE)
            .await
            .unwrap();
        (load(e, id).await, root)
    }

    fn paths(cs: &Changeset) -> Vec<String> {
        cs.changes
            .as_slice()
            .iter()
            .map(|(q, _)| q.to_string())
            .collect()
    }

    #[tokio::test]
    async fn create_stores_diff_against_first_parent() {
        let e = engine();
        let (root_id, root) = base(&e).await;
        let c = put_file(&e, b"gamma").await;
        let (cs, _) = child(
            &e,
            root_id,
            root,
            &[(p("dir/c.txt"), c), (p("a.txt"), Change::Delete)],
        )
        .await;
        assert_eq!(paths(&cs), ["a.txt", "dir/c.txt"]);
        // A fresh engine on the same store reads the same changeset.
        let e2 = Engine::new(e.objects.clone(), Arc::new(NoRefs), RepoKey(1));
        assert_eq!(*e2.get_changeset(cs.id()).await.unwrap(), cs);
        e.validate_changeset(&cs, NATIVE).await.unwrap();
    }

    #[tokio::test]
    async fn copy_from_is_attached_and_checked() {
        let e = engine();
        let (root_id, root) = base(&e).await;
        let alpha = put_file(&e, b"alpha").await;
        let new_root = root_of(&e, root, &[(p("copy.txt"), alpha)]).await;
        let mut n = new_cs(vec![root_id], new_root);
        n.copies = vec![(p("copy.txt"), p("a.txt"))];
        let id = e.create_changeset(n.clone(), NATIVE).await.unwrap();
        let cs = load(&e, id).await;
        match &cs.changes.as_slice()[0].1 {
            Change::Write { copy_from, .. } => assert_eq!(copy_from.as_ref(), Some(&p("a.txt"))),
            other => panic!("unexpected {other:?}"),
        }
        // A destination that is not a written file is refused.
        n.copies = vec![(p("a.txt"), p("a.txt"))];
        assert!(matches!(
            e.create_changeset(n.clone(), NATIVE).await,
            Err(Error::CopyNotWrite(_))
        ));
        // A source that is absent from the first parent is refused.
        n.copies = vec![(p("copy.txt"), p("nope.txt"))];
        assert!(matches!(
            e.create_changeset(n, NATIVE).await,
            Err(Error::MissingCopySource(_))
        ));
    }

    #[tokio::test]
    async fn rule1_parents_exist_and_are_distinct() {
        let e = engine();
        let (id, root) = base(&e).await;
        let mut cs = load(&e, id).await;
        cs.parents = vec![ChangesetId::of(b"nothing")];
        assert!(matches!(
            e.validate_changeset(&cs, NATIVE).await,
            Err(Error::MissingParent(_))
        ));
        cs.parents = vec![id, id];
        cs.root = root;
        assert!(matches!(
            e.validate_changeset(&cs, NATIVE).await,
            Err(Error::DuplicateParent(_))
        ));
        let missing = e
            .create_changeset(new_cs(vec![ChangesetId::of(b"nothing")], root), NATIVE)
            .await;
        assert!(matches!(missing, Err(Error::MissingParent(_))));
    }

    #[tokio::test]
    async fn rules2_and3_changes_must_equal_the_diff() {
        let e = engine();
        let (id, root) = base(&e).await;
        let c = put_file(&e, b"gamma").await;
        let (good, _) = child(&e, id, root, &[(p("c.txt"), c)]).await;
        let rebuild = |changes: Vec<(RepoPath, Change)>| {
            let mut cs = good.clone();
            cs.changes = SortedMap::new(changes).unwrap();
            cs
        };
        let ok = good.changes.as_slice().to_vec();
        let mismatch = |cs: Changeset| {
            let e = &e;
            async move {
                matches!(
                    e.validate_changeset(&cs, NATIVE).await,
                    Err(Error::ChangesMismatch(_))
                )
            }
        };
        // Missing change.
        assert!(mismatch(rebuild(vec![])).await);
        // An unchanged leaf listed as written.
        let a = e.lookup(root, &p("a.txt")).await.unwrap().unwrap();
        let same =
            Change::write(Kind::File, FileId::from_bytes(a.id), a.size, a.git, None).unwrap();
        let mut more = ok.clone();
        more.insert(0, (p("a.txt"), same));
        assert!(mismatch(rebuild(more)).await);
        // A delete of something that was never there.
        let mut extra = ok.clone();
        extra.push((p("zzz"), Change::Delete));
        assert!(mismatch(rebuild(extra)).await);
        // Wrong kind.
        let mut kind = ok.clone();
        if let Change::Write {
            file, size, git, ..
        } = kind[0].1.clone()
        {
            kind[0].1 = Change::write(Kind::Exec, file, size, git, None).unwrap();
        }
        assert!(mismatch(rebuild(kind)).await);
        // The real thing passes.
        e.validate_changeset(&good, NATIVE).await.unwrap();
    }

    #[tokio::test]
    async fn file_to_directory_replacement_is_delete_plus_writes() {
        let e = engine();
        let (id, root) = base(&e).await;
        let x = put_file(&e, b"x").await;
        let (cs, _) = child(
            &e,
            id,
            root,
            &[(p("a.txt"), Change::Delete), (p("a.txt/inner"), x)],
        )
        .await;
        let ch = cs.changes.as_slice();
        assert_eq!(ch[0], (p("a.txt"), Change::Delete));
        assert_eq!(ch[1].0, p("a.txt/inner"));
        e.validate_changeset(&cs, NATIVE).await.unwrap();
    }

    #[tokio::test]
    async fn rule4_files_must_exist_and_match() {
        let e = engine();
        let (id, root) = base(&e).await;
        let real = put_file(&e, b"gamma").await;
        let Change::Write {
            file, size, git, ..
        } = real
        else {
            unreachable!()
        };
        // Trees that record wrong size or git for stored content are refused.
        for (s, g) in [(size + 1, git), (size, blob(b"other"))] {
            let lie = Change::write(Kind::File, file, s, g, None).unwrap();
            let r = root_of(&e, root, &[(p("c.txt"), lie)]).await;
            let err = e
                .create_changeset(new_cs(vec![id], r), NATIVE)
                .await
                .unwrap_err();
            assert!(matches!(err, Error::FileMismatch(_)), "{err}");
        }
        // A file nobody stored.
        let ghost =
            Change::write(Kind::File, FileId::of(b"ghost"), 5, blob(b"ghost"), None).unwrap();
        let r = root_of(&e, root, &[(p("g.txt"), ghost)]).await;
        let err = e
            .create_changeset(new_cs(vec![id], r), NATIVE)
            .await
            .unwrap_err();
        assert!(matches!(err, Error::MissingFile { .. }));
    }

    #[tokio::test]
    async fn chunked_files_are_verified_by_streaming() {
        let e = engine();
        let (id, root) = base(&e).await;
        let mut big = Vec::new();
        let mut x = 0x2545_F491_4F6C_DD1Du64;
        while big.len() < 5 * 1024 * 1024 {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            big.extend_from_slice(&x.to_le_bytes());
        }
        let change = put_file(&e, &big).await;
        let r = root_of(&e, root, &[(p("big.bin"), change.clone())]).await;
        e.create_changeset(new_cs(vec![id], r), NATIVE)
            .await
            .unwrap();
        let Change::Write { file, size, .. } = change else {
            unreachable!()
        };
        let lie = Change::write(Kind::File, file, size, blob(b"x"), None).unwrap();
        let r = root_of(&e, root, &[(p("big.bin"), lie)]).await;
        assert!(matches!(
            e.create_changeset(new_cs(vec![id], r), NATIVE).await,
            Err(Error::FileMismatch(_))
        ));
    }

    #[tokio::test]
    async fn rule6_merge_changes_are_relative_to_first_parent() {
        let e = engine();
        let (id, root) = base(&e).await;
        let side = put_file(&e, b"side").await;
        let main = put_file(&e, b"main").await;
        let (p1, r1) = child(&e, id, root, &[(p("m.txt"), main)]).await;
        let (p2, r2) = child(&e, id, root, &[(p("s.txt"), side.clone())]).await;
        let merged = root_of(&e, r1, &[(p("s.txt"), side)]).await;
        let mid = e
            .create_changeset(new_cs(vec![p1.id(), p2.id()], merged), NATIVE)
            .await
            .unwrap();
        let m = load(&e, mid).await;
        assert_eq!(paths(&m), ["s.txt"]);
        e.validate_changeset(&m, NATIVE).await.unwrap();
        // The diff against the second parent is not accepted.
        let against_p2 = e.leafdiff(r2, merged, None).await.unwrap();
        let mut bad = m.clone();
        bad.changes = SortedMap::new(against_p2).unwrap();
        assert!(matches!(
            e.validate_changeset(&bad, NATIVE).await,
            Err(Error::ChangesMismatch(_))
        ));
    }

    #[tokio::test]
    async fn rule5_copy_source_must_exist_in_first_parent() {
        let e = engine();
        let (id, root) = base(&e).await;
        let c = put_file(&e, b"gamma").await;
        let (mut cs, _) = child(&e, id, root, &[(p("c.txt"), c)]).await;
        let Change::Write {
            kind,
            file,
            size,
            git,
            ..
        } = cs.changes.as_slice()[0].1.clone()
        else {
            unreachable!()
        };
        let with_src = |src: &str| Change::write(kind, file, size, git, Some(p(src))).unwrap();
        cs.changes = SortedMap::new(vec![(p("c.txt"), with_src("dir/b.txt"))]).unwrap();
        e.validate_changeset(&cs, NATIVE).await.unwrap();
        cs.changes = SortedMap::new(vec![(p("c.txt"), with_src("missing"))]).unwrap();
        assert!(matches!(
            e.validate_changeset(&cs, NATIVE).await,
            Err(Error::MissingCopySource(_))
        ));
    }

    #[tokio::test]
    async fn extra_keys_are_restricted() {
        let e = engine();
        let (id, root) = base(&e).await;
        let mut n = new_cs(vec![id], root);
        let one = |k: &[u8], v: &[u8]| vec![(k.to_vec(), v.to_vec())];
        // git.commit is for git import only.
        n.extra = one(b"git.commit", b"tree 00\n");
        assert!(matches!(
            e.create_changeset(n.clone(), NATIVE).await,
            Err(Error::BadExtra(_))
        ));
        e.create_changeset(n.clone(), PathPolicy::Import)
            .await
            .unwrap();
        // Unknown keys and git.tag are refused everywhere.
        for key in [&b"git.tag"[..], b"x.other"] {
            n.extra = one(key, b"v");
            assert!(matches!(
                e.create_changeset(n.clone(), PathPolicy::Import).await,
                Err(Error::BadExtra(_))
            ));
        }
        // asc.snapshot must be empty; gpgsig is allowed.
        n.extra = one(b"asc.snapshot", b"1");
        assert!(matches!(
            e.create_changeset(n.clone(), NATIVE).await,
            Err(Error::BadExtra(_))
        ));
        n.extra = vec![
            (b"git.gpgsig".to_vec(), b"sig".to_vec()),
            (b"asc.snapshot".to_vec(), vec![]),
        ];
        e.create_changeset(n.clone(), NATIVE).await.unwrap();
        n.extra = vec![
            (b"git.gpgsig".to_vec(), vec![]),
            (b"git.gpgsig".to_vec(), vec![]),
        ];
        assert!(matches!(
            e.create_changeset(n, NATIVE).await,
            Err(Error::DuplicateExtra(_))
        ));
    }

    #[tokio::test]
    async fn native_windows_safe_rejects_reserved_names_and_case_collisions() {
        let e = engine();
        let (id, root) = base(&e).await;
        let f = put_file(&e, b"x").await;

        let con_root = root_of(&e, root, &[(p("CON.txt"), f.clone())]).await;
        let n = new_cs(vec![id], con_root);
        assert!(matches!(
            e.create_changeset(n.clone(), NATIVE).await,
            Err(Error::WindowsInvalid(_))
        ));
        e.create_changeset(n.clone(), PathPolicy::Import)
            .await
            .unwrap();
        e.create_changeset(
            n,
            PathPolicy::Native {
                windows_safe: false,
            },
        )
        .await
        .unwrap();

        // `A.ts` next to an existing `a.ts`.
        let (lower, lower_root) = child(&e, id, root, &[(p("a.ts"), f.clone())]).await;
        let upper_root = root_of(&e, lower_root, &[(p("A.ts"), f.clone())]).await;
        let n = new_cs(vec![lower.id()], upper_root);
        assert!(matches!(
            e.create_changeset(n.clone(), NATIVE).await,
            Err(Error::CaseCollision(_))
        ));
        e.create_changeset(n, PathPolicy::Import).await.unwrap();

        // A collision on a directory component is caught too.
        let dir_root = root_of(&e, lower_root, &[(p("DIR/x"), f)]).await;
        assert!(matches!(
            e.create_changeset(new_cs(vec![lower.id()], dir_root), NATIVE)
                .await,
            Err(Error::CaseCollision(_))
        ));
    }

    #[tokio::test]
    async fn pre_existing_violations_elsewhere_are_not_rechecked() {
        let e = engine();
        let (id, root) = base(&e).await;
        let f = put_file(&e, b"x").await;
        let bad_root = root_of(&e, root, &[(p("n.ts"), f.clone()), (p("N.ts"), f)]).await;
        let bad = e
            .create_changeset(new_cs(vec![id], bad_root), PathPolicy::Import)
            .await
            .unwrap();
        let g = put_file(&e, b"unrelated").await;
        let next = root_of(&e, bad_root, &[(p("dir/new.txt"), g)]).await;
        e.create_changeset(new_cs(vec![bad], next), NATIVE)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn unsafe_paths_are_refused_natively() {
        let e = engine();
        let (id, root) = base(&e).await;
        let f = put_file(&e, b"x").await;
        let r = root_of(&e, root, &[(p("git~1/config"), f)]).await;
        assert!(matches!(
            e.create_changeset(new_cs(vec![id], r), NATIVE).await,
            Err(Error::UnsafePath(_))
        ));
    }
}
