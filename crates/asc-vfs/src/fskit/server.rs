//! The FSKit front end's server: [`MergedFs`] over the [`super::wire`] protocol on a
//! Unix-domain socket that only the owner can reach.
//!
//! Plain threads, as in the NFS server: an accept thread, a reader per connection and a worker
//! pool, because any operation may block on hydration. Unlike NFS the client holds its items the
//! way the kernel holds FUSE inodes: every lookup it is handed is counted for its connection and
//! released by `FORGET`, and whatever a connection still holds is released when it closes.

use std::collections::HashMap;
use std::fs;
use std::io::{self, BufReader};
use std::os::unix::fs::{DirBuilderExt, FileExt, MetadataExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;

use super::wire::{self, MAX_IO, MAX_WIRE_NAME, err, op, set};
use crate::nfs::xdr::{Garbage, Reader, Writer};
use crate::unixfs::inodes::{GITFILE, ROOT};
use crate::unixfs::merged::{MAX_LINK, OpenFile, Snapshot, read_at};
use crate::unixfs::{Attr, Errno, MergedFs, Notify};

/// What a process sees as its name: requests carry no process, so the hydration policy sees
/// this for every read through the FSKit mount.
pub const IMAGE: &str = "fskit";

const MAX_FILES: usize = 1024;
const MAX_LISTINGS: usize = 256;
const MAX_CONNS: usize = 8;
const QUEUE: usize = 256;

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Files the server keeps open, by inode: the client reads and writes by item, not by handle.
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

    /// Files open now.
    pub fn open_count(&self) -> usize {
        self.read.len() + self.write.len()
    }
}

/// The FSKit front end's [`Notify`]: content that changed behind the server is closed, so the
/// next read opens what the file is now.
pub struct Evict(pub Arc<Mutex<Files>>);

impl Notify for Evict {
    fn content(&self, ino: u64) {
        lock(&self.0).evict(ino);
    }
    fn entry(&self, _parent: u64, _name: &[u8]) {}
    fn inode(&self, _ino: u64) {}
}

/// Why a request failed.
enum Fail {
    /// Answered with this wire error.
    Err(u32),
    /// The arguments did not decode: answered with `INVAL`.
    Garbage,
}

impl From<Garbage> for Fail {
    fn from(_: Garbage) -> Self {
        Fail::Garbage
    }
}

impl From<Errno> for Fail {
    fn from(e: Errno) -> Self {
        Fail::Err(wire::err_of(e))
    }
}

type Done = Result<(), Fail>;

/// The lookups one connection holds, released when it closes.
#[derive(Default)]
struct Conn {
    held: Mutex<Held>,
}

#[derive(Default)]
struct Held {
    map: HashMap<u64, u64>,
    /// The connection closed: a lookup a queued request counts now is released at once.
    closed: bool,
}

#[derive(Default)]
struct Listings {
    next: u64,
    map: HashMap<u64, (u64, Arc<Snapshot>)>,
    order: std::collections::VecDeque<u64>,
}

struct Shared {
    fs: Arc<MergedFs>,
    files: Arc<Mutex<Files>>,
    listings: Mutex<Listings>,
}

type Job = (Vec<u8>, Arc<Conn>, Arc<Mutex<UnixStream>>);

impl Shared {
    // ----- lookups -----

    /// Counts one lookup of `ino` for `conn`; the file system counted it already.
    fn held(&self, conn: &Conn, ino: u64) {
        let mut h = lock(&conn.held);
        if h.closed {
            drop(h);
            self.fs.table.forget(ino, 1);
            return;
        }
        *h.map.entry(ino).or_insert(0) += 1;
    }

    /// Releases up to `n` lookups `conn` holds of `ino`.
    fn forget(&self, conn: &Conn, ino: u64, n: u64) {
        let take = {
            let mut h = lock(&conn.held);
            let Some(have) = h.map.get_mut(&ino) else {
                return;
            };
            let take = n.min(*have);
            *have -= take;
            if *have == 0 {
                h.map.remove(&ino);
            }
            take
        };
        if take > 0 {
            self.fs.table.forget(ino, take);
        }
    }

    /// Releases everything `conn` holds (the connection closed).
    fn release(&self, conn: &Conn) {
        let all: Vec<(u64, u64)> = {
            let mut h = lock(&conn.held);
            h.closed = true;
            h.map.drain().collect()
        };
        for (ino, n) in all {
            self.fs.table.forget(ino, n);
        }
    }

    /// The inode an argument names, made live again when the table had let it go (a daemon
    /// restart under a mounted volume).
    fn ino(&self, conn: &Conn, r: &mut Reader<'_>) -> Result<u64, Fail> {
        let ino = r.u64()?;
        if ino == ROOT
            || (ino == GITFILE && self.fs.gitfile_attr().is_ok())
            || self.fs.table.path_of(ino).is_some()
        {
            return Ok(ino);
        }
        match self.fs.table.revive(ino) {
            Ok(true) => {
                self.held(conn, ino);
                Ok(ino)
            }
            Ok(false) => Err(Fail::Err(err::STALE)),
            Err(_) => Err(Fail::Err(err::IO)),
        }
    }

    fn name<'a>(r: &mut Reader<'a>) -> Result<&'a [u8], Fail> {
        Ok(r.opaque(MAX_WIRE_NAME)?)
    }

    fn parent(&self, ino: u64) -> u64 {
        if ino == ROOT {
            ROOT
        } else {
            self.fs.table.parent_of(ino).unwrap_or(ROOT)
        }
    }

    fn put_attr(&self, w: &mut Writer, a: &Attr) {
        wire::put_attr(w, a, self.parent(a.ino));
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

    fn serve(&self, conn: &Conn, frame: &[u8]) -> Vec<u8> {
        let mut r = Reader::new(frame);
        let (Ok(xid), Ok(code)) = (r.u32(), r.u32()) else {
            // No xid to answer to: an empty reply the client drops.
            return Vec::new();
        };
        let mut body = Writer::with_capacity(256);
        let res = self.op(conn, code, &mut r, &mut body);
        let mut w = Writer::with_capacity(8 + body.len());
        w.u32(xid);
        match res {
            Ok(()) => {
                w.u32(0).raw(body.bytes());
            }
            Err(Fail::Err(e)) => {
                w.u32(e);
            }
            Err(Fail::Garbage) => {
                w.u32(err::INVAL);
            }
        }
        w.into_bytes()
    }

    fn op(&self, conn: &Conn, code: u32, r: &mut Reader<'_>, w: &mut Writer) -> Done {
        match code {
            op::HELLO => self.op_hello(r, w),
            op::GETATTR => {
                let ino = self.ino(conn, r)?;
                let a = self.fs.getattr(ino, None)?;
                self.put_attr(w, &a);
                Ok(())
            }
            op::LOOKUP => self.op_lookup(conn, r, w),
            op::FORGET => {
                let ino = r.u64()?;
                let n = r.u64()?;
                self.forget(conn, ino, n);
                Ok(())
            }
            op::SETATTR => self.op_setattr(conn, r, w),
            op::READLINK => {
                let ino = self.ino(conn, r)?;
                w.opaque(&self.fs.readlink(ino)?);
                Ok(())
            }
            op::READ => self.op_read(conn, r, w),
            op::WRITE => self.op_write(conn, r, w),
            op::CREATE => self.op_create(conn, r, w),
            op::MKDIR => {
                let dir = self.ino(conn, r)?;
                let name = Self::name(r)?;
                let mode = r.u32()?;
                let a = self.fs.mkdir(dir, name, mode)?;
                self.held(conn, a.ino);
                self.put_attr(w, &a);
                Ok(())
            }
            op::SYMLINK => {
                let dir = self.ino(conn, r)?;
                let name = Self::name(r)?;
                let target = r.opaque(MAX_LINK + 1)?;
                let a = self.fs.symlink(dir, name, target)?;
                self.held(conn, a.ino);
                self.put_attr(w, &a);
                Ok(())
            }
            op::REMOVE => {
                let dir = self.ino(conn, r)?;
                let name = Self::name(r)?;
                let is_dir = r.bool()?;
                self.fs.remove(dir, name, is_dir)?;
                Ok(())
            }
            op::RENAME => {
                let from = self.ino(conn, r)?;
                let fname = Self::name(r)?;
                let to = self.ino(conn, r)?;
                let tname = Self::name(r)?;
                let flags = r.u32()?;
                if flags & !wire::RENAME_NOREPLACE != 0 {
                    return Err(Fail::Err(err::NOTSUP));
                }
                self.fs
                    .rename(from, fname, to, tname, flags & wire::RENAME_NOREPLACE != 0)?;
                Ok(())
            }
            op::READDIR => self.op_readdir(conn, r, w),
            op::STATFS => {
                let s = self.fs.statfs()?;
                let f = s.f_frsize;
                w.u64(s.f_blocks.saturating_mul(f))
                    .u64(s.f_bfree.saturating_mul(f))
                    .u64(s.f_bavail.saturating_mul(f))
                    .u64(s.f_files)
                    .u64(s.f_ffree)
                    .u32(u32::try_from(f).unwrap_or(4096))
                    .u32(crate::unixfs::merged::MAX_NAME as u32);
                Ok(())
            }
            op::SYNC => {
                let ino = self.ino(conn, r)?;
                let open = lock(&self.files).write.get(&ino).cloned();
                if let Some(h) = open {
                    h.file.sync_all().map_err(crate::unixfs::merged::errno)?;
                }
                Ok(())
            }
            op::CLOSE => {
                let ino = r.u64()?;
                lock(&self.files).evict(ino);
                Ok(())
            }
            _ => Err(Fail::Err(err::NOSYS)),
        }
    }

    fn op_hello(&self, r: &mut Reader<'_>, w: &mut Writer) -> Done {
        let v = r.u32()?;
        if v != wire::VERSION {
            return Err(Fail::Err(err::RPCMISMATCH));
        }
        let a = self.fs.getattr(ROOT, None)?;
        w.u32(wire::VERSION).u64(ROOT);
        self.put_attr(w, &a);
        w.u32(MAX_IO);
        Ok(())
    }

    fn op_lookup(&self, conn: &Conn, r: &mut Reader<'_>, w: &mut Writer) -> Done {
        let dir = self.ino(conn, r)?;
        let name = Self::name(r)?;
        if name == b"." || name == b".." {
            // The client resolves these itself; asking for them is a client bug.
            return Err(Fail::Err(err::INVAL));
        }
        let a = self.fs.lookup(dir, name)?;
        if a.ino == GITFILE {
            // The gitfile is not counted by the table.
            self.fs.table.forget(GITFILE, 1);
        } else {
            self.held(conn, a.ino);
        }
        self.put_attr(w, &a);
        Ok(())
    }

    fn op_setattr(&self, conn: &Conn, r: &mut Reader<'_>, w: &mut Writer) -> Done {
        let ino = self.ino(conn, r)?;
        let mask = r.u32()?;
        let mode = r.u32()?;
        let uid = r.u32()?;
        let gid = r.u32()?;
        let size = r.u64()?;
        let atime = wire::get_time(r)?;
        let mtime = wire::get_time(r)?;
        let pick = |bit: u32| mask & bit != 0;
        if size > i64::MAX as u64 && pick(set::SIZE) {
            return Err(Fail::Err(err::FBIG));
        }
        let a = self.fs.setattr(
            ino,
            pick(set::MODE).then_some(mode),
            pick(set::UID).then_some(uid),
            pick(set::GID).then_some(gid),
            pick(set::SIZE).then_some(size),
            pick(set::ATIME).then_some(atime),
            pick(set::MTIME).then_some(mtime),
            None,
        )?;
        self.put_attr(w, &a);
        Ok(())
    }

    fn op_read(&self, conn: &Conn, r: &mut Reader<'_>, w: &mut Writer) -> Done {
        let ino = self.ino(conn, r)?;
        let offset = r.u64()?;
        let count = r.u32()?.min(MAX_IO);
        let f = self.file(ino, false)?;
        let data = read_at(&f.file, offset, count)?;
        let size = f.file.metadata().map_err(|_| Errno::IO)?.len();
        let eof = offset.saturating_add(data.len() as u64) >= size;
        w.bool(eof).opaque(&data);
        Ok(())
    }

    fn op_write(&self, conn: &Conn, r: &mut Reader<'_>, w: &mut Writer) -> Done {
        let ino = self.ino(conn, r)?;
        let offset = r.u64()?;
        let data = r.opaque(MAX_IO as usize)?;
        if offset
            .checked_add(data.len() as u64)
            .is_none_or(|e| e > i64::MAX as u64)
        {
            return Err(Fail::Err(err::FBIG));
        }
        let f = self.file(ino, true)?;
        f.file
            .write_all_at(data, offset)
            .map_err(crate::unixfs::merged::errno)?;
        let a = self.fs.getattr(ino, None)?;
        w.u32(u32::try_from(data.len()).unwrap_or(0));
        self.put_attr(w, &a);
        Ok(())
    }

    fn op_create(&self, conn: &Conn, r: &mut Reader<'_>, w: &mut Writer) -> Done {
        let dir = self.ino(conn, r)?;
        let name = Self::name(r)?;
        let mode = r.u32()?;
        let (a, fh) = self.fs.create(dir, name, mode)?;
        if let Some(h) = self.fs.release(fh) {
            self.keep(a.ino, h, true);
        }
        self.held(conn, a.ino);
        self.put_attr(w, &a);
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
            // The listing was dropped or belongs elsewhere: the client starts over.
            _ => Err(Fail::Err(err::STALE)),
        }
    }

    fn op_readdir(&self, conn: &Conn, r: &mut Reader<'_>, w: &mut Writer) -> Done {
        let dir = self.ino(conn, r)?;
        let cookie = r.u64()?;
        let verf = r.u64()?;
        let max = r.u32()?.clamp(1, 4096) as usize;
        let attrs = r.bool()?;
        let (id, snap) = self.listing(dir, cookie, verf)?;
        let start = usize::try_from(cookie).map_err(|_| Fail::Err(err::INVAL))?;
        if start > snap.entries.len() {
            return Err(Fail::Err(err::INVAL));
        }
        w.u64(id);
        let mut sent = 0usize;
        let mut i = start;
        while i < snap.entries.len() && sent < max {
            let (name, found) = &snap.entries[i];
            i += 1;
            if name == b"." || name == b".." {
                // FSKit lists these itself.
                continue;
            }
            let attr = match found {
                None => match self.fs.gitfile_entry(dir, name) {
                    Some(a) => a,
                    None => continue,
                },
                Some(f) => match self.fs.ino_for_listing(dir, name) {
                    u64::MAX => continue,
                    ino => self.fs.attr(ino, f),
                },
            };
            w.bool(true)
                .u64(attr.ino)
                .opaque(name)
                .u64(i as u64)
                .u32(wire::kind_of(attr.kind));
            if attrs {
                w.bool(true);
                wire::put_attr(w, &attr, dir);
            } else {
                w.bool(false);
            }
            sent += 1;
        }
        w.bool(false).bool(i >= snap.entries.len());
        Ok(())
    }
}

/// A running server; dropping it stops it and releases every lookup its clients held.
/// The open connections by number: the stream (to shut it down) and its lookup counts.
type Conns = Arc<Mutex<HashMap<u64, (UnixStream, Arc<Conn>)>>>;

pub struct Server {
    shared: Arc<Shared>,
    path: PathBuf,
    stop: Arc<AtomicBool>,
    conns: Conns,
    readers: Arc<Mutex<Vec<JoinHandle<()>>>>,
    threads: Vec<JoinHandle<()>>,
    tx: Option<SyncSender<Job>>,
}

/// Makes `dir` an owner-only directory, refusing one that is a link or belongs to someone else.
pub fn private_dir(dir: &Path) -> io::Result<()> {
    match fs::symlink_metadata(dir) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(dir)?;
        }
        Err(e) => return Err(e),
        Ok(_) => {}
    }
    let m = fs::symlink_metadata(dir)?;
    if !m.is_dir() || m.file_type().is_symlink() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{} must be a real directory", dir.display()),
        ));
    }
    if m.uid() != rustix::process::geteuid().as_raw() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("{} belongs to another user", dir.display()),
        ));
    }
    if m.mode() & 0o077 != 0 {
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

impl Server {
    /// Listens at `path`, in an owner-only directory, replacing a stale socket there.
    pub fn start(
        fs: Arc<MergedFs>,
        files: Arc<Mutex<Files>>,
        path: &Path,
        workers: usize,
    ) -> io::Result<Self> {
        let dir = path
            .parent()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "no socket directory"))?;
        private_dir(dir)?;
        match fs::symlink_metadata(path) {
            Ok(m) if m.file_type().is_symlink() || m.is_dir() => {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    format!("{} is not a socket", path.display()),
                ));
            }
            Ok(_) => {
                // A live server answers; a stale socket from a crashed one is replaced.
                if UnixStream::connect(path).is_ok() {
                    return Err(io::Error::new(
                        io::ErrorKind::AddrInUse,
                        format!("another server listens at {}", path.display()),
                    ));
                }
                fs::remove_file(path)?;
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
        let listener = UnixListener::bind(path)?;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
        let shared = Arc::new(Shared {
            fs,
            files,
            listings: Mutex::new(Listings::default()),
        });
        let stop = Arc::new(AtomicBool::new(false));
        let conns: Conns = Arc::new(Mutex::new(HashMap::new()));
        let readers: Arc<Mutex<Vec<JoinHandle<()>>>> = Arc::new(Mutex::new(Vec::new()));
        let (tx, rx) = mpsc::sync_channel::<Job>(QUEUE);
        let rx = Arc::new(Mutex::new(rx));
        let workers = workers.clamp(1, 64);
        let mut threads = Vec::with_capacity(workers + 1);
        for n in 0..workers {
            let (rx, sh) = (rx.clone(), shared.clone());
            threads.push(
                std::thread::Builder::new()
                    .name(format!("asc-fskit-{n}"))
                    .spawn(move || worker(&sh, &rx))?,
            );
        }
        {
            let (sh, stop, conns, readers, tx) = (
                shared.clone(),
                stop.clone(),
                conns.clone(),
                readers.clone(),
                tx.clone(),
            );
            threads.push(
                std::thread::Builder::new()
                    .name("asc-fskit-accept".to_owned())
                    .spawn(move || accept(&listener, &sh, &stop, &conns, &readers, &tx))?,
            );
        }
        Ok(Self {
            shared,
            path: path.to_path_buf(),
            stop,
            conns,
            readers,
            threads,
            tx: Some(tx),
        })
    }

    /// The socket the server listens at.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Lookups the connected clients hold, summed.
    pub fn held(&self) -> u64 {
        lock(&self.conns)
            .values()
            .map(|(_, c)| lock(&c.held).map.values().sum::<u64>())
            .sum()
    }

    /// Files open now.
    pub fn open_files(&self) -> usize {
        lock(&self.shared.files).open_count()
    }

    fn shutdown(&mut self) {
        if self.stop.swap(true, Ordering::SeqCst) {
            return;
        }
        // Wakes the accept thread.
        let _ = UnixStream::connect(&self.path);
        for (_, (c, _)) in lock(&self.conns).drain() {
            let _ = c.shutdown(std::net::Shutdown::Both);
        }
        self.tx = None;
        for t in self.threads.drain(..) {
            let _ = t.join();
        }
        for t in lock(&self.readers).drain(..) {
            let _ = t.join();
        }
        let _ = fs::remove_file(&self.path);
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
        let Ok((frame, conn, out)) = job else { return };
        let reply = sh.serve(&conn, &frame);
        if !reply.is_empty() {
            let mut s = lock(&out);
            let _ = wire::write_frame(&mut *s, &reply);
        }
    }
}

/// Whether the process at the other end runs as the owner. The socket's directory already
/// admits nobody else; Linux also asks the kernel for the peer's credentials.
#[cfg(target_os = "linux")]
fn peer_allowed(stream: &UnixStream) -> bool {
    rustix::net::sockopt::socket_peercred(stream)
        .is_ok_and(|c| c.uid.as_raw() == rustix::process::geteuid().as_raw())
}

#[cfg(not(target_os = "linux"))]
fn peer_allowed(_stream: &UnixStream) -> bool {
    true
}

static NEXT_CONN: AtomicU64 = AtomicU64::new(0);

fn accept(
    listener: &UnixListener,
    sh: &Arc<Shared>,
    stop: &AtomicBool,
    conns: &Conns,
    readers: &Mutex<Vec<JoinHandle<()>>>,
    tx: &SyncSender<Job>,
) {
    for stream in listener.incoming() {
        if stop.load(Ordering::SeqCst) {
            return;
        }
        let Ok(stream) = stream else { continue };
        if !peer_allowed(&stream) || lock(conns).len() >= MAX_CONNS {
            let _ = stream.shutdown(std::net::Shutdown::Both);
            continue;
        }
        let (Ok(read), Ok(keep)) = (stream.try_clone(), stream.try_clone()) else {
            continue;
        };
        let id = NEXT_CONN.fetch_add(1, Ordering::Relaxed);
        let conn = Arc::new(Conn::default());
        lock(conns).insert(id, (keep, conn.clone()));
        let out = Arc::new(Mutex::new(stream));
        let (tx, conns2, sh2) = (tx.clone(), conns.clone(), sh.clone());
        let t = std::thread::Builder::new()
            .name(format!("asc-fskit-conn-{id}"))
            .spawn(move || {
                let mut r = BufReader::with_capacity(256 * 1024, read);
                while let Ok(Some(frame)) = wire::read_frame(&mut r) {
                    if tx.send((frame, conn.clone(), out.clone())).is_err() {
                        break;
                    }
                }
                if let Some((c, _)) = lock(&conns2).remove(&id) {
                    let _ = c.shutdown(std::net::Shutdown::Both);
                }
                sh2.release(&conn);
            });
        match t {
            Ok(t) => {
                let mut rs = lock(readers);
                rs.retain(|h| !h.is_finished());
                rs.push(t);
            }
            Err(_) => {
                if let Some((c, conn)) = lock(conns).remove(&id) {
                    let _ = c.shutdown(std::net::Shutdown::Both);
                    sh.release(&conn);
                }
            }
        }
    }
}
