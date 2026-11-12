//! The Unix merged file system (SPEC §10.3, §10.4) that the Linux FUSE backend and the macOS NFS
//! backend both serve: [`inodes`] is the persisted inode table, [`overlay`] holds what the user
//! changed, and [`merged`] answers every file system operation over the base tree and the
//! overlay, with exact journaling. The front ends only translate their protocol to it.
//!
//! Every name, path and handle a front end receives is untrusted: names are validated once, paths
//! are built as `RepoPath`s, and the overlay is only reached through `SafeRoot`, so a symlink in
//! an ancestor is never followed.

use std::future::Future;
use std::io;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use asc_core::path::RepoPath;
use asc_state::StateDb;
use asc_state::state::workspaces::Workspace;
use asc_store::cachedir::CacheDir;
use asc_wc::journal::Op;
use asc_wc::redirect::Redirections;
use asc_wc::times::entry_times_of;
use tokio::runtime::Handle;

use crate::core::{CoreFs, Provider};

pub mod convert;
pub mod inodes;
pub mod merged;
pub mod overlay;
#[cfg(test)]
pub(crate) mod testkit;

pub use convert::{Unvirtualized, unvirtualize};
pub use merged::{Disk, MergedFs};

/// The error every operation answers with: a POSIX error number of this platform.
pub type Errno = rustix::io::Errno;

/// The placeholder times of paths (SPEC §10.1).
pub trait TimeSource: Send + Sync {
    /// The time, in Unix seconds, of each path, in order.
    fn times(&self, paths: &[RepoPath]) -> io::Result<Vec<i64>>;
}

/// [`TimeSource`] over `state.db`'s `entry_times` (U54v `times`).
pub struct DbTimes {
    pub db: StateDb,
    pub ws: Workspace,
    pub rt: Handle,
}

impl TimeSource for DbTimes {
    fn times(&self, paths: &[RepoPath]) -> io::Result<Vec<i64>> {
        block(&self.rt, entry_times_of(&self.db, &self.ws, paths))
            .map_err(|e| io::Error::other(e.to_string()))
    }
}

/// Where the file system appends its change hints. Every write passes through the daemon, so the
/// hints are exact.
pub trait JournalSink: Send + Sync {
    /// Never blocks and never fails.
    fn record(&self, op: Op, path: &[u8]);
}

impl JournalSink for asc_wc::journal::Journal {
    fn record(&self, op: Op, path: &[u8]) {
        asc_wc::journal::Journal::record(self, op, path);
    }
}

/// What the front end's kernel cache must forget when the file system changes behind it.
pub trait Notify: Send + Sync {
    /// The content of `ino` changed or was dropped.
    fn content(&self, ino: u64);
    /// The entry `name` of `parent` is gone or now names something else.
    fn entry(&self, parent: u64, name: &[u8]);
    /// The attributes of `ino` changed.
    fn inode(&self, ino: u64);
}

/// A front end with no kernel cache to tell.
pub struct NoNotify;

impl Notify for NoNotify {
    fn content(&self, _ino: u64) {}
    fn entry(&self, _parent: u64, _name: &[u8]) {}
    fn inode(&self, _ino: u64) {}
}

/// What a merged file system needs from the daemon.
pub struct Config {
    pub core: Arc<CoreFs>,
    /// The local content cache: clean files are served from it, without a copy.
    pub local: Option<CacheDir>,
    /// Where inode numbers persist (`state.db.inodes`).
    pub inodes: Arc<dyn inodes::InodeStore>,
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
    /// The gitdir a read-only `.git` gitfile at the root names, so git and asc find the
    /// workspace inside the mount; `None` serves no `.git`.
    pub gitfile: Option<PathBuf>,
}

/// A front end over a [`MergedFs`]: the FUSE provider on Linux, the NFS provider on macOS (or on
/// Linux when asked for). The daemon hosts either through this.
pub trait UnixProvider: Provider {
    /// The merged file system the front end serves.
    fn merged(&self) -> &Arc<MergedFs>;
    /// The backend's name, for logs and `asc doctor`.
    fn backend(&self) -> &'static str;
}

/// What reconcile journaled, for the daemon's metrics.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Reconciled {
    /// Overlay entries and deletion markers examined.
    pub examined: u64,
    pub modified: u64,
    pub created: u64,
    pub deleted: u64,
}

/// What a path is, as `stat` shows it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VKind {
    Dir,
    File,
    Symlink,
}

/// The attributes of one entry, and how long a kernel may cache them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Attr {
    pub ino: u64,
    pub size: u64,
    pub blocks: u64,
    pub atime: SystemTime,
    pub mtime: SystemTime,
    pub ctime: SystemTime,
    pub kind: VKind,
    pub perm: u16,
    pub nlink: u32,
    pub uid: u32,
    pub gid: u32,
    pub blksize: u32,
    pub ttl: Duration,
}

/// Runs `f` to completion from a thread that may or may not be inside the runtime. Front-end
/// worker threads are not; `DiskState` callers may be.
pub(crate) fn block<F>(rt: &Handle, f: F) -> F::Output
where
    F: Future + Send,
    F::Output: Send,
{
    if Handle::try_current().is_ok() {
        std::thread::scope(|s| {
            s.spawn(|| rt.block_on(f))
                .join()
                .unwrap_or_else(|e| std::panic::resume_unwind(e))
        })
    } else {
        rt.block_on(f)
    }
}
