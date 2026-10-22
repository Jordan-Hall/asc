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
            let here = child_path(acc.as_ref(), &name);
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
        let tree = Arc::new(decode_envelope::<Tree>(&data)?);
        self.trees.insert(id, tree.clone());
        Ok(tree)
    }

    /// Stores a tree and returns its id.
    pub async fn put_tree(&self, tree: Tree) -> Result<TreeId> {
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
        futures::stream::iter(ids)
            .map(|&id| self.get_tree(id))
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
        let mut out = Vec::new();
        if a != b {
            self.diff_dir(a, b, spec, &mut out).await?;
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
    ) -> Result<()> {
        enum Frame {
            Diff {
                ta: Arc<Tree>,
                tb: Arc<Tree>,
                i: usize,
                j: usize,
                prefix: Option<RepoPath>,
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
        }];
        while let Some(top) = stack.last_mut() {
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
                            let (x, y) = (ea[*i].clone(), eb[*j].clone());
                            *i += 1;
                            *j += 1;
                            let both_dirs = x.kind == Kind::Dir && y.kind == Kind::Dir;
                            if x == y || (both_dirs && x.id == y.id) {
                                continue;
                            }
                            let prefix = prefix.clone();
                            if both_dirs {
                                let path = child_path(prefix.as_ref(), &x.name);
                                if spec.is_none_or(|s| s.may_contain_dir(&path)) {
                                    if stack.len() > MAX_TREE_DEPTH {
                                        return Err(Error::TooDeep);
                                    }
                                    let (ta, tb) = futures::try_join!(
                                        self.get_tree(TreeId::from_bytes(x.id)),
                                        self.get_tree(TreeId::from_bytes(y.id))
                                    )?;
                                    stack.push(Frame::Diff {
                                        ta,
                                        tb,
                                        i: 0,
                                        j: 0,
                                        prefix: Some(path),
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
                let path = child_path(prefix.as_ref(), &e.name);
                if e.kind != Kind::Dir {
                    if spec.is_none_or(|s| s.matches(&path)) {
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
                let path = child_path(top.prefix.as_ref(), &entry.name);
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
