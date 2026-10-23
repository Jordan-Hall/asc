//! Git objects by name for the plumbing commands: an object id or revision (`HEAD:dir/file`,
//! `v1^{tree}`, `:path` for the index) resolves to a blob, tree, commit or tag, whose git bytes
//! are produced on demand. Blobs stream; trees, commits and tags are small and render whole.
//!
//! The git id of an object asc derived (a tree, a blob read from a tree) is recorded in the git map
//! as it is named, so the id printed by one command is accepted by the next.

use std::io::Write;
use std::sync::Mutex;

use asc_core::codec::decode_envelope;
use asc_core::content::ChunkAssembler;
use asc_core::git::{ObjectKind, export_tag, object_oid, render_tree, tree_oid};
use asc_core::id::{ChangesetId, FileId, GitOid, RefTarget, TagId, TreeId};
use asc_core::object::{ChunkList, Entry, Kind, Tree};
use asc_core::path::RepoPath;
use asc_engine::derive::GitMapRow;
use asc_engine::revspec::dwim_candidates;
use asc_state::state::git_map;
use asc_store::key::ObjKey;
use asc_store::objects::Objects as _;
use asc_store::refs::Refs as _;

use crate::cli::add::target::{Staged, Target};
use crate::cli::common::{CliError, Ctx, Result, WsCtx};
use crate::cli::log::refs::tag_record;
use crate::cli::refs_plumbing::args::{object_oid_of, parse_oid};
use crate::cli::revparse::commit_of;

/// A git object asc can name.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Object {
    Blob { oid: GitOid, file: FileId },
    Tree { oid: GitOid, id: TreeId },
    Commit { oid: GitOid, id: ChangesetId },
    Tag { oid: GitOid, id: TagId },
}

impl Object {
    pub fn oid(&self) -> GitOid {
        match *self {
            Self::Blob { oid, .. } | Self::Tree { oid, .. } | Self::Commit { oid, .. } | Self::Tag { oid, .. } => oid,
        }
    }

    pub fn kind(&self) -> ObjectKind {
        match self {
            Self::Blob { .. } => ObjectKind::Blob,
            Self::Tree { .. } => ObjectKind::Tree,
            Self::Commit { .. } => ObjectKind::Commit,
            Self::Tag { .. } => ObjectKind::Tag,
        }
    }

    /// git's name for the type.
    pub fn type_name(&self) -> &'static str {
        type_name(self.kind())
    }
}

/// git's name for an object type.
pub fn type_name(kind: ObjectKind) -> &'static str {
    match kind {
        ObjectKind::Blob => "blob",
        ObjectKind::Tree => "tree",
        ObjectKind::Commit => "commit",
        ObjectKind::Tag => "tag",
    }
}

/// The object type a name spells.
pub fn kind_named(name: &str) -> Option<ObjectKind> {
    Some(match name {
        "blob" => ObjectKind::Blob,
        "tree" => ObjectKind::Tree,
        "commit" => ObjectKind::Commit,
        "tag" => ObjectKind::Tag,
        _ => return None,
    })
}

fn fatal(e: impl std::fmt::Display) -> CliError {
    CliError::Fatal(e.to_string())
}

/// `^{tree}`, `^{commit}`, `^{}` and friends at the end of a revision.
fn split_peel(rev: &str) -> (&str, Option<&str>) {
    match rev.strip_suffix('}').and_then(|r| r.rfind("^{").map(|at| (r, at))) {
        Some((r, at)) => (&rev[..at], Some(&r[at + 2..])),
        None => (rev, None),
    }
}

/// Names objects of one workspace.
pub struct Resolver<'a> {
    pub ctx: &'a Ctx,
    pub ws: &'a WsCtx,
    learned: Mutex<Vec<GitMapRow>>,
}

impl<'a> Resolver<'a> {
    pub fn new(ctx: &'a Ctx, ws: &'a WsCtx) -> Self {
        Self { ctx, ws, learned: Mutex::new(Vec::new()) }
    }

    /// Records that `git` is `asc` (kept until [`flush`](Self::flush)).
    pub fn learn(&self, git: GitOid, kind: ObjectKind, asc: [u8; 32]) {
        if let Ok(mut rows) = self.learned.lock() {
            rows.push(GitMapRow { git, kind, asc });
        }
    }

    /// Writes the recorded git map rows.
    pub async fn flush(&self) -> Result<()> {
        let rows = self.learned.lock().map(|mut r| std::mem::take(&mut *r)).unwrap_or_default();
        if !rows.is_empty() {
            git_map::insert_many(&self.ws.db, self.ws.repo.key, &rows).await?;
        }
        Ok(())
    }

    /// The object an entry of a tree is.
    pub fn from_entry(&self, entry: &Entry) -> Option<Object> {
        match entry.kind {
            Kind::Dir => {
                self.learn(entry.git, ObjectKind::Tree, entry.id);
                Some(Object::Tree { oid: entry.git, id: TreeId::from_bytes(entry.id) })
            }
            Kind::File | Kind::Exec | Kind::Symlink => {
                self.learn(entry.git, ObjectKind::Blob, entry.id);
                Some(Object::Blob { oid: entry.git, file: FileId::from_bytes(entry.id) })
            }
            Kind::Gitlink => None,
        }
    }

    /// The commit object of a changeset.
    pub async fn commit(&self, id: ChangesetId) -> Result<Object> {
        let oid = self.ws.engine.git_commit_of(id).await.map_err(fatal)?;
        self.learn(oid, ObjectKind::Commit, *id.as_bytes());
        Ok(Object::Commit { oid, id })
    }

    /// Records the git id of a commit's root tree, which the commit's bytes name.
    pub async fn learn_tree_of(&self, id: ChangesetId) -> Result<()> {
        let root = self.ws.engine.get_changeset(id).await?.root;
        self.tree(root).await?;
        Ok(())
    }

    /// The tree object with this id.
    pub async fn tree(&self, id: TreeId) -> Result<Object> {
        let tree = self.ws.engine.get_tree(id).await?;
        let oid = tree_oid(&tree);
        self.learn(oid, ObjectKind::Tree, *id.as_bytes());
        Ok(Object::Tree { oid, id })
    }

    /// A full git id the git map knows (or this command just learned).
    async fn by_oid(&self, oid: GitOid) -> Result<Option<Object>> {
        let fresh = self.learned.lock().ok().and_then(|rows| rows.iter().find(|r| r.git == oid).map(|r| (r.kind, r.asc)));
        if let Some((kind, asc)) = fresh {
            return Ok(Some(match kind {
                ObjectKind::Commit => Object::Commit { oid, id: ChangesetId::from_bytes(asc) },
                ObjectKind::Tag => Object::Tag { oid, id: TagId::from_bytes(asc) },
                ObjectKind::Tree => Object::Tree { oid, id: TreeId::from_bytes(asc) },
                ObjectKind::Blob => Object::Blob { oid, file: FileId::from_bytes(asc) },
            }));
        }
        let (db, repo) = (&self.ws.db, self.ws.repo.key);
        for kind in [ObjectKind::Commit, ObjectKind::Tag, ObjectKind::Tree, ObjectKind::Blob] {
            if let Some(asc) = git_map::git_to_asc(db, repo, oid, kind).await? {
                return Ok(Some(match kind {
                    ObjectKind::Commit => Object::Commit { oid, id: ChangesetId::from_bytes(asc) },
                    ObjectKind::Tag => Object::Tag { oid, id: TagId::from_bytes(asc) },
                    ObjectKind::Tree => Object::Tree { oid, id: TreeId::from_bytes(asc) },
                    ObjectKind::Blob => Object::Blob { oid, file: FileId::from_bytes(asc) },
                }));
            }
        }
        Ok(None)
    }

    /// An annotated tag a plain name denotes.
    async fn tag_by_name(&self, name: &str) -> Result<Option<Object>> {
        for cand in dwim_candidates(name) {
            let Ok(rn) = asc_core::path::RefName::new(cand.into_bytes()) else { continue };
            if let Some(entry) = self.ws.db.refs().get(self.ws.repo.key, &rn).await? {
                return match entry.target {
                    RefTarget::Tag(id) => {
                        let oid = object_oid_of(self.ws, entry.target).await?;
                        self.learn(oid, ObjectKind::Tag, *id.as_bytes());
                        Ok(Some(Object::Tag { oid, id }))
                    }
                    RefTarget::Changeset(_) => Ok(None),
                };
            }
        }
        Ok(None)
    }

    async fn named(&self, base: &str) -> Result<Option<Object>> {
        if let Some(oid) = parse_oid(base) {
            if let Some(obj) = self.by_oid(oid).await? {
                return Ok(Some(obj));
            }
        }
        if !base.contains(['~', '^', '@']) {
            if let Some(tag) = self.tag_by_name(base).await? {
                return Ok(Some(tag));
            }
        }
        match commit_of(self.ctx, base).await? {
            Some(id) => Ok(Some(self.commit(id).await?)),
            None => Ok(None),
        }
    }

    /// `obj` peeled to a `kind` object: a tag to what it tags, a commit to its tree.
    pub async fn peel(&self, obj: Object, kind: ObjectKind) -> Result<Option<Object>> {
        let mut at = obj;
        for _ in 0..4 {
            if at.kind() == kind {
                return Ok(Some(at));
            }
            at = match at {
                Object::Tag { id, .. } => self.commit(tag_record(self.ws, id).await?.target).await?,
                Object::Commit { id, .. } if kind == ObjectKind::Tree => {
                    self.tree(self.ws.engine.get_changeset(id).await?.root).await?
                }
                _ => return Ok(None),
            };
        }
        Ok(None)
    }

    /// `^{…}`: `tree`, `commit`, `tag`, `blob`, `object` or empty (peel tags away).
    async fn peeled(&self, obj: Object, what: &str) -> Result<Option<Object>> {
        match what {
            "" => match obj {
                Object::Tag { .. } => self.peel(obj, ObjectKind::Commit).await,
                other => Ok(Some(other)),
            },
            "object" => Ok(Some(obj)),
            other => match kind_named(other) {
                Some(kind) => self.peel(obj, kind).await,
                None => Ok(None),
            },
        }
    }

    /// The entry at `path` below tree `root`, as an object.
    pub async fn at_path(&self, root: TreeId, path: &str) -> Result<Option<Object>> {
        let path = path.trim_end_matches('/');
        if path.is_empty() {
            return Ok(Some(self.tree(root).await?));
        }
        let Ok(repo_path) = RepoPath::new(path.as_bytes().to_vec()) else { return Ok(None) };
        Ok(self.ws.engine.lookup(root, &repo_path).await?.and_then(|e| self.from_entry(&e)))
    }

    /// `:path` and `:<stage>:path`: what the index holds.
    async fn in_index(&self, spec: &str) -> Result<Option<Object>> {
        let (stage, path) = match spec.split_once(':') {
            Some((s, p)) if s.len() == 1 && s.as_bytes()[0].is_ascii_digit() => (s.as_bytes()[0] - b'0', p),
            _ => (0, spec),
        };
        if stage != 0 {
            return Ok(None);
        }
        let Ok(repo_path) = RepoPath::new(path.as_bytes().to_vec()) else { return Ok(None) };
        let target = Target::open(self.ctx).await?;
        Ok(match target.staged(self.ws, &repo_path).await? {
            Staged::Leaf { leaf, .. } if leaf.kind != Kind::Gitlink => {
                self.learn(leaf.git, ObjectKind::Blob, *leaf.file.as_bytes());
                Some(Object::Blob { oid: leaf.git, file: leaf.file })
            }
            _ => None,
        })
    }

    /// The object `name` denotes: a full id, a revision, `<rev>:<path>`, `:<path>`.
    pub async fn resolve(&self, name: &str) -> Result<Option<Object>> {
        if let Some(spec) = name.strip_prefix(':') {
            return self.in_index(spec).await;
        }
        if let Some(at) = name.find(':').filter(|&at| at > 0 && !name[..at].contains("@{")) {
            let (rev, path) = (&name[..at], &name[at + 1..]);
            let Some(base) = self.revision(rev).await? else { return Ok(None) };
            let Some(Object::Tree { id, .. }) = self.peel(base, ObjectKind::Tree).await? else {
                return Ok(None);
            };
            return self.at_path(id, path).await;
        }
        self.revision(name).await
    }

    async fn revision(&self, rev: &str) -> Result<Option<Object>> {
        let (base, peel) = split_peel(rev);
        let Some(obj) = self.named(base).await? else { return Ok(None) };
        match peel {
            Some(what) => self.peeled(obj, what).await,
            None => Ok(Some(obj)),
        }
    }
}

/// The git bytes of a tree, commit or tag.
pub async fn raw(ws: &WsCtx, obj: Object) -> Result<Vec<u8>> {
    match obj {
        Object::Tree { id, .. } => {
            let tree = ws.engine.get_tree(id).await?;
            Ok(render_tree(&tree))
        }
        Object::Commit { id, .. } => crate::cli::ref_filter::raw_commit(ws, id).await,
        Object::Tag { id, .. } => {
            let rec = tag_record(ws, id).await?;
            let target = ws.engine.git_commit_of(rec.target).await.map_err(fatal)?;
            export_tag(&rec, target).map_err(fatal)
        }
        Object::Blob { file, .. } => {
            let mut out = Vec::new();
            stream_blob(ws, file, &mut out).await?;
            Ok(out)
        }
    }
}

/// How a blob is stored.
enum Stored {
    Inline(bytes::Bytes),
    Chunked(ChunkList),
}

async fn stored(ws: &WsCtx, file: FileId) -> Result<Stored> {
    if let Some(bytes) = ws.objects.get(&ObjKey::File(file)).await? {
        return Ok(Stored::Inline(bytes));
    }
    let list = ws
        .objects
        .get(&ObjKey::ChunkList(file))
        .await?
        .ok_or_else(|| CliError::Fatal(format!("unable to read blob {file}")))?;
    Ok(Stored::Chunked(decode_envelope::<ChunkList>(&list).map_err(fatal)?))
}

/// The size of an object in bytes, as `cat-file -s` prints it.
pub async fn size(ws: &WsCtx, obj: Object) -> Result<u64> {
    match obj {
        Object::Blob { file, .. } => Ok(match stored(ws, file).await? {
            Stored::Inline(bytes) => bytes.len() as u64,
            Stored::Chunked(list) => list.size,
        }),
        other => Ok(raw(ws, other).await?.len() as u64),
    }
}

/// Writes a blob's bytes to `out`, one chunk at a time.
pub async fn stream_blob(ws: &WsCtx, file: FileId, out: &mut dyn Write) -> Result<()> {
    match stored(ws, file).await? {
        Stored::Inline(bytes) => out.write_all(&bytes)?,
        Stored::Chunked(list) => {
            let chunks = list.chunks.clone();
            let mut assembler = ChunkAssembler::new(list, file).map_err(fatal)?;
            for (id, len) in &chunks {
                let chunk = ws
                    .objects
                    .get(&ObjKey::Chunk(*id))
                    .await?
                    .ok_or_else(|| CliError::Fatal(format!("unable to read blob {file}")))?;
                if chunk.len() != *len as usize {
                    return Err(CliError::Fatal(format!("blob {file} is corrupt")));
                }
                assembler.push(&chunk).map_err(fatal)?;
                out.write_all(&chunk)?;
            }
            assembler.finish().map_err(fatal)?;
        }
    }
    Ok(())
}

/// The git id of `bytes` as an object of `kind`.
pub fn hash(kind: ObjectKind, bytes: &[u8]) -> GitOid {
    object_oid(kind, bytes)
}

/// A tree's entries in git's order: a directory sorts as its name followed by `/`.
pub fn git_order(tree: &Tree) -> Vec<&Entry> {
    let mut entries: Vec<&Entry> = tree.entries().iter().collect();
    entries.sort_by(|a, b| {
        let key = |e: &Entry| {
            let mut k = e.name.as_bytes().to_vec();
            if e.kind == Kind::Dir {
                k.push(b'/');
            }
            k
        };
        key(a).cmp(&key(b))
    });
    entries
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn peel_suffixes() {
        assert_eq!(split_peel("v1^{tree}"), ("v1", Some("tree")));
        assert_eq!(split_peel("v1^{}"), ("v1", Some("")));
        assert_eq!(split_peel("HEAD~2"), ("HEAD~2", None));
        assert_eq!(split_peel("a^{commit}"), ("a", Some("commit")));
    }
}
