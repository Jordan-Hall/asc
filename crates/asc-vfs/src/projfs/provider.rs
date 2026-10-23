//! The mount: marks the root, starts virtualization with the callbacks, and stops it cleanly.
//! [`ProjfsProvider`] implements [`Provider`]; the switch and dehydration entry points belong to
//! U57 and refuse until then.

use std::collections::HashMap;
use std::ffi::c_void;
use std::io;
use std::os::windows::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::Duration;

use asc_core::id::FileId;
use asc_core::path::{Name, RepoPath};
use asc_store::cachedir::CacheDir;
use asc_store::objects::Objects;
use asc_wc::redirect::Redirections;
use asc_wc::switch::Plan;
use asc_wc::times::Switched;
use asc_wc::vstatus::{DirEntry, DiskKind, DiskStat, DiskState, PathState};
use tokio::runtime::Handle;

use super::enumerate::Session;
use super::ffi::{self, Api, Callbacks, Ctx, Guid, NotificationMapping, StartOptions};
use super::{JournalSink, TimeSource, block, data, enumerate, notify};
use crate::core::{CoreFs, DehydrateOutcome, Provider, Result, SwitchOutcome, VfsError};

/// Most existing redirected directories that get a `PRJ_NOTIFY_NONE` mapping.
pub const MAX_MAPPINGS: usize = 4096;
/// Most directories the pre-mount scan for redirected directories visits.
pub const MAX_SCAN_DIRS: usize = 200_000;
/// Longest wait for in-flight asynchronous hydrations at unmount.
const DRAIN_TIMEOUT: Duration = Duration::from_secs(30);

/// What a ProjFS provider needs from the daemon.
pub struct Config {
    /// The workspace uuid: the virtualization instance GUID and every placeholder's `ProviderID`.
    pub workspace: [u8; 16],
    pub core: Arc<CoreFs>,
    pub objects: Arc<dyn Objects>,
    /// The local content cache; content found there is served without leaving the callback.
    pub local: Option<CacheDir>,
    pub times: Arc<dyn TimeSource>,
    pub journal: Arc<dyn JournalSink>,
    pub redirections: Redirections,
    /// The runtime the callbacks run async work on.
    pub rt: Handle,
}

/// The state every callback reaches through `instanceContext`.
pub(crate) struct Inner {
    pub api: Arc<Api>,
    pub cfg: Config,
    /// The virtualization root, for the switch and reconcile code of U57.
    #[allow(dead_code)]
    pub root: PathBuf,
    pub sessions: Mutex<HashMap<Guid, Session>>,
    /// Asynchronous `GetFileData` commands by id; the flag is set by the cancel callback.
    pub commands: Mutex<HashMap<i32, Arc<AtomicBool>>>,
    pub stopping: AtomicBool,
    inflight: Mutex<usize>,
    drained: Condvar,
}

/// Lock helper: a poisoned callback table is still a usable table.
pub(crate) fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Counts an asynchronous task so unmount can wait for it.
pub(crate) struct Inflight(Arc<Inner>);

impl Inner {
    /// `None` once the provider is stopping.
    pub fn begin(self: &Arc<Self>) -> Option<Inflight> {
        let mut n = lock(&self.inflight);
        if self.stopping.load(Ordering::SeqCst) {
            return None;
        }
        *n += 1;
        Some(Inflight(self.clone()))
    }

    #[allow(dead_code)] // U57 (update/delete calls) needs the instance GUID.
    pub fn guid(&self) -> Guid {
        Guid::from_uuid_bytes(self.cfg.workspace)
    }

    /// Whether the content of `id` is in the local cache.
    pub fn is_local(&self, id: FileId) -> bool {
        self.cfg
            .local
            .as_ref()
            .is_some_and(|l| l.content_path(id).is_some())
    }
}

impl Drop for Inflight {
    fn drop(&mut self) {
        let mut n = lock(&self.0.inflight);
        *n = n.saturating_sub(1);
        if *n == 0 {
            self.0.drained.notify_all();
        }
    }
}

struct Mounted {
    inner: Arc<Inner>,
    /// `Arc::into_raw` of `inner`, as `instanceContext`.
    raw: usize,
    ctx: usize,
}

/// The ProjFS [`Provider`].
pub struct ProjfsProvider {
    api: Arc<Api>,
    cfg: Mutex<Option<Config>>,
    mounted: Mutex<Option<Mounted>>,
    disk: ProjDisk,
}

impl ProjfsProvider {
    /// Loads `ProjectedFSLib.dll`; fails with [`ffi::ProviderUnavailable`] where `Client-ProjFS`
    /// is not enabled.
    pub fn new(cfg: Config) -> std::result::Result<Self, ffi::ProviderUnavailable> {
        let api = Arc::new(Api::load()?);
        let disk = ProjDisk {
            api: api.clone(),
            core: cfg.core.clone(),
            rt: cfg.rt.clone(),
            root: Mutex::new(None),
            safe: Mutex::new(None),
        };
        Ok(Self {
            api,
            cfg: Mutex::new(Some(cfg)),
            mounted: Mutex::new(None),
            disk,
        })
    }

    /// The running instance (for the switch, reconcile and dehydration code of U57).
    #[allow(dead_code)] // U57 reaches the running instance through this.
    pub(crate) fn inner(&self) -> Option<Arc<Inner>> {
        lock(&self.mounted).as_ref().map(|m| m.inner.clone())
    }
}

fn unsupported(what: &str) -> VfsError {
    VfsError::Io(io::Error::new(io::ErrorKind::Unsupported, what.to_owned()))
}

fn hr_error(what: &str, hr: ffi::Hresult) -> VfsError {
    VfsError::Io(io::Error::other(format!(
        "{what} failed: HRESULT {:#010x}",
        hr as u32
    )))
}

/// Whether `m` is a reparse point (symlink, junction, mount point, placeholder-less reparse).
fn is_reparse(m: &std::fs::Metadata) -> bool {
    m.file_attributes() & ffi::FILE_ATTRIBUTE_REPARSE_POINT != 0
}

/// Redirected directories already on disk, as root-relative backslash paths. Reparse points are
/// not entered, redirected directories are not descended into, and the walk is bounded.
fn existing_redirections(root: &Path, red: &Redirections) -> Vec<String> {
    let mut out = Vec::new();
    if red.is_empty() {
        return out;
    }
    let mut stack = vec![(root.to_path_buf(), String::new())];
    let mut visited = 0usize;
    while let Some((dir, rel)) = stack.pop() {
        visited += 1;
        if visited > MAX_SCAN_DIRS {
            break;
        }
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        for e in rd.flatten() {
            let Ok(m) = std::fs::symlink_metadata(e.path()) else {
                continue;
            };
            if !m.is_dir() || is_reparse(&m) {
                continue;
            }
            let Some(name) = e.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            if name == ".git" {
                continue;
            }
            let child = if rel.is_empty() {
                name.clone()
            } else {
                format!("{rel}\\{name}")
            };
            if notify::is_redirected(red, name.as_bytes()) {
                if out.len() < MAX_MAPPINGS {
                    out.push(child);
                }
            } else {
                stack.push((e.path(), child));
            }
        }
    }
    out
}

impl Provider for ProjfsProvider {
    fn mount(&self, root: &Path) -> Result<()> {
        let mut mounted = lock(&self.mounted);
        if mounted.is_some() {
            return Err(VfsError::AlreadyMounted);
        }
        let cfg = lock(&self.cfg).take().ok_or(VfsError::AlreadyMounted)?;
        std::fs::create_dir_all(root)?;
        let meta = std::fs::symlink_metadata(root)?;
        if !meta.is_dir() || is_reparse(&meta) {
            *lock(&self.cfg) = Some(cfg);
            return Err(VfsError::Io(io::Error::new(
                io::ErrorKind::InvalidInput,
                "the virtualization root must be a real directory",
            )));
        }
        let root = root.to_path_buf();
        let guid = Guid::from_uuid_bytes(cfg.workspace);
        let wide_root = ffi::path_to_wide(&root);
        let attrs = meta.file_attributes() | ffi::FILE_ATTRIBUTE_NOT_CONTENT_INDEXED;
        ffi::set_attributes(&wide_root, attrs)?;
        // SAFETY: valid NUL terminated strings and GUID; null `target` and `version` are allowed.
        let hr = unsafe {
            (self.api.mark_placeholder)(
                wide_root.as_ptr(),
                std::ptr::null(),
                std::ptr::null(),
                &guid,
            )
        };
        // Already marked by an earlier mount of the same workspace.
        if hr != ffi::S_OK && hr != ffi::ERROR_REPARSE_POINT_ENCOUNTERED {
            *lock(&self.cfg) = Some(cfg);
            return Err(hr_error("PrjMarkDirectoryAsPlaceholder", hr));
        }
        let redirected = existing_redirections(&root, &cfg.redirections);
        let inner = Arc::new(Inner {
            api: self.api.clone(),
            cfg,
            root: root.clone(),
            sessions: Mutex::new(HashMap::new()),
            commands: Mutex::new(HashMap::new()),
            stopping: AtomicBool::new(false),
            inflight: Mutex::new(0),
            drained: Condvar::new(),
        });
        let wides: Vec<Vec<u16>> = redirected.iter().map(|r| ffi::to_wide(r)).collect();
        let root_mask = notify::MOUNT_MASK;
        let empty = [0u16];
        let mut maps = vec![NotificationMapping {
            mask: root_mask,
            root: empty.as_ptr(),
        }];
        maps.extend(wides.iter().map(|w| NotificationMapping {
            mask: ffi::notify::NONE,
            root: w.as_ptr(),
        }));
        let opts = StartOptions {
            flags: ffi::START_USE_NEGATIVE_PATH_CACHE,
            pool_threads: 0,
            concurrent_threads: 0,
            mappings: maps.as_mut_ptr(),
            mapping_count: u32::try_from(maps.len()).unwrap_or(0),
        };
        let callbacks = Callbacks {
            start_dir: Some(enumerate::start_dir),
            end_dir: Some(enumerate::end_dir),
            get_dir: Some(enumerate::get_dir),
            get_placeholder: Some(data::get_placeholder),
            get_file_data: Some(data::get_file_data),
            query_file_name: Some(data::query_file_name),
            notification: Some(notify::notification),
            cancel: Some(data::cancel),
        };
        let raw = Arc::into_raw(inner.clone());
        let mut ctx: Ctx = std::ptr::null_mut();
        // SAFETY: `raw` stays valid until `unmount` reclaims it; the strings in `maps` outlive
        // this call (ProjFS copies them); `callbacks` and `opts` are plain data.
        let hr = unsafe {
            (self.api.start)(
                wide_root.as_ptr(),
                &callbacks,
                raw.cast::<c_void>(),
                &opts,
                &mut ctx,
            )
        };
        if hr != ffi::S_OK {
            // SAFETY: reclaiming the reference leaked above; ProjFS never saw it succeed.
            drop(unsafe { Arc::from_raw(raw) });
            let cfg = Arc::try_unwrap(inner).ok().map(|i| i.cfg);
            *lock(&self.cfg) = cfg;
            return Err(hr_error("PrjStartVirtualizing", hr));
        }
        *lock(&self.disk.root) = Some(root.clone());
        *lock(&self.disk.safe) = Some(Arc::new(asc_wc::SafeRoot::new(root)));
        *mounted = Some(Mounted {
            inner,
            raw: raw as usize,
            ctx: ctx as usize,
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
        for flag in lock(&m.inner.commands).values() {
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
        // SAFETY: the context came from `PrjStartVirtualizing`; stopping waits for callbacks.
        unsafe { (self.api.stop)(m.ctx as Ctx) };
        // SAFETY: the reference leaked in `mount`; no callback can run after the stop.
        drop(unsafe { Arc::from_raw(m.raw as *const Inner) });
        *lock(&self.disk.root) = None;
        *lock(&self.disk.safe) = None;
        Ok(())
    }

    fn apply_switch(&self, _plan: &Plan, _times: Switched) -> Result<SwitchOutcome> {
        Err(unsupported("the ProjFS switch is implemented by U57"))
    }

    fn dehydrate(&self, _paths: &[RepoPath]) -> Result<DehydrateOutcome> {
        Err(unsupported("ProjFS dehydration is implemented by U57"))
    }

    fn disk_state(&self) -> &dyn DiskState {
        &self.disk
    }
}

impl Drop for ProjfsProvider {
    fn drop(&mut self) {
        let _ = self.unmount();
    }
}

/// `PrjGetOnDiskFileState` over the root, plus the on-disk view through [`asc_wc::SafeRoot`].
struct ProjDisk {
    api: Arc<Api>,
    core: Arc<CoreFs>,
    rt: Handle,
    root: Mutex<Option<PathBuf>>,
    safe: Mutex<Option<Arc<asc_wc::SafeRoot>>>,
}

impl ProjDisk {
    fn safe(&self) -> io::Result<Arc<asc_wc::SafeRoot>> {
        lock(&self.safe)
            .clone()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotConnected, "not mounted"))
    }
}

impl DiskState for ProjDisk {
    fn state(&self, path: &RepoPath) -> io::Result<PathState> {
        let safe = self.safe()?;
        let Some(abs) = safe.resolve(path) else {
            return Ok(PathState::Missing);
        };
        let wide = ffi::path_to_wide(&abs);
        let mut st = 0u32;
        // SAFETY: NUL terminated path and a valid out pointer.
        let hr = unsafe { (self.api.on_disk_state)(wide.as_ptr(), &mut st) };
        if hr == ffi::S_OK {
            use ffi::file_state::*;
            return Ok(if st & TOMBSTONE != 0 {
                PathState::Tombstone
            } else if st & (FULL | DIRTY_PLACEHOLDER) != 0 {
                PathState::Full
            } else if st & HYDRATED_PLACEHOLDER != 0 {
                PathState::Hydrated
            } else if st & PLACEHOLDER != 0 {
                PathState::Placeholder
            } else {
                PathState::Virtual
            });
        }
        if hr == ffi::ERROR_FILE_NOT_FOUND || hr == ffi::ERROR_PATH_NOT_FOUND {
            let found = block(&self.rt, self.core.resolve(path))
                .map_err(|e| io::Error::other(e.to_string()))?;
            return Ok(if found.is_some() {
                PathState::Virtual
            } else {
                PathState::Missing
            });
        }
        Err(io::Error::other(format!(
            "PrjGetOnDiskFileState: HRESULT {:#010x}",
            hr as u32
        )))
    }

    fn stat(&self, path: &RepoPath) -> io::Result<Option<DiskStat>> {
        let safe = self.safe()?;
        let Some(abs) = safe.resolve(path) else {
            return Ok(None);
        };
        let m = match std::fs::symlink_metadata(&abs) {
            Ok(m) => m,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e),
        };
        let kind = if m.is_dir() && !is_reparse(&m) {
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
        let safe = self.safe()?;
        let abs = safe
            .resolve(path)
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
        let safe = self.safe()?;
        let abs = safe
            .resolve(dir)
            .ok_or_else(|| io::Error::from(io::ErrorKind::NotFound))?;
        let mut out = Vec::new();
        for e in std::fs::read_dir(&abs)? {
            let e = e?;
            let Some(name) = e.file_name().to_str().and_then(|n| Name::new(n).ok()) else {
                continue;
            };
            let child = dir.join(&name);
            // Projected-only children have no on-disk state and are not listed.
            if matches!(self.state(&child)?, PathState::Virtual | PathState::Missing) {
                continue;
            }
            let is_dir = e.file_type().is_ok_and(|t| t.is_dir());
            out.push(DirEntry { name, dir: is_dir });
        }
        Ok(out)
    }
}
