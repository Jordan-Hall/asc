//! The NFSv3 (RFC 1813) and MOUNT v3 server over [`MergedFs`], on one loopback TCP port.
//!
//! Plain threads, not the async runtime: a listener, a reader per connection, and a worker pool,
//! because every operation may block on hydration and [`MergedFs`] runs its async work itself.
//!
//! NFS is stateless, so the server keeps what the kernel would otherwise hold for it: one lookup
//! of every inode a handle was handed out for (bounded; an evicted number is revived from its
//! persisted path when its handle comes back), the open files, directory listings for cookie
//! continuation, and a reply cache so a retransmitted rename or remove is not run twice.

use std::collections::{HashMap, HashSet, VecDeque};
use std::io::{self, BufReader};
use std::net::{Ipv4Addr, Shutdown, SocketAddr, TcpListener, TcpStream};
use std::os::unix::fs::FileExt;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use super::rpc::{self, Accept, AuthStat, Bad, Call};
use super::xdr::{Garbage, Reader, Writer, XResult};
use crate::unixfs::inodes::ROOT;
use crate::unixfs::merged::{MAX_LINK, MAX_NAME, OpenFile, Snapshot, read_at};
use crate::unixfs::{Attr, Errno, MergedFs, Notify, VKind};

pub const NFS_PROG: u32 = 100_003;
pub const MOUNT_PROG: u32 = 100_005;
/// The one export.
pub const EXPORT: &str = "/asc";
/// The largest `READ` and `WRITE`.
pub const RTMAX: u32 = 1 << 20;
const WTMAX: u32 = RTMAX;
/// What a process sees as its name: NFS calls carry no process, so the hydration policy sees
/// this for every read through an NFS mount.
pub const IMAGE: &str = "unknown";

const MAX_FILES: usize = 1024;
const MAX_HOLDS: usize = 100_000;
const MAX_LISTINGS: usize = 256;
const MAX_REPLIES: usize = 1024;
const MAX_CONNS: usize = 16;
const QUEUE: usize = 256;
/// Longer names are read, then refused with `NAMETOOLONG` by the file system.
const MAX_WIRE_NAME: usize = 1024;
const FH_LEN: usize = 16;

/// `nfsstat3` values (RFC 1813 §2.6).
pub mod stat {
    pub const OK: u32 = 0;
    pub const PERM: u32 = 1;
    pub const NOENT: u32 = 2;
    pub const IO: u32 = 5;
    pub const ACCES: u32 = 13;
    pub const EXIST: u32 = 17;
    pub const XDEV: u32 = 18;
    pub const NOTDIR: u32 = 20;
    pub const ISDIR: u32 = 21;
    pub const INVAL: u32 = 22;
    pub const FBIG: u32 = 27;
    pub const NOSPC: u32 = 28;
    pub const ROFS: u32 = 30;
    pub const MLINK: u32 = 31;
    pub const NAMETOOLONG: u32 = 63;
    pub const NOTEMPTY: u32 = 66;
    pub const DQUOT: u32 = 69;
    pub const STALE: u32 = 70;
    pub const BADHANDLE: u32 = 10001;
    pub const NOT_SYNC: u32 = 10002;
    pub const BAD_COOKIE: u32 = 10003;
    pub const NOTSUPP: u32 = 10004;
    pub const TOOSMALL: u32 = 10005;
    pub const SERVERFAULT: u32 = 10006;
    pub const JUKEBOX: u32 = 10008;
}

/// The `nfsstat3` of an error number. Matched by name: the numbers differ between platforms.
pub fn stat_of(e: Errno) -> u32 {
    match e {
        Errno::PERM => stat::PERM,
        Errno::NOENT => stat::NOENT,
        Errno::ACCESS => stat::ACCES,
        Errno::EXIST => stat::EXIST,
        Errno::XDEV => stat::XDEV,
        Errno::NOTDIR => stat::NOTDIR,
        Errno::ISDIR => stat::ISDIR,
        Errno::INVAL | Errno::LOOP => stat::INVAL,
        Errno::FBIG => stat::FBIG,
        Errno::NOSPC => stat::NOSPC,
        Errno::ROFS => stat::ROFS,
        Errno::MLINK => stat::MLINK,
        Errno::NAMETOOLONG => stat::NAMETOOLONG,
        Errno::NOTEMPTY => stat::NOTEMPTY,
        Errno::DQUOT => stat::DQUOT,
        Errno::STALE | Errno::BADF => stat::STALE,
        Errno::NOTSUP => stat::NOTSUPP,
        Errno::AGAIN => stat::JUKEBOX,
        _ => stat::IO,
    }
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Files the server keeps open, by inode: NFS has no open or close.
#[derive(Default)]
pub struct Files {
    read: HashMap<u64, Arc<OpenFile>>,
    write: HashMap<u64, Arc<OpenFile>>,
}

impl Files {
    fn evict(&mut self, ino: u64) {
        self.read.remove(&ino);
        self.write.remove(&ino);
    }

    /// Closes everything (unmount).
    pub fn clear(&mut self) {
        self.read.clear();
        self.write.clear();
    }

    #[cfg(test)]
    pub fn open_count(&self) -> usize {
        self.read.len() + self.write.len()
    }
}

/// The NFS front end's [`Notify`]: content that changed behind the server is closed, so the next
/// read opens what the file is now. The client's own cache follows the attributes.
pub struct Evict(pub Arc<Mutex<Files>>);

impl Notify for Evict {
    fn content(&self, ino: u64) {
        lock(&self.0).evict(ino);
    }
    fn entry(&self, _parent: u64, _name: &[u8]) {}
    fn inode(&self, _ino: u64) {}
}

/// Inode numbers a handle was handed out for, each holding one lookup, oldest first.
#[derive(Default)]
struct Holds {
    set: HashSet<u64>,
    order: VecDeque<u64>,
}

#[derive(Default)]
struct Listings {
    map: HashMap<u64, (u64, Arc<Snapshot>)>,
    order: VecDeque<u64>,
    next: u64,
}

/// Replies to calls that must not run twice, by transaction id and procedure.
#[derive(Default)]
struct Replies {
    done: HashMap<(u32, u32), Arc<Vec<u8>>>,
    order: VecDeque<(u32, u32)>,
    busy: HashSet<(u32, u32)>,
}

/// How the server is run.
#[derive(Clone, Debug)]
pub struct ServerConfig {
    /// The only user served (the work tree's owner); root may also mount.
    pub owner: u32,
    /// Only privileged source ports may connect (only the kernel's client binds them).
    pub require_resvport: bool,
    /// Ties handles to this work tree: a handle from another server is stale.
    pub tag: u64,
    pub workers: usize,
}

struct Shared {
    fs: Arc<MergedFs>,
    files: Arc<Mutex<Files>>,
    holds: Mutex<Holds>,
    listings: Mutex<Listings>,
    replies: Mutex<Replies>,
    cfg: ServerConfig,
    /// The write verifier: changes when the server restarts.
    verf: [u8; 8],
    /// Bumped by every switch; directories report it in their times so clients drop cached
    /// listings.
    generation: AtomicU64,
}

/// Why a procedure failed.
enum Fail {
    Stat(u32),
    Garbage,
}

impl From<Garbage> for Fail {
    fn from(_: Garbage) -> Self {
        Fail::Garbage
    }
}

impl From<Errno> for Fail {
    fn from(e: Errno) -> Self {
        Fail::Stat(stat_of(e))
    }
}

type Done = Result<(), Fail>;

/// What `SETATTR`, `CREATE`, `MKDIR` and `SYMLINK` ask to set.
#[derive(Default)]
struct Sattr {
    mode: Option<u32>,
    uid: Option<u32>,
    gid: Option<u32>,
    size: Option<u64>,
    atime: Option<SystemTime>,
    mtime: Option<SystemTime>,
}

impl Sattr {
    fn read(r: &mut Reader<'_>) -> XResult<Self> {
        Ok(Self {
            mode: r.option(Reader::u32)?,
            uid: r.option(Reader::u32)?,
            gid: r.option(Reader::u32)?,
            size: r.option(Reader::u64)?,
            atime: time_how(r)?,
            mtime: time_how(r)?,
        })
    }

    fn any_but_mode(&self) -> bool {
        self.uid.is_some()
            || self.gid.is_some()
            || self.size.is_some()
            || self.atime.is_some()
            || self.mtime.is_some()
    }
}

fn time_how(r: &mut Reader<'_>) -> XResult<Option<SystemTime>> {
    match r.u32()? {
        0 => Ok(None),
        1 => Ok(Some(SystemTime::now())),
        2 => {
            let s = r.u32()?;
            let ns = r.u32()?;
            Ok(Some(
                UNIX_EPOCH + Duration::new(u64::from(s), ns.min(999_999_999)),
            ))
        }
        _ => Err(Garbage),
    }
}

/// Seconds and nanoseconds as `nfstime3` holds them (times before 1970 read as 1970).
fn nfs_time(t: SystemTime) -> (u32, u32) {
    let d = t.duration_since(UNIX_EPOCH).unwrap_or_default();
    (
        u32::try_from(d.as_secs()).unwrap_or(u32::MAX),
        d.subsec_nanos(),
    )
}

/// Words after the status in each procedure's failure reply: its optional attributes, all
/// absent.
fn fail_words(proc_: u32) -> usize {
    match proc_ {
        1 => 0,
        3..=6 | 16..=20 => 1,
        15 => 3,
        14 => 4,
        _ => 2,
    }
}

/// Procedures that change the tree: a retransmission is answered from the reply cache.
fn non_idempotent(proc_: u32) -> bool {
    matches!(proc_, 2 | 8 | 9 | 10 | 12 | 13 | 14)
}

impl Shared {
    // ----- handles and inode holds -----

    fn handle(&self, ino: u64) -> [u8; FH_LEN] {
        let mut b = [0u8; FH_LEN];
        b[..8].copy_from_slice(&ino.to_be_bytes());
        b[8..].copy_from_slice(&self.cfg.tag.to_be_bytes());
        b
    }

    fn put_fh(&self, w: &mut Writer, ino: u64) {
        w.opaque(&self.handle(ino));
    }

    /// The inode a handle names, made live if the server had let it go.
    fn ino(&self, r: &mut Reader<'_>) -> Result<u64, Fail> {
        let b = r.opaque(64)?;
        if b.len() != FH_LEN {
            return Err(Fail::Stat(stat::BADHANDLE));
        }
        let mut word = [0u8; 8];
        word.copy_from_slice(&b[8..]);
        if u64::from_be_bytes(word) != self.cfg.tag {
            return Err(Fail::Stat(stat::STALE));
        }
        word.copy_from_slice(&b[..8]);
        let ino = u64::from_be_bytes(word);
        if ino == ROOT || self.fs.table.path_of(ino).is_some() {
            return Ok(ino);
        }
        match self.fs.table.revive(ino) {
            Ok(true) => {
                self.hold(ino);
                Ok(ino)
            }
            Ok(false) => Err(Fail::Stat(stat::STALE)),
            Err(_) => Err(Fail::Stat(stat::SERVERFAULT)),
        }
    }

    /// Keeps the one lookup of `ino` just counted, or drops it when one is already kept.
    fn hold(&self, ino: u64) {
        if ino == ROOT {
            self.fs.table.forget(ino, 1);
            return;
        }
        let (dup, evicted) = {
            let mut h = lock(&self.holds);
            if h.set.insert(ino) {
                h.order.push_back(ino);
                let mut evicted = Vec::new();
                while h.set.len() > MAX_HOLDS {
                    match h.order.pop_front() {
                        Some(old) if h.set.remove(&old) => evicted.push(old),
                        Some(_) => {}
                        None => break,
                    }
                }
                if h.order.len() > 2 * MAX_HOLDS {
                    let Holds { set, order } = &mut *h;
                    let mut seen = HashSet::with_capacity(set.len());
                    order.retain(|i| set.contains(i) && seen.insert(*i));
                }
                (false, evicted)
            } else {
                (true, Vec::new())
            }
        };
        if dup {
            self.fs.table.forget(ino, 1);
        }
        for old in evicted {
            self.fs.table.forget(old, 1);
        }
    }

    /// `ino` is gone: drop every lookup of it and anything open on it.
    fn drop_ino(&self, ino: u64) {
        lock(&self.holds).set.remove(&ino);
        self.fs.table.forget(ino, u64::MAX);
        lock(&self.files).evict(ino);
    }

    fn name<'a>(r: &mut Reader<'a>) -> XResult<&'a [u8]> {
        r.opaque(MAX_WIRE_NAME)
    }

    // ----- attributes -----

    fn getattr(&self, ino: u64) -> Option<Attr> {
        self.fs.getattr(ino, None).ok()
    }

    fn put_attr(&self, w: &mut Writer, a: &Attr) {
        let kind = match a.kind {
            VKind::File => 1,
            VKind::Dir => 2,
            VKind::Symlink => 5,
        };
        let (mut m, mut c) = (nfs_time(a.mtime), nfs_time(a.ctime));
        let g = self.generation.load(Ordering::Relaxed);
        if a.kind == VKind::Dir && g > 0 {
            let ns = u32::try_from(g % 1_000_000_000).unwrap_or(0);
            m.1 = ns;
            c.1 = ns;
        }
        let at = nfs_time(a.atime);
        w.u32(kind)
            .u32(u32::from(a.perm))
            .u32(a.nlink)
            .u32(a.uid)
            .u32(a.gid)
            .u64(a.size)
            .u64(a.blocks * 512)
            .u32(0)
            .u32(0)
            .u64(self.cfg.tag)
            .u64(a.ino)
            .u32(at.0)
            .u32(at.1)
            .u32(m.0)
            .u32(m.1)
            .u32(c.0)
            .u32(c.1);
    }

    fn post_attr(&self, w: &mut Writer, a: Option<&Attr>) {
        match a {
            Some(a) => {
                w.bool(true);
                self.put_attr(w, a);
            }
            None => {
                w.bool(false);
            }
        }
    }

    /// `wcc_data` with no attributes from before.
    fn wcc(&self, w: &mut Writer, a: Option<&Attr>) {
        w.bool(false);
        self.post_attr(w, a);
    }

    fn read_only(&self, ino: u64) -> bool {
        matches!(self.fs.loc(ino), Ok(Some(p)) if matches!(self.fs.view(&p), Ok(Some(f)) if f.read_only()))
    }

    // ----- open files -----

    fn file(&self, ino: u64, write: bool) -> Result<Arc<OpenFile>, Fail> {
        {
            let f = lock(&self.files);
            let have = if write {
                f.write.get(&ino)
            } else {
                f.write.get(&ino).or_else(|| f.read.get(&ino))
            };
            if let Some(h) = have {
                return Ok(h.clone());
            }
        }
        let (h, _) = self.fs.open_file(IMAGE, ino, true, write, false)?;
        let h = Arc::new(h);
        self.keep(ino, h.clone(), write);
        Ok(h)
    }

    fn keep(&self, ino: u64, h: Arc<OpenFile>, write: bool) {
        let mut f = lock(&self.files);
        let m = if write { &mut f.write } else { &mut f.read };
        if m.len() >= MAX_FILES
            && let Some(&k) = m.keys().next()
        {
            m.remove(&k);
        }
        m.insert(ino, h);
    }

    // ----- dispatch -----

    fn serve(&self, record: &[u8]) -> Option<Vec<u8>> {
        let call = match rpc::parse_call(record) {
            Ok(c) => c,
            Err(Bad::Drop) => return None,
            Err(Bad::Reply(r)) => return Some(r),
        };
        match (call.prog, call.vers) {
            (NFS_PROG, 3) => self.nfs(call),
            (MOUNT_PROG, 3) => Some(self.mount(call)),
            (NFS_PROG | MOUNT_PROG, _) => Some(rpc::accepted(
                call.xid,
                Accept::ProgMismatch(3, 3),
                &[],
            )),
            _ => Some(rpc::accepted(call.xid, Accept::ProgUnavail, &[])),
        }
    }

    fn nfs(&self, call: Call<'_>) -> Option<Vec<u8>> {
        let Call {
            xid,
            proc_,
            cred,
            mut args,
            ..
        } = call;
        if proc_ == 0 {
            return Some(rpc::accepted(xid, Accept::Success, &[]));
        }
        if proc_ > 21 {
            return Some(rpc::accepted(xid, Accept::ProcUnavail, &[]));
        }
        if cred.flavor != rpc::AUTH_SYS {
            return Some(rpc::denied(xid, AuthStat::TooWeak));
        }
        let key = (xid, proc_);
        let cached = non_idempotent(proc_);
        if cached {
            let mut r = lock(&self.replies);
            if let Some(done) = r.done.get(&key) {
                return Some(done.as_ref().clone());
            }
            if !r.busy.insert(key) {
                // Still running: the client will ask again.
                return None;
            }
        }
        let mut w = Writer::with_capacity(512);
        let res = if cred.uid == self.cfg.owner {
            self.op(proc_, &mut args, &mut w)
        } else {
            Err(Fail::Stat(stat::ACCES))
        };
        let reply = match res {
            Ok(()) => rpc::accepted(xid, Accept::Success, w.bytes()),
            Err(Fail::Garbage) => rpc::accepted(xid, Accept::GarbageArgs, &[]),
            Err(Fail::Stat(s)) => {
                let mut f = Writer::with_capacity(20);
                f.u32(s);
                for _ in 0..fail_words(proc_) {
                    f.bool(false);
                }
                rpc::accepted(xid, Accept::Success, f.bytes())
            }
        };
        if cached {
            let mut r = lock(&self.replies);
            r.busy.remove(&key);
            if r.done.len() >= MAX_REPLIES
                && let Some(old) = r.order.pop_front()
            {
                r.done.remove(&old);
            }
            if r.done.insert(key, Arc::new(reply.clone())).is_none() {
                r.order.push_back(key);
            }
        }
        Some(reply)
    }

    fn op(&self, proc_: u32, r: &mut Reader<'_>, w: &mut Writer) -> Done {
        match proc_ {
            1 => self.op_getattr(r, w),
            2 => self.op_setattr(r, w),
            3 => self.op_lookup(r, w),
            4 => self.op_access(r, w),
            5 => self.op_readlink(r, w),
            6 => self.op_read(r, w),
            7 => self.op_write(r, w),
            8 => self.op_create(r, w),
            9 => self.op_mkdir(r, w),
            10 => self.op_symlink(r, w),
            12 | 13 => self.op_remove(r, w, proc_ == 13),
            14 => self.op_rename(r, w),
            16 => self.op_readdir(r, w, false),
            17 => self.op_readdir(r, w, true),
            18 => self.op_fsstat(r, w),
            19 => self.op_fsinfo(r, w),
            20 => self.op_pathconf(r, w),
            21 => self.op_commit(r, w),
            // MKNOD and LINK: no devices, no hard links.
            _ => Err(Fail::Stat(stat::NOTSUPP)),
        }
    }

    // ----- procedures -----

    fn op_getattr(&self, r: &mut Reader<'_>, w: &mut Writer) -> Done {
        let ino = self.ino(r)?;
        let a = self.fs.getattr(ino, None)?;
        w.u32(stat::OK);
        self.put_attr(w, &a);
        Ok(())
    }

    fn op_setattr(&self, r: &mut Reader<'_>, w: &mut Writer) -> Done {
        let ino = self.ino(r)?;
        let s = Sattr::read(r)?;
        let guard = r.option(|r| Ok((r.u32()?, r.u32()?)))?;
        if let Some(ct) = guard {
            let a = self.fs.getattr(ino, None)?;
            let mut cur = Writer::new();
            self.put_attr(&mut cur, &a);
            let mut x = Reader::new(cur.bytes());
            let fields = x.fixed(84)?;
            let mut c = [0u8; 8];
            c.copy_from_slice(&fields[76..84]);
            let have = (
                u32::from_be_bytes([c[0], c[1], c[2], c[3]]),
                u32::from_be_bytes([c[4], c[5], c[6], c[7]]),
            );
            if have != ct {
                return Err(Fail::Stat(stat::NOT_SYNC));
            }
        }
        let a = self
            .fs
            .setattr(ino, s.mode, s.uid, s.gid, s.size, s.atime, s.mtime, None)?;
        w.u32(stat::OK);
        self.wcc(w, Some(&a));
        Ok(())
    }

    fn op_lookup(&self, r: &mut Reader<'_>, w: &mut Writer) -> Done {
        let dir = self.ino(r)?;
        let name = Self::name(r)?;
        let a = match name {
            b"." => self.fs.getattr(dir, None)?,
            b".." => {
                let parent = if dir == ROOT {
                    ROOT
                } else {
                    self.fs.table.parent_of(dir).ok_or(Errno::STALE)?
                };
                self.fs.getattr(parent, None)?
            }
            _ => {
                let a = self.fs.lookup(dir, name)?;
                self.hold(a.ino);
                a
            }
        };
        w.u32(stat::OK);
        self.put_fh(w, a.ino);
        self.post_attr(w, Some(&a));
        self.post_attr(w, self.getattr(dir).as_ref());
        Ok(())
    }

    fn op_access(&self, r: &mut Reader<'_>, w: &mut Writer) -> Done {
        const READ: u32 = 1;
        const LOOKUP: u32 = 2;
        const MODIFY: u32 = 4;
        const EXTEND: u32 = 8;
        const DELETE: u32 = 0x10;
        const EXECUTE: u32 = 0x20;
        let ino = self.ino(r)?;
        let want = r.u32()?;
        let a = self.fs.getattr(ino, None)?;
        let writable = a.perm & 0o200 != 0 && !self.read_only(ino);
        let mut ok = 0;
        if a.perm & 0o400 != 0 {
            ok |= READ;
        }
        if a.perm & 0o100 != 0 {
            ok |= if a.kind == VKind::Dir { LOOKUP } else { EXECUTE };
        }
        if writable {
            ok |= MODIFY | EXTEND | DELETE;
        }
        w.u32(stat::OK);
        self.post_attr(w, Some(&a));
        w.u32(want & ok);
        Ok(())
    }

    fn op_readlink(&self, r: &mut Reader<'_>, w: &mut Writer) -> Done {
        let ino = self.ino(r)?;
        let t = self.fs.readlink(ino)?;
        w.u32(stat::OK);
        self.post_attr(w, self.getattr(ino).as_ref());
        w.opaque(&t);
        Ok(())
    }

    fn op_read(&self, r: &mut Reader<'_>, w: &mut Writer) -> Done {
        let ino = self.ino(r)?;
        let offset = r.u64()?;
        let count = r.u32()?.min(RTMAX);
        let f = self.file(ino, false)?;
        let data = read_at(&f.file, offset, count)?;
        let size = f.file.metadata().map_err(|_| Errno::IO)?.len();
        let eof = offset.saturating_add(data.len() as u64) >= size;
        w.u32(stat::OK);
        self.post_attr(w, self.getattr(ino).as_ref());
        w.u32(u32::try_from(data.len()).unwrap_or(0))
            .bool(eof)
            .opaque(&data);
        Ok(())
    }

    fn op_write(&self, r: &mut Reader<'_>, w: &mut Writer) -> Done {
        let ino = self.ino(r)?;
        let offset = r.u64()?;
        let _count = r.u32()?;
        let stable = r.u32()?;
        let data = r.opaque(WTMAX as usize)?;
        if offset.checked_add(data.len() as u64).is_none_or(|e| e > i64::MAX as u64) {
            return Err(Fail::Stat(stat::FBIG));
        }
        let f = self.file(ino, true)?;
        f.file
            .write_all_at(data, offset)
            .map_err(crate::unixfs::merged::errno)?;
        match stable {
            0 => {}
            1 => f.file.sync_data().map_err(crate::unixfs::merged::errno)?,
            _ => f.file.sync_all().map_err(crate::unixfs::merged::errno)?,
        }
        w.u32(stat::OK);
        self.wcc(w, self.getattr(ino).as_ref());
        w.u32(u32::try_from(data.len()).unwrap_or(0))
            .u32(stable.min(2))
            .fixed(&self.verf);
        Ok(())
    }

    /// The reply of `CREATE`, `MKDIR` and `SYMLINK`.
    fn made(&self, w: &mut Writer, dir: u64, a: &Attr) {
        w.u32(stat::OK).bool(true);
        self.put_fh(w, a.ino);
        self.post_attr(w, Some(a));
        self.wcc(w, self.getattr(dir).as_ref());
    }

    fn op_create(&self, r: &mut Reader<'_>, w: &mut Writer) -> Done {
        let dir = self.ino(r)?;
        let name = Self::name(r)?;
        let how = r.u32()?;
        let (s, verf) = match how {
            0 | 1 => (Sattr::read(r)?, None),
            2 => {
                let v = r.fixed(8)?;
                let t = |b: &[u8]| {
                    UNIX_EPOCH + Duration::from_secs(u64::from(u32::from_be_bytes([b[0], b[1], b[2], b[3]])))
                };
                (Sattr::default(), Some((t(&v[..4]), t(&v[4..]))))
            }
            _ => return Err(Fail::Garbage),
        };
        let existing = match self.fs.lookup(dir, name) {
            Ok(a) => Some(a),
            Err(Errno::NOENT) => None,
            Err(e) => return Err(e.into()),
        };
        if let Some(a) = existing {
            self.hold(a.ino);
            let a = match (how, verf) {
                // A retransmitted exclusive create: the verifier is in the times.
                (2, Some((at, mt)))
                    if nfs_time(a.atime).0 == nfs_time(at).0
                        && nfs_time(a.mtime).0 == nfs_time(mt).0 =>
                {
                    a
                }
                (0, _) if a.kind == VKind::File => {
                    if s.any_but_mode() || s.mode.is_some() {
                        self.fs.setattr(
                            a.ino, s.mode, s.uid, s.gid, s.size, s.atime, s.mtime, None,
                        )?
                    } else {
                        a
                    }
                }
                _ => return Err(Fail::Stat(stat::EXIST)),
            };
            self.made(w, dir, &a);
            return Ok(());
        }
        let (mut a, fh) = self.fs.create(dir, name, s.mode.unwrap_or(0o644))?;
        if let Some(h) = self.fs.release(fh) {
            self.keep(a.ino, h, true);
        }
        self.hold(a.ino);
        if let Some((at, mt)) = verf {
            a = self
                .fs
                .setattr(a.ino, None, None, None, None, Some(at), Some(mt), None)?;
        } else if s.any_but_mode() {
            a = self
                .fs
                .setattr(a.ino, None, s.uid, s.gid, s.size, s.atime, s.mtime, None)?;
        }
        self.made(w, dir, &a);
        Ok(())
    }

    fn op_mkdir(&self, r: &mut Reader<'_>, w: &mut Writer) -> Done {
        let dir = self.ino(r)?;
        let name = Self::name(r)?;
        let s = Sattr::read(r)?;
        let a = self.fs.mkdir(dir, name, s.mode.unwrap_or(0o755))?;
        self.hold(a.ino);
        self.made(w, dir, &a);
        Ok(())
    }

    fn op_symlink(&self, r: &mut Reader<'_>, w: &mut Writer) -> Done {
        let dir = self.ino(r)?;
        let name = Self::name(r)?;
        let _s = Sattr::read(r)?;
        let target = r.opaque(MAX_LINK + 1)?;
        let a = self.fs.symlink(dir, name, target)?;
        self.hold(a.ino);
        self.made(w, dir, &a);
        Ok(())
    }

    fn op_remove(&self, r: &mut Reader<'_>, w: &mut Writer, rmdir: bool) -> Done {
        let dir = self.ino(r)?;
        let name = Self::name(r)?;
        if name.len() > MAX_NAME {
            return Err(Fail::Stat(stat::NAMETOOLONG));
        }
        // Looked up first, so the table knows the name and drops its persisted number.
        let pre = self.fs.lookup(dir, name).ok().map(|a| a.ino);
        let res = self.fs.remove(dir, name, rmdir);
        if let Some(i) = pre {
            if res.is_ok() {
                self.drop_ino(i);
            } else {
                self.hold(i);
            }
        }
        res?;
        w.u32(stat::OK);
        self.wcc(w, self.getattr(dir).as_ref());
        Ok(())
    }

    fn op_rename(&self, r: &mut Reader<'_>, w: &mut Writer) -> Done {
        let from = self.ino(r)?;
        let fname = Self::name(r)?;
        let to = self.ino(r)?;
        let tname = Self::name(r)?;
        let src = self.fs.lookup(from, fname).ok().map(|a| a.ino);
        let dst = self.fs.lookup(to, tname).ok().map(|a| a.ino);
        let res = self.fs.rename(from, fname, to, tname, false);
        match (&res, dst) {
            (Ok(_), Some(d)) if Some(d) != src => self.drop_ino(d),
            (_, Some(d)) => self.hold(d),
            _ => {}
        }
        if let Some(s) = src {
            self.hold(s);
        }
        res?;
        w.u32(stat::OK);
        self.wcc(w, self.getattr(from).as_ref());
        self.wcc(w, self.getattr(to).as_ref());
        Ok(())
    }

    fn listing(&self, dir: u64, cookie: u64, verf: u64) -> Result<(u64, Arc<Snapshot>), Fail> {
        if cookie == 0 {
            let snap = Arc::new(self.fs.list(dir)?);
            let mut l = lock(&self.listings);
            l.next += 1;
            let id = l.next;
            if l.map.len() >= MAX_LISTINGS
                && let Some(old) = l.order.pop_front()
            {
                l.map.remove(&old);
            }
            l.map.insert(id, (dir, snap.clone()));
            l.order.push_back(id);
            return Ok((id, snap));
        }
        match lock(&self.listings).map.get(&verf) {
            Some((d, s)) if *d == dir => Ok((verf, s.clone())),
            _ => Err(Fail::Stat(stat::BAD_COOKIE)),
        }
    }

    fn op_readdir(&self, r: &mut Reader<'_>, w: &mut Writer, plus: bool) -> Done {
        let dir = self.ino(r)?;
        let cookie = r.u64()?;
        let verf = r.u64()?;
        let (dircount, maxcount) = if plus {
            (r.u32()? as usize, r.u32()?)
        } else {
            let c = r.u32()?;
            (usize::MAX, c)
        };
        let maxcount = maxcount.min(RTMAX) as usize;
        let (id, snap) = self.listing(dir, cookie, verf)?;
        let start = usize::try_from(cookie).map_err(|_| Fail::Stat(stat::BAD_COOKIE))?;
        if start > snap.entries.len() {
            return Err(Fail::Stat(stat::BAD_COOKIE));
        }
        let da = self.fs.getattr(dir, None)?;
        w.u32(stat::OK);
        self.post_attr(w, Some(&da));
        w.u64(id);
        let mut used = w.len() + 8;
        let mut dused = 0usize;
        let mut sent = 0usize;
        let mut i = start;
        while i < snap.entries.len() {
            let (name, found) = &snap.entries[i];
            let (ino, attr) = match name.as_slice() {
                b"." => (dir, Some(da)),
                b".." => (snap.parent, self.getattr(snap.parent)),
                _ if plus => match self.fs.table.lookup(dir, name) {
                    Ok(Some(ino)) => {
                        self.hold(ino);
                        (ino, found.as_ref().map(|f| self.fs.attr(ino, f)))
                    }
                    _ => {
                        i += 1;
                        continue;
                    }
                },
                _ => match self.fs.ino_for_listing(dir, name) {
                    u64::MAX => {
                        i += 1;
                        continue;
                    }
                    ino => (ino, None),
                },
            };
            let mut e = Writer::with_capacity(160);
            e.bool(true).u64(ino).opaque(name).u64(i as u64 + 1);
            if plus {
                self.post_attr(&mut e, attr.as_ref());
                e.bool(true);
                self.put_fh(&mut e, ino);
            }
            let dsize = 8 + 4 + name.len().div_ceil(4) * 4 + 8;
            if used + e.len() > maxcount || dused + dsize > dircount {
                if sent == 0 {
                    return Err(Fail::Stat(stat::TOOSMALL));
                }
                break;
            }
            w.raw(e.bytes());
            used += e.len();
            dused += dsize;
            sent += 1;
            i += 1;
        }
        w.bool(false).bool(i >= snap.entries.len());
        Ok(())
    }

    fn op_fsstat(&self, r: &mut Reader<'_>, w: &mut Writer) -> Done {
        let ino = self.ino(r)?;
        let s = self.fs.statfs()?;
        let f = s.f_frsize;
        w.u32(stat::OK);
        self.post_attr(w, self.getattr(ino).as_ref());
        w.u64(s.f_blocks.saturating_mul(f))
            .u64(s.f_bfree.saturating_mul(f))
            .u64(s.f_bavail.saturating_mul(f))
            .u64(s.f_files)
            .u64(s.f_ffree)
            .u64(s.f_favail)
            .u32(0);
        Ok(())
    }

    fn op_fsinfo(&self, r: &mut Reader<'_>, w: &mut Writer) -> Done {
        const SYMLINK: u32 = 2;
        const HOMOGENEOUS: u32 = 8;
        const CANSETTIME: u32 = 0x10;
        let ino = self.ino(r)?;
        w.u32(stat::OK);
        self.post_attr(w, self.getattr(ino).as_ref());
        w.u32(RTMAX)
            .u32(RTMAX)
            .u32(4096)
            .u32(WTMAX)
            .u32(WTMAX)
            .u32(4096)
            .u32(64 * 1024)
            .u64(i64::MAX as u64)
            .u32(0)
            .u32(1)
            .u32(SYMLINK | HOMOGENEOUS | CANSETTIME);
        Ok(())
    }

    fn op_pathconf(&self, r: &mut Reader<'_>, w: &mut Writer) -> Done {
        let ino = self.ino(r)?;
        w.u32(stat::OK);
        self.post_attr(w, self.getattr(ino).as_ref());
        w.u32(1)
            .u32(MAX_NAME as u32)
            .bool(true)
            .bool(true)
            .bool(false)
            .bool(true);
        Ok(())
    }

    fn op_commit(&self, r: &mut Reader<'_>, w: &mut Writer) -> Done {
        let ino = self.ino(r)?;
        let _offset = r.u64()?;
        let _count = r.u32()?;
        let open = lock(&self.files).write.get(&ino).cloned();
        if let Some(h) = open {
            h.file.sync_all().map_err(crate::unixfs::merged::errno)?;
        }
        w.u32(stat::OK);
        self.wcc(w, self.getattr(ino).as_ref());
        w.fixed(&self.verf);
        Ok(())
    }

    // ----- MOUNT v3 (RFC 1813 appendix I) -----

    fn mount(&self, call: Call<'_>) -> Vec<u8> {
        const MNT3ERR_NOENT: u32 = 2;
        const MNT3ERR_ACCES: u32 = 13;
        let Call {
            xid,
            proc_,
            cred,
            mut args,
            ..
        } = call;
        let mut w = Writer::with_capacity(64);
        match proc_ {
            0 | 3 | 4 => {}
            1 => {
                if cred.flavor != rpc::AUTH_SYS {
                    return rpc::denied(xid, AuthStat::TooWeak);
                }
                let Ok(path) = args.opaque(1024) else {
                    return rpc::accepted(xid, Accept::GarbageArgs, &[]);
                };
                if cred.uid != self.cfg.owner && cred.uid != 0 {
                    w.u32(MNT3ERR_ACCES);
                } else if path != EXPORT.as_bytes() && path != b"/" {
                    w.u32(MNT3ERR_NOENT);
                } else {
                    w.u32(0);
                    self.put_fh(&mut w, ROOT);
                    w.u32(1).u32(rpc::AUTH_SYS);
                }
            }
            2 => {
                w.bool(false);
            }
            5 => {
                w.bool(true).opaque(EXPORT.as_bytes()).bool(false).bool(false);
            }
            _ => return rpc::accepted(xid, Accept::ProcUnavail, &[]),
        }
        rpc::accepted(xid, Accept::Success, w.bytes())
    }
}

type Job = (Vec<u8>, Arc<Mutex<TcpStream>>);

/// A running server. Stopping it closes every connection and joins every thread.
pub struct Server {
    shared: Arc<Shared>,
    addr: SocketAddr,
    stop: Arc<AtomicBool>,
    conns: Arc<Mutex<HashMap<u64, TcpStream>>>,
    readers: Arc<Mutex<Vec<JoinHandle<()>>>>,
    threads: Vec<JoinHandle<()>>,
    tx: Option<SyncSender<Job>>,
}

impl Server {
    /// Listens on `127.0.0.1` at an ephemeral port.
    pub fn start(fs: Arc<MergedFs>, files: Arc<Mutex<Files>>, cfg: ServerConfig) -> io::Result<Self> {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
        let addr = listener.local_addr()?;
        let boot = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let workers = cfg.workers.clamp(1, 64);
        let shared = Arc::new(Shared {
            fs,
            files,
            holds: Mutex::new(Holds::default()),
            listings: Mutex::new(Listings::default()),
            replies: Mutex::new(Replies::default()),
            cfg,
            verf: (boot as u64).to_be_bytes(),
            generation: AtomicU64::new(0),
        });
        let stop = Arc::new(AtomicBool::new(false));
        let conns: Arc<Mutex<HashMap<u64, TcpStream>>> = Arc::new(Mutex::new(HashMap::new()));
        let readers: Arc<Mutex<Vec<JoinHandle<()>>>> = Arc::new(Mutex::new(Vec::new()));
        let (tx, rx) = mpsc::sync_channel::<Job>(QUEUE);
        let rx = Arc::new(Mutex::new(rx));
        let mut threads = Vec::with_capacity(workers + 1);
        for n in 0..workers {
            let rx = rx.clone();
            let sh = shared.clone();
            threads.push(
                std::thread::Builder::new()
                    .name(format!("asc-nfs-{n}"))
                    .spawn(move || worker(&sh, &rx))?,
            );
        }
        {
            let (sh, stop, conns, readers, tx) =
                (shared.clone(), stop.clone(), conns.clone(), readers.clone(), tx.clone());
            threads.push(
                std::thread::Builder::new()
                    .name("asc-nfs-accept".to_owned())
                    .spawn(move || accept(&listener, &sh, &stop, &conns, &readers, &tx))?,
            );
        }
        Ok(Self {
            shared,
            addr,
            stop,
            conns,
            readers,
            threads,
            tx: Some(tx),
        })
    }

    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// Directories change their times, so clients drop listings cached before a switch.
    pub fn bump_generation(&self) {
        self.shared.generation.fetch_add(1, Ordering::Relaxed);
        // Listings of the old version must not page on: their cookies now say BAD_COOKIE and
        // the client lists again. Ids keep counting up, so an old verifier never matches.
        let mut l = lock(&self.shared.listings);
        l.map.clear();
        l.order.clear();
    }

    /// Lookups the server holds for handed-out handles.
    pub fn held(&self) -> usize {
        lock(&self.shared.holds).set.len()
    }

    fn shutdown(&mut self) {
        if self.stop.swap(true, Ordering::SeqCst) {
            return;
        }
        // Wakes the listener.
        let _ = TcpStream::connect_timeout(&self.addr, Duration::from_secs(1));
        for c in lock(&self.conns).values() {
            let _ = c.shutdown(Shutdown::Both);
        }
        if let Some(t) = self.threads.pop() {
            let _ = t.join();
        }
        let readers: Vec<_> = lock(&self.readers).drain(..).collect();
        for t in readers {
            let _ = t.join();
        }
        self.tx = None;
        for t in self.threads.drain(..) {
            let _ = t.join();
        }
        let holds: Vec<u64> = lock(&self.shared.holds).set.drain().collect();
        for ino in holds {
            self.shared.fs.table.forget(ino, 1);
        }
        lock(&self.shared.files).clear();
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn worker(sh: &Shared, rx: &Mutex<Receiver<Job>>) {
    loop {
        let job = lock(rx).recv();
        let Ok((record, out)) = job else { return };
        if let Some(reply) = sh.serve(&record) {
            let mut s = lock(&out);
            let _ = rpc::write_record(&mut *s, &reply);
        }
    }
}

fn accept(
    listener: &TcpListener,
    sh: &Arc<Shared>,
    stop: &AtomicBool,
    conns: &Arc<Mutex<HashMap<u64, TcpStream>>>,
    readers: &Mutex<Vec<JoinHandle<()>>>,
    tx: &SyncSender<Job>,
) {
    let mut next = 0u64;
    for stream in listener.incoming() {
        if stop.load(Ordering::SeqCst) {
            return;
        }
        let Ok(stream) = stream else { continue };
        let Ok(peer) = stream.peer_addr() else { continue };
        if !peer.ip().is_loopback()
            || (sh.cfg.require_resvport && peer.port() >= 1024)
            || lock(conns).len() >= MAX_CONNS
        {
            let _ = stream.shutdown(Shutdown::Both);
            continue;
        }
        let _ = stream.set_nodelay(true);
        let (Ok(read), Ok(keep)) = (stream.try_clone(), stream.try_clone()) else {
            continue;
        };
        next += 1;
        let id = next;
        lock(conns).insert(id, keep);
        let out = Arc::new(Mutex::new(stream));
        let (tx, conns2) = (tx.clone(), conns.clone());
        let t = std::thread::Builder::new()
            .name(format!("asc-nfs-conn-{id}"))
            .spawn(move || {
                let mut r = BufReader::with_capacity(256 * 1024, read);
                while let Ok(Some(record)) = rpc::read_record(&mut r) {
                    if tx.send((record, out.clone())).is_err() {
                        break;
                    }
                }
                if let Some(c) = lock(&conns2).remove(&id) {
                    let _ = c.shutdown(Shutdown::Both);
                }
            });
        match t {
            Ok(t) => {
                let mut rs = lock(readers);
                rs.retain(|h| !h.is_finished());
                rs.push(t);
            }
            Err(_) => {
                if let Some(c) = lock(conns).remove(&id) {
                    let _ = c.shutdown(Shutdown::Both);
                }
            }
        }
    }
}
