//! The merged file system (SPEC §10.3): the base tree served through [`CoreFs`], with the user's
//! changes in the [`Overlay`]. A path resolves to the overlay entry when there is one, and
//! otherwise to the base entry unless a deletion marker hides it. Reads of clean files go
//! straight to the local content cache, writes copy the file up first, and every change is
//! journaled as it happens, so the journal is exact and needs no reconcile.
//!
//! The FUSE and NFS front ends call the operations here and translate the answers to their
//! protocol; what is specific to one kernel (passthrough, invalidation) goes through [`Notify`].
//!
//! Locking: `ns` serialises every change to the namespace (create, unlink, rename, switch), so
//! the overlay and its deletion markers never disagree. Reads take no namespace lock.

use std::collections::{HashMap, HashSet};
use std::ffi::OsStr;
use std::fs::{self, File};
use std::future::Future;
use std::io::{self, Write};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::{FileExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, RwLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use asc_core::object::Kind;
use asc_core::path::{MAX_TREE_DEPTH, Name, RepoPath, checkout_safe};
use asc_store::cachedir::CacheDir;
use asc_wc::SafeRoot;
use asc_wc::journal::Op;
use asc_wc::redirect::Redirections;
use asc_wc::switch::Plan;
use asc_wc::vstatus::{DirEntry, DiskKind, DiskStat, DiskState, PathState};
use tokio::runtime::Handle;

use super::inodes::{InodeTable, ROOT};
use super::overlay::{MAX_CHILDREN, Overlay};
use super::{Attr, Config, Errno, JournalSink, NoNotify, Notify, TimeSource, VKind, block};
use crate::core::{CoreFs, DehydrateOutcome, Node, Result, SwitchOutcome, VfsError};
use crate::policy::Decision;

/// How long a kernel may cache entries and the attributes of clean (tree) entries.
pub const TTL_BASE: Duration = Duration::from_secs(3600);
/// Longest name the file system accepts.
pub const MAX_NAME: usize = 255;
/// Longest symlink target accepted or served.
pub const MAX_LINK: usize = 4096;
/// Most open files at once.
const MAX_HANDLES: usize = 1 << 20;
/// Most entries one directory rename copies up before it refuses with `EXDEV`.
const MAX_COPYUP: usize = 200_000;
/// Longest read served at once.
pub const MAX_READ: usize = 4 << 20;
/// Block size reported by `stat`.
pub const BLKSIZE: u32 = 4096;

pub(crate) type Res<T> = std::result::Result<T, Errno>;

pub(crate) fn errno(e: io::Error) -> Errno {
    Errno::from_io_error(&e).unwrap_or(Errno::IO)
}

pub(crate) fn vfs_errno(e: VfsError) -> Errno {
    match e {
        VfsError::Denied(_) => Errno::ACCESS,
        VfsError::NotFound(_) => Errno::NOENT,
        VfsError::NotADir(_) => Errno::NOTDIR,
        VfsError::NotAFile(_) => Errno::ISDIR,
        VfsError::TooLarge(_) => Errno::FBIG,
        VfsError::Io(e) => errno(e),
        _ => Errno::IO,
    }
}

/// An [`Errno`] as an I/O error, for the provider surface.
pub(crate) fn io_of(e: Errno) -> io::Error {
    io::Error::from_raw_os_error(e.raw_os_error())
}

pub(crate) fn sys_time(secs: i64, nanos: i64) -> SystemTime {
    let n = u32::try_from(nanos.clamp(0, 999_999_999)).unwrap_or(0);
    if secs >= 0 {
        UNIX_EPOCH + Duration::new(secs.unsigned_abs(), n)
    } else {
        UNIX_EPOCH - Duration::from_secs(secs.unsigned_abs()) + Duration::from_nanos(u64::from(n))
    }
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// The deletion markers, mirrored in memory so a directory listing does not stat one marker
/// per entry.
#[derive(Default)]
struct Whites {
    set: RwLock<HashSet<Vec<u8>>>,
}

impl Whites {
    fn load(overlay: &Overlay) -> io::Result<Self> {
        let set = overlay
            .whiteouts()?
            .into_iter()
            .map(|p| p.as_bytes().to_vec())
            .collect();
        Ok(Self {
            set: RwLock::new(set),
        })
    }

    fn is_white(&self, p: &RepoPath) -> bool {
        self.set
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .contains(p.as_bytes())
    }

    /// `p` or an ancestor of it carries a marker.
    fn hides(&self, p: &RepoPath) -> bool {
        let set = self.set.read().unwrap_or_else(|e| e.into_inner());
        let mut cur = p.as_bytes();
        loop {
            if set.contains(cur) {
                return true;
            }
            match cur.iter().rposition(|&b| b == b'/') {
                Some(i) => cur = &cur[..i],
                None => return false,
            }
        }
    }

    fn insert(&self, p: &RepoPath) {
        self.set
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .insert(p.as_bytes().to_vec());
    }

    fn remove(&self, p: &RepoPath) {
        self.set
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .remove(p.as_bytes());
    }
}

#[derive(Clone)]
pub(crate) enum Src {
    /// An entry of the base tree.
    Base(Node),
    /// A real entry of the overlay.
    Over(fs::Metadata),
    /// A redirected name, served as a symlink.
    Link,
}

/// What a path is in the merged view.
#[derive(Clone)]
pub(crate) struct Found {
    pub(crate) path: RepoPath,
    pub(crate) src: Src,
}

impl Found {
    pub(crate) fn kind(&self) -> VKind {
        match &self.src {
            Src::Base(n) => match n.kind {
                Kind::Dir | Kind::Gitlink => VKind::Dir,
                Kind::Symlink => VKind::Symlink,
                _ => VKind::File,
            },
            Src::Over(m) => {
                if m.is_dir() {
                    VKind::Dir
                } else if m.file_type().is_symlink() {
                    VKind::Symlink
                } else {
                    VKind::File
                }
            }
            Src::Link => VKind::Symlink,
        }
    }

    /// Below (or at) a composed or empty gitlink: writes are denied.
    pub(crate) fn read_only(&self) -> bool {
        matches!(&self.src, Src::Base(n) if n.read_only || n.kind == Kind::Gitlink)
    }

    pub(crate) fn in_overlay(&self) -> bool {
        matches!(self.src, Src::Over(_))
    }
}

/// An open file.
pub(crate) struct OpenFile {
    pub(crate) file: File,
    pub(crate) writable: bool,
}

/// What [`MergedFs::open`] opened.
pub(crate) struct Opened {
    pub(crate) fh: u64,
    /// A clean base file: the front end may hand it to the kernel (passthrough) or cache it.
    pub(crate) base: bool,
}

/// A directory listing fixed at `opendir`, so offsets stay valid while the directory changes.
pub(crate) struct Snapshot {
    /// `.` and `..` first (with no entry), then the merged children in name order.
    pub(crate) entries: Vec<(Vec<u8>, Option<Found>)>,
    pub(crate) parent: u64,
}

/// The merged view and every operation on it.
pub struct MergedFs {
    pub(crate) core: Arc<CoreFs>,
    local: Option<CacheDir>,
    pub(crate) table: InodeTable,
    pub(crate) overlay: Overlay,
    redirect: SafeRoot,
    reds: Redirections,
    times: Arc<dyn TimeSource>,
    journal: Arc<dyn JournalSink>,
    rt: Handle,
    root_time: i64,
    pub(crate) uid: u32,
    pub(crate) gid: u32,
    whites: Whites,
    pub(crate) ns: Mutex<()>,
    handles: Mutex<HashMap<u64, Arc<OpenFile>>>,
    pub(crate) dirs: Mutex<HashMap<u64, Arc<Snapshot>>>,
    next_fh: AtomicU64,
    notify: RwLock<Arc<dyn Notify>>,
}

pub(crate) fn join(dir: Option<&RepoPath>, name: &Name) -> RepoPath {
    match dir {
        Some(d) => d.join(name),
        None => RepoPath::new(name.as_bytes().to_vec()).expect("a name is a path"),
    }
}

impl MergedFs {
    /// Opens the overlay and the inode table.
    pub fn new(cfg: Config) -> io::Result<Self> {
        let overlay = Overlay::open(&cfg.overlay)?;
        fs::create_dir_all(&cfg.redirect)?;
        let meta = fs::metadata(overlay.root())?;
        let whites = Whites::load(&overlay)?;
        Ok(Self {
            core: cfg.core,
            local: cfg.local,
            table: InodeTable::open(cfg.inodes)?,
            overlay,
            redirect: SafeRoot::new(cfg.redirect),
            reds: cfg.redirections,
            times: cfg.times,
            journal: cfg.journal,
            rt: cfg.rt,
            root_time: cfg.root_time,
            uid: meta.uid(),
            gid: meta.gid(),
            whites,
            ns: Mutex::new(()),
            handles: Mutex::new(HashMap::new()),
            dirs: Mutex::new(HashMap::new()),
            next_fh: AtomicU64::new(1),
            notify: RwLock::new(Arc::new(NoNotify)),
        })
    }

    /// Who hears about changes made behind the kernel's back.
    pub fn set_notify(&self, n: Arc<dyn Notify>) {
        *self.notify.write().unwrap_or_else(|e| e.into_inner()) = n;
    }

    fn notify(&self) -> Arc<dyn Notify> {
        self.notify
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// The file system core, for tests and statistics.
    pub fn core(&self) -> &Arc<CoreFs> {
        &self.core
    }

    /// Bytes of regular files the overlay holds.
    pub fn overlay_bytes(&self) -> io::Result<u64> {
        self.overlay.bytes()
    }

    /// The paths the user deleted.
    pub fn whiteouts(&self) -> io::Result<Vec<RepoPath>> {
        self.overlay.whiteouts()
    }

    /// The files and directories the user created or changed, as overlay paths, for conversion.
    pub fn overlay_root(&self) -> &Path {
        self.overlay.root()
    }

    pub(crate) fn blk<F>(&self, f: F) -> F::Output
    where
        F: Future + Send,
        F::Output: Send,
    {
        block(&self.rt, f)
    }

    pub(crate) fn handle(&self, fh: u64) -> Res<Arc<OpenFile>> {
        lock(&self.handles).get(&fh).cloned().ok_or(Errno::BADF)
    }

    /// Forgets every open file and listing (on unmount).
    pub(crate) fn drop_handles(&self) {
        lock(&self.handles).clear();
        lock(&self.dirs).clear();
    }

    // ----- paths and the merged view -----

    /// The path of a live inode: `None` is the root.
    pub(crate) fn loc(&self, ino: u64) -> Res<Option<RepoPath>> {
        if ino == ROOT {
            return Ok(None);
        }
        let b = self.table.path_of(ino).ok_or(Errno::STALE)?;
        RepoPath::new(b).map(Some).map_err(|_| Errno::INVAL)
    }

    /// Validates `name` and joins it to the directory `parent`.
    pub(crate) fn child(&self, parent: u64, name: &[u8]) -> Res<(Option<RepoPath>, RepoPath)> {
        if name.is_empty()
            || name == b"."
            || name == b".."
            || name.contains(&b'/')
            || name.contains(&0)
        {
            return Err(Errno::INVAL);
        }
        if name.len() > MAX_NAME {
            return Err(Errno::NAMETOOLONG);
        }
        let pp = self.loc(parent)?;
        let n = Name::new(name.to_vec()).map_err(|_| Errno::INVAL)?;
        if let Some(p) = &pp
            && p.components().count() + 1 > MAX_TREE_DEPTH
        {
            return Err(Errno::NAMETOOLONG);
        }
        let p = join(pp.as_ref(), &n);
        Ok((pp, p))
    }

    fn is_redirect_node(&self, p: &RepoPath) -> bool {
        self.reds.is_redirected_name(p.file_name().as_bytes())
            && p.parent().is_none_or(|q| !self.reds.covers(&q))
    }

    /// The base entry at `p`, unless a deletion marker hides it.
    pub(crate) fn base(&self, p: &RepoPath) -> Res<Option<Node>> {
        if self.whites.hides(p) {
            return Ok(None);
        }
        self.blk(self.core.resolve(p)).map_err(vfs_errno)
    }

    /// Whether `ino` is a file of the base tree that nobody wrote to: its bytes are the tree's,
    /// so a caller that knows the tree need not read (and fetch) them.
    pub(crate) fn projected(&self, ino: u64) -> Res<bool> {
        let Some(p) = self.loc(ino)? else {
            return Ok(false);
        };
        self.projected_path(&p)
    }

    /// [`Self::projected`] by path.
    pub fn projected_path(&self, p: &RepoPath) -> Res<bool> {
        Ok(
            matches!(self.view(p)?, Some(Found { src: Src::Base(n), .. })
            if !matches!(n.kind, Kind::Dir | Kind::Gitlink)),
        )
    }

    pub(crate) fn view(&self, p: &RepoPath) -> Res<Option<Found>> {
        if self.is_redirect_node(p) {
            return Ok(Some(Found {
                path: p.clone(),
                src: Src::Link,
            }));
        }
        if self.reds.covers(p) {
            return Ok(None);
        }
        if let Some(m) = self.overlay.meta(p).map_err(errno)? {
            let ft = m.file_type();
            if ft.is_dir() || ft.is_file() || ft.is_symlink() {
                return Ok(Some(Found {
                    path: p.clone(),
                    src: Src::Over(m),
                }));
            }
            return Ok(None);
        }
        Ok(self.base(p)?.map(|n| Found {
            path: p.clone(),
            src: Src::Base(n),
        }))
    }

    /// The directory `dir` (`None` is the root) must exist and be writable.
    fn writable_dir(&self, dir: Option<&RepoPath>) -> Res<()> {
        let Some(d) = dir else { return Ok(()) };
        let f = self.view(d)?.ok_or(Errno::NOENT)?;
        if f.kind() != VKind::Dir {
            return Err(Errno::NOTDIR);
        }
        if f.read_only() || matches!(f.src, Src::Link) {
            return Err(Errno::ACCESS);
        }
        Ok(())
    }

    /// Checks that `p` may be created below `pp`.
    fn check_new(&self, pp: Option<&RepoPath>, p: &RepoPath) -> Res<()> {
        if !checkout_safe(p) {
            return Err(Errno::PERM);
        }
        self.writable_dir(pp)?;
        if self.reds.covers(p) || self.view(p)?.is_some() {
            return Err(Errno::EXIST);
        }
        Ok(())
    }

    fn base_time(&self, p: &RepoPath) -> i64 {
        self.times
            .times(std::slice::from_ref(p))
            .ok()
            .and_then(|v| v.first().copied())
            .unwrap_or(self.root_time)
    }

    pub(crate) fn redirect_target(&self, p: &RepoPath) -> PathBuf {
        self.redirect
            .root()
            .join(Path::new(OsStr::from_bytes(p.as_bytes())))
    }

    /// Creates the directory a redirected name points to (on first lookup).
    fn ensure_redirect(&self, p: &RepoPath) -> Res<()> {
        use std::os::unix::fs::DirBuilderExt;
        let abs = self.redirect.resolve(p).ok_or(Errno::ACCESS)?;
        match fs::DirBuilder::new()
            .recursive(true)
            .mode(0o755)
            .create(&abs)
        {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Ok(()),
            Err(e) => Err(errno(e)),
        }
    }

    pub(crate) fn attr(&self, ino: u64, f: &Found) -> Attr {
        let kind = f.kind();
        let blocks = |size: u64| size.div_ceil(512);
        let (size, perm, atime, mtime, ctime, ttl) = match &f.src {
            Src::Base(n) => {
                let t = sys_time(self.base_time(&f.path), 0);
                let size = if kind == VKind::Dir {
                    u64::from(BLKSIZE)
                } else {
                    n.size
                };
                let perm: u16 = match (n.kind, kind) {
                    (Kind::Exec, _) => 0o755,
                    (Kind::Gitlink, _) => 0o555,
                    (_, VKind::Dir) => 0o755,
                    (_, VKind::Symlink) => 0o777,
                    _ => 0o644,
                };
                (size, perm, t, t, t, TTL_BASE)
            }
            Src::Over(m) => {
                let size = if kind == VKind::Dir {
                    u64::from(BLKSIZE)
                } else {
                    m.len()
                };
                (
                    size,
                    u16::try_from(m.mode() & 0o7777).unwrap_or(0o644),
                    sys_time(m.atime(), m.atime_nsec()),
                    sys_time(m.mtime(), m.mtime_nsec()),
                    sys_time(m.ctime(), m.ctime_nsec()),
                    Duration::ZERO,
                )
            }
            Src::Link => {
                let t = sys_time(self.root_time, 0);
                let size = self.redirect_target(&f.path).as_os_str().len() as u64;
                (size, 0o777, t, t, t, TTL_BASE)
            }
        };
        Attr {
            ino,
            size,
            blocks: blocks(size),
            atime,
            mtime,
            ctime,
            kind,
            perm,
            nlink: if kind == VKind::Dir { 2 } else { 1 },
            uid: self.uid,
            gid: self.gid,
            blksize: BLKSIZE,
            ttl,
        }
    }

    pub(crate) fn root_attr(&self) -> Attr {
        let t = sys_time(self.root_time, 0);
        Attr {
            ino: ROOT,
            size: u64::from(BLKSIZE),
            blocks: 8,
            atime: t,
            mtime: t,
            ctime: t,
            kind: VKind::Dir,
            perm: 0o755,
            nlink: 2,
            uid: self.uid,
            gid: self.gid,
            blksize: BLKSIZE,
            ttl: TTL_BASE,
        }
    }

    /// The merged children of a directory, in name order, at most `limit` of them.
    pub(crate) fn merged(
        &self,
        dir: Option<&RepoPath>,
        limit: usize,
    ) -> Res<Vec<(Vec<u8>, Found)>> {
        let limit = limit.min(MAX_CHILDREN);
        let mut out: Vec<(Vec<u8>, Found)> = Vec::new();
        let mut seen: HashSet<Vec<u8>> = HashSet::new();
        for (name, _) in self.overlay.children(dir).map_err(errno)? {
            if out.len() >= limit {
                break;
            }
            let Ok(n) = Name::new(name.clone()) else {
                continue;
            };
            let child = join(dir, &n);
            if let Some(m) = self.overlay.meta(&child).map_err(errno)? {
                let ft = m.file_type();
                if ft.is_dir() || ft.is_file() || ft.is_symlink() {
                    seen.insert(name.clone());
                    out.push((
                        name,
                        Found {
                            path: child,
                            src: Src::Over(m),
                        },
                    ));
                }
            }
        }
        let base_dir = match dir {
            None => true,
            Some(d) => self
                .base(d)?
                .is_some_and(|n| matches!(n.kind, Kind::Dir | Kind::Gitlink)),
        };
        if base_dir && let Some(l) = self.blk(self.core.read_dir(dir)).map_err(vfs_errno)? {
            for n in l.entries {
                if out.len() >= limit {
                    break;
                }
                let name = n.path.file_name().as_bytes().to_vec();
                if seen.contains(&name) || self.whites.is_white(&n.path) {
                    continue;
                }
                let path = n.path.clone();
                out.push((
                    name,
                    Found {
                        path,
                        src: Src::Base(n),
                    },
                ));
            }
        }
        // Redirected names that exist show up as symlinks.
        let rd = match dir {
            None => Some(self.redirect.root().to_owned()),
            Some(d) => self.redirect.resolve(d),
        };
        if let Some(rd) = rd
            && let Ok(it) = fs::read_dir(rd)
        {
            for e in it.flatten().take(MAX_CHILDREN) {
                let name = e.file_name().as_bytes().to_vec();
                if out.len() >= limit
                    || !self.reds.is_redirected_name(&name)
                    || out.iter().any(|(n, _)| *n == name)
                    || !e.file_type().is_ok_and(|t| t.is_dir())
                {
                    continue;
                }
                if let Ok(n) = Name::new(name.clone()) {
                    out.push((
                        name,
                        Found {
                            path: join(dir, &n),
                            src: Src::Link,
                        },
                    ));
                }
            }
        }
        out.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(out)
    }

    fn is_empty_dir(&self, p: &RepoPath) -> Res<bool> {
        Ok(self.merged(Some(p), 1)?.is_empty())
    }

    // ----- content -----

    /// Streams the verified content of a base file into a new `tmp/` file of the overlay.
    fn stage(&self, n: &Node) -> Res<(File, PathBuf)> {
        let c = n.content().ok_or(Errno::ISDIR)?;
        let (mut f, tmp) = self.overlay.new_tmp().map_err(errno)?;
        let r = (|| -> Res<()> {
            let mut s = self.blk(self.core.read_content(c)).map_err(vfs_errno)?;
            let mut total = 0u64;
            while let Some(b) = self.blk(s.next()).map_err(vfs_errno)? {
                total += b.len() as u64;
                if total > c.size {
                    return Err(Errno::IO);
                }
                f.write_all(&b).map_err(errno)?;
            }
            if total != c.size {
                return Err(Errno::IO);
            }
            Ok(())
        })();
        if let Err(e) = r {
            let _ = fs::remove_file(&tmp);
            return Err(e);
        }
        Ok((f, tmp))
    }

    fn copy_mode(n: &Node) -> u32 {
        if n.kind == Kind::Exec { 0o755 } else { 0o644 }
    }

    /// Writes the base entry `n` (a file or symlink) into the overlay at `p`. The caller holds
    /// `ns` and has checked that the overlay has nothing at `p`.
    fn copy_up_locked(&self, p: &RepoPath, n: &Node) -> Res<()> {
        if n.kind == Kind::Symlink {
            let c = n.content().ok_or(Errno::IO)?;
            let s = self.blk(self.core.read_content(c)).map_err(vfs_errno)?;
            let target = self.blk(s.read_all(MAX_LINK as u64)).map_err(vfs_errno)?;
            return self
                .overlay
                .make_symlink(p, Path::new(OsStr::from_bytes(&target)))
                .map_err(errno);
        }
        let (_f, tmp) = self.stage(n)?;
        self.overlay
            .install(&tmp, p, Self::copy_mode(n))
            .map_err(|e| {
                let _ = fs::remove_file(&tmp);
                errno(e)
            })
    }

    /// Makes `p` a file of the overlay (a copy-up of the base file), returning when it is.
    fn materialise(&self, p: &RepoPath, n: &Node) -> Res<()> {
        if self.overlay.meta(p).map_err(errno)?.is_some() {
            return Ok(());
        }
        if n.kind == Kind::Symlink {
            let _g = lock(&self.ns);
            if self.overlay.meta(p).map_err(errno)?.is_none() {
                self.copy_up_locked(p, n)?;
            }
            return Ok(());
        }
        // The long part (hydration) runs outside the namespace lock.
        let (_f, tmp) = self.stage(n)?;
        let _g = lock(&self.ns);
        if self.overlay.meta(p).map_err(errno)?.is_some() {
            let _ = fs::remove_file(&tmp);
            return Ok(());
        }
        self.overlay
            .install(&tmp, p, Self::copy_mode(n))
            .map_err(|e| {
                let _ = fs::remove_file(&tmp);
                errno(e)
            })
    }

    /// Copies everything visible below the directory `p` into the overlay, so the directory
    /// can move without its base entries. The caller holds `ns`.
    fn copy_tree_locked(&self, p: &RepoPath, budget: &mut usize) -> Res<()> {
        if self.overlay.meta(p).map_err(errno)?.is_none() {
            self.overlay.make_dir(p, 0o755).map_err(errno)?;
        }
        for (_, f) in self.merged(Some(p), MAX_CHILDREN)? {
            if *budget == 0 {
                return Err(Errno::XDEV);
            }
            *budget -= 1;
            if matches!(f.src, Src::Link) {
                continue;
            }
            match (&f.src, f.kind()) {
                (_, VKind::Dir) => self.copy_tree_locked(&f.path, budget)?,
                (Src::Base(n), _) => self.copy_up_locked(&f.path, n)?,
                _ => {}
            }
        }
        Ok(())
    }

    /// A file holding the verified content of the clean base file `n` for the process `image`:
    /// the local cache entry, or an anonymous overlay file filled from the object store.
    pub(crate) fn base_file(&self, image: &str, p: &RepoPath, n: &Node) -> Res<File> {
        let c = n.content().ok_or(Errno::ISDIR)?;
        let local = self
            .local
            .as_ref()
            .and_then(|l| l.content_path(c.id))
            .and_then(|cp| File::open(cp).ok())
            .filter(|f| f.metadata().is_ok_and(|m| m.is_file() && m.len() == c.size));
        if let Some(f) = local {
            if self.core.policy().check(image) == Decision::Deny {
                self.core.stats().record_denied(image);
                return Err(Errno::ACCESS);
            }
            self.core
                .stats()
                .record_hydration(p, image, c.size, SystemTime::now());
            return Ok(f);
        }
        let mut s = self
            .blk(self.core.open_for_hydration(p, image))
            .map_err(vfs_errno)?;
        let anon = self.overlay.anonymous().map_err(errno)?;
        let mut off = 0u64;
        while let Some(b) = self.blk(s.next()).map_err(vfs_errno)? {
            anon.write_all_at(&b, off).map_err(errno)?;
            off += b.len() as u64;
        }
        if off != c.size {
            return Err(Errno::IO);
        }
        Ok(anon)
    }

    fn new_handle(&self, h: OpenFile) -> Res<u64> {
        let mut hs = lock(&self.handles);
        if hs.len() >= MAX_HANDLES {
            return Err(Errno::MFILE);
        }
        let fh = self.next_fh.fetch_add(1, Ordering::Relaxed);
        hs.insert(fh, Arc::new(h));
        Ok(fh)
    }

    // ----- operations -----

    /// The attributes of `name` in `parent`, counting one kernel lookup of it.
    pub(crate) fn lookup(&self, parent: u64, name: &[u8]) -> Res<Attr> {
        let (_, p) = self.child(parent, name)?;
        let f = self.view(&p)?.ok_or(Errno::NOENT)?;
        if matches!(f.src, Src::Link) {
            self.ensure_redirect(&p)?;
        }
        let ino = self
            .table
            .lookup(parent, name)
            .map_err(errno)?
            .ok_or(Errno::NOENT)?;
        Ok(self.attr(ino, &f))
    }

    pub(crate) fn getattr(&self, ino: u64, fh: Option<u64>) -> Res<Attr> {
        if let Some(fh) = fh
            && let Ok(h) = self.handle(fh)
            && h.writable
            && let Ok(m) = h.file.metadata()
        {
            // An open file that was unlinked has no path any more.
            if self.table.path_of(ino).is_none() {
                return Ok(self.attr(
                    ino,
                    &Found {
                        path: RepoPath::new(b"unlinked".to_vec()).map_err(|_| Errno::IO)?,
                        src: Src::Over(m),
                    },
                ));
            }
        }
        let Some(p) = self.loc(ino)? else {
            return Ok(self.root_attr());
        };
        let f = self.view(&p)?.ok_or(Errno::NOENT)?;
        Ok(self.attr(ino, &f))
    }

    /// Opens a file. `write` and `read` are the access asked for; `trunc` throws the content
    /// away first. `image` names the process, for the hydration policy.
    pub(crate) fn open(
        &self,
        image: &str,
        ino: u64,
        read: bool,
        write: bool,
        trunc: bool,
    ) -> Res<Opened> {
        let p = self.loc(ino)?.ok_or(Errno::ISDIR)?;
        let f = self.view(&p)?.ok_or(Errno::NOENT)?;
        match f.kind() {
            VKind::Dir => return Err(Errno::ISDIR),
            VKind::Symlink => return Err(Errno::LOOP),
            VKind::File => {}
        }
        let writable = write || trunc;
        if writable {
            if f.read_only() {
                return Err(Errno::ACCESS);
            }
            if let Src::Base(n) = &f.src {
                if trunc && write {
                    // The content is thrown away: no need to hydrate it.
                    let _g = lock(&self.ns);
                    if self.overlay.meta(&p).map_err(errno)?.is_none() {
                        let (_t, tmp) = self.overlay.new_tmp().map_err(errno)?;
                        self.overlay
                            .install(&tmp, &p, Self::copy_mode(n))
                            .map_err(errno)?;
                    }
                } else {
                    self.materialise(&p, n)?;
                }
            }
            let file = self
                .overlay
                .open_file(&p, read, true, false, trunc)
                .map_err(errno)?;
            self.journal.record(Op::Modified, p.as_bytes());
            self.notify().content(ino);
            let fh = self.new_handle(OpenFile {
                file,
                writable: true,
            })?;
            return Ok(Opened { fh, base: false });
        }
        match &f.src {
            Src::Over(_) => {
                let file = self
                    .overlay
                    .open_file(&p, true, false, false, false)
                    .map_err(errno)?;
                let fh = self.new_handle(OpenFile {
                    file,
                    writable: false,
                })?;
                Ok(Opened { fh, base: false })
            }
            Src::Base(n) => {
                let file = self.base_file(image, &p, n)?;
                let fh = self.new_handle(OpenFile {
                    file,
                    writable: false,
                })?;
                Ok(Opened { fh, base: true })
            }
            Src::Link => Err(Errno::LOOP),
        }
    }

    pub(crate) fn read(&self, fh: u64, offset: u64, size: u32) -> Res<Vec<u8>> {
        let h = self.handle(fh)?;
        read_at(&h.file, offset, size)
    }

    pub(crate) fn write(&self, fh: u64, offset: u64, data: &[u8]) -> Res<u32> {
        let h = self.handle(fh)?;
        if !h.writable {
            return Err(Errno::BADF);
        }
        h.file.write_all_at(data, offset).map_err(errno)?;
        u32::try_from(data.len()).map_err(|_| Errno::INVAL)
    }

    /// Closes a handle; answers what it was.
    pub(crate) fn release(&self, fh: u64) -> Option<Arc<OpenFile>> {
        lock(&self.handles).remove(&fh)
    }

    /// Creates the file `name` in `parent` and opens it for writing.
    pub(crate) fn create(&self, parent: u64, name: &[u8], mode: u32) -> Res<(Attr, u64)> {
        let (pp, p) = self.child(parent, name)?;
        let _g = lock(&self.ns);
        self.check_new(pp.as_ref(), &p)?;
        let file = self
            .overlay
            .create_file(&p, mode & 0o7777, false)
            .map_err(errno)?;
        let ino = self
            .table
            .lookup(parent, name)
            .map_err(errno)?
            .ok_or(Errno::NOENT)?;
        let m = file.metadata().map_err(errno)?;
        let fh = self.new_handle(OpenFile {
            file,
            writable: true,
        })?;
        self.journal.record(Op::Created, p.as_bytes());
        let attr = self.attr(
            ino,
            &Found {
                path: p,
                src: Src::Over(m),
            },
        );
        Ok((attr, fh))
    }

    pub(crate) fn mkdir(&self, parent: u64, name: &[u8], mode: u32) -> Res<Attr> {
        let (pp, p) = self.child(parent, name)?;
        let _g = lock(&self.ns);
        self.check_new(pp.as_ref(), &p)?;
        self.overlay.make_dir(&p, mode & 0o7777).map_err(errno)?;
        self.journal.record(Op::Created, p.as_bytes());
        let f = self.view(&p)?.ok_or(Errno::IO)?;
        let ino = self
            .table
            .lookup(parent, name)
            .map_err(errno)?
            .ok_or(Errno::NOENT)?;
        Ok(self.attr(ino, &f))
    }

    pub(crate) fn symlink(&self, parent: u64, name: &[u8], target: &[u8]) -> Res<Attr> {
        let (pp, p) = self.child(parent, name)?;
        if target.is_empty() || target.len() > MAX_LINK || target.contains(&0) {
            return Err(Errno::NAMETOOLONG);
        }
        let _g = lock(&self.ns);
        self.check_new(pp.as_ref(), &p)?;
        self.overlay
            .make_symlink(&p, Path::new(OsStr::from_bytes(target)))
            .map_err(errno)?;
        self.journal.record(Op::Created, p.as_bytes());
        let f = self.view(&p)?.ok_or(Errno::IO)?;
        let ino = self
            .table
            .lookup(parent, name)
            .map_err(errno)?
            .ok_or(Errno::NOENT)?;
        Ok(self.attr(ino, &f))
    }

    pub(crate) fn readlink(&self, ino: u64) -> Res<Vec<u8>> {
        let p = self.loc(ino)?.ok_or(Errno::INVAL)?;
        let f = self.view(&p)?.ok_or(Errno::NOENT)?;
        match &f.src {
            Src::Link => Ok(self.redirect_target(&p).into_os_string().into_vec()),
            Src::Over(m) if m.file_type().is_symlink() => Ok(self
                .overlay
                .read_link(&p)
                .map_err(errno)?
                .into_os_string()
                .into_vec()),
            Src::Base(n) if n.kind == Kind::Symlink => {
                let c = n.content().ok_or(Errno::IO)?;
                let s = self.blk(self.core.read_content(c)).map_err(vfs_errno)?;
                self.blk(s.read_all(MAX_LINK as u64)).map_err(vfs_errno)
            }
            _ => Err(Errno::INVAL),
        }
    }

    /// Removes the file or directory `name` of `parent`; answers the inode it had, if it had
    /// one.
    pub(crate) fn remove(&self, parent: u64, name: &[u8], dir: bool) -> Res<Option<u64>> {
        let (pp, p) = self.child(parent, name)?;
        let _g = lock(&self.ns);
        let f = self.view(&p)?.ok_or(Errno::NOENT)?;
        if let Src::Link = f.src {
            // Removing a redirected name empties what it points to; the name stays.
            if dir {
                return Err(Errno::NOTDIR);
            }
            if let Some(abs) = self.redirect.resolve(&p) {
                let _ = fs::remove_dir_all(&abs);
            }
            self.redirect.forget(&p);
            return Ok(None);
        }
        self.writable_dir(pp.as_ref())?;
        if f.read_only() {
            return Err(Errno::ACCESS);
        }
        match (dir, f.kind()) {
            (true, VKind::Dir) => {
                if !self.is_empty_dir(&p)? {
                    return Err(Errno::NOTEMPTY);
                }
            }
            (true, _) => return Err(Errno::NOTDIR),
            (false, VKind::Dir) => return Err(Errno::ISDIR),
            (false, _) => {}
        }
        if f.in_overlay() {
            if dir {
                self.overlay.remove_dir(&p).map_err(errno)?;
            } else {
                self.overlay.remove_file(&p).map_err(errno)?;
            }
        }
        if self.base(&p)?.is_some() {
            self.overlay.set_white(&p).map_err(errno)?;
            self.whites.insert(&p);
        }
        let ino = self.table.peek(parent, name);
        if let Some(ino) = ino {
            self.notify().content(ino);
        }
        self.table.unlink(parent, name).map_err(errno)?;
        self.journal.record(Op::Deleted, p.as_bytes());
        Ok(ino)
    }

    /// Moves `name` of `parent` to `newname` of `newparent`. With `noreplace` an existing target
    /// is an error; otherwise an existing target is replaced (and its inode answered).
    pub(crate) fn rename(
        &self,
        parent: u64,
        name: &[u8],
        newparent: u64,
        newname: &[u8],
        noreplace: bool,
    ) -> Res<Option<u64>> {
        let (spp, from) = self.child(parent, name)?;
        let (dpp, to) = self.child(newparent, newname)?;
        if !checkout_safe(&to) {
            return Err(Errno::PERM);
        }
        if self.reds.covers(&from) || self.reds.covers(&to) {
            return Err(Errno::PERM);
        }
        let _g = lock(&self.ns);
        let src = self.view(&from)?.ok_or(Errno::NOENT)?;
        self.writable_dir(spp.as_ref())?;
        self.writable_dir(dpp.as_ref())?;
        if src.read_only() {
            return Err(Errno::ACCESS);
        }
        if from == to {
            return Ok(None);
        }
        let is_dir = src.kind() == VKind::Dir;
        if is_dir && to.starts_with_dir(&from) {
            return Err(Errno::INVAL);
        }
        if let Some(d) = self.view(&to)? {
            if noreplace {
                return Err(Errno::EXIST);
            }
            if d.read_only() {
                return Err(Errno::ACCESS);
            }
            match (is_dir, d.kind() == VKind::Dir) {
                (true, false) => return Err(Errno::NOTDIR),
                (false, true) => return Err(Errno::ISDIR),
                (true, true) if !self.is_empty_dir(&to)? => return Err(Errno::NOTEMPTY),
                _ => {}
            }
        }
        let from_base = self.base(&from)?.is_some();
        let to_base = self.base(&to)?.is_some();
        // Everything that moves must be in the overlay first.
        match &src.src {
            Src::Base(n) if is_dir => {
                let mut budget = MAX_COPYUP;
                if n.kind == Kind::Gitlink {
                    return Err(Errno::ACCESS);
                }
                self.copy_tree_locked(&from, &mut budget)?;
            }
            Src::Base(n) => self.copy_up_locked(&from, n)?,
            Src::Over(_) if is_dir && from_base => {
                let mut budget = MAX_COPYUP;
                self.copy_tree_locked(&from, &mut budget)?;
            }
            _ => {}
        }
        self.overlay.rename(&from, &to).map_err(errno)?;
        if to_base {
            self.overlay.set_white(&to).map_err(errno)?;
            self.whites.insert(&to);
        }
        if from_base {
            self.overlay.set_white(&from).map_err(errno)?;
            self.whites.insert(&from);
        }
        if let Some(ino) = self.table.peek(parent, name) {
            self.notify().content(ino);
        }
        let replaced = self.table.peek(newparent, newname);
        self.table
            .rename(parent, name, newparent, newname)
            .map_err(errno)?;
        if is_dir {
            self.journal.record(Op::DirRenamedFrom, from.as_bytes());
            self.journal.record(Op::DirRenamedTo, to.as_bytes());
        } else {
            self.journal.record(Op::RenamedFrom, from.as_bytes());
            self.journal.record(Op::Created, to.as_bytes());
        }
        Ok(replaced)
    }

    /// Changes the mode, size or times of `ino` (through the open handle `fh` when it is a
    /// writable one). Owner changes other than to the mount's owner are refused.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn setattr(
        &self,
        ino: u64,
        mode: Option<u32>,
        uid: Option<u32>,
        gid: Option<u32>,
        size: Option<u64>,
        atime: Option<SystemTime>,
        mtime: Option<SystemTime>,
        fh: Option<u64>,
    ) -> Res<Attr> {
        if uid.is_some_and(|u| u != self.uid) || gid.is_some_and(|g| g != self.gid) {
            return Err(Errno::PERM);
        }
        let changes = mode.is_some() || size.is_some() || atime.is_some() || mtime.is_some();
        if !changes {
            return self.getattr(ino, fh);
        }
        if let Some(h) = fh.and_then(|fh| self.handle(fh).ok())
            && h.writable
        {
            Self::apply_attrs(&h.file, mode, size, atime, mtime)?;
            if let Some(p) = self.loc(ino)? {
                self.journal.record(Op::Modified, p.as_bytes());
            }
            return self.getattr(ino, fh);
        }
        let p = self.loc(ino)?.ok_or(Errno::ISDIR)?;
        let f = self.view(&p)?.ok_or(Errno::NOENT)?;
        match f.kind() {
            VKind::Dir => {
                return if size.is_some() {
                    Err(Errno::ISDIR)
                } else {
                    self.getattr(ino, fh)
                };
            }
            VKind::Symlink => return self.getattr(ino, fh),
            VKind::File => {}
        }
        if f.read_only() {
            return Err(Errno::ACCESS);
        }
        // A mode that is already the file's changes nothing (the NFS client's cache poke).
        if size.is_none()
            && atime.is_none()
            && mtime.is_none()
            && let Some(m) = mode
            && u32::from(self.attr(ino, &f).perm) == m & 0o7777
        {
            return Ok(self.attr(ino, &f));
        }
        if let Src::Base(n) = &f.src {
            if size == Some(0) {
                let _g = lock(&self.ns);
                if self.overlay.meta(&p).map_err(errno)?.is_none() {
                    let (_t, tmp) = self.overlay.new_tmp().map_err(errno)?;
                    self.overlay
                        .install(&tmp, &p, Self::copy_mode(n))
                        .map_err(errno)?;
                }
            } else {
                self.materialise(&p, n)?;
            }
        }
        let file = self
            .overlay
            .open_file(&p, false, true, false, false)
            .map_err(errno)?;
        Self::apply_attrs(&file, mode, size, atime, mtime)?;
        self.journal.record(Op::Modified, p.as_bytes());
        self.notify().content(ino);
        self.getattr(ino, None)
    }

    fn apply_attrs(
        file: &File,
        mode: Option<u32>,
        size: Option<u64>,
        atime: Option<SystemTime>,
        mtime: Option<SystemTime>,
    ) -> Res<()> {
        if let Some(s) = size {
            file.set_len(s).map_err(errno)?;
        }
        if let Some(m) = mode {
            file.set_permissions(fs::Permissions::from_mode(m & 0o7777))
                .map_err(errno)?;
        }
        if atime.is_some() || mtime.is_some() {
            let mut t = fs::FileTimes::new();
            if let Some(a) = atime {
                t = t.set_accessed(a);
            }
            if let Some(m) = mtime {
                t = t.set_modified(m);
            }
            file.set_times(t).map_err(errno)?;
        }
        Ok(())
    }

    /// The listing of the directory `ino`, fixed now.
    pub(crate) fn list(&self, ino: u64) -> Res<Snapshot> {
        let dir = self.loc(ino)?;
        if let Some(d) = &dir {
            let f = self.view(d)?.ok_or(Errno::NOENT)?;
            if f.kind() != VKind::Dir {
                return Err(Errno::NOTDIR);
            }
        }
        let parent = self.table.parent_of(ino).unwrap_or(ROOT);
        let mut entries: Vec<(Vec<u8>, Option<Found>)> =
            vec![(b".".to_vec(), None), (b"..".to_vec(), None)];
        entries.extend(
            self.merged(dir.as_ref(), MAX_CHILDREN)?
                .into_iter()
                .map(|(n, f)| (n, Some(f))),
        );
        Ok(Snapshot { entries, parent })
    }

    pub(crate) fn opendir(&self, ino: u64) -> Res<u64> {
        let snap = self.list(ino)?;
        let mut ds = lock(&self.dirs);
        if ds.len() >= MAX_HANDLES {
            return Err(Errno::MFILE);
        }
        let fh = self.next_fh.fetch_add(1, Ordering::Relaxed);
        ds.insert(fh, Arc::new(snap));
        Ok(fh)
    }

    pub(crate) fn snapshot(&self, fh: u64) -> Res<Arc<Snapshot>> {
        lock(&self.dirs).get(&fh).cloned().ok_or(Errno::BADF)
    }

    pub(crate) fn releasedir(&self, fh: u64) {
        lock(&self.dirs).remove(&fh);
    }

    /// The inode number of `name` below `parent`, assigned on first sight and not counted as a
    /// kernel lookup.
    pub(crate) fn ino_for_listing(&self, parent: u64, name: &[u8]) -> u64 {
        match self.table.lookup(parent, name) {
            Ok(Some(i)) => {
                self.table.forget(i, 1);
                i
            }
            _ => u64::MAX,
        }
    }

    /// The overlay's file system statistics.
    pub(crate) fn statfs(&self) -> Res<rustix::fs::StatVfs> {
        rustix::fs::statvfs(self.overlay.root())
    }

    // ----- switch -----

    /// Rewrites one path of a switch plan in the overlay; the base tree already is the new one.
    fn switch_one(&self, p: &RepoPath, deleted: bool) -> Res<()> {
        // A user file standing where the new tree wants a directory is replaced.
        let mut anc = p.parent();
        let mut chain = Vec::new();
        while let Some(a) = anc {
            anc = a.parent();
            chain.push(a);
        }
        for a in chain.into_iter().rev() {
            if let Some(m) = self.overlay.meta(&a).map_err(errno)?
                && !m.is_dir()
            {
                self.overlay.remove_tree(&a).map_err(errno)?;
            }
        }
        if self.overlay.meta(p).map_err(errno)?.is_some() {
            self.overlay.remove_tree(p).map_err(errno)?;
        }
        if self.whites.is_white(p) {
            self.overlay.clear_white(p).map_err(errno)?;
            self.whites.remove(p);
        }
        if !deleted && self.whites.hides(p) {
            // The user deleted a directory above: the new file stays visible as a new entry.
            if let Some(n) = self.blk(self.core.resolve(p)).map_err(vfs_errno)?
                && matches!(n.kind, Kind::File | Kind::Exec | Kind::Symlink)
            {
                self.copy_up_locked(p, &n)?;
            }
        }
        Ok(())
    }

    /// Forgets the entries and inodes that changed behind the kernel's back.
    fn invalidate(&self, changed: &[RepoPath]) {
        let n = self.notify();
        let mut touched: HashSet<u64> = HashSet::new();
        for p in changed {
            let mut prefix = Vec::new();
            let mut cur = Some(p.clone());
            while let Some(c) = cur {
                cur = c.parent();
                prefix.push(c);
            }
            for q in prefix.into_iter().rev() {
                let Some(ino) = self.table.find(&q) else {
                    continue;
                };
                let gone = q != *p && !matches!(self.view(&q), Ok(Some(_)));
                if q == *p || gone {
                    let parent = self.table.parent_of(ino).unwrap_or(ROOT);
                    let name = q.file_name().as_bytes().to_vec();
                    let _ = self.table.unlink(parent, &name);
                    n.entry(parent, &name);
                    n.content(ino);
                }
                if touched.insert(ino) {
                    n.inode(ino);
                }
            }
        }
    }

    /// Rewrites the overlay for the new version and tells the kernel what changed; answers the
    /// paths the plan touched. The caller has already pointed [`CoreFs`] at the new tree
    /// (`set_root`). Open files keep the content they were opened on, as with an unlink, so
    /// nothing stays pending.
    pub fn apply_switch(&self, plan: &Plan) -> Result<(SwitchOutcome, Vec<RepoPath>)> {
        if !plan.conflicts.is_empty() {
            return Err(VfsError::Conflicts);
        }
        let mut changed = Vec::with_capacity(plan.updates.len() + plan.deletes.len());
        {
            let _g = lock(&self.ns);
            for (p, _) in &plan.updates {
                self.switch_one(p, false)
                    .map_err(|e| VfsError::Io(io_of(e)))?;
                changed.push(p.clone());
            }
            for p in &plan.deletes {
                self.switch_one(p, true)
                    .map_err(|e| VfsError::Io(io_of(e)))?;
                changed.push(p.clone());
            }
        }
        self.invalidate(&changed);
        Ok((SwitchOutcome::default(), changed))
    }

    /// A clean file holds nothing locally, so this only drops what the kernel cached. Modified
    /// or missing paths are skipped.
    pub fn dehydrate(&self, paths: &[RepoPath]) -> Result<DehydrateOutcome> {
        let mut out = DehydrateOutcome::default();
        let n = self.notify();
        for p in paths {
            let found = self.view(p).map_err(|e| VfsError::Io(io_of(e)))?;
            match found {
                Some(f) if matches!(f.src, Src::Base(_)) => {
                    out.dehydrated += 1;
                    if let Some(ino) = self.table.find(p) {
                        n.content(ino);
                        n.inode(ino);
                    }
                }
                _ => out.skipped.push(p.clone()),
            }
        }
        Ok(out)
    }
}

/// Reads up to `size` bytes (at most [`MAX_READ`]) of `file` at `offset`.
pub(crate) fn read_at(file: &File, offset: u64, size: u32) -> Res<Vec<u8>> {
    let want = (size as usize).min(MAX_READ);
    let mut buf = vec![0u8; want];
    let mut n = 0usize;
    while n < want {
        let r = file
            .read_at(&mut buf[n..], offset + n as u64)
            .map_err(errno)?;
        if r == 0 {
            break;
        }
        n += r;
    }
    buf.truncate(n);
    Ok(buf)
}

/// The on-disk view for virtual status: the overlay is what is on disk.
pub struct Disk {
    pub(crate) fs: Arc<MergedFs>,
}

impl Disk {
    pub fn new(fs: Arc<MergedFs>) -> Self {
        Self { fs }
    }
}

impl DiskState for Disk {
    fn state(&self, path: &RepoPath) -> io::Result<PathState> {
        let i = &self.fs;
        if i.overlay.meta(path)?.is_some() {
            return Ok(PathState::Full);
        }
        let in_tree = i
            .blk(i.core.resolve(path))
            .map_err(|e| io::Error::other(e.to_string()))?
            .is_some();
        Ok(match (in_tree, i.whites.hides(path)) {
            (true, true) => PathState::Tombstone,
            (true, false) => PathState::Virtual,
            (false, _) => PathState::Missing,
        })
    }

    fn stat(&self, path: &RepoPath) -> io::Result<Option<DiskStat>> {
        let Some(f) = self.fs.view(path).map_err(io_of)? else {
            return Ok(None);
        };
        let exec = match &f.src {
            Src::Over(m) => m.mode() & 0o111 != 0,
            Src::Base(n) => n.kind == Kind::Exec,
            Src::Link => false,
        };
        let size = match &f.src {
            Src::Over(m) => m.len(),
            Src::Base(n) => n.size,
            Src::Link => 0,
        };
        let kind = match f.kind() {
            VKind::Dir => DiskKind::Dir,
            VKind::Symlink => DiskKind::Symlink,
            VKind::File => DiskKind::File { exec: Some(exec) },
        };
        Ok(Some(DiskStat { kind, size }))
    }

    fn read(&self, path: &RepoPath, max: u64) -> io::Result<Vec<u8>> {
        use std::io::Read;
        let i = &self.fs;
        let f = i
            .view(path)
            .map_err(io_of)?
            .ok_or_else(|| io::Error::from(io::ErrorKind::NotFound))?;
        let too_large = || io::Error::new(io::ErrorKind::InvalidData, "file too large");
        match (&f.src, f.kind()) {
            (_, VKind::Dir) => Err(io::Error::new(io::ErrorKind::InvalidInput, "not a file")),
            (Src::Over(_), VKind::Symlink) => {
                Ok(i.overlay.read_link(path)?.into_os_string().into_vec())
            }
            (Src::Over(m), _) => {
                if m.len() > max {
                    return Err(too_large());
                }
                let mut out = Vec::new();
                i.overlay
                    .open_file(path, true, false, false, false)?
                    .take(max + 1)
                    .read_to_end(&mut out)?;
                if out.len() as u64 > max {
                    return Err(too_large());
                }
                Ok(out)
            }
            (Src::Base(n), _) => {
                let c = n
                    .content()
                    .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "not a file"))?;
                let s = i
                    .blk(i.core.read_content(c))
                    .map_err(|e| io::Error::other(e.to_string()))?;
                i.blk(s.read_all(max)).map_err(|e| match e {
                    VfsError::TooLarge(_) => too_large(),
                    e => io::Error::other(e.to_string()),
                })
            }
            (Src::Link, _) => Ok(i.redirect_target(path).into_os_string().into_vec()),
        }
    }

    fn list(&self, dir: &RepoPath) -> io::Result<Vec<DirEntry>> {
        let mut out = Vec::new();
        for (name, is_dir) in self.fs.overlay.children(Some(dir))? {
            if let Ok(name) = Name::new(name) {
                out.push(DirEntry { name, dir: is_dir });
            }
        }
        Ok(out)
    }
}
