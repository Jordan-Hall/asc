//! The mount: registers the root as a sync root of this workspace, connects the callbacks, starts
//! the change watcher, and stops all of it cleanly. [`CloudFilesProvider`] implements
//! [`Provider`]; the switch, reconcile, dehydration and conversion code live in `switch`, `walk`
//! and `unvirtualize`.

use std::collections::{BTreeSet, HashMap};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, SystemTime};

use asc_core::id::FileId;
use asc_core::path::{Name, RepoPath};
use asc_wc::SafeRoot;
use asc_wc::switch::Plan;
use asc_wc::times::Switched;
use asc_wc::vstatus::{DirEntry, DiskKind, DiskStat, DiskState, PathState};
use tokio::runtime::Handle;
use windows::Win32::Storage::CloudFilters::{CF_CONNECTION_KEY, CfDisconnectSyncRoot};

use super::ffi::{self, ProviderUnavailable};
use super::switch::Store;
use super::watch::Watcher;
use super::{Ident, callbacks, populate};
use crate::core::{CoreFs, DehydrateOutcome, Provider, Result, SwitchOutcome, VfsError};
use crate::win::{block, rel_path, verbatim};

pub use crate::win::Config;

/// Longest wait for in-flight asynchronous hydrations at unmount.
const DRAIN_TIMEOUT: Duration = Duration::from_secs(30);

/// State shared by the callbacks, the watcher and the provider.
pub struct Inner {
    pub cfg: Config,
    /// The sync root, canonical and without the verbatim prefix (the filter's own spelling).
    pub root: PathBuf,
    /// The drive of the root (`d:`), lower case.
    drive: String,
    /// The root below its drive (`\work\ws`), lower case: callbacks name volume-relative paths.
    prefix: String,
    /// The gate every work tree path goes through (no link in an ancestor is followed). Verbatim,
    /// so paths past `MAX_PATH` work.
    pub safe: Arc<SafeRoot>,
    /// The connection key; zero before the connection succeeded.
    pub key: AtomicI64,
    /// When this instance started: the read time of files the hydration log has not seen.
    pub started: SystemTime,
    /// Asynchronous `FETCH_DATA` transfers by transfer key; the flag is set by the cancel callback.
    pub transfers: Mutex<HashMap<i64, Arc<AtomicBool>>>,
    pub stopping: AtomicBool,
    /// What the user deleted or moved away while this instance served the root, from the watcher;
    /// `worktree convert --plain` keeps these deletions.
    pub deleted: Mutex<BTreeSet<RepoPath>>,
    inflight: Mutex<usize>,
    drained: Condvar,
}

/// Lock helper: a poisoned table is still a usable table.
pub(crate) fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Counts an asynchronous task so unmount can wait for it.
pub(crate) struct Inflight(Arc<Inner>);

impl Drop for Inflight {
    fn drop(&mut self) {
        let mut n = lock(&self.0.inflight);
        *n = n.saturating_sub(1);
        if *n == 0 {
            self.0.drained.notify_all();
        }
    }
}

/// Splits a plain absolute path into its drive (`d:`) and the rest, both lower case.
fn split_root(root: &Path) -> Option<(String, String)> {
    let s = root.to_str()?;
    let b = s.as_bytes();
    if b.len() < 2 || b[1] != b':' || !b[0].is_ascii_alphabetic() {
        return None;
    }
    let rest = s[2..].trim_end_matches('\\');
    Some((s[..2].to_ascii_lowercase(), rest.to_lowercase()))
}

impl Inner {
    fn new(cfg: Config, root: PathBuf) -> io::Result<Self> {
        let (drive, prefix) = split_root(&root).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "the sync root must be a path on a local drive",
            )
        })?;
        Ok(Self {
            safe: Arc::new(SafeRoot::new(verbatim(&root))),
            cfg,
            root,
            drive,
            prefix,
            key: AtomicI64::new(0),
            started: SystemTime::now(),
            transfers: Mutex::new(HashMap::new()),
            stopping: AtomicBool::new(false),
            deleted: Mutex::new(BTreeSet::new()),
            inflight: Mutex::new(0),
            drained: Condvar::new(),
        })
    }

    /// `None` once the provider is stopping.
    pub(crate) fn begin(self: &Arc<Self>) -> Option<Inflight> {
        let mut n = lock(&self.inflight);
        if self.stopping.load(Ordering::SeqCst) {
            return None;
        }
        *n += 1;
        Some(Inflight(self.clone()))
    }

    /// Whether the content of `id` is in the local cache.
    pub fn is_local(&self, id: FileId) -> bool {
        self.cfg
            .local
            .as_ref()
            .is_some_and(|l| l.content_path(id).is_some())
    }

    /// The root-relative path of a path the filter names (volume-relative, with or without a
    /// drive or the verbatim prefix). `Some(None)` is the root; `None` is outside the root or not
    /// a valid repository path.
    pub fn repo_path(&self, full: &str) -> Option<Option<RepoPath>> {
        let s = full.strip_prefix(r"\\?\").unwrap_or(full);
        let s = match s.as_bytes() {
            [d, b':', ..] if d.is_ascii_alphabetic() => {
                if !s[..2].eq_ignore_ascii_case(&self.drive) {
                    return None;
                }
                &s[2..]
            }
            _ => s,
        };
        let s = s.replace('/', "\\");
        // Compare char by char so a case fold that changes the byte length is still exact.
        let mut it = s.char_indices();
        let mut cut = 0usize;
        for want in self.prefix.chars() {
            let (i, c) = it.next()?;
            if !c.to_lowercase().eq(want.to_lowercase()) {
                return None;
            }
            cut = i + c.len_utf8();
        }
        let tail = &s[cut..];
        if tail.is_empty() {
            return Some(None);
        }
        let rel = tail.strip_prefix('\\')?.trim_end_matches('\\');
        rel_path(rel).ok()
    }

    /// The absolute (verbatim) path of `path`, through the gate; `None` where a link stands in
    /// an ancestor.
    pub fn abs(&self, path: Option<&RepoPath>) -> Option<PathBuf> {
        match path {
            None => Some(self.safe.root().to_path_buf()),
            Some(p) => self.safe.resolve(p),
        }
    }

    /// The times of `paths`, in order.
    pub fn times(&self, paths: &[RepoPath]) -> io::Result<Vec<i64>> {
        let t = self.cfg.times.times(paths)?;
        if t.len() != paths.len() {
            return Err(io::Error::other("the time source returned a short list"));
        }
        Ok(t)
    }

    /// The tree entry at `path`, case-insensitively.
    pub fn resolve(&self, path: &RepoPath) -> io::Result<Option<crate::core::Node>> {
        block(&self.cfg.rt, self.cfg.core.resolve(path)).map_err(vfs_io)
    }
}

pub(crate) fn vfs_io(e: VfsError) -> io::Error {
    match e {
        VfsError::Io(e) => e,
        VfsError::NotFound(_) => io::Error::from(io::ErrorKind::NotFound),
        other => io::Error::other(other.to_string()),
    }
}

struct Mounted {
    inner: Arc<Inner>,
    /// `Arc::into_raw` of `inner`, as the callback context.
    raw: usize,
    watcher: Watcher,
}

/// The Cloud Files [`Provider`].
pub struct CloudFilesProvider {
    cfg: Mutex<Option<Config>>,
    mounted: Mutex<Option<Mounted>>,
    disk: CfDisk,
    /// Where the switch intent is kept, once the daemon says.
    pub(crate) store: Mutex<Option<Store>>,
}

impl CloudFilesProvider {
    /// Fails with [`ProviderUnavailable`] where the Cloud Files filter is not running.
    pub fn new(cfg: Config) -> std::result::Result<Self, ProviderUnavailable> {
        ffi::platform_available()?;
        let disk = CfDisk {
            core: cfg.core.clone(),
            rt: cfg.rt.clone(),
            inner: Mutex::new(None),
        };
        Ok(Self {
            cfg: Mutex::new(Some(cfg)),
            mounted: Mutex::new(None),
            disk,
            store: Mutex::new(None),
        })
    }

    /// The running instance (for the switch, reconcile and dehydration code).
    pub(crate) fn inner(&self) -> Option<Arc<Inner>> {
        lock(&self.mounted).as_ref().map(|m| m.inner.clone())
    }

    /// The items the user deleted while the root was served, as root-relative paths; a deleted
    /// directory is one path. Cloud Files keeps no tombstones: this is what the watcher saw go
    /// and is still absent, plus tracked children missing from listed directories.
    pub fn tombstones(&self) -> Result<Vec<RepoPath>> {
        let inner = self.inner().ok_or(VfsError::NotMounted)?;
        let mut out = super::walk::missing(&inner)?;
        let heard: Vec<RepoPath> = lock(&inner.deleted).iter().cloned().collect();
        for p in heard {
            let absent = inner.safe.resolve(&p).is_some_and(
                |abs| matches!(std::fs::symlink_metadata(abs), Err(e) if ffi::is_not_found(&e)),
            );
            // A switch's own removals are heard too; only tracked paths can be deleted ones.
            if absent && inner.resolve(&p)?.is_some() {
                out.push(p);
            }
        }
        out.sort();
        out.dedup();
        // A path below a deleted directory is covered by the directory.
        let mut kept: Vec<RepoPath> = Vec::with_capacity(out.len());
        for p in out {
            if kept.last().is_some_and(|d| p.starts_with_dir(d)) {
                continue;
            }
            kept.push(p);
        }
        Ok(kept)
    }
}

impl Provider for CloudFilesProvider {
    fn mount(&self, root: &Path) -> Result<()> {
        let mut mounted = lock(&self.mounted);
        if mounted.is_some() {
            return Err(VfsError::AlreadyMounted);
        }
        let cfg = lock(&self.cfg).take().ok_or(VfsError::AlreadyMounted)?;
        let give_back = |cfg: Config, e: VfsError| {
            *lock(&self.cfg) = Some(cfg);
            Err(e)
        };
        if let Err(e) = std::fs::create_dir_all(root) {
            return give_back(cfg, e.into());
        }
        let meta = match std::fs::symlink_metadata(root) {
            Ok(m) => m,
            Err(e) => return give_back(cfg, e.into()),
        };
        if !meta.is_dir() || meta.file_type().is_symlink() {
            return give_back(
                cfg,
                VfsError::Io(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "the sync root must be a real directory",
                )),
            );
        }
        let root = match std::fs::canonicalize(root) {
            Ok(r) => ffi::plain(&r),
            Err(e) => return give_back(cfg, e.into()),
        };
        // A root another program registered (OneDrive) is never taken over; a root inside another
        // workspace's sync root is refused by the registration itself.
        if let Some(owner) = ffi::sync_root_provider(&root)
            && owner != ffi::PROVIDER_NAME
        {
            return give_back(
                cfg,
                VfsError::Io(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    format!(
                        "{} is already inside a sync root of {owner}",
                        root.display()
                    ),
                )),
            );
        }
        // Registering again resets the root to "never listed", which would hide what is on disk
        // from reconcile; a root already registered for this workspace is only connected to.
        let registered = ffi::is_our_sync_root(&root)
            && ffi::sync_root_identity(&root).as_deref() == Some(&cfg.workspace[..]);
        if !registered && let Err(e) = ffi::register(&root, &cfg.workspace) {
            return give_back(cfg, e.into());
        }
        let inner = match Inner::new(cfg, root.clone()) {
            Ok(i) => Arc::new(i),
            Err(e) => return Err(e.into()),
        };
        let raw = Arc::into_raw(inner.clone());
        let key = match callbacks::connect(&root, raw.cast()) {
            Ok(k) => k,
            Err(e) => {
                // SAFETY: reclaiming the reference leaked above; the filter never saw it succeed.
                drop(unsafe { Arc::from_raw(raw) });
                let cfg = Arc::try_unwrap(inner).ok().map(|i| i.cfg);
                *lock(&self.cfg) = cfg;
                return Err(e.into());
            }
        };
        inner.key.store(key.0, Ordering::SeqCst);
        let watcher = match Watcher::start(inner.clone()) {
            Ok(w) => w,
            Err(e) => {
                // SAFETY: the key came from the connection above.
                let _ = unsafe { CfDisconnectSyncRoot(key) };
                // SAFETY: no callback runs after the disconnect returned.
                drop(unsafe { Arc::from_raw(raw) });
                return Err(e.into());
            }
        };
        *lock(&self.disk.inner) = Some(inner.clone());
        *mounted = Some(Mounted {
            inner,
            raw: raw as usize,
            watcher,
        });
        Ok(())
    }

    fn unmount(&self) -> Result<()> {
        let Some(m) = lock(&self.mounted).take() else {
            return Err(VfsError::NotMounted);
        };
        {
            // Setting the flag under the counter lock keeps `begin` from racing past it.
            let n = lock(&m.inner.inflight);
            m.inner.stopping.store(true, Ordering::SeqCst);
            drop(n);
        }
        for flag in lock(&m.inner.transfers).values() {
            flag.store(true, Ordering::SeqCst);
        }
        {
            let n = lock(&m.inner.inflight);
            let _ = m
                .inner
                .drained
                .wait_timeout_while(n, DRAIN_TIMEOUT, |n| *n > 0)
                .unwrap_or_else(|e| e.into_inner());
        }
        m.watcher.stop();
        let key = CF_CONNECTION_KEY(m.inner.key.swap(0, Ordering::SeqCst));
        // SAFETY: the key came from `CfConnectSyncRoot`; disconnecting waits for callbacks.
        let _ = unsafe { CfDisconnectSyncRoot(key) };
        // SAFETY: the reference leaked in `mount`; no callback can run after the disconnect.
        drop(unsafe { Arc::from_raw(m.raw as *const Inner) });
        *lock(&self.disk.inner) = None;
        Ok(())
    }

    fn apply_switch(&self, plan: &Plan, times: Switched) -> Result<SwitchOutcome> {
        self.apply_plan(plan, times)
    }

    fn dehydrate(&self, paths: &[RepoPath]) -> Result<DehydrateOutcome> {
        self.dehydrate_paths(paths)
    }

    fn disk_state(&self) -> &dyn DiskState {
        &self.disk
    }
}

impl Drop for CloudFilesProvider {
    fn drop(&mut self) {
        let _ = self.unmount();
    }
}

/// The on-disk view of a served root: placeholder state from the filter, plus the tree for what
/// is not on disk.
struct CfDisk {
    core: Arc<CoreFs>,
    rt: Handle,
    inner: Mutex<Option<Arc<Inner>>>,
}

impl CfDisk {
    fn inner(&self) -> io::Result<Arc<Inner>> {
        lock(&self.inner)
            .clone()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotConnected, "not mounted"))
    }

    fn tracked(&self, path: &RepoPath) -> io::Result<Option<crate::core::Node>> {
        block(&self.rt, self.core.resolve(path)).map_err(vfs_io)
    }
}

/// What a path is from its probe and its tree entry.
pub(crate) fn classify(p: &ffi::Probe, tracked: Option<&crate::core::Node>) -> PathState {
    if !p.is_placeholder() || !p.in_sync {
        return PathState::Full;
    }
    if p.is_dir() {
        return PathState::Placeholder;
    }
    if let Some(n) = tracked
        && p.ident() != Some(Ident::of(n))
    {
        return PathState::Full;
    }
    if is_hydrated(p) {
        PathState::Hydrated
    } else {
        PathState::Placeholder
    }
}

/// `CF_PLACEHOLDER_STATE_PARTIAL` is clear, no recall on data access, and all bytes are there.
pub(crate) fn is_hydrated(p: &ffi::Probe) -> bool {
    use windows::Win32::Storage::CloudFilters::CF_PLACEHOLDER_STATE_PARTIAL;
    const RECALL_ON_DATA_ACCESS: u32 = 0x40_0000;
    let size = match p.ident() {
        Some(Ident::File { size, .. }) => size,
        _ => 0,
    };
    p.state & CF_PLACEHOLDER_STATE_PARTIAL.0 == 0
        && p.attrs & RECALL_ON_DATA_ACCESS == 0
        && p.on_disk >= size
}

impl DiskState for CfDisk {
    fn state(&self, path: &RepoPath) -> io::Result<PathState> {
        let inner = self.inner()?;
        let Some(abs) = inner.abs(Some(path)) else {
            return Ok(PathState::Missing);
        };
        match ffi::probe_opt(&abs)? {
            Some(p) => {
                let tracked = if p.is_placeholder() && !p.is_dir() {
                    self.tracked(path)?
                } else {
                    None
                };
                Ok(classify(&p, tracked.as_ref()))
            }
            None => {
                let Some(_) = self.tracked(path)? else {
                    return Ok(PathState::Missing);
                };
                // Gone from a directory whose children are all on disk: the user deleted it.
                let parent_listed = match path.parent() {
                    None => true,
                    Some(dir) => match inner.abs(Some(&dir)).map(|a| ffi::probe_opt(&a)) {
                        Some(Ok(Some(d))) => d.is_dir() && !d.is_unpopulated(),
                        _ => false,
                    },
                };
                let root_listed = path.parent().is_some()
                    || ffi::probe_opt(inner.safe.root())?.is_some_and(|r| !r.is_unpopulated());
                Ok(if parent_listed && root_listed {
                    PathState::Tombstone
                } else {
                    PathState::Virtual
                })
            }
        }
    }

    fn stat(&self, path: &RepoPath) -> io::Result<Option<DiskStat>> {
        let inner = self.inner()?;
        let Some(abs) = inner.abs(Some(path)) else {
            return Ok(None);
        };
        let m = match std::fs::symlink_metadata(&abs) {
            Ok(m) => m,
            Err(e) if ffi::is_not_found(&e) => return Ok(None),
            Err(e) => return Err(e),
        };
        let kind = if m.is_dir() {
            DiskKind::Dir
        } else if m.file_type().is_symlink() {
            DiskKind::Symlink
        } else if m.is_file() {
            DiskKind::File { exec: None }
        } else {
            DiskKind::Other
        };
        Ok(Some(DiskStat {
            kind,
            size: m.len(),
        }))
    }

    fn read(&self, path: &RepoPath, max: u64) -> io::Result<Vec<u8>> {
        use std::io::Read;
        let inner = self.inner()?;
        let abs = inner
            .abs(Some(path))
            .ok_or_else(|| io::Error::from(io::ErrorKind::NotFound))?;
        let m = std::fs::symlink_metadata(&abs)?;
        if m.file_type().is_symlink() {
            let t = std::fs::read_link(&abs)?;
            return Ok(t.to_string_lossy().replace('\\', "/").into_bytes());
        }
        if !m.is_file() {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "not a file"));
        }
        if m.len() > max {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "file too large"));
        }
        let mut out = Vec::new();
        std::fs::File::open(&abs)?
            .take(max + 1)
            .read_to_end(&mut out)?;
        if out.len() as u64 > max {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "file too large"));
        }
        Ok(out)
    }

    fn list(&self, dir: &RepoPath) -> io::Result<Vec<DirEntry>> {
        let inner = self.inner()?;
        let abs = inner
            .abs(Some(dir))
            .ok_or_else(|| io::Error::from(io::ErrorKind::NotFound))?;
        // A directory not listed yet: its children come on disk first, so what is listed is what
        // stands there, never a guess.
        if ffi::probe(&abs)?.is_unpopulated() {
            populate::populate(&inner, Some(dir), &abs)?;
        }
        let mut out = Vec::new();
        for e in std::fs::read_dir(&abs)? {
            let e = e?;
            let Some(name) = e.file_name().to_str().and_then(|n| Name::new(n).ok()) else {
                continue;
            };
            let is_dir = e.file_type().is_ok_and(|t| t.is_dir());
            out.push(DirEntry { name, dir: is_dir });
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roots_split_into_drive_and_rest() {
        assert_eq!(
            split_root(Path::new(r"D:\Work\WS")),
            Some(("d:".into(), r"\work\ws".into()))
        );
        assert_eq!(
            split_root(Path::new(r"E:\")),
            Some(("e:".into(), String::new()))
        );
        assert_eq!(split_root(Path::new(r"\\server\share")), None);
    }

    #[test]
    fn hydration_needs_every_byte_and_no_partial_state() {
        let id = Ident::File {
            id: FileId::from_bytes([1; 32]),
            size: 10,
        };
        let mut p = ffi::Probe {
            state: windows::Win32::Storage::CloudFilters::CF_PLACEHOLDER_STATE_PLACEHOLDER.0,
            on_disk: 10,
            in_sync: true,
            identity: id.encode(),
            ..Default::default()
        };
        assert!(is_hydrated(&p));
        p.on_disk = 4;
        assert!(!is_hydrated(&p));
        p.on_disk = 10;
        p.state |= windows::Win32::Storage::CloudFilters::CF_PLACEHOLDER_STATE_PARTIAL.0;
        assert!(!is_hydrated(&p));
    }

    #[test]
    fn classification_follows_sync_state_and_identity() {
        use asc_core::object::Kind;
        let node = crate::core::Node {
            path: RepoPath::new("a.txt").unwrap(),
            kind: Kind::File,
            id: [1; 32],
            size: 3,
            git: asc_core::id::GitOid::from_bytes([0; 20]),
            read_only: false,
        };
        let placeholder = windows::Win32::Storage::CloudFilters::CF_PLACEHOLDER_STATE_PLACEHOLDER.0;
        let p = ffi::Probe {
            state: placeholder,
            on_disk: 0,
            in_sync: true,
            identity: Ident::of(&node).encode(),
            ..Default::default()
        };
        assert_eq!(classify(&p, Some(&node)), PathState::Placeholder);
        let hydrated = ffi::Probe {
            on_disk: 3,
            ..p.clone()
        };
        assert_eq!(classify(&hydrated, Some(&node)), PathState::Hydrated);
        let dirty = ffi::Probe {
            in_sync: false,
            ..p.clone()
        };
        assert_eq!(classify(&dirty, Some(&node)), PathState::Full);
        let other = crate::core::Node {
            id: [2; 32],
            ..node.clone()
        };
        assert_eq!(classify(&p, Some(&other)), PathState::Full);
        assert_eq!(classify(&ffi::Probe::default(), None), PathState::Full);
    }
}
