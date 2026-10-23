//! Scope algebra (SPEC §23.2.3): the set of doc ordinals a revision holds, without walking
//! the repository.
//!
//! * [`Tdocs`] is the `tdocs.v1` value: a roaring bitmap of the ords of every File/Exec entry
//!   in a subtree plus `dups`, the count of each ord that sits at two or more paths. Counts
//!   are what make deletion exact: removing one of two paths that hold one `FileId` keeps it.
//! * [`ScopeEngine::tdocs`] derives it per `TreeId` (memoised for subtrees of at least
//!   [`STORE_MIN_FILES`] files, so a new revision costs `depth x changed paths` tree reads).
//! * [`ScopeEngine::scope`] adds the counted skip-list delta for a historical revision. The
//!   delta is the tree diff against the checkpoint, never a changeset's own `changes` list
//!   (pushed metadata that nothing proves equal to the trees).
//! * [`ScopeEngine::xdocs`] derives the bitmap of ords excluded at every path they have, per
//!   `(TreeId, rules digest)`, with `.ascattributes` first and `.gitattributes` the fallback.
//! * [`ScopeEngine::resolve`] applies path filters, `xdocs` and the stack-diff to a revision.
//! * [`ScopeEngine::locate`] maps an ord back to paths.
//!
//! Every input is untrusted: stored values are validated on decode, and each walk has a
//! visit, depth or size bound. The entry budget belongs to one public operation: every
//! subtree derivation, `locate` step and `xdocs` pass of that operation draws on it, and each
//! `TreeId` is derived once per operation.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use bytes::Bytes;
use asc_core::id::{ChangesetId, FileId, TreeId};
use asc_core::object::{Change, Kind};
use asc_core::path::{Name, RepoPath};
use asc_engine::Engine;
use asc_engine::derive::Skip;
use asc_engine::history::HistoryApi;
use asc_store::codec::encode_stored;
use asc_store::key::ObjKey;
use asc_store::objects::{self, Objects};
use asc_store::refs::Refs;
use roaring::RoaringBitmap;

use crate::config::CodeintelConfig;
use crate::doc::{DocError, OrdTable};

/// Subtrees with at least this many files get a stored `tdocs` (SPEC §23.2.3).
pub const STORE_MIN_FILES: u64 = 256;
/// Deepest directory nesting a walk follows.
pub const MAX_DEPTH: usize = 256;
/// Tree entries one derivation may visit.
pub const MAX_ENTRIES: usize = 16 << 20;
/// Changed paths a counted delta may hold before the tree derivation is used instead.
pub const MAX_DELTA_PATHS: usize = 1 << 16;
/// Paths of a stack-diff.
pub const MAX_STACK_PATHS: usize = 1 << 17;
/// Largest attribute file read.
pub const MAX_ATTR_FILE: u64 = 1 << 20;
/// Longest repository path a walk builds.
pub const MAX_PATH_BYTES: usize = 4096;
/// Largest stored derived value accepted on decode.
pub const MAX_VALUE_BYTES: usize = 64 << 20;
/// Tree entries `locate` may visit.
pub const MAX_LOCATE_VISITS: usize = 1 << 20;
/// Ancestor distances probed for a derived `tdocs` (the `skip.v1` levels).
pub const PROBE_DISTANCES: [usize; 4] = [1, 4, 16, 64];

const TDOCS: &str = "tdocs";
const XDOCS: &str = "xdocs";
const VERSION: u32 = 1;

#[derive(Debug, thiserror::Error)]
pub enum ScopeError {
    #[error("store: {0}")]
    Store(String),
    #[error("{0} exceeds its bound")]
    TooLarge(&'static str),
    #[error("stored {0} value does not decode")]
    BadValue(&'static str),
    #[error("doc ordinals: {0}")]
    Ord(#[from] DocError),
    #[error("count algebra is inconsistent")]
    Inconsistent,
    #[error("no merge base")]
    NoMergeBase,
    #[error("attributes: {0}")]
    Attributes(String),
}

pub type Result<T, E = ScopeError> = std::result::Result<T, E>;

fn st<E: std::fmt::Display>(e: E) -> ScopeError {
    ScopeError::Store(e.to_string())
}

/// The `FileId` to ord mapping of one namespace. `ord_for` assigns an ord to a new
/// `FileId` at once; an implementor that persists ords also enqueues the file job.
pub trait OrdSource: Send + Sync {
    fn ord_for(&self, id: FileId) -> Result<u32, DocError>;
    fn get(&self, id: &FileId) -> Option<u32>;
}

impl OrdSource for Mutex<OrdTable> {
    fn ord_for(&self, id: FileId) -> Result<u32, DocError> {
        let mut t = self.lock().unwrap_or_else(|p| p.into_inner());
        t.ord_for(id).map(|(o, _)| o)
    }

    fn get(&self, id: &FileId) -> Option<u32> {
        self.lock().unwrap_or_else(|p| p.into_inner()).get(id)
    }
}

// ------------------------------------------------------------------------------ Tdocs

/// A counted set of ords: the bitmap, and the count of every ord held at two or more paths
/// (an ord that is not listed has count 1).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Tdocs {
    bitmap: RoaringBitmap,
    dups: BTreeMap<u32, u32>,
}

impl Tdocs {
    pub fn bitmap(&self) -> &RoaringBitmap {
        &self.bitmap
    }

    pub fn dups(&self) -> &BTreeMap<u32, u32> {
        &self.dups
    }

    /// Paths holding `ord`: 0 when absent.
    pub fn count(&self, ord: u32) -> u32 {
        if !self.bitmap.contains(ord) {
            0
        } else {
            self.dups.get(&ord).copied().unwrap_or(1)
        }
    }

    /// File entries counted: distinct ords plus the extra paths of duplicates.
    pub fn total_files(&self) -> u64 {
        self.bitmap.len() + self.dups.values().map(|&c| u64::from(c) - 1).sum::<u64>()
    }

    /// A set holding `ord` once.
    pub fn single(ord: u32) -> Tdocs {
        let mut t = Tdocs::default();
        t.bitmap.insert(ord);
        t
    }

    fn set_count(&mut self, ord: u32, count: u32) {
        match count {
            0 => {
                self.bitmap.remove(ord);
                self.dups.remove(&ord);
            }
            1 => {
                self.bitmap.insert(ord);
                self.dups.remove(&ord);
            }
            c => {
                self.bitmap.insert(ord);
                self.dups.insert(ord, c);
            }
        }
    }

    /// Adds one path holding `ord`.
    pub fn add_one(&mut self, ord: u32) -> Result<()> {
        let c = self
            .count(ord)
            .checked_add(1)
            .ok_or(ScopeError::TooLarge("path count"))?;
        self.set_count(ord, c);
        Ok(())
    }

    /// Adds every path of `other` (counts sum).
    pub fn add(&mut self, other: &Tdocs) -> Result<()> {
        if self.bitmap.is_empty() {
            *self = other.clone();
            return Ok(());
        }
        let overlap = &self.bitmap & &other.bitmap;
        let mut sums = Vec::with_capacity(overlap.len().min(1 << 16) as usize);
        for o in &overlap {
            let c = self
                .count(o)
                .checked_add(other.count(o))
                .ok_or(ScopeError::TooLarge("path count"))?;
            sums.push((o, c));
        }
        for (&o, &c) in &other.dups {
            if !overlap.contains(o) {
                self.dups.insert(o, c);
            }
        }
        self.bitmap |= &other.bitmap;
        for (o, c) in sums {
            self.set_count(o, c);
        }
        Ok(())
    }

    /// Removes every path of `other`. An ord leaves the bitmap only when its count reaches 0.
    pub fn sub(&mut self, other: &Tdocs) -> Result<()> {
        for o in &other.bitmap {
            let c = self
                .count(o)
                .checked_sub(other.count(o))
                .ok_or(ScopeError::Inconsistent)?;
            self.set_count(o, c);
        }
        Ok(())
    }

    /// The stored form: version, bitmap, dups.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(9 + self.bitmap.serialized_size() + 8 * self.dups.len());
        out.push(VERSION as u8);
        out.extend_from_slice(&(self.bitmap.serialized_size() as u32).to_le_bytes());
        self.bitmap
            .serialize_into(&mut out)
            .expect("writing to a Vec cannot fail");
        out.extend_from_slice(&(self.dups.len() as u32).to_le_bytes());
        for (&o, &c) in &self.dups {
            out.extend_from_slice(&o.to_le_bytes());
            out.extend_from_slice(&c.to_le_bytes());
        }
        out
    }

    /// Validates and decodes the stored form: bounded, dups strictly ascending, each count at
    /// least 2 and each ord in the bitmap.
    pub fn decode(b: &[u8]) -> Result<Tdocs> {
        let bad = || ScopeError::BadValue(TDOCS);
        if b.len() > MAX_VALUE_BYTES || b.first() != Some(&(VERSION as u8)) {
            return Err(bad());
        }
        let (bitmap, rest) = read_bitmap(&b[1..]).ok_or_else(bad)?;
        let (n, rest) = read_u32(rest).ok_or_else(bad)?;
        if u64::from(n) > bitmap.len() || rest.len() as u64 != u64::from(n) * 8 {
            return Err(bad());
        }
        let mut dups = BTreeMap::new();
        let mut prev: Option<u32> = None;
        for c in rest.chunks_exact(8) {
            let o = u32::from_le_bytes(c[..4].try_into().expect("4 bytes"));
            let n = u32::from_le_bytes(c[4..].try_into().expect("4 bytes"));
            if n < 2 || prev.is_some_and(|p| p >= o) || !bitmap.contains(o) {
                return Err(bad());
            }
            prev = Some(o);
            dups.insert(o, n);
        }
        Ok(Tdocs { bitmap, dups })
    }
}

fn read_u32(b: &[u8]) -> Option<(u32, &[u8])> {
    let (h, rest) = b.split_first_chunk::<4>()?;
    Some((u32::from_le_bytes(*h), rest))
}

/// A length-prefixed roaring bitmap.
fn read_bitmap(b: &[u8]) -> Option<(RoaringBitmap, &[u8])> {
    let (n, rest) = read_u32(b)?;
    let n = n as usize;
    if n > rest.len() {
        return None;
    }
    let bm = RoaringBitmap::deserialize_from(&rest[..n]).ok()?;
    Some((bm, &rest[n..]))
}

fn write_bitmap(out: &mut Vec<u8>, bm: &RoaringBitmap) {
    out.extend_from_slice(&(bm.serialized_size() as u32).to_le_bytes());
    bm.serialize_into(&mut *out)
        .expect("writing to a Vec cannot fail");
}

/// `xdocs` memo value: ords with at least one excluded path, and with at least one kept path.
#[derive(Clone, Debug, Default, PartialEq)]
struct Xdocs {
    excluded: RoaringBitmap,
    included: RoaringBitmap,
}

impl Xdocs {
    fn encode(&self) -> Vec<u8> {
        let mut out = vec![VERSION as u8];
        write_bitmap(&mut out, &self.excluded);
        write_bitmap(&mut out, &self.included);
        out
    }

    fn decode(b: &[u8]) -> Result<Xdocs> {
        let bad = || ScopeError::BadValue(XDOCS);
        if b.len() > MAX_VALUE_BYTES || b.first() != Some(&(VERSION as u8)) {
            return Err(bad());
        }
        let (excluded, rest) = read_bitmap(&b[1..]).ok_or_else(bad)?;
        let (included, rest) = read_bitmap(rest).ok_or_else(bad)?;
        if !rest.is_empty() {
            return Err(bad());
        }
        Ok(Xdocs { excluded, included })
    }
}

// ------------------------------------------------------------------------------ queries

/// A scope request (SPEC §23.2.3).
#[derive(Clone, Debug)]
pub struct ScopeQuery<'a> {
    pub rev: ChangesetId,
    /// Literal path prefixes; empty means the whole tree.
    pub include: Vec<RepoPath>,
    /// Literal path prefixes subtracted from the include set.
    pub exclude: Vec<RepoPath>,
    /// `--stack-diff`: the trunk head; the scope is cut to the ords of `paths(base..rev)`.
    pub stack_diff_trunk: Option<ChangesetId>,
    /// The `xdocs` rules to subtract; `None` is `include_excluded`.
    pub rules: Option<&'a CodeintelConfig>,
}

impl<'a> ScopeQuery<'a> {
    pub fn new(rev: ChangesetId) -> Self {
        ScopeQuery {
            rev,
            include: Vec::new(),
            exclude: Vec::new(),
            stack_diff_trunk: None,
            rules: None,
        }
    }
}

type BoxFut<'a, T> = Pin<Box<dyn Future<Output = Result<T>> + Send + 'a>>;

/// Scope derivation over one namespace: the engine, the object store the engine uses (for
/// memos) and the ord table.
pub struct ScopeEngine<O: Objects, R: Refs, S: OrdSource> {
    engine: Arc<Engine<O, R>>,
    objects: Arc<O>,
    ords: Arc<S>,
    /// Tree entries one operation may visit.
    max_entries: usize,
    /// Tree entries visited by every operation so far (observability).
    visited: AtomicUsize,
}

/// State of one public operation: the remaining entry budget and the subtrees already
/// derived, so a repeated `TreeId` costs nothing and no sub-call gets a fresh budget.
struct Walk {
    budget: usize,
    subtrees: HashMap<TreeId, Arc<Tdocs>>,
}

impl Walk {
    fn new(budget: usize) -> Walk {
        Walk {
            budget,
            subtrees: HashMap::new(),
        }
    }
}

impl<O: Objects + 'static, R: Refs + 'static, S: OrdSource + 'static> ScopeEngine<O, R, S> {
    /// `objects` must be the store `engine` was built on.
    pub fn new(engine: Arc<Engine<O, R>>, objects: Arc<O>, ords: Arc<S>) -> Self {
        ScopeEngine {
            engine,
            objects,
            ords,
            max_entries: MAX_ENTRIES,
            visited: AtomicUsize::new(0),
        }
    }

    /// Lowers the per-operation entry budget (default [`MAX_ENTRIES`]).
    pub fn with_max_entries(mut self, n: usize) -> Self {
        self.max_entries = n.min(MAX_ENTRIES);
        self
    }

    /// Tree entries visited by all operations on this engine so far.
    pub fn entries_visited(&self) -> usize {
        self.visited.load(Ordering::Relaxed)
    }

    /// Draws `n` entries from the operation's budget.
    fn charge(&self, walk: &mut Walk, n: usize, what: &'static str) -> Result<()> {
        walk.budget = walk
            .budget
            .checked_sub(n)
            .ok_or(ScopeError::TooLarge(what))?;
        self.visited.fetch_add(n, Ordering::Relaxed);
        Ok(())
    }

    // -------------------------------------------------------------------------- memos

    fn memo_key(name: &'static str, id: [u8; 32]) -> ObjKey {
        ObjKey::Derived {
            name,
            version: VERSION,
            id,
        }
    }

    /// A damaged memo counts as absent: it is recomputable.
    async fn memo_get(&self, key: &ObjKey) -> Result<Option<Bytes>> {
        match self.objects.get(key).await {
            Ok(v) => Ok(v),
            Err(objects::Error::Corrupt(_) | objects::Error::Codec(..)) => Ok(None),
            Err(e) => Err(st(e)),
        }
    }

    async fn memo_put(&self, key: &ObjKey, value: &[u8]) -> Result<()> {
        self.objects
            .put(key, encode_stored(value))
            .await
            .map_err(st)
    }

    /// The stored `tdocs` of `root`, if one exists and decodes.
    async fn stored_tdocs(&self, root: TreeId) -> Result<Option<Tdocs>> {
        let key = Self::memo_key(TDOCS, *root.as_bytes());
        Ok(match self.memo_get(&key).await? {
            Some(b) => Tdocs::decode(&b).ok(),
            None => None,
        })
    }

    async fn store_tdocs(&self, root: TreeId, t: &Tdocs) -> Result<()> {
        if t.total_files() >= STORE_MIN_FILES {
            let key = Self::memo_key(TDOCS, *root.as_bytes());
            self.memo_put(&key, &t.encode()).await?;
        }
        Ok(())
    }

    // -------------------------------------------------------------------------- tdocs

    /// `tdocs(root)`: memoised for large subtrees, expanded from tree objects otherwise.
    pub async fn tdocs(&self, root: TreeId) -> Result<Arc<Tdocs>> {
        self.derive_tree(root, 0, &mut Walk::new(self.max_entries))
            .await
    }

    fn derive_tree<'a>(
        &'a self,
        id: TreeId,
        depth: usize,
        walk: &'a mut Walk,
    ) -> BoxFut<'a, Arc<Tdocs>> {
        Box::pin(async move {
            if depth > MAX_DEPTH {
                return Err(ScopeError::TooLarge("directory depth"));
            }
            if let Some(t) = walk.subtrees.get(&id) {
                return Ok(t.clone());
            }
            let tree = self.engine.get_tree(id).await.map_err(st)?;
            let entries = tree.entries();
            // A directory with no subdirectory and fewer than 256 entries cannot be stored.
            let storable = entries.len() as u64 >= STORE_MIN_FILES
                || entries.iter().any(|e| e.kind == Kind::Dir);
            if storable && let Some(t) = self.stored_tdocs(id).await? {
                let t = Arc::new(t);
                walk.subtrees.insert(id, t.clone());
                return Ok(t);
            }
            self.charge(walk, entries.len(), "tree walk")?;
            let mut acc = Tdocs::default();
            for e in entries {
                match e.kind {
                    Kind::File | Kind::Exec => {
                        let ord = self.ords.ord_for(FileId::from_bytes(e.id))?;
                        acc.add_one(ord)?;
                    }
                    Kind::Dir => {
                        let child = self
                            .derive_tree(TreeId::from_bytes(e.id), depth + 1, walk)
                            .await?;
                        acc.add(&child)?;
                    }
                    Kind::Symlink | Kind::Gitlink => {}
                }
            }
            self.store_tdocs(id, &acc).await?;
            let acc = Arc::new(acc);
            walk.subtrees.insert(id, acc.clone());
            Ok(acc)
        })
    }

    /// `tdocs` of the entry at `path` below `root`: a file is a singleton, a directory its
    /// subtree, anything else (absent, symlink, gitlink) empty. O(depth) tree reads.
    pub async fn tdocs_at(&self, root: TreeId, path: &RepoPath) -> Result<Tdocs> {
        self.tdocs_at_in(root, path, &mut Walk::new(self.max_entries))
            .await
    }

    async fn tdocs_at_in(&self, root: TreeId, path: &RepoPath, walk: &mut Walk) -> Result<Tdocs> {
        let Some(e) = self.engine.lookup(root, path).await.map_err(st)? else {
            return Ok(Tdocs::default());
        };
        match e.kind {
            Kind::File | Kind::Exec => {
                Ok(Tdocs::single(self.ords.ord_for(FileId::from_bytes(e.id))?))
            }
            Kind::Dir => Ok((*self.derive_tree(TreeId::from_bytes(e.id), 0, walk).await?).clone()),
            Kind::Symlink | Kind::Gitlink => Ok(Tdocs::default()),
        }
    }

    // -------------------------------------------------------------------------- scope

    /// `scope(rev)`: the memo of the root when derived; else, for a revision near an ancestor
    /// with a derived root, that checkpoint with the counted tree delta; else the tree
    /// derivation, which reuses every shared subtree.
    pub async fn scope(&self, rev: ChangesetId) -> Result<Arc<Tdocs>> {
        self.scope_in(rev, &mut Walk::new(self.max_entries)).await
    }

    async fn scope_in(&self, rev: ChangesetId, walk: &mut Walk) -> Result<Arc<Tdocs>> {
        let cs = self.engine.get_changeset(rev).await.map_err(st)?;
        if let Some(t) = self.stored_tdocs(cs.root).await? {
            return Ok(Arc::new(t));
        }
        if let Some(t) = self.scope_by_delta(rev, cs.root, walk).await? {
            self.store_tdocs(cs.root, &t).await?;
            return Ok(Arc::new(t));
        }
        self.derive_tree(cs.root, 0, walk).await
    }

    /// Root of the nearest ancestor, at a probe distance, whose `tdocs` is stored. One
    /// `has_many` for all probes.
    async fn find_checkpoint(&self, rev: ChangesetId) -> Result<Option<TreeId>> {
        let skip = self.engine.get_derived::<Skip>(rev).await.map_err(st)?;
        let mut roots = Vec::new();
        for level in 0..PROBE_DISTANCES.len() {
            if let Some(&anc) = skip.ptrs.get(level) {
                roots.push(self.engine.get_changeset(anc).await.map_err(st)?.root);
            }
        }
        let keys: Vec<ObjKey> = roots
            .iter()
            .map(|r| Self::memo_key(TDOCS, *r.as_bytes()))
            .collect();
        if keys.is_empty() {
            return Ok(None);
        }
        let present = self.objects.has_many(&keys).await.map_err(st)?;
        Ok(roots
            .into_iter()
            .zip(present)
            .find_map(|(r, p)| p.then_some(r)))
    }

    /// `tdocs(A.root) (+) delta`, where `A` is a nearby ancestor with a stored `tdocs` and the
    /// delta is the leaf diff `A.root -> root`. The diff comes from the trees, not from any
    /// changeset's `changes` list: that list is pushed metadata, so a value derived from it
    /// and memoised under a `TreeId` would let one lying changeset poison every revision that
    /// shares the tree. Equal subtrees are skipped unread, and old values of changed paths
    /// are read from `A.root` with one `lookup` each, so tree reads are bounded by changed
    /// paths x depth. `None` when no checkpoint is near, the delta is too big or does not fit
    /// the checkpoint (the caller derives from the tree).
    async fn scope_by_delta(
        &self,
        rev: ChangesetId,
        root: TreeId,
        walk: &mut Walk,
    ) -> Result<Option<Tdocs>> {
        let Some(a_root) = self.find_checkpoint(rev).await? else {
            return Ok(None);
        };
        let Some(base) = self.stored_tdocs(a_root).await? else {
            return Ok(None);
        };
        let (diff, visits) = match self
            .engine
            .leafdiff_limited(a_root, root, None, walk.budget, MAX_DELTA_PATHS)
            .await
        {
            Ok(d) => d,
            Err(asc_engine::tree::Error::TooLarge(_)) => return Ok(None),
            Err(e) => return Err(st(e)),
        };
        if diff.len() > MAX_DELTA_PATHS {
            return Ok(None);
        }
        self.charge(walk, visits, "tree walk")?;
        // Ancestor and descendant paths in one diff (a file/directory switch): the leaf
        // algebra does not hold, so derive from the tree.
        let paths: HashSet<&RepoPath> = diff.iter().map(|(p, _)| p).collect();
        let mut last: HashMap<&RepoPath, Option<FileId>> = HashMap::with_capacity(diff.len());
        for (p, change) in &diff {
            let mut anc = p.parent();
            while let Some(a) = anc {
                if paths.contains(&a) {
                    return Ok(None);
                }
                anc = a.parent();
            }
            last.insert(p, counted(change));
        }
        let mut removed: HashMap<u32, u32> = HashMap::new();
        let mut added: HashMap<u32, u32> = HashMap::new();
        for (p, new) in &last {
            let old = match self.engine.lookup(a_root, p).await.map_err(st)? {
                None => None,
                Some(e) if matches!(e.kind, Kind::File | Kind::Exec) => {
                    Some(FileId::from_bytes(e.id))
                }
                Some(e) if e.kind == Kind::Dir => return Ok(None),
                Some(_) => None,
            };
            if old == *new {
                continue;
            }
            if let Some(f) = old {
                *removed.entry(self.ords.ord_for(f)?).or_default() += 1;
            }
            if let Some(f) = *new {
                *added.entry(self.ords.ord_for(f)?).or_default() += 1;
            }
        }
        let mut out = base;
        let touched: HashSet<u32> = removed.keys().chain(added.keys()).copied().collect();
        for o in touched {
            let c = i64::from(out.count(o)) - i64::from(removed.get(&o).copied().unwrap_or(0))
                + i64::from(added.get(&o).copied().unwrap_or(0));
            let Ok(c) = u32::try_from(c) else {
                return Ok(None);
            };
            out.set_count(o, c);
        }
        Ok(Some(out))
    }

    // -------------------------------------------------------------------------- resolve

    /// The kept ords of a query: scope, path filters, `xdocs` and stack-diff.
    pub async fn resolve(&self, q: &ScopeQuery<'_>) -> Result<RoaringBitmap> {
        let cs = self.engine.get_changeset(q.rev).await.map_err(st)?;
        // One budget and one subtree memo for the whole query.
        let mut walk = Walk::new(self.max_entries);
        let (include, exclude) = normalise(&q.include, &q.exclude);
        let mut base = if include.is_empty() && q.include.is_empty() {
            (*self.scope_in(q.rev, &mut walk).await?).clone()
        } else {
            let mut t = Tdocs::default();
            for p in &include {
                t.add(&self.tdocs_at_in(cs.root, p, &mut walk).await?)?;
            }
            t
        };
        for p in &exclude {
            base.sub(&self.tdocs_at_in(cs.root, p, &mut walk).await?)?;
        }
        let mut kept = base.bitmap;
        if let Some(cfg) = q.rules {
            kept -= &*self.xdocs_in(cs.root, cfg, &mut walk).await?;
        }
        if let Some(trunk) = q.stack_diff_trunk {
            kept &= self.stack_ords(q.rev, trunk, cs.root, &mut walk).await?;
        }
        Ok(kept)
    }

    /// The ords of the files at `paths(base..top)` in `top`, base = `merge_base(top, trunk)`.
    async fn stack_ords(
        &self,
        top: ChangesetId,
        trunk: ChangesetId,
        root: TreeId,
        walk: &mut Walk,
    ) -> Result<RoaringBitmap> {
        let base = self
            .engine
            .merge_base(top, &[trunk], false)
            .await
            .map_err(st)?
            .into_iter()
            .next()
            .ok_or(ScopeError::NoMergeBase)?;
        let paths = self.engine.paths_since(base, top).await.map_err(st)?;
        if paths.len() > MAX_STACK_PATHS {
            return Err(ScopeError::TooLarge("stack paths"));
        }
        let mut out = RoaringBitmap::new();
        for p in paths.iter() {
            out |= &self.tdocs_at_in(root, p, walk).await?.bitmap;
        }
        Ok(out)
    }

    // -------------------------------------------------------------------------- locate

    /// Up to `limit` paths (sorted) at which `ord` sits below `root`. Descends only into
    /// subtrees whose `tdocs` holds the ord.
    pub async fn locate(&self, root: TreeId, ord: u32, limit: usize) -> Result<Vec<RepoPath>> {
        let mut out: Vec<RepoPath> = Vec::new();
        let mut stack = vec![(root, Vec::<u8>::new(), 0usize)];
        let mut walk = Walk::new(self.max_entries.min(MAX_LOCATE_VISITS));
        while let Some((id, prefix, depth)) = stack.pop() {
            if out.len() >= limit {
                break;
            }
            if depth > MAX_DEPTH {
                return Err(ScopeError::TooLarge("directory depth"));
            }
            let tree = self.engine.get_tree(id).await.map_err(st)?;
            self.charge(&mut walk, tree.entries().len(), "locate walk")?;
            for e in tree.entries() {
                let mut path = prefix.clone();
                if !path.is_empty() {
                    path.push(b'/');
                }
                path.extend_from_slice(e.name.as_bytes());
                if path.len() > MAX_PATH_BYTES {
                    continue;
                }
                match e.kind {
                    Kind::File | Kind::Exec => {
                        if self.ords.get(&FileId::from_bytes(e.id)) == Some(ord)
                            && let Ok(p) = RepoPath::new(path)
                            && out.len() < limit
                        {
                            out.push(p);
                        }
                    }
                    Kind::Dir => {
                        let child = TreeId::from_bytes(e.id);
                        if self
                            .derive_tree(child, depth + 1, &mut walk)
                            .await?
                            .bitmap
                            .contains(ord)
                        {
                            stack.push((child, path, depth + 1));
                        }
                    }
                    Kind::Symlink | Kind::Gitlink => {}
                }
            }
        }
        out.sort();
        Ok(out)
    }

    // -------------------------------------------------------------------------- xdocs

    /// The ords of `root` excluded at every path they have: by `cfg`'s excludes, or by
    /// `linguist-generated`, `linguist-vendored` or `-text`/`binary` attributes. Attribute
    /// files are read per directory, `.ascattributes` first and `.gitattributes` the
    /// fallback, never merged. Memoised per `(TreeId, rules digest)`.
    pub async fn xdocs(&self, root: TreeId, cfg: &CodeintelConfig) -> Result<Arc<RoaringBitmap>> {
        self.xdocs_in(root, cfg, &mut Walk::new(self.max_entries))
            .await
    }

    /// `xdocs` drawing on the operation's budget.
    async fn xdocs_in(
        &self,
        root: TreeId,
        cfg: &CodeintelConfig,
        walk: &mut Walk,
    ) -> Result<Arc<RoaringBitmap>> {
        let mut ctx = XCtx::new(cfg, walk.budget);
        let x = self.xdocs_dir(&mut ctx, root, Vec::new(), None, 0).await;
        let used = walk.budget - ctx.budget;
        self.visited.fetch_add(used, Ordering::Relaxed);
        walk.budget = ctx.budget;
        let x = x?;
        Ok(Arc::new(&x.excluded - &x.included))
    }

    fn xdocs_dir<'a>(
        &'a self,
        ctx: &'a mut XCtx,
        id: TreeId,
        dir: Vec<u8>,
        parent: Option<Arc<AttrNode>>,
        depth: usize,
    ) -> BoxFut<'a, Arc<Xdocs>> {
        Box::pin(async move {
            if depth > MAX_DEPTH {
                return Err(ScopeError::TooLarge("directory depth"));
            }
            let tree = self.engine.get_tree(id).await.map_err(st)?;
            let entries = tree.entries();
            let node = attr_node(&tree, &dir, parent);
            let mut h = blake3::Hasher::new_derive_key("asc v1 ci xdocs rules");
            h.update(&ctx.cfg_digest);
            h.update(&node.as_ref().map_or([0; 32], |n| n.chain));
            h.update(&dir);
            let digest = *h.finalize().as_bytes();
            let mut mk = blake3::Hasher::new_derive_key("asc v1 ci xdocs key");
            mk.update(id.as_bytes());
            mk.update(&digest);
            let key = Self::memo_key(XDOCS, *mk.finalize().as_bytes());
            let storable = entries.len() as u64 >= STORE_MIN_FILES
                || entries.iter().any(|e| e.kind == Kind::Dir);
            if storable
                && let Some(b) = self.memo_get(&key).await?
                && let Ok(x) = Xdocs::decode(&b)
            {
                return Ok(Arc::new(x));
            }
            ctx.budget = ctx
                .budget
                .checked_sub(entries.len())
                .ok_or(ScopeError::TooLarge("tree walk"))?;
            let chain_data = match &node {
                Some(n) => self.attr_chain(ctx, n).await?,
                None => Vec::new(),
            };
            let mut files: Vec<(Vec<u8>, u32)> = Vec::new();
            let mut dirs: Vec<(TreeId, Vec<u8>)> = Vec::new();
            for e in entries {
                let mut path = dir.clone();
                if !path.is_empty() {
                    path.push(b'/');
                }
                path.extend_from_slice(e.name.as_bytes());
                if path.len() > MAX_PATH_BYTES {
                    return Err(ScopeError::TooLarge("path length"));
                }
                match e.kind {
                    Kind::File | Kind::Exec => {
                        let ord = self.ords.ord_for(FileId::from_bytes(e.id))?;
                        files.push((path, ord));
                    }
                    Kind::Dir => dirs.push((TreeId::from_bytes(e.id), path)),
                    Kind::Symlink | Kind::Gitlink => {}
                }
            }
            let flags = excluded_flags(&ctx.cfg, &chain_data, &files)?;
            let mut acc = Xdocs::default();
            for ((_, ord), ex) in files.iter().zip(flags) {
                if ex {
                    acc.excluded.insert(*ord);
                } else {
                    acc.included.insert(*ord);
                }
            }
            for (child, path) in dirs {
                let child = self
                    .xdocs_dir(ctx, child, path, node.clone(), depth + 1)
                    .await?;
                acc.excluded |= &child.excluded;
                acc.included |= &child.included;
            }
            if acc.excluded.len() + acc.included.len() >= STORE_MIN_FILES {
                self.memo_put(&key, &acc.encode()).await?;
            }
            Ok(Arc::new(acc))
        })
    }

    /// The attribute files from the root down to `node`, as `(directory, bytes)`. An
    /// oversized or unreadable file reads as empty. Each file is read once per call.
    async fn attr_chain(
        &self,
        ctx: &mut XCtx,
        node: &Arc<AttrNode>,
    ) -> Result<Vec<(Vec<u8>, Bytes)>> {
        let mut nodes = Vec::new();
        let mut cur = Some(node.clone());
        while let Some(n) = cur {
            nodes.push(n.clone());
            cur = n.parent.clone();
        }
        let mut out = Vec::with_capacity(nodes.len());
        for n in nodes.into_iter().rev() {
            let data = match ctx.files.get(&n.chain) {
                Some(d) => d.clone(),
                None => {
                    let d = match self.engine.read_file(n.file, n.size).await {
                        Ok(d) if d.len() as u64 <= MAX_ATTR_FILE => d,
                        _ => Bytes::new(),
                    };
                    ctx.files.insert(n.chain, d.clone());
                    d
                }
            };
            out.push((n.dir.clone(), data));
        }
        Ok(out)
    }
}

/// Whether each file is excluded: by `cfg`, or by the attribute chain (`linguist-generated`
/// or `linguist-vendored` set or `=true`, `text` unset through `-text` or `binary`).
/// Synchronous: the attribute types are not `Send`, so they never live across an await.
fn excluded_flags(
    cfg: &CodeintelConfig,
    chain: &[(Vec<u8>, Bytes)],
    files: &[(Vec<u8>, u32)],
) -> Result<Vec<bool>> {
    use bstr::ByteSlice;
    use gix_attributes::StateRef;
    let attr_err = |e: &dyn std::fmt::Display| ScopeError::Attributes(e.to_string());
    let mut search = None;
    let mut collection = gix_attributes::search::MetadataCollection::default();
    let mut outcome = gix_attributes::search::Outcome::default();
    if !chain.is_empty() && !files.is_empty() {
        let mut buf = Vec::new();
        let mut s = gix_attributes::Search::new_globals(
            None::<std::path::PathBuf>,
            &mut buf,
            &mut collection,
        )
        .map_err(|e| attr_err(&e))?;
        for (dir, data) in chain {
            let source = if dir.is_empty() {
                std::path::PathBuf::from(".gitattributes")
            } else {
                std::path::PathBuf::from(format!("{}/.gitattributes", String::from_utf8_lossy(dir)))
            };
            s.add_patterns_buffer(
                data,
                source,
                Some(std::path::Path::new("")),
                &mut collection,
                true,
            )
            .map_err(|e| attr_err(&e))?;
        }
        search = Some(s);
    }
    let mut out = Vec::with_capacity(files.len());
    for (path, _) in files {
        let mut ex = cfg.is_excluded(&String::from_utf8_lossy(path));
        if !ex && let Some(s) = &search {
            outcome.initialize_with_selection(
                &collection,
                ["linguist-generated", "linguist-vendored", "text"],
            );
            s.pattern_matching_relative_path(
                path.as_bstr(),
                gix_attributes::glob::pattern::Case::Sensitive,
                Some(false),
                &mut outcome,
            );
            ex = outcome.iter_selected().any(|m| {
                match (m.assignment.name.as_str(), m.assignment.state) {
                    ("linguist-generated" | "linguist-vendored", StateRef::Set) => true,
                    ("linguist-generated" | "linguist-vendored", StateRef::Value(v)) => {
                        v.as_bstr() == b"true".as_bstr()
                    }
                    ("text", StateRef::Unset) => true,
                    _ => false,
                }
            });
        }
        out.push(ex);
    }
    Ok(out)
}

/// The FileId counted by `tdocs` for a change: File/Exec writes only.
fn counted(c: &Change) -> Option<FileId> {
    match c {
        Change::Write {
            kind: Kind::File | Kind::Exec,
            file,
            ..
        } => Some(*file),
        _ => None,
    }
}

/// Drops redundant filters. An include under another include (or at or under an exclude) is
/// dropped; an exclude under another exclude, or outside every include, is dropped.
fn normalise(include: &[RepoPath], exclude: &[RepoPath]) -> (Vec<RepoPath>, Vec<RepoPath>) {
    let under = |p: &RepoPath, d: &RepoPath| p == d || p.starts_with_dir(d);
    let mut inc: Vec<RepoPath> = Vec::new();
    for p in include {
        if exclude.iter().any(|x| under(p, x)) {
            continue;
        }
        if include.iter().any(|d| d != p && under(p, d)) || inc.contains(p) {
            continue;
        }
        inc.push(p.clone());
    }
    let mut exc: Vec<RepoPath> = Vec::new();
    for p in exclude {
        if exclude.iter().any(|d| d != p && under(p, d)) || exc.contains(p) {
            continue;
        }
        if !include.is_empty() && !inc.iter().any(|d| under(p, d)) {
            continue;
        }
        exc.push(p.clone());
    }
    (inc, exc)
}

// ------------------------------------------------------------------------------ attributes

/// One attribute file in the chain from the root to a directory.
struct AttrNode {
    parent: Option<Arc<AttrNode>>,
    dir: Vec<u8>,
    file: FileId,
    size: u64,
    /// Digest of the chain up to and including this node.
    chain: [u8; 32],
}

/// The attribute file of `dir`: `.ascattributes` when it exists (even if unusable), else
/// `.gitattributes`. Only a regular file within the size bound counts; a symlink is never
/// followed. Returns the chain node for `dir`, which is `parent` when `dir` has none.
fn attr_node(
    tree: &asc_core::object::Tree,
    dir: &[u8],
    parent: Option<Arc<AttrNode>>,
) -> Option<Arc<AttrNode>> {
    let entry = [".ascattributes", ".gitattributes"]
        .iter()
        .find_map(|n| tree.get(&Name::new(*n).ok()?));
    let Some(e) = entry else {
        return parent;
    };
    if !matches!(e.kind, Kind::File | Kind::Exec) || e.size > MAX_ATTR_FILE {
        return parent;
    }
    let file = FileId::from_bytes(e.id);
    let mut h = blake3::Hasher::new_derive_key("asc v1 ci attr chain");
    h.update(&parent.as_ref().map_or([0; 32], |p| p.chain));
    h.update(&(dir.len() as u32).to_le_bytes());
    h.update(dir);
    h.update(file.as_bytes());
    Some(Arc::new(AttrNode {
        parent,
        dir: dir.to_vec(),
        file,
        size: e.size,
        chain: *h.finalize().as_bytes(),
    }))
}

/// State of one `xdocs` derivation.
struct XCtx {
    cfg: CodeintelConfig,
    cfg_digest: [u8; 32],
    /// Attribute file bytes by chain digest.
    files: HashMap<[u8; 32], Bytes>,
    budget: usize,
}

impl XCtx {
    fn new(cfg: &CodeintelConfig, budget: usize) -> XCtx {
        XCtx {
            cfg: cfg.clone(),
            cfg_digest: cfg.rules_digest(),
            files: HashMap::new(),
            budget,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(ords: &[u32]) -> Tdocs {
        let mut t = Tdocs::default();
        for &o in ords {
            t.add_one(o).unwrap();
        }
        t
    }

    #[test]
    fn counts_sum_and_subtract_exactly() {
        let mut a = set(&[1, 2, 2]);
        assert_eq!(a.count(2), 2);
        assert_eq!(a.total_files(), 3);
        a.add(&set(&[2, 3])).unwrap();
        assert_eq!(a.count(2), 3);
        a.sub(&set(&[2, 2])).unwrap();
        assert_eq!((a.count(2), a.dups().get(&2)), (1, None));
        a.sub(&set(&[2])).unwrap();
        assert!(!a.bitmap().contains(2));
        assert!(matches!(a.sub(&set(&[9])), Err(ScopeError::Inconsistent)));
    }

    #[test]
    fn codec_round_trips_and_rejects_damage() {
        let t = set(&[1, 5, 5, 9, 9, 9]);
        assert_eq!(Tdocs::decode(&t.encode()).unwrap(), t);
        let mut b = t.encode();
        b.pop();
        assert!(Tdocs::decode(&b).is_err());
        assert!(Tdocs::decode(&[]).is_err());
        assert!(Tdocs::decode(&[9, 0, 0, 0, 0]).is_err());
        // A dup of an ord that is not in the bitmap.
        let mut bad = set(&[1]);
        bad.dups.insert(7, 2);
        assert!(Tdocs::decode(&bad.encode()).is_err());
        // A dup with count 1.
        let mut bad = set(&[1]);
        bad.dups.insert(1, 1);
        assert!(Tdocs::decode(&bad.encode()).is_err());
        let x = Xdocs {
            excluded: [1u32, 2].into_iter().collect(),
            included: [3u32].into_iter().collect(),
        };
        assert_eq!(Xdocs::decode(&x.encode()).unwrap(), x);
        assert!(Xdocs::decode(&x.encode()[..5]).is_err());
    }

    #[test]
    fn filters_normalise() {
        let p = |s: &str| RepoPath::new(s).unwrap();
        let (i, e) = normalise(
            &[p("a"), p("a/b"), p("c")],
            &[p("c"), p("a/x"), p("a/x/y"), p("z")],
        );
        assert_eq!(i, vec![p("a")]);
        assert_eq!(e, vec![p("a/x")]);
    }
}
