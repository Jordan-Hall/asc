//! Tree apply, diff, lookup and walk (SPEC §4.1).
//!
//! Every operation touches only the directories on the spines of the paths involved. Trees are
//! read on demand through the engine's cache; nothing materialises a whole tree.

use std::cmp::Ordering;
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use asc_core::codec::{CodecError, Enveloped, decode_envelope};
use asc_core::git::tree_oid;
use asc_core::id::{FileId, TreeId};
use asc_core::object::{Change, Entry, Kind, ObjectError, Tree};
use asc_core::path::{MAX_TREE_DEPTH, Name, PathError, RepoPath};
use asc_store::codec::encode_stored;
use asc_store::key::ObjKey;
use asc_store::objects::{self, Objects};
use asc_store::refs::Refs;
use futures::stream::{BoxStream, StreamExt, TryStreamExt};

use crate::Engine;

pub use pathspec::{Pathspec, PathspecError};

/// Child trees read concurrently ahead of use when a directory is rebuilt.
pub const LOAD_WINDOW: usize = 16;

/// Entries one `leafdiff` may visit. Trees are DAGs, so a few shared subtrees can expand to
/// an unbounded number of leaves; past this the diff fails instead of exhausting memory.
pub const MAX_DIFF_VISITS: usize = 4 << 20;

/// Longest path, in bytes, that `leafdiff` and `walk` build. A tree DAG repeats one long prefix
/// for every leaf below it, so path length multiplies output size; names and nesting alone are
/// otherwise bounded only by the 256 MiB tree cap and [`MAX_TREE_DEPTH`]. Matches Linux
/// `PATH_MAX`; a longer path is refused, not truncated.
pub const MAX_WALK_PATH_BYTES: usize = 4096;

/// Path bytes (plus a fixed charge per entry) one `leafdiff` may collect. The visit budget
/// bounds entries; this bounds what each entry costs.
pub const MAX_DIFF_OUT_BYTES: usize = 64 << 20;

/// Bytes charged against [`MAX_DIFF_OUT_BYTES`] per reported leaf, on top of its path.
const OUT_ENTRY_CHARGE: usize = 128;

/// Tree operation failures.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Store(#[from] objects::Error),
    #[error("stored tree does not decode: {0}")]
    Codec(#[from] CodecError),
    #[error(transparent)]
    Object(#[from] ObjectError),
    #[error(transparent)]
    Path(#[from] PathError),
    #[error("tree nesting exceeds {MAX_TREE_DEPTH} levels")]
    TooDeep,
    #[error("tree diff visits more than {0} entries")]
    TooLarge(usize),
    #[error("tree diff output exceeds {0} bytes")]
    OutputTooLarge(usize),
    #[error("path below {0} is longer than {MAX_WALK_PATH_BYTES} bytes")]
    PathTooLong(String),
    #[error("tree {0} is missing")]
    MissingTree(TreeId),
    #[error("{0} is changed twice in one apply")]
    DuplicatePath(RepoPath),
    #[error("{0} is below an existing file that is not deleted")]
    PathThroughFile(RepoPath),
    #[error("{0} is written over a directory that keeps entries")]
    WriteOverDir(RepoPath),
    #[error("{0} is a directory and cannot be deleted as a leaf")]
    DeleteDir(RepoPath),
    #[error("{0} is written with a directory kind")]
    BadWriteKind(RepoPath),
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

/// A node of the change trie: the change at this exact path and the changes below it.
struct Node {
    path: RepoPath,
    leaf: Option<Change>,
    children: BTreeMap<Name, Node>,
}

impl Drop for Node {
    /// Drops the trie iteratively; the default glue recurses once per level.
    fn drop(&mut self) {
        let mut work: Vec<Node> = std::mem::take(&mut self.children).into_values().collect();
        while let Some(mut n) = work.pop() {
            work.extend(std::mem::take(&mut n.children).into_values());
        }
    }
}

impl Node {
    fn new(path: RepoPath) -> Self {
        Self {
            path,
            leaf: None,
            children: BTreeMap::new(),
        }
    }
}

fn build_trie(changes: &[(RepoPath, Change)]) -> Result<BTreeMap<Name, Node>> {
    let mut root: BTreeMap<Name, Node> = BTreeMap::new();
    for (path, change) in changes {
        let mut level = &mut root;
        let mut acc: Option<RepoPath> = None;
        let mut comps = path.components().peekable();
        let mut depth = 0usize;
        while let Some(comp) = comps.next() {
            depth += 1;
            if depth > MAX_TREE_DEPTH {
                return Err(Error::TooDeep);
            }
            let name = Name::new(comp)?;
            let here = bounded_child_path(acc.as_ref(), &name)?;
            let node = level.entry(name).or_insert_with(|| Node::new(here.clone()));
            if comps.peek().is_none() {
                if node.leaf.is_some() {
                    return Err(Error::DuplicatePath(here));
                }
                node.leaf = Some(change.clone());
                break;
            }
            acc = Some(here);
            level = &mut node.children;
        }
    }
    Ok(root)
}

/// The tree entry a non-delete leaf change produces.
fn leaf_entry(name: Name, path: &RepoPath, change: &Change) -> Result<Entry> {
    match change {
        Change::Write {
            kind,
            file,
            size,
            git,
            ..
        } if matches!(kind, Kind::File | Kind::Exec | Kind::Symlink) => Ok(Entry {
            name,
            kind: *kind,
            id: *file.as_bytes(),
            size: *size,
            git: *git,
        }),
        Change::Gitlink { commit } => Ok(Entry {
            name,
            kind: Kind::Gitlink,
            id: [0; 32],
            size: 0,
            git: *commit,
        }),
        _ => Err(Error::BadWriteKind(path.clone())),
    }
}

/// The leaf change that reproduces the non-directory entry `e` when written.
fn entry_change(e: &Entry) -> Change {
    match e.kind {
        Kind::Gitlink => Change::Gitlink { commit: e.git },
        kind => Change::Write {
            kind,
            file: FileId::from_bytes(e.id),
            size: e.size,
            git: e.git,
            copy_from: None,
        },
    }
}

fn child_path(prefix: Option<&RepoPath>, name: &Name) -> RepoPath {
    match prefix {
        Some(p) => p.join(name),
        None => RepoPath::from(name.clone()),
    }
}

/// [`child_path`] that refuses a result longer than [`MAX_WALK_PATH_BYTES`] before allocating it.
fn bounded_child_path(prefix: Option<&RepoPath>, name: &Name) -> Result<RepoPath> {
    let len = prefix.map_or(0, |p| p.as_bytes().len() + 1) + name.as_bytes().len();
    if len > MAX_WALK_PATH_BYTES {
        // Only the prefix is named: the offending component may be hundreds of megabytes.
        let at = prefix.map_or_else(String::new, |p| {
            let b = p.as_bytes();
            String::from_utf8_lossy(&b[..b.len().min(64)]).into_owned()
        });
        return Err(Error::PathTooLong(at));
    }
    Ok(child_path(prefix, name))
}

/// Refuses a tree holding an entry name longer than [`MAX_WALK_PATH_BYTES`]. Such a name can
/// never appear in a path asc builds, and diffing would copy and compare it once per directory
/// pair. `what` names the tree in the error.
fn check_name_lengths(tree: &Tree, what: &str) -> Result<()> {
    match tree
        .entries()
        .iter()
        .find(|e| e.name.as_bytes().len() > MAX_WALK_PATH_BYTES)
    {
        Some(e) => Err(Error::PathTooLong(format!(
            "tree {what} (entry of {} bytes)",
            e.name.as_bytes().len()
        ))),
        None => Ok(()),
    }
}

fn empty_tree() -> Tree {
    Tree::new(Vec::new(), true).expect("an empty root tree is valid")
}

impl<O: Objects, R: Refs> Engine<O, R> {
    /// The id of the empty root tree.
    pub fn empty_tree_id() -> TreeId {
        empty_tree().id()
    }

    /// Reads a tree through the cache. The empty tree is always available.
    pub async fn get_tree(&self, id: TreeId) -> Result<Arc<Tree>> {
        if let Some(t) = self.trees.get(&id) {
            return Ok(t);
        }
        if id == Self::empty_tree_id() {
            return Ok(Arc::new(empty_tree()));
        }
        let data = self
            .objects
            .get(&ObjKey::Tree(id))
            .await?
            .ok_or(Error::MissingTree(id))?;
        let tree = decode_envelope::<Tree>(&data)?;
        // Stored trees are untrusted: a name past the path bound can never appear in a path
        // asc builds, and diffing would otherwise copy and compare it once per directory pair.
        check_name_lengths(&tree, &id.to_string())?;
        let tree = Arc::new(tree);
        self.trees.insert(id, tree.clone());
        Ok(tree)
    }

    /// Stores a tree and returns its id.
    ///
    /// Refuses an entry name longer than [`MAX_WALK_PATH_BYTES`], exactly as [`Self::get_tree`]
    /// would on the way back, so no caller can cache or store a tree the walkers cannot bound.
    pub async fn put_tree(&self, tree: Tree) -> Result<TreeId> {
        check_name_lengths(&tree, "being stored")?;
        let envelope = tree.to_envelope();
        let id = TreeId::of(&envelope);
        self.objects
            .put(&ObjKey::Tree(id), encode_stored(&envelope))
            .await?;
        self.trees.insert(id, Arc::new(tree));
        Ok(id)
    }

    /// Applies leaf changes to `root` and returns the new root.
    ///
    /// Only the directories on the changed paths are read and rewritten, bottom-up. Directories
    /// left empty are pruned; `size` and `git` of rewritten directory entries are recomputed.
    /// A write below an existing file needs a `Delete` of that file in the same call; a write
    /// over an existing directory needs a `Delete` of every leaf below it. Deleting an absent
    /// path is a no-op.
    pub async fn apply(&self, root: TreeId, changes: &[(RepoPath, Change)]) -> Result<TreeId> {
        if changes.is_empty() {
            return Ok(root);
        }
        let trie = build_trie(changes)?;
        let base = self.get_tree(root).await?;
        let entries = self.apply_dir(Some(base), &trie).await?;
        self.put_tree(Tree::new(entries, true)?).await
    }

    /// Reads trees concurrently, in order, with at most [`LOAD_WINDOW`] reads in flight.
    pub async fn get_trees(&self, ids: &[TreeId]) -> Result<Vec<Arc<Tree>>> {
        futures::stream::iter(ids.to_vec())
            .map(|id| self.get_tree(id))
            .buffered(LOAD_WINDOW)
            .try_collect()
            .await
    }

    /// New entries of the root directory after applying the trie level `nodes` to `base`.
    ///
    /// Iterative: one frame per directory on the spine, so stack use does not depend on depth.
    async fn apply_dir(
        &self,
        base: Option<Arc<Tree>>,
        nodes: &BTreeMap<Name, Node>,
    ) -> Result<Vec<Entry>> {
        struct Frame<'a> {
            base: Option<Arc<Tree>>,
            nodes: Vec<(&'a Name, &'a Node)>,
            k: usize,
            /// Position in the base entries already copied to `out`.
            i: usize,
            out: Vec<Entry>,
            /// The child being built: its name, node and existing entry.
            pending: Option<(&'a Name, &'a Node, Option<Entry>)>,
            /// Child base trees fetched ahead of use, by node index.
            loaded: HashMap<usize, Arc<Tree>>,
        }
        fn frame<'a>(base: Option<Arc<Tree>>, nodes: &'a BTreeMap<Name, Node>) -> Frame<'a> {
            Frame {
                base,
                nodes: nodes.iter().collect(),
                k: 0,
                i: 0,
                out: Vec::new(),
                pending: None,
                loaded: HashMap::new(),
            }
        }
        let mut stack = vec![frame(base, nodes)];
        loop {
            let Some(top) = stack.last_mut() else {
                return Ok(Vec::new());
            };
            let base = top.base.clone();
            let old: &[Entry] = base.as_deref().map_or(&[], Tree::entries);
            let Some(&(name, node)) = top.nodes.get(top.k) else {
                let Some(done) = stack.pop() else {
                    return Ok(Vec::new());
                };
                let mut out = done.out;
                out.extend_from_slice(&old[done.i..]);
                let Some(parent) = stack.last_mut() else {
                    return Ok(out);
                };
                let Some((name, node, existing)) = parent.pending.take() else {
                    return Err(Error::TooDeep);
                };
                let entry = self
                    .finish_entry(name, existing.as_ref(), node, Some(out))
                    .await?;
                parent.out.extend(entry);
                continue;
            };
            top.k += 1;
            let pos = top.i + old[top.i..].partition_point(|e| e.name < *name);
            let existing = old.get(pos).filter(|e| e.name == *name).cloned();
            top.out.extend_from_slice(&old[top.i..pos]);
            top.i = pos + usize::from(existing.is_some());

            let existing_dir = existing.as_ref().filter(|e| e.kind == Kind::Dir);
            let deleting = matches!(node.leaf, Some(Change::Delete));
            if deleting && existing_dir.is_some() {
                return Err(Error::DeleteDir(node.path.clone()));
            }
            if node.children.is_empty() {
                let entry = self
                    .finish_entry(name, existing.as_ref(), node, None)
                    .await?;
                top.out.extend(entry);
                continue;
            }
            let below_base = match (&existing, existing_dir) {
                (_, Some(d)) => {
                    let idx = top.k - 1;
                    if !top.loaded.contains_key(&idx) {
                        // Fetch this child and the next ones that will need their base tree,
                        // concurrently, at most LOAD_WINDOW at a time.
                        let mut want = vec![(idx, TreeId::from_bytes(d.id))];
                        let end = (idx + LOAD_WINDOW).min(top.nodes.len());
                        for j in idx + 1..end {
                            let (n, node_j) = top.nodes[j];
                            if node_j.children.is_empty() {
                                continue;
                            }
                            let at = old.partition_point(|e| e.name < *n);
                            if let Some(e) =
                                old.get(at).filter(|e| e.name == *n && e.kind == Kind::Dir)
                            {
                                want.push((j, TreeId::from_bytes(e.id)));
                            }
                        }
                        let ids: Vec<TreeId> = want.iter().map(|w| w.1).collect();
                        let trees = self.get_trees(&ids).await?;
                        let Some(top) = stack.last_mut() else {
                            return Err(Error::TooDeep);
                        };
                        for ((j, _), t) in want.into_iter().zip(trees) {
                            top.loaded.insert(j, t);
                        }
                    }
                    let Some(top) = stack.last_mut() else {
                        return Err(Error::TooDeep);
                    };
                    top.loaded.remove(&idx)
                }
                (Some(_), None) if !deleting => {
                    return Err(Error::PathThroughFile(node.path.clone()));
                }
                _ => None,
            };
            if stack.len() > MAX_TREE_DEPTH {
                return Err(Error::TooDeep);
            }
            if let Some(top) = stack.last_mut() {
                top.pending = Some((name, node, existing));
            }
            stack.push(frame(below_base, &node.children));
        }
    }

    /// The entry that replaces `existing` at `name`, or `None` when the name disappears.
    /// `below` is the rebuilt content of the directory when the node has children.
    async fn finish_entry(
        &self,
        name: &Name,
        existing: Option<&Entry>,
        node: &Node,
        below: Option<Vec<Entry>>,
    ) -> Result<Option<Entry>> {
        let existing_dir = existing.filter(|e| e.kind == Kind::Dir);
        let deleting = matches!(node.leaf, Some(Change::Delete));
        let below_nonempty = below.as_ref().is_some_and(|b| !b.is_empty());
        match &node.leaf {
            Some(change @ (Change::Write { .. } | Change::Gitlink { .. })) => {
                if below_nonempty || (existing_dir.is_some() && below.is_none()) {
                    return Err(Error::WriteOverDir(node.path.clone()));
                }
                leaf_entry(name.clone(), &node.path, change).map(Some)
            }
            Some(Change::Delete) | None => match below {
                Some(entries) if !entries.is_empty() => {
                    self.dir_entry(name.clone(), entries).await.map(Some)
                }
                Some(_) => Ok(None),
                None if deleting => Ok(None),
                None => Ok(existing.cloned()),
            },
        }
    }

    /// Stores a directory and returns its entry with recomputed `size` and `git`.
    async fn dir_entry(&self, name: Name, entries: Vec<Entry>) -> Result<Entry> {
        let size = entries.iter().fold(0u64, |s, e| s.saturating_add(e.size));
        let tree = Tree::new(entries, false)?;
        let git = tree_oid(&tree);
        let id = self.put_tree(tree).await?;
        Ok(Entry {
            name,
            kind: Kind::Dir,
            id: *id.as_bytes(),
            size,
            git,
        })
    }

    /// The entry at `path`, or `None` when absent or below a file. Reads one tree per component.
    pub async fn lookup(&self, root: TreeId, path: &RepoPath) -> Result<Option<Entry>> {
        let mut tree = self.get_tree(root).await?;
        let mut comps = path.components().peekable();
        while let Some(comp) = comps.next() {
            let name = Name::new(comp)?;
            let Some(entry) = tree.get(&name).cloned() else {
                return Ok(None);
            };
            if comps.peek().is_none() {
                return Ok(Some(entry));
            }
            if entry.kind != Kind::Dir {
                return Ok(None);
            }
            tree = self.get_tree(TreeId::from_bytes(entry.id)).await?;
        }
        Ok(None)
    }

    /// Leaf changes that turn `a` into `b`, sorted by path.
    ///
    /// Subtrees with equal ids are skipped unread. A path that changes between file and
    /// directory yields a `Delete` of the old leaves plus `Write`s of the new ones. With a
    /// pathspec, only matching paths are reported and non-matching subtrees are not read.
    pub async fn leafdiff(
        &self,
        a: TreeId,
        b: TreeId,
        spec: Option<&Pathspec>,
    ) -> Result<Vec<(RepoPath, Change)>> {
        self.leafdiff_bounded(a, b, spec, MAX_DIFF_VISITS).await
    }

    /// [`Self::leafdiff`] that fails with [`Error::TooLarge`] after `budget` visited entries.
    pub async fn leafdiff_bounded(
        &self,
        a: TreeId,
        b: TreeId,
        spec: Option<&Pathspec>,
        budget: usize,
    ) -> Result<Vec<(RepoPath, Change)>> {
        let mut out = Vec::new();
        if a != b {
            self.diff_dir(a, b, spec, &mut out, budget).await?;
        }
        out.sort_by(|x, y| x.0.cmp(&y.0));
        Ok(out)
    }

    /// Iterative: an explicit stack of directory frames, so stack use does not depend on depth.
    async fn diff_dir(
        &self,
        a: TreeId,
        b: TreeId,
        spec: Option<&Pathspec>,
        out: &mut Vec<(RepoPath, Change)>,
        budget: usize,
    ) -> Result<()> {
        let mut visits = 0usize;
        let mut out_bytes = 0usize;
        enum Frame {
            Diff {
                ta: Arc<Tree>,
                tb: Arc<Tree>,
                i: usize,
                j: usize,
                prefix: Option<RepoPath>,
                /// Child pairs fetched ahead of use, by tree id.
                loaded: HashMap<TreeId, Arc<Tree>>,
            },
            /// All of a one-sided directory's entries, reported as added or deleted.
            Emit {
                tree: Arc<Tree>,
                next: usize,
                prefix: RepoPath,
                added: bool,
            },
        }
        let (ta, tb) = futures::try_join!(self.get_tree(a), self.get_tree(b))?;
        let mut stack = vec![Frame::Diff {
            ta,
            tb,
            i: 0,
            j: 0,
            prefix: None,
            loaded: HashMap::new(),
        }];
        while let Some(top) = stack.last_mut() {
            visits += 1;
            if visits > budget {
                return Err(Error::TooLarge(budget));
            }
            // Entries to report as one-sided: (entry, directory prefix, added).
            let one_sided: Vec<(Entry, Option<RepoPath>, bool)>;
            match top {
                Frame::Emit {
                    tree,
                    next,
                    prefix,
                    added,
                } => {
                    let Some(child) = tree.entries().get(*next).cloned() else {
                        stack.pop();
                        continue;
                    };
                    *next += 1;
                    one_sided = vec![(child, Some(prefix.clone()), *added)];
                }
                Frame::Diff {
                    ta,
                    tb,
                    i,
                    j,
                    prefix,
                    ..
                } => {
                    let (ea, eb) = (ta.entries(), tb.entries());
                    if *i >= ea.len() && *j >= eb.len() {
                        stack.pop();
                        continue;
                    }
                    let order = match (ea.get(*i), eb.get(*j)) {
                        (Some(x), Some(y)) => x.name.cmp(&y.name),
                        (Some(_), None) => Ordering::Less,
                        _ => Ordering::Greater,
                    };
                    match order {
                        Ordering::Less => {
                            one_sided = vec![(ea[*i].clone(), prefix.clone(), false)];
                            *i += 1;
                        }
                        Ordering::Greater => {
                            one_sided = vec![(eb[*j].clone(), prefix.clone(), true)];
                            *j += 1;
                        }
                        Ordering::Equal => {
                            // Compare in place: a skipped pair costs no copy.
                            let (xr, yr) = (&ea[*i], &eb[*j]);
                            let both_dirs = xr.kind == Kind::Dir && yr.kind == Kind::Dir;
                            let same = xr == yr || (both_dirs && xr.id == yr.id);
                            let pair = (!same).then(|| (xr.clone(), yr.clone()));
                            *i += 1;
                            *j += 1;
                            let Some((x, y)) = pair else {
                                continue;
                            };
                            let prefix = prefix.clone();
                            let ahead = (ta.clone(), tb.clone(), *i, *j);
                            if both_dirs {
                                let path = bounded_child_path(prefix.as_ref(), &x.name)?;
                                if spec.is_none_or(|s| s.may_contain_dir(&path)) {
                                    if stack.len() > MAX_TREE_DEPTH {
                                        return Err(Error::TooDeep);
                                    }
                                    let (xid, yid) =
                                        (TreeId::from_bytes(x.id), TreeId::from_bytes(y.id));
                                    let cached = match stack.last_mut() {
                                        Some(Frame::Diff { loaded, .. }) => {
                                            (loaded.remove(&xid), loaded.remove(&yid))
                                        }
                                        _ => (None, None),
                                    };
                                    let (ta, tb) = match cached {
                                        (Some(ta), Some(tb)) => (ta, tb),
                                        _ => {
                                            // Read this pair and the next changed pairs of the
                                            // frame together, at most LOAD_WINDOW trees.
                                            let mut ids = vec![xid, yid];
                                            let (ta0, tb0, mut a, mut b) = ahead;
                                            let (ea, eb) = (ta0.entries(), tb0.entries());
                                            while ids.len() + 2 <= LOAD_WINDOW
                                                && a < ea.len()
                                                && b < eb.len()
                                            {
                                                match ea[a].name.cmp(&eb[b].name) {
                                                    Ordering::Less => a += 1,
                                                    Ordering::Greater => b += 1,
                                                    Ordering::Equal => {
                                                        let (p, q) = (&ea[a], &eb[b]);
                                                        a += 1;
                                                        b += 1;
                                                        if p.kind == Kind::Dir
                                                            && q.kind == Kind::Dir
                                                            && p.id != q.id
                                                            // An over-long path is a read-ahead miss
                                                            // here; the pair's own turn refuses it.
                                                            && spec.is_none_or(|s| {
                                                                bounded_child_path(
                                                                    prefix.as_ref(),
                                                                    &p.name,
                                                                )
                                                                .is_ok_and(|c| s.may_contain_dir(&c))
                                                            })
                                                        {
                                                            ids.push(TreeId::from_bytes(p.id));
                                                            ids.push(TreeId::from_bytes(q.id));
                                                        }
                                                    }
                                                }
                                            }
                                            let trees = self.get_trees(&ids).await?;
                                            if let Some(Frame::Diff { loaded, .. }) =
                                                stack.last_mut()
                                            {
                                                for (id, t) in ids.iter().zip(&trees).skip(2) {
                                                    loaded.insert(*id, t.clone());
                                                }
                                            }
                                            (trees[0].clone(), trees[1].clone())
                                        }
                                    };
                                    stack.push(Frame::Diff {
                                        ta,
                                        tb,
                                        i: 0,
                                        j: 0,
                                        prefix: Some(path),
                                        loaded: HashMap::new(),
                                    });
                                }
                                continue;
                            }
                            // A leaf replaced by a leaf is one `Write`; a directory on either
                            // side is expanded to leaves.
                            let mut v = Vec::new();
                            if x.kind == Kind::Dir || y.kind == Kind::Dir {
                                v.push((x, prefix.clone(), false));
                            }
                            v.push((y, prefix, true));
                            one_sided = v;
                        }
                    }
                }
            }
            for (e, prefix, added) in one_sided {
                let path = bounded_child_path(prefix.as_ref(), &e.name)?;
                if e.kind != Kind::Dir {
                    if spec.is_none_or(|s| s.matches(&path)) {
                        out_bytes += path.as_bytes().len() + OUT_ENTRY_CHARGE;
                        if out_bytes > MAX_DIFF_OUT_BYTES {
                            return Err(Error::OutputTooLarge(MAX_DIFF_OUT_BYTES));
                        }
                        out.push((
                            path,
                            if added {
                                entry_change(&e)
                            } else {
                                Change::Delete
                            },
                        ));
                    }
                } else if spec.is_none_or(|s| s.may_contain_dir(&path)) {
                    if stack.len() > MAX_TREE_DEPTH {
                        return Err(Error::TooDeep);
                    }
                    let tree = self.get_tree(TreeId::from_bytes(e.id)).await?;
                    stack.push(Frame::Emit {
                        tree,
                        next: 0,
                        prefix: path,
                        added,
                    });
                }
            }
        }
        Ok(())
    }

    /// Streams the leaf entries (files, symlinks, gitlinks) under `root` that match `spec`.
    ///
    /// Order is tree order: entries by name, descending into directories in place. Memory is
    /// bounded by the depth of the tree; non-matching subtrees are not read.
    pub fn walk<'a>(
        &'a self,
        root: TreeId,
        spec: Option<&'a Pathspec>,
    ) -> BoxStream<'a, Result<(RepoPath, Entry)>> {
        struct Frame {
            prefix: Option<RepoPath>,
            tree: Arc<Tree>,
            next: usize,
        }
        // The root is read on the first poll, so building the stream does no I/O.
        let init: (Option<TreeId>, Vec<Frame>) = (Some(root), Vec::new());
        futures::stream::try_unfold(init, move |(mut pending_root, mut stack)| async move {
            if let Some(id) = pending_root.take() {
                let tree = self.get_tree(id).await?;
                stack.push(Frame {
                    prefix: None,
                    tree,
                    next: 0,
                });
            }
            loop {
                let Some(top) = stack.last_mut() else {
                    return Ok(None);
                };
                let Some(entry) = top.tree.entries().get(top.next).cloned() else {
                    stack.pop();
                    continue;
                };
                top.next += 1;
                let path = bounded_child_path(top.prefix.as_ref(), &entry.name)?;
                if entry.kind == Kind::Dir {
                    if spec.is_none_or(|s| s.may_contain_dir(&path)) {
                        if stack.len() > MAX_TREE_DEPTH {
                            return Err(Error::TooDeep);
                        }
                        let tree = self.get_tree(TreeId::from_bytes(entry.id)).await?;
                        stack.push(Frame {
                            prefix: Some(path),
                            tree,
                            next: 0,
                        });
                    }
                } else if spec.is_none_or(|s| s.matches(&path)) {
                    return Ok(Some(((path, entry), (None, stack))));
                }
            }
        })
        .boxed()
    }
}

mod pathspec {
    //! Git-style pathspecs: literal prefixes, `:(glob)` patterns and `:(exclude)` items.

    use asc_core::path::RepoPath;
    use globset::{GlobBuilder, GlobMatcher};

    /// Why a pathspec item was rejected.
    #[derive(Debug, thiserror::Error, PartialEq, Eq)]
    pub enum PathspecError {
        #[error("empty pathspec item")]
        Empty,
        #[error("pathspec magic is not closed with a parenthesis")]
        Unterminated,
        #[error("unsupported pathspec magic {0:?}")]
        UnknownMagic(String),
        #[error("glob pattern must be UTF-8")]
        NonUtf8Glob,
        #[error("invalid glob {pattern:?}: {reason}")]
        BadGlob { pattern: String, reason: String },
    }

    enum Item {
        /// A file or directory; matches itself and everything below it.
        Prefix(Vec<u8>),
        Glob(GlobItem),
    }

    struct GlobItem {
        matcher: GlobMatcher,
        /// Leading pattern components without wildcards.
        literal: Vec<Vec<u8>>,
        /// Component count when the pattern cannot match at varying depth.
        fixed_depth: Option<usize>,
    }

    fn under(path: &[u8], dir: &[u8]) -> bool {
        path.len() > dir.len() && path.starts_with(dir) && path[dir.len()] == b'/'
    }

    impl Item {
        fn matches(&self, path: &RepoPath) -> bool {
            let p = path.as_bytes();
            match self {
                Item::Prefix(pre) => p == pre.as_slice() || under(p, pre),
                Item::Glob(g) => g.matcher.is_match(&*String::from_utf8_lossy(p)),
            }
        }

        /// False only when nothing below `dir` can match.
        fn may_contain(&self, dir: &RepoPath) -> bool {
            let d = dir.as_bytes();
            match self {
                Item::Prefix(pre) => d == pre.as_slice() || under(d, pre) || under(pre, d),
                Item::Glob(g) => {
                    let comps: Vec<&[u8]> = d.split(|&c| c == b'/').collect();
                    let agrees = comps.iter().zip(&g.literal).all(|(a, b)| a == b);
                    agrees && g.fixed_depth.is_none_or(|n| comps.len() < n)
                }
            }
        }
    }

    /// A set of include and exclude items. A path matches when it satisfies some include (or
    /// there are none) and no exclude.
    #[derive(Default)]
    pub struct Pathspec {
        includes: Vec<Item>,
        excludes: Vec<Item>,
    }

    impl Pathspec {
        /// Parses git pathspec items. Plain items are literal prefixes; `:(glob)` items are
        /// glob patterns where `*` stops at `/` and `**` crosses it; `:(exclude)`, `:!` and
        /// `:^` negate. Magic words `literal` and `top` are accepted and ignored. Globs match
        /// the path as lossy UTF-8.
        pub fn parse<I, S>(items: I) -> Result<Self, PathspecError>
        where
            I: IntoIterator<Item = S>,
            S: AsRef<[u8]>,
        {
            let mut spec = Pathspec::default();
            for raw in items {
                let (item, exclude) = parse_item(raw.as_ref())?;
                if exclude {
                    spec.excludes.push(item);
                } else {
                    spec.includes.push(item);
                }
            }
            Ok(spec)
        }

        /// Whether the leaf at `path` is selected.
        pub fn matches(&self, path: &RepoPath) -> bool {
            (self.includes.is_empty() || self.includes.iter().any(|i| i.matches(path)))
                && !self.excludes.iter().any(|e| e.matches(path))
        }

        /// False only when no path below `dir` can be selected.
        pub fn may_contain_dir(&self, dir: &RepoPath) -> bool {
            let d = dir.as_bytes();
            let excluded = self.excludes.iter().any(|e| match e {
                Item::Prefix(pre) => d == pre.as_slice() || under(d, pre),
                Item::Glob(_) => false,
            });
            !excluded
                && (self.includes.is_empty() || self.includes.iter().any(|i| i.may_contain(dir)))
        }
    }

    fn parse_item(raw: &[u8]) -> Result<(Item, bool), PathspecError> {
        let (mut glob, mut exclude) = (false, false);
        let mut rest = raw;
        if let Some(after) = raw.strip_prefix(b":(") {
            let close = after
                .iter()
                .position(|&c| c == b')')
                .ok_or(PathspecError::Unterminated)?;
            for word in after[..close].split(|&c| c == b',') {
                match word {
                    b"glob" => glob = true,
                    b"exclude" => exclude = true,
                    b"literal" | b"top" | b"" => {}
                    w => {
                        return Err(PathspecError::UnknownMagic(
                            String::from_utf8_lossy(w).into_owned(),
                        ));
                    }
                }
            }
            rest = &after[close + 1..];
        } else if let Some(after) = raw.strip_prefix(b":!").or_else(|| raw.strip_prefix(b":^")) {
            exclude = true;
            rest = after;
        }
        let rest = rest.strip_prefix(b"./").unwrap_or(rest);
        if rest.is_empty() {
            return Err(PathspecError::Empty);
        }
        if !glob {
            let trimmed = rest.strip_suffix(b"/").unwrap_or(rest);
            return Ok((Item::Prefix(trimmed.to_vec()), exclude));
        }
        let pattern = std::str::from_utf8(rest).map_err(|_| PathspecError::NonUtf8Glob)?;
        let matcher = GlobBuilder::new(pattern)
            .literal_separator(true)
            .build()
            .map_err(|e| PathspecError::BadGlob {
                pattern: pattern.to_owned(),
                reason: e.to_string(),
            })?
            .compile_matcher();
        let comps: Vec<&str> = pattern.split('/').collect();
        let literal = comps
            .iter()
            .take_while(|c| !c.contains(|ch| "*?[]{}\\".contains(ch)))
            .map(|c| c.as_bytes().to_vec())
            .collect();
        let varying = pattern.contains("**") || pattern.contains('{');
        let fixed_depth = (!varying).then_some(comps.len());
        Ok((
            Item::Glob(GlobItem {
                matcher,
                literal,
                fixed_depth,
            }),
            exclude,
        ))
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};

    use async_trait::async_trait;
    use bytes::Bytes;
    use asc_core::git::blob_oid;
    use asc_store::bucket::BucketObjects;
    use asc_store::key::KeyPrefix;
    use asc_store::refs::{RefEntry, RefError, RefLogEntry, RefTxn, RepoKey};
    use proptest::prelude::*;

    use super::*;

    struct NoRefs;

    #[async_trait]
    impl Refs for NoRefs {
        async fn get(
            &self,
            _: RepoKey,
            _: &asc_core::path::RefName,
        ) -> asc_store::refs::Result<Option<RefEntry>> {
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
            _: &asc_core::path::RefName,
            _: u32,
        ) -> asc_store::refs::Result<Vec<RefLogEntry>> {
            Ok(Vec::new())
        }
    }

    /// Counts tree reads and writes to the wrapped store.
    struct Counting {
        inner: BucketObjects,
        gets: AtomicUsize,
        puts: AtomicUsize,
        in_flight: AtomicUsize,
        max_in_flight: AtomicUsize,
    }

    #[async_trait]
    impl Objects for Counting {
        async fn get(&self, key: &ObjKey) -> objects::Result<Option<Bytes>> {
            self.gets.fetch_add(1, SeqCst);
            let now = self.in_flight.fetch_add(1, SeqCst) + 1;
            self.max_in_flight.fetch_max(now, SeqCst);
            tokio::task::yield_now().await;
            let got = self.inner.get(key).await;
            self.in_flight.fetch_sub(1, SeqCst);
            got
        }
        async fn get_stored(&self, key: &ObjKey) -> objects::Result<Option<Bytes>> {
            self.inner.get_stored(key).await
        }
        async fn get_range(
            &self,
            key: &ObjKey,
            r: std::ops::Range<u64>,
        ) -> objects::Result<Option<Bytes>> {
            self.inner.get_range(key, r).await
        }
        async fn put(&self, key: &ObjKey, stored: Bytes) -> objects::Result<()> {
            self.puts.fetch_add(1, SeqCst);
            self.inner.put(key, stored).await
        }
        async fn has_many(&self, keys: &[ObjKey]) -> objects::Result<Vec<bool>> {
            self.inner.has_many(keys).await
        }
        async fn delete(&self, key: &ObjKey) -> objects::Result<()> {
            self.inner.delete(key).await
        }
        fn list(
            &self,
            p: KeyPrefix,
        ) -> BoxStream<'_, objects::Result<(ObjKey, std::time::SystemTime)>> {
            self.inner.list(p)
        }
    }

    fn counting() -> Arc<Counting> {
        Arc::new(Counting {
            inner: BucketObjects::memory(),
            gets: AtomicUsize::new(0),
            puts: AtomicUsize::new(0),
            in_flight: AtomicUsize::new(0),
            max_in_flight: AtomicUsize::new(0),
        })
    }

    fn engine_on(objs: Arc<Counting>) -> Engine<Counting, NoRefs> {
        Engine::new(objs, Arc::new(NoRefs), RepoKey(1))
    }

    fn engine() -> Engine<Counting, NoRefs> {
        engine_on(counting())
    }

    fn p(s: &str) -> RepoPath {
        RepoPath::new(s).unwrap()
    }

    fn file(seed: u8) -> Change {
        Change::write(
            Kind::File,
            FileId::of(&[seed]),
            u64::from(seed),
            blob_oid(&[seed]),
            None,
        )
        .unwrap()
    }

    /// A leaf value in the generated trees: a seed and a kind tag (0 file, 1 exec, 2 symlink,
    /// 3 gitlink).
    fn leaf(seed: u8, tag: u8) -> Change {
        match tag % 4 {
            0 => file(seed),
            1 => {
                Change::write(Kind::Exec, FileId::of(&[seed]), 1, blob_oid(&[seed]), None).unwrap()
            }
            2 => Change::write(
                Kind::Symlink,
                FileId::of(&[seed]),
                2,
                blob_oid(&[seed, 1]),
                None,
            )
            .unwrap(),
            _ => Change::Gitlink {
                commit: blob_oid(&[seed, 9]),
            },
        }
    }

    async fn build<O: Objects>(e: &Engine<O, NoRefs>, leaves: &[(RepoPath, Change)]) -> TreeId {
        e.apply(Engine::<O, NoRefs>::empty_tree_id(), leaves)
            .await
            .unwrap()
    }

    async fn all_leaves<O: Objects>(
        e: &Engine<O, NoRefs>,
        root: TreeId,
        spec: Option<&Pathspec>,
    ) -> Vec<(RepoPath, Change)> {
        let mut v: Vec<_> = e
            .walk(root, spec)
            .map_ok(|(p, en)| (p, entry_change(&en)))
            .try_collect()
            .await
            .unwrap();
        v.sort_by(|a, b| a.0.cmp(&b.0));
        v
    }

    #[tokio::test]
    async fn apply_builds_nested_tree_with_sizes_and_git() {
        let e = engine();
        let root = build(
            &e,
            &[
                (p("a/b/x"), file(3)),
                (p("a/y"), file(4)),
                (p("z"), file(5)),
            ],
        )
        .await;
        let a = e.lookup(root, &p("a")).await.unwrap().unwrap();
        assert_eq!(a.kind, Kind::Dir);
        assert_eq!(a.size, 7);
        let a_tree = e.get_tree(TreeId::from_bytes(a.id)).await.unwrap();
        assert_eq!(a.git, tree_oid(&a_tree));
        let b = e.lookup(root, &p("a/b")).await.unwrap().unwrap();
        assert_eq!(b.size, 3);
        assert!(e.lookup(root, &p("a/b/x/deeper")).await.unwrap().is_none());
        assert!(e.lookup(root, &p("nope")).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn delete_prunes_empty_directories_to_empty_root() {
        let e = engine();
        let root = build(&e, &[(p("a/b/c"), file(1))]).await;
        let gone = e
            .apply(root, &[(p("a/b/c"), Change::Delete)])
            .await
            .unwrap();
        assert_eq!(gone, Engine::<Counting, NoRefs>::empty_tree_id());
        // Deleting what is not there changes nothing.
        let same = e.apply(root, &[(p("q/r"), Change::Delete)]).await.unwrap();
        assert_eq!(same, root);
    }

    #[tokio::test]
    async fn apply_is_order_independent_and_matches_one_shot_build() {
        let e = engine();
        let step1 = build(&e, &[(p("a/x"), file(1))]).await;
        let step2 = e.apply(step1, &[(p("a/y"), file(2))]).await.unwrap();
        let once = build(&e, &[(p("a/y"), file(2)), (p("a/x"), file(1))]).await;
        assert_eq!(step2, once);
    }

    #[tokio::test]
    async fn write_through_file_needs_its_delete() {
        let e = engine();
        let root = build(&e, &[(p("a"), file(1))]).await;
        let err = e.apply(root, &[(p("a/x"), file(2))]).await.unwrap_err();
        assert!(matches!(err, Error::PathThroughFile(path) if path == p("a")));
        let ok = e
            .apply(root, &[(p("a"), Change::Delete), (p("a/x"), file(2))])
            .await
            .unwrap();
        assert_eq!(
            ok,
            build(&e, &[(p("a/x"), file(2))]).await,
            "file replaced by a directory"
        );
    }

    #[tokio::test]
    async fn write_over_directory_needs_all_leaves_deleted() {
        let e = engine();
        let root = build(&e, &[(p("a/x"), file(1)), (p("a/y"), file(2))]).await;
        let err = e.apply(root, &[(p("a"), file(3))]).await.unwrap_err();
        assert!(matches!(err, Error::WriteOverDir(path) if path == p("a")));
        let err = e
            .apply(root, &[(p("a"), file(3)), (p("a/x"), Change::Delete)])
            .await
            .unwrap_err();
        assert!(matches!(err, Error::WriteOverDir(path) if path == p("a")));
        let ok = e
            .apply(
                root,
                &[
                    (p("a"), file(3)),
                    (p("a/x"), Change::Delete),
                    (p("a/y"), Change::Delete),
                ],
            )
            .await
            .unwrap();
        assert_eq!(ok, build(&e, &[(p("a"), file(3))]).await);
        let err = e
            .apply(root, &[(p("a"), Change::Delete)])
            .await
            .unwrap_err();
        assert!(matches!(err, Error::DeleteDir(_)));
    }

    #[tokio::test]
    async fn duplicate_paths_are_rejected() {
        let e = engine();
        let err = e
            .apply(
                Engine::<Counting, NoRefs>::empty_tree_id(),
                &[(p("a"), file(1)), (p("a"), file(2))],
            )
            .await
            .unwrap_err();
        assert!(matches!(err, Error::DuplicatePath(_)));
    }

    #[tokio::test]
    async fn file_directory_replacement_diffs_both_ways() {
        let e = engine();
        let as_file = build(&e, &[(p("a"), file(1)), (p("k"), file(9))]).await;
        let as_dir = build(
            &e,
            &[
                (p("a/x"), file(2)),
                (p("a/y/z"), file(3)),
                (p("k"), file(9)),
            ],
        )
        .await;
        let fwd = e.leafdiff(as_file, as_dir, None).await.unwrap();
        assert_eq!(
            fwd,
            vec![
                (p("a"), Change::Delete),
                (p("a/x"), file(2)),
                (p("a/y/z"), file(3)),
            ]
        );
        assert_eq!(e.apply(as_file, &fwd).await.unwrap(), as_dir);
        let back = e.leafdiff(as_dir, as_file, None).await.unwrap();
        assert_eq!(
            back,
            vec![
                (p("a"), file(1)),
                (p("a/x"), Change::Delete),
                (p("a/y/z"), Change::Delete),
            ]
        );
        assert_eq!(e.apply(as_dir, &back).await.unwrap(), as_file);
    }

    #[tokio::test]
    async fn shared_subtree_dag_diff_is_bounded() {
        let e = engine();
        let leaf = Tree::new(
            vec![Entry {
                name: Name::new("f").unwrap(),
                kind: Kind::File,
                id: *FileId::of(&[1]).as_bytes(),
                size: 1,
                git: blob_oid(&[1]),
            }],
            false,
        )
        .unwrap();
        let mut child = e.put_tree(leaf).await.unwrap();
        // 16 entries per level, all the same child: 16^8 leaves from nine small objects.
        for level in 0..8 {
            let entries = (0..16)
                .map(|i| Entry {
                    name: Name::new(format!("d{i:02}")).unwrap(),
                    kind: Kind::Dir,
                    id: *child.as_bytes(),
                    size: 0,
                    git: blob_oid(b""),
                })
                .collect();
            child = e
                .put_tree(Tree::new(entries, level == 7).unwrap())
                .await
                .unwrap();
        }
        let empty = Engine::<Counting, NoRefs>::empty_tree_id();
        let err = e.leafdiff(empty, child, None).await.unwrap_err();
        // Short paths: the collected-bytes bound trips well before the 4M visits do.
        assert!(
            matches!(err, Error::OutputTooLarge(MAX_DIFF_OUT_BYTES)),
            "{err}"
        );
        let err = e
            .leafdiff_bounded(empty, child, None, 1000)
            .await
            .unwrap_err();
        assert!(matches!(err, Error::TooLarge(1000)));
    }

    /// A tree of `n` entries named `{stem}{index:03}`, each of `kind` pointing at `id`.
    async fn fan(
        e: &Engine<Counting, NoRefs>,
        stem: &str,
        n: usize,
        kind: Kind,
        id: [u8; 32],
    ) -> TreeId {
        let entries = (0..n)
            .map(|i| Entry {
                name: Name::new(format!("{stem}{i:03}")).unwrap(),
                kind,
                id,
                size: u64::from(kind == Kind::File),
                git: blob_oid(b""),
            })
            .collect();
        e.put_tree(Tree::new(entries, false).unwrap())
            .await
            .unwrap()
    }

    /// A tree holding one directory entry `name` that points at `child`.
    async fn dir_with(
        e: &Engine<Counting, NoRefs>,
        name: &str,
        child: TreeId,
        is_root: bool,
    ) -> TreeId {
        let entry = Entry {
            name: Name::new(name).unwrap(),
            kind: Kind::Dir,
            id: *child.as_bytes(),
            size: 0,
            git: blob_oid(b""),
        };
        put_raw(e, Tree::new(vec![entry], is_root).unwrap()).await
    }

    #[tokio::test]
    async fn long_name_dag_diff_is_refused_not_expanded() {
        let e = engine();
        let empty = Engine::<Counting, NoRefs>::empty_tree_id();
        // A few visits, but every leaf would repeat a prefix far past the path bound.
        let files = fan(&e, "f", 64, Kind::File, *FileId::of(&[1]).as_bytes()).await;
        let dirs = fan(&e, "d", 64, Kind::Dir, *files.as_bytes()).await;
        let long = "n".repeat(MAX_WALK_PATH_BYTES + 1);
        let root = dir_with(&e, &long, dirs, true).await;
        let err = e.leafdiff(empty, root, None).await.unwrap_err();
        assert!(matches!(err, Error::PathTooLong(_)), "{err}");
        // The same through a chain of names that are each short but add up.
        let half = "m".repeat(MAX_WALK_PATH_BYTES / 2);
        let inner = dir_with(&e, &half, dirs, false).await;
        let root = dir_with(&e, &half, inner, true).await;
        let err = e.leafdiff(empty, root, None).await.unwrap_err();
        assert!(matches!(err, Error::PathTooLong(_)), "{err}");
        // The walker builds the same paths and refuses them too.
        let walked: Vec<_> = e.walk(root, None).collect().await;
        assert!(matches!(walked.last(), Some(Err(Error::PathTooLong(_)))));
    }

    /// Writes `tree` straight to the store, as a hostile writer or bucket could, bypassing the
    /// name check in `put_tree`. Nothing is cached, so a read decodes it from the store.
    async fn put_raw(e: &Engine<Counting, NoRefs>, tree: Tree) -> TreeId {
        let envelope = tree.to_envelope();
        let id = TreeId::of(&envelope);
        e.objects
            .put(&ObjKey::Tree(id), encode_stored(&envelope))
            .await
            .unwrap();
        id
    }

    /// A stored tree holding `entries`, written straight to the store.
    async fn put(e: &Engine<Counting, NoRefs>, entries: Vec<Entry>, is_root: bool) -> TreeId {
        put_raw(e, Tree::new(entries, is_root).unwrap()).await
    }

    /// `put_tree` is the only other way a `Tree` reaches the cache, so it applies the same name
    /// bound as `get_tree`: a caller cannot park an oversized name in the cache or the store.
    #[tokio::test]
    async fn put_tree_refuses_an_oversized_name() {
        let objs = counting();
        let e = engine_on(objs.clone());
        let entry = |len: usize| Entry {
            name: Name::new("n".repeat(len)).unwrap(),
            kind: Kind::File,
            id: *FileId::of(&[3]).as_bytes(),
            size: 1,
            git: blob_oid(b"n"),
        };
        let big = Tree::new(vec![entry(MAX_WALK_PATH_BYTES + 1)], true).unwrap();
        let id = TreeId::of(&big.to_envelope());
        let err = e.put_tree(big).await.unwrap_err();
        assert!(matches!(err, Error::PathTooLong(_)), "{err}");
        // Refused before anything was stored or cached.
        assert!(matches!(
            e.get_tree(id).await.unwrap_err(),
            Error::MissingTree(_)
        ));
        // A name of exactly the bound still goes in.
        let ok = Tree::new(vec![entry(MAX_WALK_PATH_BYTES)], true).unwrap();
        let id = e.put_tree(ok).await.unwrap();
        e.get_tree(id).await.unwrap();
    }

    /// `build_trie` builds a path per trie node; one longer than the bound is refused before
    /// any tree is read or written.
    #[tokio::test]
    async fn apply_refuses_a_path_past_the_walk_bound() {
        let e = engine();
        let empty = Engine::<Counting, NoRefs>::empty_tree_id();
        let comp = "c".repeat(200);
        let long = vec![comp.as_str(); MAX_WALK_PATH_BYTES / 200 + 1].join("/");
        assert!(long.len() > MAX_WALK_PATH_BYTES);
        let change = Change::write(Kind::File, FileId::of(&[4]), 1, blob_oid(b"p"), None).unwrap();
        let err = e
            .apply(empty, &[(RepoPath::new(long).unwrap(), change)])
            .await
            .unwrap_err();
        assert!(matches!(err, Error::PathTooLong(_)), "{err}");
    }

    /// An oversized name that is skipped, not reported, must still be refused: the diff may not
    /// copy or compare it once per directory pair.
    #[tokio::test]
    async fn an_oversized_name_in_a_skipped_pair_is_refused() {
        let objs = counting();
        let w = engine_on(objs.clone());
        let e_tree = fan(&w, "f", 2, Kind::File, *FileId::of(&[1]).as_bytes()).await;
        let big = Entry {
            name: Name::new("b".repeat(MAX_WALK_PATH_BYTES * 64)).unwrap(),
            kind: Kind::File,
            id: *FileId::of(&[2]).as_bytes(),
            size: 1,
            git: blob_oid(b"big"),
        };
        let shared = |git: &[u8]| Entry {
            name: Name::new("s").unwrap(),
            kind: Kind::Dir,
            id: *e_tree.as_bytes(),
            size: 0,
            git: blob_oid(git),
        };
        // The two trees differ (`git` of "s"), but the pair "s" is skipped on equal ids and the
        // oversized entry is identical on both sides: nothing is ever reported.
        let ta = put(&w, vec![big.clone(), shared(b"a")], false).await;
        let tb = put(&w, vec![big, shared(b"b")], false).await;
        let roots = |t: TreeId| {
            (0..4)
                .map(|i| Entry {
                    name: Name::new(format!("d{i:03}")).unwrap(),
                    kind: Kind::Dir,
                    id: *t.as_bytes(),
                    size: 0,
                    git: blob_oid(b""),
                })
                .collect::<Vec<_>>()
        };
        let (ra, rb) = (
            put(&w, roots(ta), true).await,
            put(&w, roots(tb), true).await,
        );
        // A cold engine decodes the stored trees, as a server reading a hostile bucket would.
        let cold = engine_on(objs);
        let err = cold.leafdiff(ra, rb, None).await.unwrap_err();
        assert!(matches!(err, Error::PathTooLong(_)), "{err}");
    }

    #[tokio::test]
    async fn paths_within_the_bound_still_diff() {
        let e = engine();
        let empty = Engine::<Counting, NoRefs>::empty_tree_id();
        let files = fan(&e, "f", 2, Kind::File, *FileId::of(&[1]).as_bytes()).await;
        let name = "n".repeat(MAX_WALK_PATH_BYTES - 5);
        let root = dir_with(&e, &name, files, true).await;
        let diff = e.leafdiff(empty, root, None).await.unwrap();
        assert_eq!(diff.len(), 2);
        assert_eq!(diff[0].0.as_bytes().len(), MAX_WALK_PATH_BYTES);
    }

    #[tokio::test]
    async fn long_prefix_fan_out_hits_the_output_byte_budget() {
        let e = engine();
        let empty = Engine::<Counting, NoRefs>::empty_tree_id();
        // 256 x 256 leaves from three small trees, each path near the bound: a few hundred
        // thousand visits would pass, but the collected bytes cannot.
        let files = fan(&e, "f", 256, Kind::File, *FileId::of(&[1]).as_bytes()).await;
        let dirs = fan(&e, "d", 256, Kind::Dir, *files.as_bytes()).await;
        let name = "n".repeat(MAX_WALK_PATH_BYTES - 20);
        let root = dir_with(&e, &name, dirs, true).await;
        let err = e.leafdiff(empty, root, None).await.unwrap_err();
        assert!(
            matches!(err, Error::OutputTooLarge(MAX_DIFF_OUT_BYTES)),
            "{err}"
        );
    }

    #[tokio::test]
    async fn gitlink_round_trips() {
        let e = engine();
        let g = Change::Gitlink {
            commit: blob_oid(b"sub"),
        };
        let root = build(&e, &[(p("vendor/sub"), g.clone())]).await;
        let en = e.lookup(root, &p("vendor/sub")).await.unwrap().unwrap();
        assert_eq!((en.kind, en.id, en.size), (Kind::Gitlink, [0; 32], 0));
        let d = e
            .leafdiff(Engine::<Counting, NoRefs>::empty_tree_id(), root, None)
            .await
            .unwrap();
        assert_eq!(d, vec![(p("vendor/sub"), g)]);
    }

    #[tokio::test]
    async fn diff_of_equal_trees_reads_nothing() {
        let objs = counting();
        let e = engine_on(objs.clone());
        let root = build(&e, &[(p("a/b"), file(1))]).await;
        let fresh = engine_on(objs.clone());
        let before = objs.gets.load(SeqCst);
        assert!(fresh.leafdiff(root, root, None).await.unwrap().is_empty());
        assert_eq!(objs.gets.load(SeqCst), before);
    }

    #[tokio::test]
    async fn apply_and_diff_touch_only_the_changed_spine() {
        let objs = counting();
        let e = engine_on(objs.clone());
        let mut leaves = Vec::new();
        for d in 0..30u8 {
            for f in 0..5u8 {
                leaves.push((p(&format!("d{d:02}/sub/f{f}")), file(f)));
            }
        }
        let root = build(&e, &leaves).await;

        let cold = engine_on(objs.clone());
        let (g0, p0) = (objs.gets.load(SeqCst), objs.puts.load(SeqCst));
        let next = cold
            .apply(root, &[(p("d07/sub/f2"), file(200))])
            .await
            .unwrap();
        // Spine: root, d07, d07/sub. Each read once, each rewritten once.
        assert_eq!(objs.gets.load(SeqCst) - g0, 3);
        assert_eq!(objs.puts.load(SeqCst) - p0, 3);

        let cold = engine_on(objs.clone());
        let g1 = objs.gets.load(SeqCst);
        let d = cold.leafdiff(root, next, None).await.unwrap();
        assert_eq!(d, vec![(p("d07/sub/f2"), file(200))]);
        // Two roots, plus both sides of the two changed directories.
        assert_eq!(objs.gets.load(SeqCst) - g1, 6);
    }

    #[tokio::test]
    async fn missing_tree_is_reported() {
        let e = engine();
        let ghost = TreeId::from_bytes([7; 32]);
        assert!(matches!(
            e.get_tree(ghost).await,
            Err(Error::MissingTree(id)) if id == ghost
        ));
    }

    #[tokio::test]
    async fn wide_apply_loads_children_concurrently_within_the_window() {
        let objs = counting();
        let e = engine_on(objs.clone());
        let empty = Engine::<Counting, NoRefs>::empty_tree_id();
        let base: Vec<_> = (0..40)
            .map(|i| (p(&format!("d{i:02}/x")), file(1)))
            .collect();
        let root = e.apply(empty, &base).await.unwrap();

        // A fresh engine has no cached trees, so every child directory is read from the store.
        let fresh = engine_on(objs.clone());
        objs.max_in_flight.store(0, SeqCst);
        let edits: Vec<_> = (0..40)
            .map(|i| (p(&format!("d{i:02}/y")), file(2)))
            .collect();
        let next = fresh.apply(root, &edits).await.unwrap();
        let max = objs.max_in_flight.load(SeqCst);
        assert!(max > 1, "child loads ran one at a time");
        assert!(max <= LOAD_WINDOW, "{max} reads in flight");
        assert_eq!(all_leaves(&fresh, next, None).await.len(), 80);
    }

    #[tokio::test]
    async fn wide_diff_loads_children_concurrently_within_the_window() {
        let objs = counting();
        let e = engine_on(objs.clone());
        let empty = Engine::<Counting, NoRefs>::empty_tree_id();
        let leaves = |tag: u8| -> Vec<(RepoPath, Change)> {
            (0..40)
                .map(|i| (p(&format!("d{i:02}/x")), file(tag)))
                .collect()
        };
        let a = e.apply(empty, &leaves(1)).await.unwrap();
        let b = e.apply(empty, &leaves(2)).await.unwrap();

        let fresh = engine_on(objs.clone());
        objs.max_in_flight.store(0, SeqCst);
        let changes = fresh.leafdiff(a, b, None).await.unwrap();
        assert_eq!(changes.len(), 40);
        let max = objs.max_in_flight.load(SeqCst);
        assert!(max > 1, "child loads ran one at a time");
        assert!(max <= LOAD_WINDOW, "{max} reads in flight");
    }

    fn deep_path(levels: usize) -> RepoPath {
        let mut s = "d/".repeat(levels - 1);
        s.push('f');
        RepoPath::new(s).unwrap()
    }

    /// Runs `f` on a 256 KiB stack, where unbounded recursion would overflow.
    fn on_small_stack(f: impl FnOnce() + Send + 'static) {
        std::thread::Builder::new()
            .stack_size(256 * 1024)
            .spawn(f)
            .unwrap()
            .join()
            .unwrap();
    }

    #[test]
    fn too_deep_apply_is_rejected_and_2000_levels_work_on_a_small_stack() {
        on_small_stack(|| {
            let rt = tokio::runtime::Builder::new_current_thread()
                .build()
                .unwrap();
            rt.block_on(async {
                let e = engine();
                let empty = Engine::<Counting, NoRefs>::empty_tree_id();
                let too_deep = [(deep_path(3000), file(1))];
                assert!(matches!(
                    e.apply(empty, &too_deep).await,
                    Err(Error::TooDeep)
                ));
                let first = e.apply(empty, &[(deep_path(2000), file(1))]).await.unwrap();
                let mut other = "d/".repeat(1999);
                other.push('g');
                let second = e.apply(first, &[(p(&other), file(2))]).await.unwrap();
                assert_eq!(all_leaves(&e, second, None).await.len(), 2);
                assert_eq!(e.leafdiff(empty, first, None).await.unwrap().len(), 1);
                assert_eq!(e.leafdiff(first, second, None).await.unwrap().len(), 1);
                assert_eq!(e.leafdiff(second, empty, None).await.unwrap().len(), 2);
            });
        });
    }

    fn spec(items: &[&str]) -> Pathspec {
        Pathspec::parse(items).unwrap()
    }

    #[test]
    fn pathspec_literal_glob_and_exclude() {
        let s = spec(&["src"]);
        assert!(s.matches(&p("src")) && s.matches(&p("src/a/b.rs")));
        assert!(!s.matches(&p("srcs/a")) && !s.matches(&p("other")));

        let g = spec(&[":(glob)src/**/*.rs"]);
        assert!(g.matches(&p("src/a/b.rs")) && g.matches(&p("src/b.rs")));
        assert!(!g.matches(&p("src/a/b.txt")) && !g.matches(&p("lib/b.rs")));

        let star = spec(&[":(glob)*.rs"]);
        assert!(star.matches(&p("a.rs")), "single star matches top level");
        assert!(!star.matches(&p("d/a.rs")), "single star stops at slash");

        let ex = spec(&["src", ":(exclude)src/gen", ":!src/old.rs"]);
        assert!(ex.matches(&p("src/a.rs")));
        assert!(!ex.matches(&p("src/gen/x.rs")) && !ex.matches(&p("src/old.rs")));

        let only_ex = spec(&[":(exclude)docs"]);
        assert!(only_ex.matches(&p("src/a")) && !only_ex.matches(&p("docs/a")));
    }

    #[test]
    fn pathspec_directory_pruning_is_conservative() {
        let s = spec(&["a/b"]);
        assert!(s.may_contain_dir(&p("a")) && s.may_contain_dir(&p("a/b/c")));
        assert!(!s.may_contain_dir(&p("x")) && !s.may_contain_dir(&p("a/c")));

        let g = spec(&[":(glob)a/*/f.rs"]);
        assert!(g.may_contain_dir(&p("a")) && g.may_contain_dir(&p("a/q")));
        assert!(!g.may_contain_dir(&p("b")), "literal prefix disagrees");
        assert!(!g.may_contain_dir(&p("a/q/r")), "fixed depth exceeded");

        let deep = spec(&[":(glob)**/f.rs"]);
        assert!(deep.may_contain_dir(&p("any/where/deep")));

        let ex = spec(&["src", ":(exclude)src/gen"]);
        assert!(!ex.may_contain_dir(&p("src/gen")) && !ex.may_contain_dir(&p("src/gen/x")));
        assert!(ex.may_contain_dir(&p("src/other")));
    }

    #[test]
    fn pathspec_rejects_bad_items() {
        assert_eq!(Pathspec::parse([""]).err(), Some(PathspecError::Empty));
        assert_eq!(
            Pathspec::parse([":(glob"]).err(),
            Some(PathspecError::Unterminated)
        );
        assert!(matches!(
            Pathspec::parse([":(icase)a"]).err(),
            Some(PathspecError::UnknownMagic(m)) if m == "icase"
        ));
        assert!(matches!(
            Pathspec::parse([":(glob)a[b"]).err(),
            Some(PathspecError::BadGlob { .. })
        ));
    }

    #[tokio::test]
    async fn walk_and_diff_respect_pathspec() {
        let e = engine();
        let a = build(
            &e,
            &[
                (p("src/a.rs"), file(1)),
                (p("src/gen/b.rs"), file(2)),
                (p("docs/c.md"), file(3)),
            ],
        )
        .await;
        let b = e
            .apply(
                a,
                &[
                    (p("src/a.rs"), file(10)),
                    (p("src/gen/b.rs"), file(20)),
                    (p("docs/c.md"), file(30)),
                ],
            )
            .await
            .unwrap();
        let s = spec(&["src", ":(exclude)src/gen"]);
        let walked = all_leaves(&e, a, Some(&s)).await;
        assert_eq!(walked, vec![(p("src/a.rs"), file(1))]);
        let d = e.leafdiff(a, b, Some(&s)).await.unwrap();
        assert_eq!(d, vec![(p("src/a.rs"), file(10))]);
    }

    #[tokio::test]
    async fn walk_streams_in_tree_order_without_pathspec() {
        let e = engine();
        let root = build(
            &e,
            &[
                (p("b"), file(1)),
                (p("a/z"), file(2)),
                (p("a/y"), file(3)),
                (p("a.txt"), file(4)),
            ],
        )
        .await;
        let paths: Vec<String> = e
            .walk(root, None)
            .map_ok(|(p, _)| p.to_string())
            .try_collect()
            .await
            .unwrap();
        // Tree order sorts names, so `a` (and its contents) comes before `a.txt`.
        assert_eq!(paths, ["a/y", "a/z", "a.txt", "b"]);
    }

    // ---------------------------------------------------------------- property tests

    /// Leaves of a generated tree: no path is a directory prefix of another.
    type Model = BTreeMap<RepoPath, Change>;

    fn model_strategy() -> impl Strategy<Value = Model> {
        let comp = prop::sample::select(vec!["a", "b", "c", "d"]);
        let path = prop::collection::vec(comp, 1..4).prop_map(|c| c.join("/"));
        let value = (any::<u8>(), any::<u8>());
        prop::collection::vec((path, value), 0..14).prop_map(|items| {
            let mut m = Model::new();
            for (path, (seed, tag)) in items {
                let rp = p(&path);
                let clash = m
                    .keys()
                    .any(|k| k.starts_with_dir(&rp) || rp.starts_with_dir(k));
                if !clash {
                    m.insert(rp, leaf(seed, tag));
                }
            }
            m
        })
    }

    fn naive_diff(a: &Model, b: &Model) -> Vec<(RepoPath, Change)> {
        let mut out: Vec<_> = a
            .keys()
            .filter(|k| !b.contains_key(*k))
            .map(|k| (k.clone(), Change::Delete))
            .collect();
        out.extend(
            b.iter()
                .filter(|(k, v)| a.get(*k) != Some(*v))
                .map(|(k, v)| (k.clone(), v.clone())),
        );
        out.sort_by(|x, y| x.0.cmp(&y.0));
        out
    }

    fn to_changes(m: &Model) -> Vec<(RepoPath, Change)> {
        m.iter().map(|(k, v)| (k.clone(), v.clone())).collect()
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(64))]

        #[test]
        fn apply_of_leafdiff_reproduces_target(ma in model_strategy(), mb in model_strategy()) {
            let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
            rt.block_on(async {
                let e = engine();
                let a = build(&e, &to_changes(&ma)).await;
                let b = build(&e, &to_changes(&mb)).await;
                let diff = e.leafdiff(a, b, None).await.unwrap();
                prop_assert_eq!(&diff, &naive_diff(&ma, &mb));
                prop_assert_eq!(e.apply(a, &diff).await.unwrap(), b);
                // The tree reads back as the model.
                prop_assert_eq!(all_leaves(&e, b, None).await, to_changes(&mb));
                for (path, change) in &mb {
                    let found = e.lookup(b, path).await.unwrap().unwrap();
                    prop_assert_eq!(entry_change(&found), change.clone());
                }
                Ok(())
            })?;
        }

        #[test]
        fn pathspec_diff_equals_filtered_full_diff(ma in model_strategy(), mb in model_strategy()) {
            let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
            rt.block_on(async {
                let e = engine();
                let a = build(&e, &to_changes(&ma)).await;
                let b = build(&e, &to_changes(&mb)).await;
                for s in [spec(&["a"]), spec(&[":(glob)*/b/*"]), spec(&["b", ":(exclude)b/c"])] {
                    let got = e.leafdiff(a, b, Some(&s)).await.unwrap();
                    let want: Vec<_> = naive_diff(&ma, &mb)
                        .into_iter()
                        .filter(|(path, _)| s.matches(path))
                        .collect();
                    prop_assert_eq!(got, want);
                    let walked = all_leaves(&e, b, Some(&s)).await;
                    let expect: Vec<_> = to_changes(&mb)
                        .into_iter()
                        .filter(|(path, _)| s.matches(path))
                        .collect();
                    prop_assert_eq!(walked, expect);
                }
                Ok(())
            })?;
        }
    }
}
