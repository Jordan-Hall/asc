//! The FUSE inode table (SPEC §10.3). Inode numbers are allocated sequentially, persisted in
//! `state.db.inodes` through an [`InodeStore`], and never reused while the workspace exists, so
//! they are stable across remounts and match the inode numbers the git index recorded.
//!
//! The live nodes (those the kernel has looked up) form a tree of `(parent, name)` links, so a
//! rename moves one node and everything below it follows. The persisted rows map a path to the
//! inode it had when it was last looked up; a row can go stale below a renamed directory, which
//! only costs a fresh number on the next lookup, never a wrong answer.

use std::collections::HashMap;
use std::io;
use std::sync::{Arc, Mutex, MutexGuard};

use asc_core::path::{MAX_TREE_DEPTH, RepoPath};
use asc_state::StateDb;
use asc_state::WorkspaceId;
use asc_state::state::tables::inodes;
use tokio::runtime::Handle;

use super::block;

/// The root directory's inode number.
pub const ROOT: u64 = 1;
/// Inode numbers reserved per persisted high-water mark.
const BLOCK: u64 = 1024;
/// Rows with an inode at or above this are high-water marks, not paths.
const MARK_BASE: u64 = 1 << 62;
/// Most live nodes kept; the kernel is asked to forget before this is reached in practice.
pub const MAX_LIVE: usize = 4_000_000;

/// Where inode numbers persist.
pub trait InodeStore: Send + Sync {
    fn ino_of(&self, path: &[u8]) -> io::Result<Option<u64>>;
    fn path_of(&self, ino: u64) -> io::Result<Option<Vec<u8>>>;
    /// Fails when the inode or the path already has a row.
    fn assign(&self, ino: u64, path: &[u8]) -> io::Result<()>;
    fn remove(&self, ino: u64) -> io::Result<()>;
}

/// [`InodeStore`] over `state.db`'s `inodes` table.
pub struct DbInodes {
    pub db: StateDb,
    pub ws: WorkspaceId,
    pub rt: Handle,
}

fn io_err(e: asc_state::Error) -> io::Error {
    io::Error::other(e.to_string())
}

impl InodeStore for DbInodes {
    fn ino_of(&self, path: &[u8]) -> io::Result<Option<u64>> {
        block(&self.rt, inodes::ino_of(&self.db, self.ws, path)).map_err(io_err)
    }
    fn path_of(&self, ino: u64) -> io::Result<Option<Vec<u8>>> {
        block(&self.rt, inodes::path_of(&self.db, self.ws, ino)).map_err(io_err)
    }
    fn assign(&self, ino: u64, path: &[u8]) -> io::Result<()> {
        block(&self.rt, inodes::assign(&self.db, self.ws, ino, path)).map_err(io_err)
    }
    fn remove(&self, ino: u64) -> io::Result<()> {
        block(&self.rt, inodes::remove(&self.db, self.ws, ino)).map_err(io_err)
    }
}

/// Rows by inode and by path.
type Rows = (HashMap<u64, Vec<u8>>, HashMap<Vec<u8>, u64>);

/// An in-memory store, for tests and for mounts without a database.
#[derive(Default)]
pub struct MemInodes {
    rows: Mutex<Rows>,
}

impl MemInodes {
    fn rows(&self) -> MutexGuard<'_, Rows> {
        self.rows.lock().unwrap_or_else(|e| e.into_inner())
    }
}

impl InodeStore for MemInodes {
    fn ino_of(&self, path: &[u8]) -> io::Result<Option<u64>> {
        Ok(self.rows().1.get(path).copied())
    }
    fn path_of(&self, ino: u64) -> io::Result<Option<Vec<u8>>> {
        Ok(self.rows().0.get(&ino).cloned())
    }
    fn assign(&self, ino: u64, path: &[u8]) -> io::Result<()> {
        let mut r = self.rows();
        if r.0.contains_key(&ino) || r.1.contains_key(path) {
            return Err(io::Error::from(io::ErrorKind::AlreadyExists));
        }
        r.0.insert(ino, path.to_vec());
        r.1.insert(path.to_vec(), ino);
        Ok(())
    }
    fn remove(&self, ino: u64) -> io::Result<()> {
        let mut r = self.rows();
        if let Some(p) = r.0.remove(&ino) {
            r.1.remove(&p);
        }
        Ok(())
    }
}

struct Ent {
    /// `0` for a node removed from the tree that the kernel still remembers.
    parent: u64,
    name: Vec<u8>,
    lookups: u64,
    children: u32,
}

struct Inner {
    ents: HashMap<u64, Ent>,
    by_name: HashMap<(u64, Vec<u8>), u64>,
    next: u64,
    /// Inode numbers below this have a persisted high-water mark.
    limit: u64,
}

/// The live inode tree over a persistent [`InodeStore`].
pub struct InodeTable {
    store: Arc<dyn InodeStore>,
    inner: Mutex<Inner>,
}

fn mark_path(i: u64) -> Vec<u8> {
    format!("\0hw{i}").into_bytes()
}

impl InodeTable {
    /// Opens the table, resuming after every number ever handed out.
    pub fn open(store: Arc<dyn InodeStore>) -> io::Result<Self> {
        let have = |i: u64| -> io::Result<bool> { Ok(store.path_of(MARK_BASE + i)?.is_some()) };
        // Marks are contiguous from 0: gallop to a missing one, then bisect.
        let mut hi = 1u64;
        while have(hi - 1)? {
            hi = hi
                .checked_mul(2)
                .ok_or_else(|| io::Error::other("inode marks"))?;
        }
        let (mut lo, mut hi) = (hi / 2, hi - 1);
        // `lo` marks are present (or 0), the mark at index `hi` is absent.
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            if have(mid)? {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        let marks = lo;
        let next = (marks * BLOCK).max(ROOT + 1);
        let t = Self {
            store,
            inner: Mutex::new(Inner {
                ents: HashMap::new(),
                by_name: HashMap::new(),
                next,
                limit: marks * BLOCK,
            }),
        };
        t.lock().ents.insert(
            ROOT,
            Ent {
                parent: 0,
                name: Vec::new(),
                lookups: 1,
                children: 0,
            },
        );
        Ok(t)
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The next never-used number, persisting a new high-water mark when the block is spent.
    fn allocate(&self, g: &mut Inner) -> io::Result<u64> {
        if g.next >= g.limit {
            let i = g.limit / BLOCK;
            self.store.assign(MARK_BASE + i, &mark_path(i))?;
            g.limit += BLOCK;
        }
        let ino = g.next;
        g.next += 1;
        Ok(ino)
    }

    /// The path bytes of a live node (empty for the root); `None` for an unknown or removed one.
    pub fn path_of(&self, ino: u64) -> Option<Vec<u8>> {
        let g = self.lock();
        Self::path_in(&g, ino)
    }

    fn path_in(g: &Inner, ino: u64) -> Option<Vec<u8>> {
        let mut parts: Vec<&[u8]> = Vec::new();
        let mut cur = ino;
        while cur != ROOT {
            let e = g.ents.get(&cur)?;
            if e.parent == 0 || parts.len() >= MAX_TREE_DEPTH {
                return None;
            }
            parts.push(&e.name);
            cur = e.parent;
        }
        parts.reverse();
        Some(parts.join(&b'/'))
    }

    /// The parent of a live node (`None` for the root and for removed nodes).
    pub fn parent_of(&self, ino: u64) -> Option<u64> {
        self.lock()
            .ents
            .get(&ino)
            .map(|e| e.parent)
            .filter(|&p| p != 0)
    }

    /// The live node `name` of `parent`, without creating one.
    pub fn peek(&self, parent: u64, name: &[u8]) -> Option<u64> {
        self.lock().by_name.get(&(parent, name.to_vec())).copied()
    }

    /// The live node at `path`, without creating one.
    pub fn find(&self, path: &RepoPath) -> Option<u64> {
        let g = self.lock();
        let mut cur = ROOT;
        for c in path.components() {
            cur = *g.by_name.get(&(cur, c.to_vec()))?;
        }
        Some(cur)
    }

    /// The inode of `name` in `parent`, assigned on first sight, counting one kernel lookup.
    /// `None` when `parent` is not a live node.
    pub fn lookup(&self, parent: u64, name: &[u8]) -> io::Result<Option<u64>> {
        let mut g = self.lock();
        if let Some(&ino) = g.by_name.get(&(parent, name.to_vec())) {
            if let Some(e) = g.ents.get_mut(&ino) {
                e.lookups += 1;
            }
            return Ok(Some(ino));
        }
        if g.ents.len() >= MAX_LIVE {
            return Err(io::Error::other("too many inodes"));
        }
        let Some(pp) = Self::path_in(&g, parent) else {
            return Ok(None);
        };
        let mut bytes = pp;
        if !bytes.is_empty() {
            bytes.push(b'/');
        }
        bytes.extend_from_slice(name);
        let ino = match self.store.ino_of(&bytes)? {
            Some(i) if !g.ents.contains_key(&i) => i,
            stale => {
                if let Some(i) = stale {
                    // The row belongs to a node that has since moved: free the path.
                    self.store.remove(i)?;
                }
                let i = self.allocate(&mut g)?;
                self.store.assign(i, &bytes)?;
                i
            }
        };
        g.ents.insert(
            ino,
            Ent {
                parent,
                name: name.to_vec(),
                lookups: 1,
                children: 0,
            },
        );
        g.by_name.insert((parent, name.to_vec()), ino);
        if let Some(p) = g.ents.get_mut(&parent) {
            p.children += 1;
        }
        Ok(Some(ino))
    }

    /// The kernel forgets `n` lookups of `ino`.
    pub fn forget(&self, ino: u64, n: u64) {
        let mut g = self.lock();
        let Some(e) = g.ents.get_mut(&ino) else {
            return;
        };
        e.lookups = e.lookups.saturating_sub(n);
        let mut cur = ino;
        // Drop the node, then any ancestor that nothing keeps alive any more.
        while cur != ROOT {
            let Some(e) = g.ents.get(&cur) else { break };
            if e.lookups > 0 || e.children > 0 {
                break;
            }
            let (parent, name) = (e.parent, e.name.clone());
            g.ents.remove(&cur);
            if parent == 0 {
                break;
            }
            g.by_name.remove(&(parent, name));
            match g.ents.get_mut(&parent) {
                Some(p) => p.children = p.children.saturating_sub(1),
                None => break,
            }
            cur = parent;
        }
    }

    /// `name` of `parent` is gone (unlink, rmdir, or replaced by a rename). Its number is not
    /// reused; the kernel's remaining lookups drain through [`Self::forget`].
    pub fn unlink(&self, parent: u64, name: &[u8]) -> io::Result<()> {
        let mut g = self.lock();
        let Some(ino) = g.by_name.remove(&(parent, name.to_vec())) else {
            return Ok(());
        };
        if let Some(e) = g.ents.get_mut(&ino) {
            e.parent = 0;
        }
        if let Some(p) = g.ents.get_mut(&parent) {
            p.children = p.children.saturating_sub(1);
        }
        self.store.remove(ino)
    }

    /// Moves a node. A node already at the target is unlinked first. Returns the moved
    /// inode, which keeps its number.
    pub fn rename(
        &self,
        parent: u64,
        name: &[u8],
        new_parent: u64,
        new_name: &[u8],
    ) -> io::Result<Option<u64>> {
        {
            let g = self.lock();
            if !g.by_name.contains_key(&(parent, name.to_vec())) {
                return Ok(None);
            }
        }
        self.unlink(new_parent, new_name)?;
        let mut g = self.lock();
        let Some(ino) = g.by_name.remove(&(parent, name.to_vec())) else {
            return Ok(None);
        };
        if let Some(p) = g.ents.get_mut(&parent) {
            p.children = p.children.saturating_sub(1);
        }
        if let Some(e) = g.ents.get_mut(&ino) {
            e.parent = new_parent;
            e.name = new_name.to_vec();
        }
        g.by_name.insert((new_parent, new_name.to_vec()), ino);
        if let Some(p) = g.ents.get_mut(&new_parent) {
            p.children += 1;
        }
        self.store.remove(ino)?;
        if let Some(path) = Self::path_in(&g, ino) {
            // A stale row for the target path would block the assignment.
            if let Some(old) = self.store.ino_of(&path)? {
                self.store.remove(old)?;
            }
            self.store.assign(ino, &path)?;
        }
        Ok(Some(ino))
    }

    /// Live node count, for tests and statistics.
    pub fn live(&self) -> usize {
        self.lock().ents.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table(store: &Arc<MemInodes>) -> InodeTable {
        InodeTable::open(store.clone()).unwrap()
    }

    fn rp(s: &str) -> RepoPath {
        RepoPath::new(s).unwrap()
    }

    #[test]
    fn numbers_are_sequential_and_stable_across_reopen() {
        let store = Arc::new(MemInodes::default());
        let t = table(&store);
        let a = t.lookup(ROOT, b"a").unwrap().unwrap();
        let b = t.lookup(ROOT, b"b").unwrap().unwrap();
        let ab = t.lookup(a, b"x").unwrap().unwrap();
        assert_eq!((a, b, ab), (2, 3, 4));
        assert_eq!(t.lookup(ROOT, b"a").unwrap(), Some(a));
        assert_eq!(t.path_of(ab).unwrap(), b"a/x");
        drop(t);

        let t = table(&store);
        // Looked up in another order, the same paths get the same numbers.
        let ab2 = {
            let a2 = t.lookup(ROOT, b"a").unwrap().unwrap();
            assert_eq!(a2, a);
            t.lookup(a2, b"x").unwrap().unwrap()
        };
        assert_eq!(ab2, ab);
        // A new path never reuses a number handed out before the restart.
        let c = t.lookup(ROOT, b"c").unwrap().unwrap();
        assert!(c > ab, "{c}");
    }

    #[test]
    fn removed_numbers_are_not_reused() {
        let store = Arc::new(MemInodes::default());
        let t = table(&store);
        let a = t.lookup(ROOT, b"a").unwrap().unwrap();
        t.unlink(ROOT, b"a").unwrap();
        assert!(t.path_of(a).is_none());
        let again = t.lookup(ROOT, b"a").unwrap().unwrap();
        assert_ne!(a, again);
        drop(t);
        let t = table(&store);
        let c = t.lookup(ROOT, b"zz").unwrap().unwrap();
        assert!(c > again);
    }

    #[test]
    fn marks_survive_many_blocks() {
        let store = Arc::new(MemInodes::default());
        let t = table(&store);
        let mut last = 0;
        for i in 0..(BLOCK * 3 + 5) {
            last = t.lookup(ROOT, format!("f{i}").as_bytes()).unwrap().unwrap();
        }
        drop(t);
        let t = table(&store);
        let n = t.lookup(ROOT, b"new").unwrap().unwrap();
        assert!(n > last, "{n} {last}");
    }

    #[test]
    fn rename_moves_a_subtree_and_keeps_numbers() {
        let store = Arc::new(MemInodes::default());
        let t = table(&store);
        let d = t.lookup(ROOT, b"d").unwrap().unwrap();
        let f = t.lookup(d, b"f").unwrap().unwrap();
        let e = t.lookup(ROOT, b"e").unwrap().unwrap();
        assert_eq!(t.rename(ROOT, b"d", ROOT, b"e").unwrap(), Some(d));
        // The target was replaced; its number is orphaned.
        assert!(t.path_of(e).is_none());
        assert_eq!(t.path_of(d).unwrap(), b"e");
        assert_eq!(t.path_of(f).unwrap(), b"e/f");
        assert_eq!(t.find(&rp("e/f")), Some(f));
        assert_eq!(t.peek(ROOT, b"d"), None);
        assert_eq!(store.ino_of(b"e").unwrap(), Some(d));
    }

    #[test]
    fn a_stale_row_below_a_moved_directory_gets_a_fresh_number() {
        let store = Arc::new(MemInodes::default());
        let t = table(&store);
        let d = t.lookup(ROOT, b"d").unwrap().unwrap();
        let f = t.lookup(d, b"f").unwrap().unwrap();
        t.rename(ROOT, b"d", ROOT, b"e").unwrap();
        // `d/f` still has a row pointing at the live node under `e`.
        let d2 = t.lookup(ROOT, b"d").unwrap().unwrap();
        assert_ne!(d2, d);
        let f2 = t.lookup(d2, b"f").unwrap().unwrap();
        assert_ne!(f2, f);
        assert_eq!(t.path_of(f).unwrap(), b"e/f");
        assert_eq!(t.path_of(f2).unwrap(), b"d/f");
    }

    #[test]
    fn forget_drops_unreferenced_nodes_and_keeps_ancestors_of_live_ones() {
        let store = Arc::new(MemInodes::default());
        let t = table(&store);
        let a = t.lookup(ROOT, b"a").unwrap().unwrap();
        let b = t.lookup(a, b"b").unwrap().unwrap();
        t.forget(a, 1);
        // `a` stays: `b` is below it.
        assert_eq!(t.path_of(b).unwrap(), b"a/b");
        t.forget(b, 1);
        assert_eq!(t.live(), 1);
        // Forgotten nodes keep their persisted number.
        assert_eq!(t.lookup(ROOT, b"a").unwrap(), Some(a));
    }

    #[test]
    fn lookup_below_an_unknown_parent_is_none() {
        let t = table(&Arc::new(MemInodes::default()));
        assert_eq!(t.lookup(99, b"x").unwrap(), None);
    }
}
