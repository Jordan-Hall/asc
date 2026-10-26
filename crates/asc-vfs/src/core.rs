//! Shared provider logic (SPEC §10): the [`Provider`] trait the OS backends implement, and
//! [`CoreFs`], which resolves paths against the base tree, filters names, streams verified content
//! and applies the hydration policy. Every path and name that reaches this module is untrusted.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};
use std::time::SystemTime;

use asc_core::codec::{CodecError, decode_envelope};
use asc_core::content::{CHUNK_THRESHOLD, ChunkAssembler, ContentError};
use asc_core::id::{ChunkId, FileId, GitOid, TreeId};
use asc_core::object::{Change, ChunkList, Entry, Kind, Tree};
use asc_core::path::{Name, RepoPath, casefold, checkout_safe, windows_valid};
use asc_store::cachedir::CacheDir;
use asc_store::key::ObjKey;
use asc_store::objects::Objects;
use asc_wc::redirect::Redirections;
use asc_wc::switch::Plan;
use asc_wc::times::Switched;
use asc_wc::vstatus::{DirEntry, DiskKind, DiskStat, DiskState, PathState};
use bytes::Bytes;

use crate::policy::{Decision, HydrationPolicy};
use crate::stats::Stats;

/// Default budget of the decoded-tree cache (SPEC §16.4: tree cache at most 32 MiB).
pub const DEFAULT_TREE_CACHE_BYTES: u64 = 32 * 1024 * 1024;
/// Longest entry name projected where unrepresentable names are filtered (NTFS limit).
pub const MAX_PROJECTED_NAME: usize = 255;

/// Provider errors.
#[derive(Debug, thiserror::Error)]
pub enum VfsError {
    #[error("object store: {0}")]
    Store(#[from] asc_store::objects::Error),
    #[error("tree {0} is missing")]
    MissingTree(TreeId),
    #[error("object {0} is missing")]
    MissingObject(ObjKey),
    #[error("cannot decode {what}: {source}")]
    Decode {
        what: &'static str,
        source: CodecError,
    },
    #[error("content: {0}")]
    Content(#[from] ContentError),
    #[error("content is {got} bytes, the tree says {want}")]
    SizeMismatch { want: u64, got: u64 },
    #[error("hydration denied for `{0}`")]
    Denied(String),
    #[error("not a file: {0}")]
    NotAFile(RepoPath),
    #[error("not a directory: {0}")]
    NotADir(RepoPath),
    #[error("not found: {0}")]
    NotFound(RepoPath),
    #[error("invalid path")]
    BadPath,
    #[error("the switch plan has conflicts")]
    Conflicts,
    #[error("not mounted")]
    NotMounted,
    #[error("already mounted")]
    AlreadyMounted,
    #[error("content larger than the {0} byte limit")]
    TooLarge(u64),
    #[error("io: {0}")]
    Io(#[from] io::Error),
}

pub type Result<T, E = VfsError> = std::result::Result<T, E>;

/// Composes the tree of a gitlink (filled by U60). Returns `None` when the submodule is not
/// configured; the gitlink is then an empty directory.
pub trait Compose: Send + Sync {
    fn subtree(&self, path: &RepoPath, commit: &GitOid) -> Option<TreeId>;
}

/// What a [`CoreFs`] filters and where it reads.
pub struct Options {
    /// Lookups ignore case (Windows). Later case-colliding siblings are not projected.
    pub case_insensitive: bool,
    /// Hide names the OS cannot hold (SPEC §2.8): not `windows_valid`, over-long, or colliding.
    pub hide_unrepresentable: bool,
    /// Byte budget of the decoded-tree cache.
    pub tree_cache_bytes: u64,
    pub redirections: Redirections,
    pub policy: HydrationPolicy,
    /// The local content cache, used to tell backends where a file already is.
    pub local: Option<CacheDir>,
    pub compose: Option<Arc<dyn Compose>>,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            case_insensitive: false,
            hide_unrepresentable: false,
            tree_cache_bytes: DEFAULT_TREE_CACHE_BYTES,
            redirections: Redirections::default(),
            policy: HydrationPolicy::default(),
            local: None,
            compose: None,
        }
    }
}

impl Options {
    /// The Windows (ProjFS) configuration.
    pub fn windows() -> Self {
        Self {
            case_insensitive: true,
            hide_unrepresentable: true,
            ..Self::default()
        }
    }
}

/// A resolved tree entry. `path` has the case the tree spells.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Node {
    pub path: RepoPath,
    pub kind: Kind,
    /// A `FileId` or `TreeId`; zero for a gitlink without a composed tree.
    pub id: [u8; 32],
    pub size: u64,
    pub git: GitOid,
    /// Below (or at) a composed gitlink: writes fail with access denied.
    pub read_only: bool,
}

/// The identity of a file's bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ContentRef {
    pub id: FileId,
    pub size: u64,
}

impl Node {
    pub fn is_dir(&self) -> bool {
        self.kind == Kind::Dir
    }

    pub fn tree_id(&self) -> Option<TreeId> {
        self.is_dir().then(|| TreeId::from_bytes(self.id))
    }

    /// The content of a file, exec file or symlink.
    pub fn content(&self) -> Option<ContentRef> {
        matches!(self.kind, Kind::File | Kind::Exec | Kind::Symlink).then(|| ContentRef {
            id: FileId::from_bytes(self.id),
            size: self.size,
        })
    }
}

/// A directory listing: projected children in byte order, and the names left out because the OS
/// cannot represent them (for `asc status`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DirListing {
    pub entries: Vec<Node>,
    pub unrepresentable: Vec<Name>,
}

/// A decoded directory with its projected view precomputed.
struct Dir {
    tree: Arc<Tree>,
    /// Indices into the tree of the projected entries, ascending.
    visible: Vec<u32>,
    unrepresentable: Vec<u32>,
    /// Case-folded name to index, for case-insensitive lookups.
    fold: HashMap<Vec<u8>, u32>,
}

#[derive(Clone)]
struct DirWeight;

impl quick_cache::Weighter<TreeId, Arc<Dir>> for DirWeight {
    fn weight(&self, _: &TreeId, d: &Arc<Dir>) -> u64 {
        let names: u64 = d
            .tree
            .entries()
            .iter()
            .map(|e| e.name.as_bytes().len() as u64)
            .sum();
        let n = d.tree.entries().len() as u64;
        256 + n * 112 + names * 2
    }
}

/// Path resolution and content access over one base tree.
pub struct CoreFs {
    objects: Arc<dyn Objects>,
    root: RwLock<TreeId>,
    opts: Options,
    dirs: quick_cache::sync::Cache<TreeId, Arc<Dir>, DirWeight>,
    stats: Arc<Stats>,
}

fn name_path(name: &[u8]) -> Option<RepoPath> {
    RepoPath::new(name.to_vec()).ok()
}

impl CoreFs {
    pub fn new(objects: Arc<dyn Objects>, root: TreeId, opts: Options) -> Self {
        let cap = opts.tree_cache_bytes.max(1);
        let items = usize::try_from(cap / 512)
            .unwrap_or(usize::MAX)
            .clamp(16, 1 << 20);
        Self {
            objects,
            root: RwLock::new(root),
            dirs: quick_cache::sync::Cache::with_weighter(items, cap, DirWeight),
            opts,
            stats: Arc::new(Stats::new()),
        }
    }

    pub fn stats(&self) -> &Arc<Stats> {
        &self.stats
    }

    pub fn policy(&self) -> &HydrationPolicy {
        &self.opts.policy
    }

    pub fn root(&self) -> TreeId {
        *self.root.read().unwrap_or_else(|e| e.into_inner())
    }

    /// Points the file system at another base tree (after a switch).
    pub fn set_root(&self, id: TreeId) {
        *self.root.write().unwrap_or_else(|e| e.into_inner()) = id;
    }

    async fn load_dir(&self, id: TreeId) -> Result<Arc<Dir>> {
        if let Some(d) = self.dirs.get(&id) {
            return Ok(d);
        }
        let tree = match self.objects.get(&ObjKey::Tree(id)).await? {
            Some(data) => decode_envelope::<Tree>(&data).map_err(|source| VfsError::Decode {
                what: "tree",
                source,
            })?,
            None => {
                let empty = Tree::new(Vec::new(), true).map_err(|_| VfsError::MissingTree(id))?;
                if empty.id() != id {
                    return Err(VfsError::MissingTree(id));
                }
                empty
            }
        };
        let dir = Arc::new(self.project(tree));
        self.dirs.insert(id, dir.clone());
        Ok(dir)
    }

    /// Applies the name filters of SPEC §2.8 and §10.5 to one directory.
    fn project(&self, tree: Tree) -> Dir {
        let o = &self.opts;
        let mut visible = Vec::new();
        let mut unrep = Vec::new();
        let mut fold: HashMap<Vec<u8>, u32> = HashMap::new();
        let folding = o.case_insensitive || o.hide_unrepresentable;
        for (i, e) in tree.entries().iter().enumerate() {
            let n = e.name.as_bytes();
            let i = i as u32;
            if o.redirections.is_redirected_name(n) {
                continue;
            }
            if !name_path(n).is_some_and(|p| checkout_safe(&p)) {
                continue;
            }
            if o.hide_unrepresentable && (n.len() > MAX_PROJECTED_NAME || !windows_valid(&e.name)) {
                unrep.push(i);
                continue;
            }
            if folding {
                let f = casefold(n);
                if fold.contains_key(&f) {
                    // The earlier sibling in byte order owns the folded name.
                    unrep.push(i);
                    continue;
                }
                fold.insert(f, i);
            }
            visible.push(i);
        }
        if !o.case_insensitive {
            fold.clear();
        }
        Dir {
            tree: Arc::new(tree),
            visible,
            unrepresentable: unrep,
            fold,
        }
    }

    fn lookup(&self, dir: &Dir, comp: &[u8]) -> Option<usize> {
        if self.opts.case_insensitive {
            return dir.fold.get(&casefold(comp)).map(|&i| i as usize);
        }
        let entries = dir.tree.entries();
        let idx = entries
            .binary_search_by(|e| e.name.as_bytes().cmp(comp))
            .ok()?;
        dir.visible.binary_search(&(idx as u32)).ok().map(|_| idx)
    }

    fn compose(&self, path: &RepoPath, e: &Entry) -> Option<TreeId> {
        self.opts.compose.as_ref()?.subtree(path, &e.git)
    }

    fn node(&self, path: RepoPath, e: &Entry, inherited_ro: bool) -> Node {
        if e.kind == Kind::Gitlink
            && let Some(t) = self.compose(&path, e)
        {
            return Node {
                path,
                kind: Kind::Dir,
                id: *t.as_bytes(),
                size: 0,
                git: e.git,
                read_only: true,
            };
        }
        Node {
            path,
            kind: e.kind,
            id: e.id,
            size: e.size,
            git: e.git,
            read_only: inherited_ro,
        }
    }

    /// Resolves `path` against the base tree. `None` when it is absent, filtered, or unsafe.
    pub async fn resolve(&self, path: &RepoPath) -> Result<Option<Node>> {
        if !checkout_safe(path) || self.opts.redirections.covers(path) {
            return Ok(None);
        }
        let total = path.components().count();
        let mut dir = self.load_dir(self.root()).await?;
        let mut read_only = false;
        let mut canon: Vec<u8> = Vec::with_capacity(path.as_bytes().len());
        for (i, comp) in path.components().enumerate() {
            let Some(idx) = self.lookup(&dir, comp) else {
                return Ok(None);
            };
            let e = dir.tree.entries()[idx].clone();
            if i > 0 {
                canon.push(b'/');
            }
            canon.extend_from_slice(e.name.as_bytes());
            let here = RepoPath::new(canon.clone()).map_err(|_| VfsError::BadPath)?;
            let node = self.node(here, &e, read_only);
            if i + 1 == total {
                return Ok(Some(node));
            }
            read_only = node.read_only;
            match node.tree_id() {
                Some(t) => dir = self.load_dir(t).await?,
                None => return Ok(None),
            }
        }
        Ok(None)
    }

    /// Lists a directory (`None` is the root). `Ok(None)` when it is absent.
    pub async fn read_dir(&self, dir: Option<&RepoPath>) -> Result<Option<DirListing>> {
        let (d, base, ro) = match dir {
            None => (self.load_dir(self.root()).await?, None, false),
            Some(p) => {
                let Some(n) = self.resolve(p).await? else {
                    return Ok(None);
                };
                match n.kind {
                    Kind::Dir => {
                        let t = n.tree_id().ok_or(VfsError::BadPath)?;
                        (self.load_dir(t).await?, Some(n.path), n.read_only)
                    }
                    Kind::Gitlink => return Ok(Some(DirListing::default())),
                    _ => return Err(VfsError::NotADir(p.clone())),
                }
            }
        };
        let entries_of = d.tree.entries();
        let join = |name: &Name| match &base {
            Some(b) => b.join(name),
            None => RepoPath::new(name.as_bytes().to_vec()).expect("a tree name is a path"),
        };
        let mut out = DirListing::default();
        for &i in &d.visible {
            let e = &entries_of[i as usize];
            let full = join(&e.name);
            if !checkout_safe(&full) {
                continue;
            }
            out.entries.push(self.node(full, e, ro));
        }
        out.unrepresentable = d
            .unrepresentable
            .iter()
            .map(|&i| entries_of[i as usize].name.clone())
            .collect();
        Ok(Some(out))
    }

    /// Opens the verified content of `c`. No policy and no counters: the caller is the daemon.
    pub async fn read_content(&self, c: ContentRef) -> Result<ContentStream> {
        let local = self.opts.local.as_ref().and_then(|l| l.content_path(c.id));
        let kind = if c.size == 0 {
            if c.id != FileId::of(b"") {
                return Err(ContentError::FileMismatch.into());
            }
            StreamKind::Done
        } else if c.size <= CHUNK_THRESHOLD {
            StreamKind::Inline { id: c.id }
        } else {
            let data = self
                .objects
                .get(&ObjKey::ChunkList(c.id))
                .await?
                .ok_or(VfsError::MissingObject(ObjKey::ChunkList(c.id)))?;
            let list = decode_envelope::<ChunkList>(&data).map_err(|source| VfsError::Decode {
                what: "chunk list",
                source,
            })?;
            if list.size != c.size {
                return Err(VfsError::SizeMismatch {
                    want: c.size,
                    got: list.size,
                });
            }
            let chunks = list.chunks.clone();
            StreamKind::Chunked {
                asm: Box::new(Some(ChunkAssembler::new(list, c.id)?)),
                chunks,
                next: 0,
            }
        };
        Ok(ContentStream {
            objects: self.objects.clone(),
            size: c.size,
            sent: 0,
            local,
            kind,
        })
    }

    /// A read that hydrates `path` for the process `image`: refused for denied images, counted
    /// otherwise.
    pub async fn open_for_hydration(&self, path: &RepoPath, image: &str) -> Result<ContentStream> {
        if self.opts.policy.check(image) == Decision::Deny {
            self.stats.record_denied(image);
            return Err(VfsError::Denied(crate::policy::image_name(image)));
        }
        let node = self
            .resolve(path)
            .await?
            .ok_or_else(|| VfsError::NotFound(path.clone()))?;
        let c = node
            .content()
            .ok_or(VfsError::NotAFile(node.path.clone()))?;
        let stream = self.read_content(c).await?;
        self.stats
            .record_hydration(&node.path, image, c.size, SystemTime::now());
        Ok(stream)
    }
}

enum StreamKind {
    Done,
    Inline {
        id: FileId,
    },
    Chunked {
        asm: Box<Option<ChunkAssembler>>,
        chunks: Vec<(ChunkId, u32)>,
        next: usize,
    },
}

/// Content in pieces of at most 4 MiB, each verified against its id. A whole chunked file is
/// verified when the stream ends: a consumer treats the content as good only after `next`
/// returned `None`.
pub struct ContentStream {
    objects: Arc<dyn Objects>,
    size: u64,
    sent: u64,
    local: Option<PathBuf>,
    kind: StreamKind,
}

impl ContentStream {
    pub fn size(&self) -> u64 {
        self.size
    }

    /// The file in the local content cache, when it is there: backends may reflink or copy it.
    /// Its bytes are not verified by this stream.
    pub fn local_path(&self) -> Option<&Path> {
        self.local.as_deref()
    }

    pub async fn next(&mut self) -> Result<Option<Bytes>> {
        match &mut self.kind {
            StreamKind::Done => Ok(None),
            StreamKind::Inline { id } => {
                let key = ObjKey::File(*id);
                let data = self
                    .objects
                    .get(&key)
                    .await?
                    .ok_or(VfsError::MissingObject(key))?;
                self.kind = StreamKind::Done;
                if data.len() as u64 != self.size {
                    return Err(VfsError::SizeMismatch {
                        want: self.size,
                        got: data.len() as u64,
                    });
                }
                self.sent = self.size;
                Ok(Some(data))
            }
            StreamKind::Chunked { asm, chunks, next } => {
                let Some(&(cid, _)) = chunks.get(*next) else {
                    let done = (**asm).take().ok_or(ContentError::ExtraChunk)?;
                    done.finish()?;
                    self.kind = StreamKind::Done;
                    return Ok(None);
                };
                let key = ObjKey::Chunk(cid);
                let data = self
                    .objects
                    .get(&key)
                    .await?
                    .ok_or(VfsError::MissingObject(key))?;
                (**asm)
                    .as_mut()
                    .ok_or(ContentError::ExtraChunk)?
                    .push(&data)?;
                *next += 1;
                self.sent += data.len() as u64;
                Ok(Some(data))
            }
        }
    }

    /// Reads everything, refusing more than `max` bytes.
    pub async fn read_all(mut self, max: u64) -> Result<Vec<u8>> {
        if self.size > max {
            return Err(VfsError::TooLarge(max));
        }
        let mut out = Vec::with_capacity(usize::try_from(self.size).unwrap_or(0));
        while let Some(b) = self.next().await? {
            out.extend_from_slice(&b);
        }
        Ok(out)
    }
}

/// The paths a switch could not finish because they were held open.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SwitchOutcome {
    pub pending: Vec<RepoPath>,
}

/// The result of a dehydration request.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DehydrateOutcome {
    /// Paths now back to empty placeholders.
    pub dehydrated: usize,
    /// Paths left alone (modified, missing, or in use).
    pub skipped: Vec<RepoPath>,
}

/// What an OS backend (ProjFS, FUSE) provides.
pub trait Provider: Send + Sync {
    /// Starts serving the work tree at `root`.
    fn mount(&self, root: &Path) -> Result<()>;
    fn unmount(&self) -> Result<()>;
    /// Rewrites the paths of `plan` to the new version, stamping changed ones with `times.time`.
    /// A path held open stays pending. Refuses a plan with conflicts.
    fn apply_switch(&self, plan: &Plan, times: Switched) -> Result<SwitchOutcome>;
    /// Returns clean hydrated placeholders to empty ones.
    fn dehydrate(&self, paths: &[RepoPath]) -> Result<DehydrateOutcome>;
    /// The on-disk view, for virtual status and reconcile.
    fn disk_state(&self) -> &dyn DiskState;
}

#[derive(Clone, Debug)]
struct MemEntry {
    state: PathState,
    kind: DiskKind,
    data: Vec<u8>,
    time: i64,
}

#[derive(Default)]
struct MemInner {
    root: Option<PathBuf>,
    entries: BTreeMap<RepoPath, MemEntry>,
    held: BTreeSet<RepoPath>,
}

/// An in-memory [`Provider`] for tests of the layers above the OS.
#[derive(Default)]
pub struct MemProvider {
    inner: Mutex<MemInner>,
}

impl MemProvider {
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, MemInner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn mounted_root(&self) -> Option<PathBuf> {
        self.lock().root.clone()
    }

    /// Marks a path as held open by a process.
    pub fn hold(&self, path: &RepoPath) {
        self.lock().held.insert(path.clone());
    }

    pub fn release(&self, path: &RepoPath) {
        self.lock().held.remove(path);
    }

    /// Simulates a read that hydrated a placeholder.
    pub fn hydrate(&self, path: &RepoPath, data: &[u8]) {
        if let Some(e) = self.lock().entries.get_mut(path)
            && e.state == PathState::Placeholder
        {
            e.state = PathState::Hydrated;
            e.data = data.to_vec();
        }
    }

    /// Simulates a user write.
    pub fn write(&self, path: &RepoPath, data: &[u8]) {
        self.lock().entries.insert(
            path.clone(),
            MemEntry {
                state: PathState::Full,
                kind: DiskKind::File { exec: Some(false) },
                data: data.to_vec(),
                time: 0,
            },
        );
    }

    pub fn time_of(&self, path: &RepoPath) -> Option<i64> {
        self.lock().entries.get(path).map(|e| e.time)
    }

    pub fn state_of(&self, path: &RepoPath) -> PathState {
        self.lock()
            .entries
            .get(path)
            .map_or(PathState::Missing, |e| e.state)
    }
}

impl Provider for MemProvider {
    fn mount(&self, root: &Path) -> Result<()> {
        let mut i = self.lock();
        if i.root.is_some() {
            return Err(VfsError::AlreadyMounted);
        }
        i.root = Some(root.to_path_buf());
        Ok(())
    }

    fn unmount(&self) -> Result<()> {
        self.lock()
            .root
            .take()
            .map(|_| ())
            .ok_or(VfsError::NotMounted)
    }

    fn apply_switch(&self, plan: &Plan, times: Switched) -> Result<SwitchOutcome> {
        let mut i = self.lock();
        if i.root.is_none() {
            return Err(VfsError::NotMounted);
        }
        if !plan.conflicts.is_empty() {
            return Err(VfsError::Conflicts);
        }
        let mut out = SwitchOutcome::default();
        for (path, change) in &plan.updates {
            if i.held.contains(path) {
                out.pending.push(path.clone());
                continue;
            }
            let kind = match change {
                Change::Write {
                    kind: Kind::Symlink,
                    ..
                } => DiskKind::Symlink,
                Change::Write { kind, .. } => DiskKind::File {
                    exec: Some(*kind == Kind::Exec),
                },
                _ => DiskKind::Dir,
            };
            i.entries.insert(
                path.clone(),
                MemEntry {
                    state: PathState::Placeholder,
                    kind,
                    data: Vec::new(),
                    time: times.time,
                },
            );
        }
        for path in &plan.deletes {
            if i.held.contains(path) {
                out.pending.push(path.clone());
            } else {
                i.entries.remove(path);
            }
        }
        Ok(out)
    }

    fn dehydrate(&self, paths: &[RepoPath]) -> Result<DehydrateOutcome> {
        let mut i = self.lock();
        let mut out = DehydrateOutcome::default();
        for p in paths {
            let held = i.held.contains(p);
            match i.entries.get_mut(p) {
                Some(e) if !held && e.state == PathState::Hydrated => {
                    e.state = PathState::Placeholder;
                    e.data.clear();
                    out.dehydrated += 1;
                }
                Some(e) if !held && e.state == PathState::Placeholder => out.dehydrated += 1,
                _ => out.skipped.push(p.clone()),
            }
        }
        Ok(out)
    }

    fn disk_state(&self) -> &dyn DiskState {
        self
    }
}

impl DiskState for MemProvider {
    fn state(&self, path: &RepoPath) -> io::Result<PathState> {
        let i = self.lock();
        if let Some(e) = i.entries.get(path) {
            return Ok(e.state);
        }
        let below = i
            .entries
            .keys()
            .any(|k| k != path && k.starts_with_dir(path));
        Ok(if below {
            PathState::Placeholder
        } else {
            PathState::Missing
        })
    }

    fn stat(&self, path: &RepoPath) -> io::Result<Option<DiskStat>> {
        let i = self.lock();
        if let Some(e) = i.entries.get(path) {
            return Ok(Some(DiskStat {
                kind: e.kind,
                size: e.data.len() as u64,
            }));
        }
        let below = i
            .entries
            .keys()
            .any(|k| k != path && k.starts_with_dir(path));
        Ok(below.then_some(DiskStat {
            kind: DiskKind::Dir,
            size: 0,
        }))
    }

    fn read(&self, path: &RepoPath, max: u64) -> io::Result<Vec<u8>> {
        let i = self.lock();
        let e = i
            .entries
            .get(path)
            .ok_or_else(|| io::Error::from(io::ErrorKind::NotFound))?;
        if e.data.len() as u64 > max {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "file too large"));
        }
        Ok(e.data.clone())
    }

    fn list(&self, dir: &RepoPath) -> io::Result<Vec<DirEntry>> {
        let i = self.lock();
        let depth = dir.components().count();
        let mut out: BTreeMap<Name, bool> = BTreeMap::new();
        for (k, e) in &i.entries {
            if !k.starts_with_dir(dir) || k == dir {
                continue;
            }
            let comps: Vec<&[u8]> = k.components().collect();
            let Some(first) = comps.get(depth) else {
                continue;
            };
            let Ok(name) = Name::new(first.to_vec()) else {
                continue;
            };
            let is_dir = comps.len() > depth + 1 || e.kind == DiskKind::Dir;
            *out.entry(name).or_insert(false) |= is_dir;
        }
        Ok(out
            .into_iter()
            .map(|(name, dir)| DirEntry { name, dir })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use asc_core::codec::Enveloped;
    use asc_core::content::{Body, ingest};
    use asc_store::bucket::BucketObjects;
    use asc_store::codec::encode_stored;

    use super::*;

    const ZERO_GIT: GitOid = GitOid::from_bytes([0; 20]);

    struct Fx {
        store: Arc<BucketObjects>,
    }

    fn rp(s: &str) -> RepoPath {
        RepoPath::new(s).unwrap()
    }

    fn entry(name: &str, kind: Kind, id: [u8; 32], size: u64) -> Entry {
        Entry {
            name: Name::new(name).unwrap(),
            kind,
            id,
            size,
            git: ZERO_GIT,
        }
    }

    impl Fx {
        fn new() -> Self {
            Self {
                store: Arc::new(BucketObjects::memory()),
            }
        }

        async fn put(&self, key: ObjKey, data: &[u8]) {
            self.store.put(&key, encode_stored(data)).await.unwrap();
        }

        async fn file(&self, name: &str, data: &[u8]) -> Entry {
            let id = FileId::of(data);
            self.put(ObjKey::File(id), data).await;
            entry(name, Kind::File, *id.as_bytes(), data.len() as u64)
        }

        async fn tree(&self, mut entries: Vec<Entry>) -> TreeId {
            entries.sort_by(|a, b| a.name.cmp(&b.name));
            let t = Tree::new(entries, true).unwrap();
            self.put(ObjKey::Tree(t.id()), &t.to_envelope()).await;
            t.id()
        }

        fn fs(&self, root: TreeId, opts: Options) -> CoreFs {
            CoreFs::new(self.store.clone(), root, opts)
        }

        async fn dir_entry(&self, name: &str, children: Vec<Entry>) -> Entry {
            let id = self.tree(children).await;
            entry(name, Kind::Dir, *id.as_bytes(), 0)
        }
    }

    fn names(l: &DirListing) -> Vec<String> {
        l.entries.iter().map(|n| n.path.to_string()).collect()
    }

    #[tokio::test]
    async fn lookup_resolves_nested_paths() {
        let fx = Fx::new();
        let main = fx.file("main.rs", b"fn main() {}").await;
        let src = fx.dir_entry("src", vec![main]).await;
        let readme = fx.file("README", b"hi").await;
        let root = fx.tree(vec![src, readme]).await;
        let fs = fx.fs(root, Options::default());

        let n = fs.resolve(&rp("src/main.rs")).await.unwrap().unwrap();
        assert_eq!(n.kind, Kind::File);
        assert_eq!(n.size, 12);
        assert!(fs.resolve(&rp("src")).await.unwrap().unwrap().is_dir());
        assert!(fs.resolve(&rp("src/nope")).await.unwrap().is_none());
        assert!(fs.resolve(&rp("README/x")).await.unwrap().is_none());
        assert!(fs.resolve(&rp("readme")).await.unwrap().is_none());

        let l = fs.read_dir(None).await.unwrap().unwrap();
        assert_eq!(names(&l), ["README", "src"]);
        let l = fs.read_dir(Some(&rp("src"))).await.unwrap().unwrap();
        assert_eq!(names(&l), ["src/main.rs"]);
        assert!(fs.read_dir(Some(&rp("gone"))).await.unwrap().is_none());
        assert!(matches!(
            fs.read_dir(Some(&rp("README"))).await,
            Err(VfsError::NotADir(_))
        ));
    }

    #[tokio::test]
    async fn missing_tree_is_an_error_and_empty_root_works() {
        let fx = Fx::new();
        let empty = Tree::new(Vec::new(), true).unwrap().id();
        let fs = fx.fs(empty, Options::default());
        assert!(fs.read_dir(None).await.unwrap().unwrap().entries.is_empty());
        let fs = fx.fs(TreeId::from_bytes([9; 32]), Options::default());
        assert!(matches!(
            fs.resolve(&rp("a")).await,
            Err(VfsError::MissingTree(_))
        ));
    }

    #[tokio::test]
    async fn case_insensitive_lookup_returns_tree_spelling() {
        let fx = Fx::new();
        let f = fx.file("Makefile", b"all:").await;
        let src = fx.dir_entry("Src", vec![f]).await;
        let root = fx.tree(vec![src]).await;
        let fs = fx.fs(
            root,
            Options {
                case_insensitive: true,
                ..Options::default()
            },
        );
        let n = fs.resolve(&rp("sRC/makeFILE")).await.unwrap().unwrap();
        assert_eq!(n.path, rp("Src/Makefile"));
        let strict = fx.fs(root, Options::default());
        assert!(strict.resolve(&rp("sRC/makeFILE")).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn unrepresentable_names_are_filtered_and_listed() {
        let fx = Fx::new();
        let a = fx.file("README", b"a").await;
        let b = fx.file("readme", b"b").await;
        let c = fx.file("bad.", b"c").await;
        let d = fx.file("a:b", b"d").await;
        let e = fx.file("ok.txt", b"e").await;
        let root = fx.tree(vec![a, b, c, d, e]).await;
        let fs = fx.fs(root, Options::windows());

        let l = fs.read_dir(None).await.unwrap().unwrap();
        assert_eq!(names(&l), ["README", "ok.txt"]);
        let mut hidden: Vec<_> = l
            .unrepresentable
            .iter()
            .map(|n| String::from_utf8_lossy(n.as_bytes()).into_owned())
            .collect();
        hidden.sort();
        assert_eq!(hidden, ["a:b", "bad.", "readme"]);
        assert!(fs.resolve(&rp("bad.")).await.unwrap().is_none());
        // The case-colliding later sibling does not shadow the earlier one.
        assert_eq!(
            fs.resolve(&rp("readme")).await.unwrap().unwrap().path,
            rp("README")
        );
        // A POSIX configuration projects everything.
        let posix = fx.fs(root, Options::default());
        assert_eq!(
            posix.read_dir(None).await.unwrap().unwrap().entries.len(),
            5
        );
    }

    #[tokio::test]
    async fn unsafe_names_are_never_projected() {
        let fx = Fx::new();
        let a = fx.file("git~1", b"x").await;
        let b = fx.file("fine", b"y").await;
        let asc = fx.file(".asc", b"z").await;
        let nested = fx
            .dir_entry(
                "sub",
                vec![fx.file("git~1", b"w").await, fx.file("ok", b"v").await],
            )
            .await;
        let root = fx.tree(vec![a, b, asc, nested]).await;
        let fs = fx.fs(root, Options::default());
        let l = fs.read_dir(None).await.unwrap().unwrap();
        assert_eq!(names(&l), ["fine", "sub"]);
        assert!(fs.resolve(&rp("git~1")).await.unwrap().is_none());
        assert!(fs.resolve(&rp(".asc")).await.unwrap().is_none());
        assert!(fs.resolve(&rp("sub/git~1")).await.unwrap().is_none());
        let l = fs.read_dir(Some(&rp("sub"))).await.unwrap().unwrap();
        assert_eq!(names(&l), ["sub/ok"]);
    }

    #[tokio::test]
    async fn redirected_names_are_filtered_at_every_level() {
        let fx = Fx::new();
        let nm = fx
            .dir_entry("node_modules", vec![fx.file("x.js", b"1").await])
            .await;
        let inner = fx
            .dir_entry("node_modules", vec![fx.file("y.js", b"2").await])
            .await;
        let pkg = fx
            .dir_entry("pkg", vec![inner, fx.file("a.js", b"3").await])
            .await;
        let root = fx.tree(vec![nm, pkg]).await;
        let fs = fx.fs(
            root,
            Options {
                redirections: Redirections::parse("node_modules\n"),
                ..Options::default()
            },
        );
        assert_eq!(names(&fs.read_dir(None).await.unwrap().unwrap()), ["pkg"]);
        assert_eq!(
            names(&fs.read_dir(Some(&rp("pkg"))).await.unwrap().unwrap()),
            ["pkg/a.js"]
        );
        assert!(fs.resolve(&rp("node_modules")).await.unwrap().is_none());
        assert!(
            fs.resolve(&rp("pkg/node_modules/y.js"))
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn content_streams_inline_and_chunked_with_verification() {
        let fx = Fx::new();
        let small = fx.file("s", b"hello").await;
        let empty = entry("e", Kind::File, *FileId::of(b"").as_bytes(), 0);

        let mut big = Vec::with_capacity(5 << 20);
        let mut x = 0x9E37_79B9_7F4A_7C15u64;
        while big.len() < (5 << 20) {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            big.extend_from_slice(&x.to_le_bytes());
        }
        let ing = ingest(Cursor::new(&big)).unwrap();
        let Body::Chunked { list, chunks } = ing.body else {
            panic!("expected a chunked body");
        };
        fx.put(ObjKey::ChunkList(ing.file), &list.to_envelope())
            .await;
        for (id, data) in &chunks {
            fx.put(ObjKey::Chunk(*id), data).await;
        }
        let bigentry = entry("big", Kind::File, *ing.file.as_bytes(), ing.size);
        // A lying size must fail before any chunk is read.
        let liar = entry("liar", Kind::File, *ing.file.as_bytes(), ing.size - 1);
        let root = fx.tree(vec![small, empty, bigentry, liar]).await;
        let fs = fx.fs(root, Options::default());

        let n = fs.resolve(&rp("s")).await.unwrap().unwrap();
        let got = fs
            .read_content(n.content().unwrap())
            .await
            .unwrap()
            .read_all(10)
            .await
            .unwrap();
        assert_eq!(got, b"hello");
        let n = fs.resolve(&rp("e")).await.unwrap().unwrap();
        let got = fs
            .read_content(n.content().unwrap())
            .await
            .unwrap()
            .read_all(10)
            .await
            .unwrap();
        assert!(got.is_empty());

        let n = fs.resolve(&rp("big")).await.unwrap().unwrap();
        let mut s = fs.read_content(n.content().unwrap()).await.unwrap();
        let mut total = Vec::new();
        let mut pieces = 0;
        while let Some(b) = s.next().await.unwrap() {
            assert!(b.len() <= 4 << 20);
            total.extend_from_slice(&b);
            pieces += 1;
        }
        assert!(pieces >= 2);
        assert_eq!(total, big);
        assert!(s.next().await.unwrap().is_none());

        let n = fs.resolve(&rp("big")).await.unwrap().unwrap();
        assert!(matches!(
            fs.read_content(n.content().unwrap())
                .await
                .unwrap()
                .read_all(1024)
                .await,
            Err(VfsError::TooLarge(1024))
        ));
        let n = fs.resolve(&rp("liar")).await.unwrap().unwrap();
        assert!(matches!(
            fs.read_content(n.content().unwrap()).await,
            Err(VfsError::SizeMismatch { .. })
        ));
    }

    #[tokio::test]
    async fn content_missing_or_wrong_size_is_an_error() {
        let fx = Fx::new();
        let ghost = entry("ghost", Kind::File, [7; 32], 4);
        let wrong = {
            let id = FileId::of(b"abcd");
            fx.put(ObjKey::File(id), b"abcd").await;
            entry("wrong", Kind::File, *id.as_bytes(), 3)
        };
        let root = fx.tree(vec![ghost, wrong]).await;
        let fs = fx.fs(root, Options::default());
        for p in ["ghost", "wrong"] {
            let n = fs.resolve(&rp(p)).await.unwrap().unwrap();
            let mut s = fs.read_content(n.content().unwrap()).await.unwrap();
            assert!(s.next().await.is_err(), "{p}");
        }
    }

    #[tokio::test]
    async fn deny_list_refuses_and_counts_hydrations() {
        let fx = Fx::new();
        let f = fx.file("a.txt", b"data").await;
        let root = fx.tree(vec![f]).await;
        let fs = fx.fs(root, Options::default());
        let p = rp("a.txt");

        let err = fs
            .open_for_hydration(&p, r"C:\Windows\System32\SearchProtocolHost.exe")
            .await;
        assert!(matches!(err, Err(VfsError::Denied(_))));
        assert!(fs.stats().path(&p).is_none());
        assert_eq!(
            fs.stats()
                .process_stats("searchprotocolhost.exe")
                .unwrap()
                .denied,
            1
        );

        let s = fs.open_for_hydration(&p, r"C:\tools\rg.exe").await.unwrap();
        assert_eq!(s.read_all(100).await.unwrap(), b"data");
        let st = fs.stats().path(&p).unwrap();
        assert_eq!((st.hydrations, st.bytes), (1, 4));
        assert!(fs.stats().last_read("RG.EXE").is_some());

        assert!(matches!(
            fs.open_for_hydration(&rp("nope"), "rg.exe").await,
            Err(VfsError::NotFound(_))
        ));
        let open = fx.fs(
            root,
            Options {
                policy: HydrationPolicy::allow_all(),
                ..Options::default()
            },
        );
        assert!(
            open.open_for_hydration(&p, "searchindexer.exe")
                .await
                .is_ok()
        );
    }

    struct Fixed(TreeId);

    impl Compose for Fixed {
        fn subtree(&self, path: &RepoPath, _: &GitOid) -> Option<TreeId> {
            (path.as_bytes() == b"sdk").then_some(self.0)
        }
    }

    #[tokio::test]
    async fn gitlinks_compose_read_only_or_stay_empty() {
        let fx = Fx::new();
        let lib = fx.file("lib.rs", b"//").await;
        let nested = fx.tree(vec![lib]).await;
        let link = |n: &str| Entry {
            name: Name::new(n).unwrap(),
            kind: Kind::Gitlink,
            id: [0; 32],
            size: 0,
            git: GitOid::from_bytes([5; 20]),
        };
        let root = fx.tree(vec![link("sdk"), link("other")]).await;

        let plain = fx.fs(root, Options::default());
        let n = plain.resolve(&rp("sdk")).await.unwrap().unwrap();
        assert_eq!(n.kind, Kind::Gitlink);
        assert!(
            plain
                .read_dir(Some(&rp("sdk")))
                .await
                .unwrap()
                .unwrap()
                .entries
                .is_empty()
        );

        let fs = fx.fs(
            root,
            Options {
                compose: Some(Arc::new(Fixed(nested))),
                ..Options::default()
            },
        );
        let n = fs.resolve(&rp("sdk/lib.rs")).await.unwrap().unwrap();
        assert!(n.read_only);
        assert_eq!(n.kind, Kind::File);
        assert!(fs.resolve(&rp("sdk")).await.unwrap().unwrap().read_only);
        assert_eq!(
            fs.resolve(&rp("other")).await.unwrap().unwrap().kind,
            Kind::Gitlink
        );
        assert!(!fs.read_dir(None).await.unwrap().unwrap().entries[0].read_only);
    }

    #[tokio::test]
    async fn tree_cache_stays_bounded_and_set_root_switches() {
        let fx = Fx::new();
        let a = fx.file("a", b"1").await;
        let b = fx.file("b", b"2").await;
        let r1 = fx.tree(vec![a]).await;
        let r2 = fx.tree(vec![b]).await;
        let fs = fx.fs(
            r1,
            Options {
                tree_cache_bytes: 1,
                ..Options::default()
            },
        );
        assert!(fs.resolve(&rp("a")).await.unwrap().is_some());
        fs.set_root(r2);
        assert_eq!(fs.root(), r2);
        assert!(fs.resolve(&rp("a")).await.unwrap().is_none());
        assert!(fs.resolve(&rp("b")).await.unwrap().is_some());
    }

    #[test]
    fn mem_provider_switch_holds_and_dehydrates() {
        use asc_core::id::FileId;
        let p = MemProvider::new();
        let times = Switched {
            epoch: 2,
            time: 500,
        };
        let plan = Plan::default();
        assert!(matches!(
            p.apply_switch(&plan, times),
            Err(VfsError::NotMounted)
        ));
        p.mount(Path::new("ws")).unwrap();
        assert!(matches!(
            p.mount(Path::new("ws")),
            Err(VfsError::AlreadyMounted)
        ));

        let write = |kind| Change::write(kind, FileId::of(b"x"), 1, ZERO_GIT, None).unwrap();
        let plan = Plan {
            updates: vec![
                (rp("a/one"), write(Kind::File)),
                (rp("a/two"), write(Kind::Exec)),
            ],
            ..Plan::default()
        };
        p.hold(&rp("a/two"));
        let out = p.apply_switch(&plan, times).unwrap();
        assert_eq!(out.pending, [rp("a/two")]);
        assert_eq!(p.time_of(&rp("a/one")), Some(500));
        assert_eq!(p.state_of(&rp("a/two")), PathState::Missing);
        let ds = p.disk_state();
        assert_eq!(ds.state(&rp("a")).unwrap(), PathState::Placeholder);
        let names: Vec<_> = ds
            .list(&rp("a"))
            .unwrap()
            .into_iter()
            .map(|e| e.name)
            .collect();
        assert_eq!(names, [Name::new("one").unwrap()]);

        p.hydrate(&rp("a/one"), b"x");
        assert_eq!(p.state_of(&rp("a/one")), PathState::Hydrated);
        p.write(&rp("w"), b"user");
        let out = p.dehydrate(&[rp("a/one"), rp("w"), rp("zzz")]).unwrap();
        assert_eq!(out.dehydrated, 1);
        assert_eq!(out.skipped, [rp("w"), rp("zzz")]);
        assert_eq!(p.state_of(&rp("a/one")), PathState::Placeholder);

        let del = Plan {
            deletes: vec![rp("a/one")],
            conflicts: vec![],
            ..Plan::default()
        };
        p.apply_switch(&del, times).unwrap();
        assert_eq!(p.state_of(&rp("a/one")), PathState::Missing);
        let bad = Plan {
            conflicts: vec![rp("w")],
            ..Plan::default()
        };
        assert!(matches!(
            p.apply_switch(&bad, times),
            Err(VfsError::Conflicts)
        ));
        p.unmount().unwrap();
        assert!(matches!(p.unmount(), Err(VfsError::NotMounted)));
    }
}
