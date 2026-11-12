//! The `fuser` front end over the merged file system (SPEC §10.3): translates each kernel request
//! to [`MergedFs`], hands clean files to the kernel through passthrough where it is available,
//! and tells the kernel what a switch or a dehydration changed behind its back.

use std::collections::HashMap;
use std::ffi::OsStr;
use std::fs;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, SystemTime};

use asc_core::path::RepoPath;
use asc_store::cachedir::CacheDir;
use asc_wc::redirect::Redirections;
use asc_wc::switch::Plan;
use asc_wc::times::Switched;
use asc_wc::vstatus::DiskState;
use fuser::{
    AccessFlags, BackingId, BsdFileFlags, Errno, FileAttr, FileHandle, FileType, FopenFlags,
    Generation, INodeNo, InitFlags, KernelConfig, LockOwner, MountOption, Notifier, OpenFlags,
    RenameFlags, ReplyAttr, ReplyCreate, ReplyData, ReplyDirectory, ReplyDirectoryPlus, ReplyEmpty,
    ReplyEntry, ReplyOpen, ReplyStatfs, ReplyWrite, ReplyXattr, Request, TimeOrNow, WriteFlags,
};
use tokio::runtime::Handle;

use super::passthrough::Passthrough;
use super::{JournalSink, TimeSource, WATCH_NOTE};
use crate::core::{CoreFs, DehydrateOutcome, Provider, Result, SwitchOutcome, VfsError};
use crate::unixfs::inodes::InodeStore;
use crate::unixfs::merged::{BLKSIZE, MAX_NAME, TTL_BASE};
use crate::unixfs::{self, Attr, Disk, MergedFs, Notify, VKind};

/// The passthrough id each open handle holds a count on, with the handle's inode.
type Backing = Arc<Mutex<HashMap<u64, (u64, Arc<BackingId>)>>>;

/// `open(2)` flag bits (Linux).
const O_TRUNC: i32 = 0o1000;
const O_ACCMODE: i32 = 3;
const O_WRONLY: i32 = 1;

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
    /// The gitdir the root's read-only `.git` gitfile names (see [`unixfs::Config::gitfile`]).
    pub gitfile: Option<PathBuf>,
    /// Try kernel passthrough for clean files.
    pub passthrough: bool,
    /// Who besides the owner may use the mount (a CI job's container).
    pub access: Access,
}

/// Who may use a FUSE mount. The kernel still checks file modes (`default_permissions`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Access {
    /// Only the user who mounted it (FUSE's default).
    #[default]
    Owner,
    /// The owner and root (`allow_root`): a container that runs as root.
    Root,
    /// Every user (`allow_other`): a container that runs as another user.
    Anyone,
}

impl Access {
    /// Parses `asc.vfs.mountAccess`: `owner`, `root` or `all`.
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "owner" => Some(Self::Owner),
            "root" => Some(Self::Root),
            "all" => Some(Self::Anyone),
            _ => None,
        }
    }
}

impl Config {
    /// The FUSE configuration over the merged file system's.
    pub fn over(cfg: unixfs::Config, passthrough: bool) -> Self {
        Self {
            core: cfg.core,
            local: cfg.local,
            inodes: cfg.inodes,
            overlay: cfg.overlay,
            redirect: cfg.redirect,
            redirections: cfg.redirections,
            times: cfg.times,
            journal: cfg.journal,
            rt: cfg.rt,
            root_time: cfg.root_time,
            gitfile: cfg.gitfile,
            passthrough,
            access: Access::Owner,
        }
    }
}

fn fe(e: unixfs::Errno) -> Errno {
    Errno::from_i32(e.raw_os_error())
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn unix_time(t: TimeOrNow) -> SystemTime {
    match t {
        TimeOrNow::SpecificTime(t) => t,
        TimeOrNow::Now => SystemTime::now(),
    }
}

fn file_type(k: VKind) -> FileType {
    match k {
        VKind::Dir => FileType::Directory,
        VKind::File => FileType::RegularFile,
        VKind::Symlink => FileType::Symlink,
    }
}

fn fattr(a: &Attr) -> FileAttr {
    FileAttr {
        ino: INodeNo(a.ino),
        size: a.size,
        blocks: a.blocks,
        atime: a.atime,
        mtime: a.mtime,
        ctime: a.ctime,
        crtime: a.mtime,
        kind: file_type(a.kind),
        perm: a.perm,
        nlink: a.nlink,
        uid: a.uid,
        gid: a.gid,
        rdev: 0,
        blksize: a.blksize,
        flags: 0,
    }
}

fn image_of(pid: u32) -> String {
    fs::read_link(format!("/proc/{pid}/exe"))
        .ok()
        .and_then(|p| p.into_os_string().into_string().ok())
        .unwrap_or_else(|| "unknown".to_owned())
}

/// The kernel side: passthrough registrations and the invalidation channel.
#[derive(Default)]
struct Kernel {
    pt: Passthrough,
    notifier: Mutex<Option<Notifier>>,
}

impl Notify for Kernel {
    fn content(&self, ino: u64) {
        self.pt.invalidate(ino);
    }

    fn entry(&self, parent: u64, name: &[u8]) {
        if let Some(n) = lock(&self.notifier).as_ref() {
            let _ = n.inval_entry(INodeNo(parent), OsStr::from_bytes(name));
        }
    }

    fn inode(&self, ino: u64) {
        if let Some(n) = lock(&self.notifier).as_ref() {
            let _ = n.inval_inode(INodeNo(ino), 0, 0);
        }
    }
}

/// The `fuser` file system over [`MergedFs`].
struct FuseFs {
    fs: Arc<MergedFs>,
    kernel: Arc<Kernel>,
    want_pt: bool,
    /// The passthrough id each open handle holds a count on.
    backing: Backing,
}

impl FuseFs {
    fn entry(&self, r: unixfs::merged::Res<Attr>, reply: ReplyEntry) {
        match r {
            Ok(a) => reply.entry_with_ttls(&a.ttl, &TTL_BASE, &fattr(&a), Generation(0)),
            Err(e) => reply.error(fe(e)),
        }
    }

    fn attr(r: unixfs::merged::Res<Attr>, reply: ReplyAttr) {
        match r {
            Ok(a) => reply.attr(&a.ttl, &fattr(&a)),
            Err(e) => reply.error(fe(e)),
        }
    }

    fn empty<T>(r: unixfs::merged::Res<T>, reply: ReplyEmpty) {
        match r {
            Ok(_) => reply.ok(),
            Err(e) => reply.error(fe(e)),
        }
    }
}

impl fuser::Filesystem for FuseFs {
    fn init(&mut self, _req: &Request, config: &mut KernelConfig) -> io::Result<()> {
        let _ = config.add_capabilities(
            InitFlags::FUSE_DO_READDIRPLUS
                | InitFlags::FUSE_READDIRPLUS_AUTO
                | InitFlags::FUSE_PARALLEL_DIROPS,
        );
        if self.want_pt
            && config.add_capabilities(InitFlags::FUSE_PASSTHROUGH).is_ok()
            && config.set_max_stack_depth(2).is_ok()
        {
            self.kernel.pt.set_negotiated(true);
        }
        Ok(())
    }

    fn lookup(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        self.entry(self.fs.lookup(parent.0, name.as_bytes()), reply);
    }

    fn forget(&self, _req: &Request, ino: INodeNo, nlookup: u64) {
        self.fs.table.forget(ino.0, nlookup);
    }

    fn getattr(&self, _req: &Request, ino: INodeNo, fh: Option<FileHandle>, reply: ReplyAttr) {
        Self::attr(self.fs.getattr(ino.0, fh.map(|f| f.0)), reply);
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
        Self::attr(
            self.fs.setattr(
                ino.0,
                mode,
                uid,
                gid,
                size,
                atime.map(unix_time),
                mtime.map(unix_time),
                fh.map(|f| f.0),
            ),
            reply,
        );
    }

    fn readlink(&self, _req: &Request, ino: INodeNo, reply: ReplyData) {
        match self.fs.readlink(ino.0) {
            Ok(t) => reply.data(&t),
            Err(e) => reply.error(fe(e)),
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
        self.entry(
            self.fs.mkdir(parent.0, name.as_bytes(), mode & !umask),
            reply,
        );
    }

    fn unlink(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        Self::empty(self.fs.remove(parent.0, name.as_bytes(), false), reply);
    }

    fn rmdir(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        Self::empty(self.fs.remove(parent.0, name.as_bytes(), true), reply);
    }

    fn symlink(
        &self,
        _req: &Request,
        parent: INodeNo,
        link_name: &OsStr,
        target: &Path,
        reply: ReplyEntry,
    ) {
        self.entry(
            self.fs.symlink(
                parent.0,
                link_name.as_bytes(),
                target.as_os_str().as_bytes(),
            ),
            reply,
        );
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
        if flags.contains(RenameFlags::RENAME_EXCHANGE)
            || flags.contains(RenameFlags::RENAME_WHITEOUT)
        {
            reply.error(Errno::EINVAL);
            return;
        }
        Self::empty(
            self.fs.rename(
                parent.0,
                name.as_bytes(),
                newparent.0,
                newname.as_bytes(),
                flags.contains(RenameFlags::RENAME_NOREPLACE),
            ),
            reply,
        );
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
        let acc = flags.0 & O_ACCMODE;
        let trunc = flags.0 & O_TRUNC != 0;
        let opened = match self.fs.open(
            &image_of(req.pid()),
            ino.0,
            acc != O_WRONLY,
            acc != 0,
            trunc,
        ) {
            Ok(o) => o,
            Err(e) => return reply.error(fe(e)),
        };
        if !opened.base {
            return reply.opened(FileHandle(opened.fh), FopenFlags::empty());
        }
        let id = if self.want_pt {
            self.fs
                .handle(opened.fh)
                .ok()
                .and_then(|h| self.kernel.pt.acquire(ino.0, &reply, &h.file))
        } else {
            None
        };
        match id {
            Some(id) => {
                lock(&self.backing).insert(opened.fh, (ino.0, id.clone()));
                // Passthrough and a kept page cache exclude each other.
                reply.opened_passthrough(FileHandle(opened.fh), FopenFlags::empty(), &id);
            }
            None => reply.opened(FileHandle(opened.fh), FopenFlags::FOPEN_KEEP_CACHE),
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
        match self.fs.read(fh.0, offset, size) {
            Ok(b) => reply.data(&b),
            Err(e) => reply.error(fe(e)),
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
        match self.fs.write(fh.0, offset, data) {
            Ok(n) => reply.written(n),
            Err(e) => reply.error(fe(e)),
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
        self.fs.release(fh.0);
        if let Some((ino, id)) = lock(&self.backing).remove(&fh.0) {
            self.kernel.pt.release(ino, &id);
        }
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
        let r = self.fs.handle(fh.0).and_then(|h| {
            if !h.writable {
                return Ok(());
            }
            if datasync {
                h.file.sync_data()
            } else {
                h.file.sync_all()
            }
            .map_err(unixfs::merged::errno)
        });
        Self::empty(r, reply);
    }

    fn opendir(&self, _req: &Request, ino: INodeNo, _flags: OpenFlags, reply: ReplyOpen) {
        match self.fs.opendir(ino.0) {
            Ok(fh) => reply.opened(FileHandle(fh), FopenFlags::empty()),
            Err(e) => reply.error(fe(e)),
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
        let snap = match self.fs.snapshot(fh.0) {
            Ok(s) => s,
            Err(e) => return reply.error(fe(e)),
        };
        let Ok(start) = usize::try_from(offset) else {
            return reply.error(Errno::EINVAL);
        };
        for (i, (name, f)) in snap.entries.iter().enumerate().skip(start) {
            let next = (i + 1) as u64;
            let (eino, kind) = match (name.as_slice(), f) {
                (b".", _) => (ino.0, FileType::Directory),
                (b"..", _) => (snap.parent, FileType::Directory),
                (_, Some(f)) => (self.fs.ino_for_listing(ino.0, name), file_type(f.kind())),
                (_, None) => match self.fs.gitfile_entry(ino.0, name) {
                    Some(a) => (a.ino, FileType::RegularFile),
                    None => continue,
                },
            };
            if reply.add(INodeNo(eino), next, kind, OsStr::from_bytes(name)) {
                break;
            }
        }
        reply.ok();
    }

    fn readdirplus(
        &self,
        _req: &Request,
        ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        mut reply: ReplyDirectoryPlus,
    ) {
        let snap = match self.fs.snapshot(fh.0) {
            Ok(s) => s,
            Err(e) => return reply.error(fe(e)),
        };
        let Ok(start) = usize::try_from(offset) else {
            return reply.error(Errno::EINVAL);
        };
        for (i, (name, f)) in snap.entries.iter().enumerate().skip(start) {
            let next = (i + 1) as u64;
            let os = OsStr::from_bytes(name);
            match (name.as_slice(), f) {
                (b"." | b"..", _) => {
                    let target = if name == b"." { ino.0 } else { snap.parent };
                    let mut attr = fattr(&self.fs.root_attr());
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
                    let Ok(Some(eino)) = self.fs.table.lookup(ino.0, name) else {
                        continue;
                    };
                    let a = self.fs.attr(eino, f);
                    if reply.add(INodeNo(eino), next, os, &a.ttl, &fattr(&a), Generation(0)) {
                        // Not delivered: the kernel did not look it up.
                        self.fs.table.forget(eino, 1);
                        break;
                    }
                }
                (_, None) => {
                    if let Some(a) = self.fs.gitfile_entry(ino.0, name)
                        && reply.add(INodeNo(a.ino), next, os, &a.ttl, &fattr(&a), Generation(0))
                    {
                        break;
                    }
                }
            }
        }
        reply.ok();
    }

    fn releasedir(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        _flags: OpenFlags,
        reply: ReplyEmpty,
    ) {
        self.fs.releasedir(fh.0);
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
        match self.fs.statfs() {
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
            Err(e) => reply.error(fe(e)),
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

    fn getxattr(&self, _req: &Request, ino: INodeNo, name: &OsStr, size: u32, reply: ReplyXattr) {
        if name.as_bytes() != super::PROJECTED_XATTR.as_bytes() {
            reply.error(Errno::NO_XATTR);
            return;
        }
        match self.fs.projected(ino.0) {
            Ok(true) if size == 0 => reply.size(1),
            Ok(true) => reply.data(b"1"),
            Ok(false) => reply.error(Errno::NO_XATTR),
            Err(e) => reply.error(fe(e)),
        }
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
        match self.fs.create(parent.0, name.as_bytes(), mode & !umask) {
            Ok((attr, fh)) => reply.created(
                &Duration::ZERO,
                &fattr(&attr),
                Generation(0),
                FileHandle(fh),
                FopenFlags::empty(),
            ),
            Err(e) => reply.error(fe(e)),
        }
    }
}

/// The FUSE [`Provider`].
pub struct FuseProvider {
    fs: Arc<MergedFs>,
    kernel: Arc<Kernel>,
    want_pt: bool,
    access: Access,
    backing: Backing,
    session: Mutex<Option<fuser::BackgroundSession>>,
    mount: Mutex<Option<PathBuf>>,
    disk: Disk,
}

impl FuseProvider {
    /// Opens the overlay and the inode table; nothing is mounted yet.
    pub fn new(cfg: Config) -> io::Result<Self> {
        let want_pt = cfg.passthrough;
        let access = cfg.access;
        let fs = Arc::new(MergedFs::new(unixfs::Config {
            core: cfg.core,
            local: cfg.local,
            inodes: cfg.inodes,
            overlay: cfg.overlay,
            redirect: cfg.redirect,
            redirections: cfg.redirections,
            times: cfg.times,
            journal: cfg.journal,
            rt: cfg.rt,
            root_time: cfg.root_time,
            gitfile: cfg.gitfile,
        })?);
        let kernel = Arc::new(Kernel::default());
        fs.set_notify(kernel.clone());
        Ok(Self {
            disk: Disk::new(fs.clone()),
            fs,
            kernel,
            want_pt,
            access,
            backing: Arc::new(Mutex::new(HashMap::new())),
            session: Mutex::new(None),
            mount: Mutex::new(None),
        })
    }

    /// The file system core, for tests and statistics.
    pub fn core(&self) -> &Arc<CoreFs> {
        self.fs.core()
    }

    /// Bytes of regular files the overlay holds.
    pub fn overlay_bytes(&self) -> io::Result<u64> {
        self.fs.overlay_bytes()
    }

    /// The paths the user deleted.
    pub fn whiteouts(&self) -> io::Result<Vec<RepoPath>> {
        self.fs.whiteouts()
    }

    /// The merged file system, for conversion back to a plain work tree.
    pub fn merged(&self) -> &Arc<MergedFs> {
        &self.fs
    }

    /// Whether the kernel accepted passthrough, and how many backing files were registered.
    pub fn passthrough_stats(&self) -> (bool, u64) {
        (self.kernel.pt.usable(), self.kernel.pt.registered())
    }

    /// The note to print after a switch when a process watches the mount with inotify (which
    /// does not see changes the file system makes), else `None`.
    pub fn switch_note(&self) -> Option<String> {
        let root = lock(&self.mount).clone()?;
        match super::inotify_users(&root) {
            Ok(p) if !p.is_empty() => Some(WATCH_NOTE.to_owned()),
            _ => None,
        }
    }
}

impl unixfs::UnixProvider for FuseProvider {
    fn merged(&self) -> &Arc<MergedFs> {
        &self.fs
    }
    fn backend(&self) -> &'static str {
        "fuse"
    }
    fn switch_note(&self) -> Option<String> {
        FuseProvider::switch_note(self)
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
        cfg.acl = match self.access {
            Access::Owner => fuser::SessionACL::Owner,
            Access::Root => fuser::SessionACL::RootAndOwner,
            Access::Anyone => fuser::SessionACL::All,
        };
        cfg.n_threads = Some(
            std::thread::available_parallelism()
                .map_or(2, std::num::NonZero::get)
                .clamp(2, 8),
        );
        let bg = fuser::spawn_mount(
            FuseFs {
                fs: self.fs.clone(),
                kernel: self.kernel.clone(),
                want_pt: self.want_pt,
                backing: self.backing.clone(),
            },
            root,
            &cfg,
        )?;
        *lock(&self.kernel.notifier) = Some(bg.notifier());
        *lock(&self.mount) = Some(root.to_path_buf());
        *session = Some(bg);
        Ok(())
    }

    fn unmount(&self) -> Result<()> {
        let Some(bg) = lock(&self.session).take() else {
            return Err(VfsError::NotMounted);
        };
        *lock(&self.kernel.notifier) = None;
        bg.umount_and_join()?;
        *lock(&self.mount) = None;
        self.fs.drop_handles();
        lock(&self.backing).clear();
        Ok(())
    }

    fn apply_switch(&self, plan: &Plan, _times: Switched) -> Result<SwitchOutcome> {
        self.fs.apply_switch(plan).map(|(o, _)| o)
    }

    fn dehydrate(&self, paths: &[RepoPath]) -> Result<DehydrateOutcome> {
        self.fs.dehydrate(paths)
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
