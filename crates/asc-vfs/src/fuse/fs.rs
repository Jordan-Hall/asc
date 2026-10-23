//! The FUSE file system (SPEC §10.3): the base tree served through [`CoreFs`], with the user's
//! changes in the [`Overlay`]. A path resolves to the overlay entry when there is one, and
//! otherwise to the base entry unless a deletion marker hides it. Reads of clean files go
//! straight to the local content cache (through kernel passthrough where it is available),
//! writes copy the file up first, and every change is journaled as it happens, so the journal
//! is exact and needs no reconcile.
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
use asc_wc::times::Switched;
use asc_wc::vstatus::{DirEntry, DiskKind, DiskStat, DiskState, PathState};
use fuser::{
    AccessFlags, BackingId, BsdFileFlags, Errno, FileAttr, FileHandle, FileType, FopenFlags,
    Generation, INodeNo, InitFlags, KernelConfig, LockOwner, MountOption, Notifier, OpenFlags,
    RenameFlags, ReplyAttr, ReplyCreate, ReplyData, ReplyDirectory, ReplyDirectoryPlus, ReplyEmpty,
    ReplyEntry, ReplyOpen, ReplyStatfs, ReplyWrite, ReplyXattr, Request, TimeOrNow, WriteFlags,
};
use tokio::runtime::Handle;

use super::inodes::{InodeStore, InodeTable, ROOT};
use super::overlay::{MAX_CHILDREN, Overlay};
use super::passthrough::Passthrough;
use super::{JournalSink, TimeSource, WATCH_NOTE, block};
use crate::core::{CoreFs, DehydrateOutcome, Node, Provider, Result, SwitchOutcome, VfsError};
use crate::policy::Decision;

/// How long the kernel may cache entries and the attributes of clean (tree) entries.
const TTL_BASE: Duration = Duration::from_secs(3600);
/// Longest name the file system accepts.
const MAX_NAME: usize = 255;
/// Longest symlink target accepted or served.
const MAX_LINK: usize = 4096;
/// Most open files at once.
const MAX_HANDLES: usize = 1 << 20;
/// Most entries one directory rename copies up before it refuses with `EXDEV`.
const MAX_COPYUP: usize = 200_000;
/// Longest read served at once.
const MAX_READ: usize = 4 << 20;
/// Block size reported by `stat`.
const BLKSIZE: u32 = 4096;
/// `open(2)` flag bits (Linux).
const O_TRUNC: i32 = 0o1000;
const O_ACCMODE: i32 = 3;
const O_WRONLY: i32 = 1;
const ESTALE: i32 = 116;

/// What a FUSE provider needs from the daemon.
pub struct Config {
    pub core: Arc<CoreFs>,
    /// The local content cache: clean files are served from it, without a copy.
    pub local: Option<CacheDir>,
    /// Where inode numbers persist (`state.db.inodes`).
    pub inodes: Arc<dyn InodeStore>,
    /// `wc/<ws>/overlay/`.
    pub overlay: PathBuf,
    /// `$ASC_HOME/redirect/<ws>/`: where redirected names point.
    pub redirect: PathBuf,
    pub redirections: Redirections,
    pub times: Arc<dyn TimeSource>,
    pub journal: Arc<dyn JournalSink>,
    /// The runtime the file system runs async work on.
    pub rt: Handle,
    /// The time, in Unix seconds, the root directory reports.
    pub root_time: i64,
    /// Try kernel passthrough for clean files.
    pub passthrough: bool,
}

type Res<T> = std::result::Result<T, Errno>;

fn errno(e: io::Error) -> Errno {
    Errno::from(e)
}

fn vfs_errno(e: VfsError) -> Errno {
    match e {
        VfsError::Denied(_) => Errno::EACCES,
        VfsError::NotFound(_) => Errno::ENOENT,
        VfsError::NotADir(_) => Errno::ENOTDIR,
        VfsError::NotAFile(_) => Errno::EISDIR,
        VfsError::TooLarge(_) => Errno::EFBIG,
        VfsError::Io(e) => Errno::from(e),
        _ => Errno::EIO,
    }
}

fn sys_time(secs: i64, nanos: i64) -> SystemTime {
    let n = u32::try_from(nanos.clamp(0, 999_999_999)).unwrap_or(0);
    if secs >= 0 {
        UNIX_EPOCH + Duration::new(secs.unsigned_abs(), n)
    } else {
        UNIX_EPOCH - Duration::from_secs(secs.unsigned_abs()) + Duration::from_nanos(u64::from(n))
    }
}

fn unix_time(t: TimeOrNow) -> SystemTime {
    match t {
        TimeOrNow::SpecificTime(t) => t,
        TimeOrNow::Now => SystemTime::now(),
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
enum Src {
    /// An entry of the base tree.
    Base(Node),
    /// A real entry of the overlay.
    Over(fs::Metadata),
    /// A redirected name, served as a symlink.
    Link,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum VKind {
    Dir,
    File,
    Symlink,
}

/// What a path is in the merged view.
#[derive(Clone)]
struct Found {
    path: RepoPath,
    src: Src,
}

impl Found {
    fn kind(&self) -> VKind {
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
    fn read_only(&self) -> bool {
        matches!(&self.src, Src::Base(n) if n.read_only || n.kind == Kind::Gitlink)
    }

    fn in_overlay(&self) -> bool {
        matches!(self.src, Src::Over(_))
    }
}

fn file_type(k: VKind) -> FileType {
    match k {
        VKind::Dir => FileType::Directory,
        VKind::File => FileType::RegularFile,
        VKind::Symlink => FileType::Symlink,
    }
}

/// An open file.
struct OpenFile {
    ino: u64,
    file: File,
    writable: bool,
    /// The passthrough id this handle holds a count on.
    backing: Option<Arc<BackingId>>,
}

/// A directory listing fixed at `opendir`, so offsets stay valid while the directory changes.
struct Snapshot {
    entries: Vec<(Vec<u8>, Option<Found>)>,
    parent: u64,
}

pub(crate) struct Inner {
    core: Arc<CoreFs>,
    local: Option<CacheDir>,
    table: InodeTable,
    overlay: Overlay,
    redirect: SafeRoot,
    reds: Redirections,
    times: Arc<dyn TimeSource>,
    journal: Arc<dyn JournalSink>,
    rt: Handle,
    root_time: i64,
    uid: u32,
    gid: u32,
    whites: Whites,
    ns: Mutex<()>,
    handles: Mutex<HashMap<u64, Arc<OpenFile>>>,
    dirs: Mutex<HashMap<u64, Arc<Snapshot>>>,
    next_fh: AtomicU64,
    pt: Passthrough,
    want_pt: bool,
    notifier: Mutex<Option<Notifier>>,
    mount: Mutex<Option<PathBuf>>,
}

fn join(dir: Option<&RepoPath>, name: &Name) -> RepoPath {
    match dir {
        Some(d) => d.join(name),
        None => RepoPath::new(name.as_bytes().to_vec()).expect("a name is a path"),
    }
}

fn image_of(pid: u32) -> String {
    fs::read_link(format!("/proc/{pid}/exe"))
        .ok()
        .and_then(|p| p.into_os_string().into_string().ok())
        .unwrap_or_else(|| "unknown".to_owned())
}

impl Inner {
    fn blk<F>(&self, f: F) -> F::Output
    where
        F: Future + Send,
        F::Output: Send,
    {
        block(&self.rt, f)
    }

    fn handle(&self, fh: u64) -> Res<Arc<OpenFile>> {
        lock(&self.handles).get(&fh).cloned().ok_or(Errno::EBADF)
    }

    // ----- paths and the merged view -----

    /// The path of a live inode: `None` is the root.
    fn loc(&self, ino: u64) -> Res<Option<RepoPath>> {
        if ino == ROOT {
            return Ok(None);
        }
        let b = self
            .table
            .path_of(ino)
            .ok_or_else(|| Errno::from_i32(ESTALE))?;
        RepoPath::new(b).map(Some).map_err(|_| Errno::EINVAL)
    }

    /// Validates `name` and joins it to the directory `parent`.
    fn child(&self, parent: u64, name: &OsStr) -> Res<(Option<RepoPath>, RepoPath)> {
        let nb = name.as_bytes();
        if nb.is_empty() || nb == b"." || nb == b".." || nb.contains(&b'/') || nb.contains(&0) {
            return Err(Errno::EINVAL);
        }
        if nb.len() > MAX_NAME {
            return Err(Errno::ENAMETOOLONG);
        }
        let pp = self.loc(parent)?;
        let n = Name::new(nb.to_vec()).map_err(|_| Errno::EINVAL)?;
        if let Some(p) = &pp
            && p.components().count() + 1 > MAX_TREE_DEPTH
        {
            return Err(Errno::ENAMETOOLONG);
        }
        let p = join(pp.as_ref(), &n);
        Ok((pp, p))
    }

    fn is_redirect_node(&self, p: &RepoPath) -> bool {
        self.reds.is_redirected_name(p.file_name().as_bytes())
            && p.parent().is_none_or(|q| !self.reds.covers(&q))
    }

    /// The base entry at `p`, unless a deletion marker hides it.
    fn base(&self, p: &RepoPath) -> Res<Option<Node>> {
        if self.whites.hides(p) {
            return Ok(None);
        }
        self.blk(self.core.resolve(p)).map_err(vfs_errno)
    }

    fn view(&self, p: &RepoPath) -> Res<Option<Found>> {
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
        let f = self.view(d)?.ok_or(Errno::ENOENT)?;
        if f.kind() != VKind::Dir {
            return Err(Errno::ENOTDIR);
        }
        if f.read_only() || matches!(f.src, Src::Link) {
            return Err(Errno::EACCES);
        }
        Ok(())
    }

    /// Checks that `p` may be created below `pp`.
    fn check_new(&self, pp: Option<&RepoPath>, p: &RepoPath) -> Res<()> {
        if !checkout_safe(p) {
            return Err(Errno::EPERM);
        }
        self.writable_dir(pp)?;
        if self.reds.covers(p) || self.view(p)?.is_some() {
            return Err(Errno::EEXIST);
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

    fn redirect_target(&self, p: &RepoPath) -> PathBuf {
        self.redirect
            .root()
            .join(Path::new(OsStr::from_bytes(p.as_bytes())))
    }

    /// Creates the directory a redirected name points to (on first lookup).
    fn ensure_redirect(&self, p: &RepoPath) -> Res<()> {
        use std::os::unix::fs::DirBuilderExt;
        let abs = self.redirect.resolve(p).ok_or(Errno::EACCES)?;
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

    fn attr(&self, ino: u64, f: &Found) -> (FileAttr, Duration) {
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
        (
            FileAttr {
                ino: INodeNo(ino),
                size,
                blocks: blocks(size),
                atime,
                mtime,
                ctime,
                crtime: mtime,
                kind: file_type(kind),
                perm,
                nlink: if kind == VKind::Dir { 2 } else { 1 },
                uid: self.uid,
                gid: self.gid,
                rdev: 0,
                blksize: BLKSIZE,
                flags: 0,
            },
            ttl,
        )
    }

    fn root_attr(&self) -> FileAttr {
        let t = sys_time(self.root_time, 0);
        FileAttr {
            ino: INodeNo(ROOT),
            size: u64::from(BLKSIZE),
            blocks: 8,
            atime: t,
            mtime: t,
            ctime: t,
            crtime: t,
            kind: FileType::Directory,
            perm: 0o755,
            nlink: 2,
            uid: self.uid,
            gid: self.gid,
            rdev: 0,
            blksize: BLKSIZE,
            flags: 0,
        }
    }

    /// The merged children of a directory, in name order, at most `limit` of them.
    fn merged(&self, dir: Option<&RepoPath>, limit: usize) -> Res<Vec<(Vec<u8>, Found)>> {
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
        let c = n.content().ok_or(Errno::EISDIR)?;
        let (mut f, tmp) = self.overlay.new_tmp().map_err(errno)?;
        let r = (|| -> Res<()> {
            let mut s = self.blk(self.core.read_content(c)).map_err(vfs_errno)?;
            let mut total = 0u64;
            while let Some(b) = self.blk(s.next()).map_err(vfs_errno)? {
                total += b.len() as u64;
                if total > c.size {
                    return Err(Errno::EIO);
                }
                f.write_all(&b).map_err(errno)?;
            }
            if total != c.size {
                return Err(Errno::EIO);
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
            let c = n.content().ok_or(Errno::EIO)?;
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
                return Err(Errno::EXDEV);
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

    /// A file holding the verified content of the clean base file `n` for the process `pid`:
    /// the local cache entry, or an anonymous overlay file filled from the object store.
    fn base_file(&self, pid: u32, p: &RepoPath, n: &Node) -> Res<File> {
        let c = n.content().ok_or(Errno::EISDIR)?;
        let image = image_of(pid);
        let local = self
            .local
            .as_ref()
            .and_then(|l| l.content_path(c.id))
            .and_then(|cp| File::open(cp).ok())
            .filter(|f| f.metadata().is_ok_and(|m| m.is_file() && m.len() == c.size));
        if let Some(f) = local {
            if self.core.policy().check(&image) == Decision::Deny {
                self.core.stats().record_denied(&image);
                return Err(Errno::EACCES);
            }
            self.core
                .stats()
                .record_hydration(p, &image, c.size, SystemTime::now());
            return Ok(f);
        }
        let mut s = self
            .blk(self.core.open_for_hydration(p, &image))
            .map_err(vfs_errno)?;
        let anon = self.overlay.anonymous().map_err(errno)?;
        let mut off = 0u64;
        while let Some(b) = self.blk(s.next()).map_err(vfs_errno)? {
            anon.write_all_at(&b, off).map_err(errno)?;
            off += b.len() as u64;
        }
        if off != c.size {
            return Err(Errno::EIO);
        }
        Ok(anon)
    }

    fn new_handle(&self, h: OpenFile) -> Res<u64> {
        let mut hs = lock(&self.handles);
        if hs.len() >= MAX_HANDLES {
            return Err(Errno::EMFILE);
        }
        let fh = self.next_fh.fetch_add(1, Ordering::Relaxed);
        hs.insert(fh, Arc::new(h));
        Ok(fh)
    }

    // ----- operations -----

    fn lookup(&self, parent: u64, name: &OsStr) -> Res<(FileAttr, Duration)> {
        let (_, p) = self.child(parent, name)?;
        let f = self.view(&p)?.ok_or(Errno::ENOENT)?;
        if matches!(f.src, Src::Link) {
            self.ensure_redirect(&p)?;
        }
        let ino = self
            .table
            .lookup(parent, name.as_bytes())
            .map_err(errno)?
            .ok_or(Errno::ENOENT)?;
        Ok(self.attr(ino, &f))
    }

    fn getattr(&self, ino: u64, fh: Option<u64>) -> Res<(FileAttr, Duration)> {
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
                        path: RepoPath::new(b"unlinked".to_vec()).map_err(|_| Errno::EIO)?,
                        src: Src::Over(m),
                    },
                ));
            }
        }
        let Some(p) = self.loc(ino)? else {
            return Ok((self.root_attr(), TTL_BASE));
        };
        let f = self.view(&p)?.ok_or(Errno::ENOENT)?;
        Ok(self.attr(ino, &f))
    }

    fn open(
        &self,
        pid: u32,
        ino: u64,
        flags: i32,
        reply: &ReplyOpen,
    ) -> Res<(u64, FopenFlags, Option<Arc<BackingId>>)> {
        let p = self.loc(ino)?.ok_or(Errno::EISDIR)?;
        let f = self.view(&p)?.ok_or(Errno::ENOENT)?;
        match f.kind() {
            VKind::Dir => return Err(Errno::EISDIR),
            VKind::Symlink => return Err(Errno::ELOOP),
            VKind::File => {}
        }
        let acc = flags & O_ACCMODE;
        let trunc = flags & O_TRUNC != 0;
        let writable = acc != 0 || trunc;
        if writable {
            if f.read_only() {
                return Err(Errno::EACCES);
            }
            if let Src::Base(n) = &f.src {
                if trunc && acc != 0 {
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
                .open_file(&p, acc != O_WRONLY, true, false, trunc)
                .map_err(errno)?;
            self.journal.record(Op::Modified, p.as_bytes());
            self.pt.invalidate(ino);
            let fh = self.new_handle(OpenFile {
                ino,
                file,
                writable: true,
                backing: None,
            })?;
            return Ok((fh, FopenFlags::empty(), None));
        }
        match &f.src {
            Src::Over(_) => {
                let file = self
                    .overlay
                    .open_file(&p, true, false, false, false)
                    .map_err(errno)?;
                let fh = self.new_handle(OpenFile {
                    ino,
                    file,
                    writable: false,
                    backing: None,
                })?;
                Ok((fh, FopenFlags::empty(), None))
            }
            Src::Base(n) => {
                let file = self.base_file(pid, &p, n)?;
                let backing = if self.want_pt {
                    self.pt.acquire(ino, reply, &file)
                } else {
                    None
                };
                let fh = self.new_handle(OpenFile {
                    ino,
                    file,
                    writable: false,
                    backing: backing.clone(),
                })?;
                // Passthrough and a kept page cache exclude each other.
                let fl = if backing.is_some() {
                    FopenFlags::empty()
                } else {
                    FopenFlags::FOPEN_KEEP_CACHE
                };
                Ok((fh, fl, backing))
            }
            Src::Link => Err(Errno::ELOOP),
        }
    }

    fn read(&self, fh: u64, offset: u64, size: u32) -> Res<Vec<u8>> {
        let h = self.handle(fh)?;
        let want = (size as usize).min(MAX_READ);
        let mut buf = vec![0u8; want];
        let mut n = 0usize;
        while n < want {
            let r = h
                .file
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

    fn write(&self, fh: u64, offset: u64, data: &[u8]) -> Res<u32> {
        let h = self.handle(fh)?;
        if !h.writable {
            return Err(Errno::EBADF);
        }
        h.file.write_all_at(data, offset).map_err(errno)?;
        u32::try_from(data.len()).map_err(|_| Errno::EINVAL)
    }

    fn release(&self, fh: u64) {
        let h = lock(&self.handles).remove(&fh);
        if let Some(h) = h
            && let Some(id) = &h.backing
        {
            self.pt.release(h.ino, id);
        }
    }

    fn create(&self, parent: u64, name: &OsStr, mode: u32, umask: u32) -> Res<(FileAttr, u64)> {
        let (pp, p) = self.child(parent, name)?;
        let _g = lock(&self.ns);
        self.check_new(pp.as_ref(), &p)?;
        let file = self
            .overlay
            .create_file(&p, mode & !umask & 0o7777, false)
            .map_err(errno)?;
        let ino = self
            .table
            .lookup(parent, name.as_bytes())
            .map_err(errno)?
            .ok_or(Errno::ENOENT)?;
        let m = file.metadata().map_err(errno)?;
        let fh = self.new_handle(OpenFile {
            ino,
            file,
            writable: true,
            backing: None,
        })?;
        self.journal.record(Op::Created, p.as_bytes());
        let (attr, _) = self.attr(
            ino,
            &Found {
                path: p,
                src: Src::Over(m),
            },
        );
        Ok((attr, fh))
    }

    fn mkdir(&self, parent: u64, name: &OsStr, mode: u32, umask: u32) -> Res<(FileAttr, Duration)> {
        let (pp, p) = self.child(parent, name)?;
        let _g = lock(&self.ns);
        self.check_new(pp.as_ref(), &p)?;
        self.overlay
            .make_dir(&p, mode & !umask & 0o7777)
            .map_err(errno)?;
        self.journal.record(Op::Created, p.as_bytes());
        let f = self.view(&p)?.ok_or(Errno::EIO)?;
        let ino = self
            .table
            .lookup(parent, name.as_bytes())
            .map_err(errno)?
            .ok_or(Errno::ENOENT)?;
        Ok(self.attr(ino, &f))
    }

    fn symlink(&self, parent: u64, name: &OsStr, target: &Path) -> Res<(FileAttr, Duration)> {
        let (pp, p) = self.child(parent, name)?;
        if target.as_os_str().is_empty() || target.as_os_str().len() > MAX_LINK {
            return Err(Errno::ENAMETOOLONG);
        }
        let _g = lock(&self.ns);
        self.check_new(pp.as_ref(), &p)?;
        self.overlay.make_symlink(&p, target).map_err(errno)?;
        self.journal.record(Op::Created, p.as_bytes());
        let f = self.view(&p)?.ok_or(Errno::EIO)?;
        let ino = self
            .table
            .lookup(parent, name.as_bytes())
            .map_err(errno)?
            .ok_or(Errno::ENOENT)?;
        Ok(self.attr(ino, &f))
    }

    fn readlink(&self, ino: u64) -> Res<Vec<u8>> {
        let p = self.loc(ino)?.ok_or(Errno::EINVAL)?;
        let f = self.view(&p)?.ok_or(Errno::ENOENT)?;
        match &f.src {
            Src::Link => Ok(self.redirect_target(&p).into_os_string().into_vec()),
            Src::Over(m) if m.file_type().is_symlink() => Ok(self
                .overlay
                .read_link(&p)
                .map_err(errno)?
                .into_os_string()
                .into_vec()),
            Src::Base(n) if n.kind == Kind::Symlink => {
                let c = n.content().ok_or(Errno::EIO)?;
                let s = self.blk(self.core.read_content(c)).map_err(vfs_errno)?;
                self.blk(s.read_all(MAX_LINK as u64)).map_err(vfs_errno)
            }
            _ => Err(Errno::EINVAL),
        }
    }

    /// Removes the file or directory `name` of `parent`.
    fn remove(&self, parent: u64, name: &OsStr, dir: bool) -> Res<()> {
        let (pp, p) = self.child(parent, name)?;
        let _g = lock(&self.ns);
        let f = self.view(&p)?.ok_or(Errno::ENOENT)?;
        if let Src::Link = f.src {
            // Removing a redirected name empties what it points to; the name stays.
            if dir {
                return Err(Errno::ENOTDIR);
            }
            if let Some(abs) = self.redirect.resolve(&p) {
                let _ = fs::remove_dir_all(&abs);
            }
            self.redirect.forget(&p);
            return Ok(());
        }
        self.writable_dir(pp.as_ref())?;
        if f.read_only() {
            return Err(Errno::EACCES);
        }
        match (dir, f.kind()) {
            (true, VKind::Dir) => {
                if !self.is_empty_dir(&p)? {
                    return Err(Errno::ENOTEMPTY);
                }
            }
            (true, _) => return Err(Errno::ENOTDIR),
            (false, VKind::Dir) => return Err(Errno::EISDIR),
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
        if let Some(ino) = self.table.peek(parent, name.as_bytes()) {
            self.pt.invalidate(ino);
        }
        self.table.unlink(parent, name.as_bytes()).map_err(errno)?;
        self.journal.record(Op::Deleted, p.as_bytes());
        Ok(())
    }

    fn rename(
        &self,
        parent: u64,
        name: &OsStr,
        newparent: u64,
        newname: &OsStr,
        flags: RenameFlags,
    ) -> Res<()> {
        if flags.contains(RenameFlags::RENAME_EXCHANGE)
            || flags.contains(RenameFlags::RENAME_WHITEOUT)
        {
            return Err(Errno::EINVAL);
        }
        let (spp, from) = self.child(parent, name)?;
        let (dpp, to) = self.child(newparent, newname)?;
        if !checkout_safe(&to) {
            return Err(Errno::EPERM);
        }
        if self.reds.covers(&from) || self.reds.covers(&to) {
            return Err(Errno::EPERM);
        }
        let _g = lock(&self.ns);
        let src = self.view(&from)?.ok_or(Errno::ENOENT)?;
        self.writable_dir(spp.as_ref())?;
        self.writable_dir(dpp.as_ref())?;
        if src.read_only() {
            return Err(Errno::EACCES);
        }
        if from == to {
            return Ok(());
        }
        let is_dir = src.kind() == VKind::Dir;
        if is_dir && to.starts_with_dir(&from) {
            return Err(Errno::EINVAL);
        }
        if let Some(d) = self.view(&to)? {
            if flags.contains(RenameFlags::RENAME_NOREPLACE) {
                return Err(Errno::EEXIST);
            }
            if d.read_only() {
                return Err(Errno::EACCES);
            }
            match (is_dir, d.kind() == VKind::Dir) {
                (true, false) => return Err(Errno::ENOTDIR),
                (false, true) => return Err(Errno::EISDIR),
                (true, true) if !self.is_empty_dir(&to)? => return Err(Errno::ENOTEMPTY),
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
                    return Err(Errno::EACCES);
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
        if let Some(ino) = self.table.peek(parent, name.as_bytes()) {
            self.pt.invalidate(ino);
        }
        self.table
            .rename(parent, name.as_bytes(), newparent, newname.as_bytes())
            .map_err(errno)?;
        if is_dir {
            self.journal.record(Op::DirRenamedFrom, from.as_bytes());
            self.journal.record(Op::DirRenamedTo, to.as_bytes());
        } else {
            self.journal.record(Op::RenamedFrom, from.as_bytes());
            self.journal.record(Op::Created, to.as_bytes());
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn setattr(
        &self,
        ino: u64,
        mode: Option<u32>,
        uid: Option<u32>,
        gid: Option<u32>,
        size: Option<u64>,
        atime: Option<TimeOrNow>,
        mtime: Option<TimeOrNow>,
        fh: Option<u64>,
    ) -> Res<(FileAttr, Duration)> {
        if uid.is_some_and(|u| u != self.uid) || gid.is_some_and(|g| g != self.gid) {
            return Err(Errno::EPERM);
        }
        let changes = mode.is_some() || size.is_some() || atime.is_some() || mtime.is_some();
        if !changes {
            return self.getattr(ino, fh);
        }
        if let Some(h) = fh.and_then(|fh| self.handle(fh).ok())
            && h.writable
        {
            self.apply_attrs(&h.file, mode, size, atime, mtime)?;
            if let Some(p) = self.loc(ino)? {
                self.journal.record(Op::Modified, p.as_bytes());
            }
            return self.getattr(ino, fh);
        }
        let p = self.loc(ino)?.ok_or(Errno::EISDIR)?;
        let f = self.view(&p)?.ok_or(Errno::ENOENT)?;
        match f.kind() {
            VKind::Dir => {
                return if size.is_some() {
                    Err(Errno::EISDIR)
                } else {
                    self.getattr(ino, fh)
                };
            }
            VKind::Symlink => return self.getattr(ino, fh),
            VKind::File => {}
        }
        if f.read_only() {
            return Err(Errno::EACCES);
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
        self.apply_attrs(&file, mode, size, atime, mtime)?;
        self.journal.record(Op::Modified, p.as_bytes());
        self.pt.invalidate(ino);
        self.getattr(ino, None)
    }

    fn apply_attrs(
        &self,
        file: &File,
        mode: Option<u32>,
        size: Option<u64>,
        atime: Option<TimeOrNow>,
        mtime: Option<TimeOrNow>,
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
                t = t.set_accessed(unix_time(a));
            }
            if let Some(m) = mtime {
                t = t.set_modified(unix_time(m));
            }
            file.set_times(t).map_err(errno)?;
        }
        Ok(())
    }

    fn opendir(&self, ino: u64) -> Res<u64> {
        let dir = self.loc(ino)?;
        if let Some(d) = &dir {
            let f = self.view(d)?.ok_or(Errno::ENOENT)?;
            if f.kind() != VKind::Dir {
                return Err(Errno::ENOTDIR);
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
        let mut ds = lock(&self.dirs);
        if ds.len() >= MAX_HANDLES {
            return Err(Errno::EMFILE);
        }
        let fh = self.next_fh.fetch_add(1, Ordering::Relaxed);
        ds.insert(fh, Arc::new(Snapshot { entries, parent }));
        Ok(fh)
    }

    fn snapshot(&self, fh: u64) -> Res<Arc<Snapshot>> {
        lock(&self.dirs).get(&fh).cloned().ok_or(Errno::EBADF)
    }

    /// The inode number of `name` below `parent`, assigned on first sight and not counted as a
    /// kernel lookup.
    fn ino_for_listing(&self, parent: u64, name: &[u8]) -> u64 {
        match self.table.lookup(parent, name) {
            Ok(Some(i)) => {
                self.table.forget(i, 1);
                i
            }
            _ => u64::MAX,
        }
    }

    fn readdir(&self, ino: u64, fh: u64, offset: u64, reply: &mut ReplyDirectory) -> Res<()> {
        let snap = self.snapshot(fh)?;
        let start = usize::try_from(offset).map_err(|_| Errno::EINVAL)?;
        for (i, (name, f)) in snap.entries.iter().enumerate().skip(start) {
            let next = (i + 1) as u64;
            let (eino, kind) = match (name.as_slice(), f) {
                (b".", _) => (ino, FileType::Directory),
                (b"..", _) => (snap.parent, FileType::Directory),
                (_, Some(f)) => (self.ino_for_listing(ino, name), file_type(f.kind())),
                _ => continue,
            };
            if reply.add(INodeNo(eino), next, kind, OsStr::from_bytes(name)) {
                break;
            }
        }
        Ok(())
    }

    fn readdirplus(
        &self,
        ino: u64,
        fh: u64,
        offset: u64,
        reply: &mut ReplyDirectoryPlus,
    ) -> Res<()> {
        let snap = self.snapshot(fh)?;
        let start = usize::try_from(offset).map_err(|_| Errno::EINVAL)?;
        for (i, (name, f)) in snap.entries.iter().enumerate().skip(start) {
            let next = (i + 1) as u64;
            let os = OsStr::from_bytes(name);
            match (name.as_slice(), f) {
                (b"." | b"..", _) => {
                    let target = if name == b"." { ino } else { snap.parent };
                    let mut attr = self.root_attr();
                    attr.ino = INodeNo(target);
                    if reply.add(
                        INodeNo(target),
                        next,
                        os,
                        &Duration::ZERO,
                        &attr,
                        Generation(0),
                    ) {
                        break;
                    }
                }
                (_, Some(f)) => {
                    let Ok(Some(eino)) = self.table.lookup(ino, name) else {
                        continue;
                    };
                    let (attr, ttl) = self.attr(eino, f);
                    if reply.add(INodeNo(eino), next, os, &ttl, &attr, Generation(0)) {
                        // Not delivered: the kernel did not look it up.
                        self.table.forget(eino, 1);
                        break;
                    }
                }
                _ => {}
            }
        }
        Ok(())
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

    /// Tells the kernel which entries and inodes changed behind its back.
    fn invalidate(&self, changed: &[RepoPath]) {
        let n = lock(&self.notifier);
        let Some(n) = n.as_ref() else { return };
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
                    let _ = n.inval_entry(INodeNo(parent), OsStr::from_bytes(&name));
                    self.pt.invalidate(ino);
                }
                if touched.insert(ino) {
                    let _ = n.inval_inode(INodeNo(ino), 0, 0);
                }
            }
        }
    }
}

/// The `fuser` file system over [`Inner`].
struct FuseFs {
    inner: Arc<Inner>,
}

impl fuser::Filesystem for FuseFs {
    fn init(&mut self, _req: &Request, config: &mut KernelConfig) -> io::Result<()> {
        let _ = config.add_capabilities(
            InitFlags::FUSE_DO_READDIRPLUS
                | InitFlags::FUSE_READDIRPLUS_AUTO
                | InitFlags::FUSE_PARALLEL_DIROPS,
        );
        if self.inner.want_pt
            && config.add_capabilities(InitFlags::FUSE_PASSTHROUGH).is_ok()
            && config.set_max_stack_depth(2).is_ok()
        {
            self.inner.pt.set_negotiated(true);
        }
        Ok(())
    }

    fn lookup(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        match self.inner.lookup(parent.0, name) {
            Ok((attr, ttl)) => reply.entry_with_ttls(&ttl, &TTL_BASE, &attr, Generation(0)),
            Err(e) => reply.error(e),
        }
    }

    fn forget(&self, _req: &Request, ino: INodeNo, nlookup: u64) {
        self.inner.table.forget(ino.0, nlookup);
    }

    fn getattr(&self, _req: &Request, ino: INodeNo, fh: Option<FileHandle>, reply: ReplyAttr) {
        match self.inner.getattr(ino.0, fh.map(|f| f.0)) {
            Ok((attr, ttl)) => reply.attr(&ttl, &attr),
            Err(e) => reply.error(e),
        }
    }

    fn setattr(
        &self,
        _req: &Request,
        ino: INodeNo,
        mode: Option<u32>,
        uid: Option<u32>,
        gid: Option<u32>,
        size: Option<u64>,
        atime: Option<TimeOrNow>,
        mtime: Option<TimeOrNow>,
        _ctime: Option<SystemTime>,
        fh: Option<FileHandle>,
        _crtime: Option<SystemTime>,
        _chgtime: Option<SystemTime>,
        _bkuptime: Option<SystemTime>,
        _flags: Option<BsdFileFlags>,
        reply: ReplyAttr,
    ) {
        match self
            .inner
            .setattr(ino.0, mode, uid, gid, size, atime, mtime, fh.map(|f| f.0))
        {
            Ok((attr, ttl)) => reply.attr(&ttl, &attr),
            Err(e) => reply.error(e),
        }
    }

    fn readlink(&self, _req: &Request, ino: INodeNo, reply: ReplyData) {
        match self.inner.readlink(ino.0) {
            Ok(t) => reply.data(&t),
            Err(e) => reply.error(e),
        }
    }

    fn mknod(
        &self,
        _req: &Request,
        _parent: INodeNo,
        _name: &OsStr,
        _mode: u32,
        _umask: u32,
        _rdev: u32,
        reply: ReplyEntry,
    ) {
        reply.error(Errno::EPERM);
    }

    fn mkdir(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        mode: u32,
        umask: u32,
        reply: ReplyEntry,
    ) {
        match self.inner.mkdir(parent.0, name, mode, umask) {
            Ok((attr, ttl)) => reply.entry_with_ttls(&ttl, &TTL_BASE, &attr, Generation(0)),
            Err(e) => reply.error(e),
        }
    }

    fn unlink(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        match self.inner.remove(parent.0, name, false) {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(e),
        }
    }

    fn rmdir(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        match self.inner.remove(parent.0, name, true) {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(e),
        }
    }

    fn symlink(
        &self,
        _req: &Request,
        parent: INodeNo,
        link_name: &OsStr,
        target: &Path,
        reply: ReplyEntry,
    ) {
        match self.inner.symlink(parent.0, link_name, target) {
            Ok((attr, ttl)) => reply.entry_with_ttls(&ttl, &TTL_BASE, &attr, Generation(0)),
            Err(e) => reply.error(e),
        }
    }

    fn rename(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        newparent: INodeNo,
        newname: &OsStr,
        flags: RenameFlags,
        reply: ReplyEmpty,
    ) {
        match self
            .inner
            .rename(parent.0, name, newparent.0, newname, flags)
        {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(e),
        }
    }

    fn link(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _newparent: INodeNo,
        _newname: &OsStr,
        reply: ReplyEntry,
    ) {
        reply.error(Errno::EPERM);
    }

    fn open(&self, req: &Request, ino: INodeNo, flags: OpenFlags, reply: ReplyOpen) {
        match self.inner.open(req.pid(), ino.0, flags.0, &reply) {
            Ok((fh, fl, Some(id))) => reply.opened_passthrough(FileHandle(fh), fl, &id),
            Ok((fh, fl, None)) => reply.opened(FileHandle(fh), fl),
            Err(e) => reply.error(e),
        }
    }

    fn read(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        size: u32,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        reply: ReplyData,
    ) {
        match self.inner.read(fh.0, offset, size) {
            Ok(b) => reply.data(&b),
            Err(e) => reply.error(e),
        }
    }

    fn write(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        data: &[u8],
        _write_flags: WriteFlags,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        reply: ReplyWrite,
    ) {
        match self.inner.write(fh.0, offset, data) {
            Ok(n) => reply.written(n),
            Err(e) => reply.error(e),
        }
    }

    fn flush(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _fh: FileHandle,
        _lock_owner: LockOwner,
        reply: ReplyEmpty,
    ) {
        reply.ok();
    }

    fn release(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        _flush: bool,
        reply: ReplyEmpty,
    ) {
        self.inner.release(fh.0);
        reply.ok();
    }

    fn fsync(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        datasync: bool,
        reply: ReplyEmpty,
    ) {
        let r = self.inner.handle(fh.0).and_then(|h| {
            if !h.writable {
                return Ok(());
            }
            if datasync {
                h.file.sync_data()
            } else {
                h.file.sync_all()
            }
            .map_err(errno)
        });
        match r {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(e),
        }
    }

    fn opendir(&self, _req: &Request, ino: INodeNo, _flags: OpenFlags, reply: ReplyOpen) {
        match self.inner.opendir(ino.0) {
            Ok(fh) => reply.opened(FileHandle(fh), FopenFlags::empty()),
            Err(e) => reply.error(e),
        }
    }

    fn readdir(
        &self,
        _req: &Request,
        ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        mut reply: ReplyDirectory,
    ) {
        match self.inner.readdir(ino.0, fh.0, offset, &mut reply) {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(e),
        }
    }

    fn readdirplus(
        &self,
        _req: &Request,
        ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        mut reply: ReplyDirectoryPlus,
    ) {
        match self.inner.readdirplus(ino.0, fh.0, offset, &mut reply) {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(e),
        }
    }

    fn releasedir(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        _flags: OpenFlags,
        reply: ReplyEmpty,
    ) {
        lock(&self.inner.dirs).remove(&fh.0);
        reply.ok();
    }

    fn fsyncdir(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _fh: FileHandle,
        _datasync: bool,
        reply: ReplyEmpty,
    ) {
        reply.ok();
    }

    fn statfs(&self, _req: &Request, _ino: INodeNo, reply: ReplyStatfs) {
        match rustix::fs::statvfs(self.inner.overlay.root()) {
            Ok(s) => reply.statfs(
                s.f_blocks,
                s.f_bfree,
                s.f_bavail,
                s.f_files,
                s.f_ffree,
                u32::try_from(s.f_bsize).unwrap_or(BLKSIZE),
                u32::try_from(s.f_namemax)
                    .unwrap_or(255)
                    .min(MAX_NAME as u32),
                u32::try_from(s.f_frsize).unwrap_or(BLKSIZE),
            ),
            Err(e) => reply.error(Errno::from_i32(e.raw_os_error())),
        }
    }

    fn setxattr(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _name: &OsStr,
        _value: &[u8],
        _flags: i32,
        _position: u32,
        reply: ReplyEmpty,
    ) {
        reply.error(Errno::EOPNOTSUPP);
    }

    fn getxattr(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _name: &OsStr,
        _size: u32,
        reply: ReplyXattr,
    ) {
        reply.error(Errno::NO_XATTR);
    }

    fn listxattr(&self, _req: &Request, _ino: INodeNo, size: u32, reply: ReplyXattr) {
        if size == 0 {
            reply.size(0);
        } else {
            reply.data(&[]);
        }
    }

    fn removexattr(&self, _req: &Request, _ino: INodeNo, _name: &OsStr, reply: ReplyEmpty) {
        reply.error(Errno::EOPNOTSUPP);
    }

    fn access(&self, _req: &Request, _ino: INodeNo, _mask: AccessFlags, reply: ReplyEmpty) {
        // `default_permissions` makes the kernel decide from the attributes.
        reply.ok();
    }

    fn create(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        mode: u32,
        umask: u32,
        _flags: i32,
        reply: ReplyCreate,
    ) {
        match self.inner.create(parent.0, name, mode, umask) {
            Ok((attr, fh)) => {
                reply.created(
                    &Duration::ZERO,
                    &attr,
                    Generation(0),
                    FileHandle(fh),
                    FopenFlags::empty(),
                );
            }
            Err(e) => reply.error(e),
        }
    }
}

/// The FUSE [`Provider`].
pub struct FuseProvider {
    inner: Arc<Inner>,
    session: Mutex<Option<fuser::BackgroundSession>>,
    disk: FuseDisk,
}

impl FuseProvider {
    /// Opens the overlay and the inode table; nothing is mounted yet.
    pub fn new(cfg: Config) -> io::Result<Self> {
        let overlay = Overlay::open(&cfg.overlay)?;
        fs::create_dir_all(&cfg.redirect)?;
        let meta = fs::metadata(overlay.root())?;
        let whites = Whites::load(&overlay)?;
        let inner = Arc::new(Inner {
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
            pt: Passthrough::default(),
            want_pt: cfg.passthrough,
            notifier: Mutex::new(None),
            mount: Mutex::new(None),
        });
        Ok(Self {
            disk: FuseDisk {
                inner: inner.clone(),
            },
            inner,
            session: Mutex::new(None),
        })
    }

    /// The file system core, for tests and statistics.
    pub fn core(&self) -> &Arc<CoreFs> {
        &self.inner.core
    }

    /// Bytes of regular files the overlay holds.
    pub fn overlay_bytes(&self) -> io::Result<u64> {
        self.inner.overlay.bytes()
    }

    /// The paths the user deleted.
    pub fn whiteouts(&self) -> io::Result<Vec<RepoPath>> {
        self.inner.overlay.whiteouts()
    }

    /// Whether the kernel accepted passthrough, and how many backing files were registered.
    pub fn passthrough_stats(&self) -> (bool, u64) {
        (self.inner.pt.usable(), self.inner.pt.registered())
    }

    /// The note to print after a switch when a process watches the mount with inotify (which
    /// does not see changes the file system makes), else `None`.
    pub fn switch_note(&self) -> Option<String> {
        let root = lock(&self.inner.mount).clone()?;
        match super::inotify_users(&root) {
            Ok(p) if !p.is_empty() => Some(WATCH_NOTE.to_owned()),
            _ => None,
        }
    }
}

impl Provider for FuseProvider {
    fn mount(&self, root: &Path) -> Result<()> {
        let mut session = lock(&self.session);
        if session.is_some() {
            return Err(VfsError::AlreadyMounted);
        }
        fs::create_dir_all(root)?;
        let meta = fs::symlink_metadata(root)?;
        if !meta.is_dir() || asc_wc::safe_path::is_link(&meta) {
            return Err(VfsError::Io(io::Error::new(
                io::ErrorKind::InvalidInput,
                "the mount point must be a real directory",
            )));
        }
        let mut cfg = fuser::Config::default();
        cfg.mount_options = vec![
            MountOption::FSName("asc".to_owned()),
            MountOption::Subtype("asc".to_owned()),
            MountOption::DefaultPermissions,
            MountOption::NoDev,
            MountOption::NoSuid,
            MountOption::NoAtime,
        ];
        cfg.n_threads = Some(
            std::thread::available_parallelism()
                .map_or(2, std::num::NonZero::get)
                .clamp(2, 8),
        );
        let bg = fuser::spawn_mount(
            FuseFs {
                inner: self.inner.clone(),
            },
            root,
            &cfg,
        )?;
        *lock(&self.inner.notifier) = Some(bg.notifier());
        *lock(&self.inner.mount) = Some(root.to_path_buf());
        *session = Some(bg);
        Ok(())
    }

    fn unmount(&self) -> Result<()> {
        let Some(bg) = lock(&self.session).take() else {
            return Err(VfsError::NotMounted);
        };
        *lock(&self.inner.notifier) = None;
        bg.umount_and_join()?;
        *lock(&self.inner.mount) = None;
        lock(&self.inner.handles).clear();
        lock(&self.inner.dirs).clear();
        Ok(())
    }

    /// Rewrites the overlay for the new version and tells the kernel what changed. The caller
    /// has already pointed [`CoreFs`] at the new tree (`set_root`). Open files keep the content
    /// they were opened on, as with an unlink, so nothing stays pending.
    fn apply_switch(&self, plan: &Plan, _times: Switched) -> Result<SwitchOutcome> {
        if !plan.conflicts.is_empty() {
            return Err(VfsError::Conflicts);
        }
        let mut changed = Vec::with_capacity(plan.updates.len() + plan.deletes.len());
        {
            let _g = lock(&self.inner.ns);
            for (p, _) in &plan.updates {
                self.inner
                    .switch_one(p, false)
                    .map_err(|e| VfsError::Io(io::Error::from_raw_os_error(i32::from(e))))?;
                changed.push(p.clone());
            }
            for p in &plan.deletes {
                self.inner
                    .switch_one(p, true)
                    .map_err(|e| VfsError::Io(io::Error::from_raw_os_error(i32::from(e))))?;
                changed.push(p.clone());
            }
        }
        self.inner.invalidate(&changed);
        Ok(SwitchOutcome::default())
    }

    /// A clean file holds nothing locally, so this drops the kernel's cached pages and
    /// passthrough registration. Modified or missing paths are skipped.
    fn dehydrate(&self, paths: &[RepoPath]) -> Result<DehydrateOutcome> {
        let mut out = DehydrateOutcome::default();
        let mut clean = Vec::new();
        for p in paths {
            let found = self
                .inner
                .view(p)
                .map_err(|e| VfsError::Io(io::Error::from_raw_os_error(i32::from(e))))?;
            match found {
                Some(f) if matches!(f.src, Src::Base(_)) => {
                    out.dehydrated += 1;
                    clean.push(p.clone());
                }
                _ => out.skipped.push(p.clone()),
            }
        }
        if let Some(n) = lock(&self.inner.notifier).as_ref() {
            for p in &clean {
                if let Some(ino) = self.inner.table.find(p) {
                    self.inner.pt.invalidate(ino);
                    let _ = n.inval_inode(INodeNo(ino), 0, 0);
                }
            }
        }
        Ok(out)
    }

    fn disk_state(&self) -> &dyn DiskState {
        &self.disk
    }
}

impl Drop for FuseProvider {
    fn drop(&mut self) {
        let _ = self.unmount();
    }
}

/// The on-disk view for virtual status: the overlay is what is on disk.
struct FuseDisk {
    inner: Arc<Inner>,
}

fn io_of(e: Errno) -> io::Error {
    io::Error::from_raw_os_error(i32::from(e))
}

impl DiskState for FuseDisk {
    fn state(&self, path: &RepoPath) -> io::Result<PathState> {
        let i = &self.inner;
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
        let Some(f) = self.inner.view(path).map_err(io_of)? else {
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
        let i = &self.inner;
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
        let i = &self.inner;
        let mut out = Vec::new();
        for (name, is_dir) in i.overlay.children(Some(dir))? {
            if let Ok(name) = Name::new(name) {
                out.push(DirEntry { name, dir: is_dir });
            }
        }
        Ok(out)
    }
}
