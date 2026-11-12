//! What every virtual workspace host shares, whichever OS backend serves it (SPEC §10): opening
//! the workspace the way the daemon's other jobs do, the local-state check a switch plans with,
//! prefetch over the file system core, and hydration statistics.
//!
//! The provider and engine calls block, so each runs on a blocking thread of the daemon's runtime;
//! the command layer's futures are not `Send`, so opening the workspace does too.

use std::collections::{BTreeSet, HashSet};
use std::path::PathBuf;
use std::sync::Arc;

use asc_core::git::blob_oid;
use asc_core::id::TreeId;
use asc_core::object::{Entry, Kind};
use asc_core::path::RepoPath;
use asc_rpc::server::wire;
use asc_state::StateDb;
use asc_state::state::workspaces::{self, Workspace};
use asc_store::cachedir::CacheDir;
use asc_vfs::core::{CoreFs, VfsError};
use asc_vfs::policy::DEFAULT_DENY;
use asc_wc::switch::{self as plan, Dirty};
use asc_wc::vstatus::{DiskKind, DiskState, MAX_HASH_BYTES, PathState};

use super::cloud::prod::open_ctx;
use super::scheduler::parse_size;
use super::service::{HostContext, HostError, HostStats};
use crate::cli::common::{CliEngine, ObjectStack};
use crate::cli::restore::root_of;
use crate::refs_guard::ReadOnlyRefs;

/// Most files and directories one prefetch visits.
pub(super) const MAX_PREFETCH: usize = 5_000_000;

pub(super) fn failed(e: impl std::fmt::Display) -> HostError {
    HostError::Failed(e.to_string())
}

pub(super) fn vfs(e: VfsError) -> HostError {
    match e {
        VfsError::Conflicts => HostError::Precondition("the switch has conflicts".into()),
        other => failed(other),
    }
}

/// Runs a blocking provider or engine call off the async threads.
pub(super) async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> Result<T, HostError> + Send + 'static,
) -> Result<T, HostError> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| HostError::Failed(format!("the provider task failed: {e}")))?
}

pub(super) fn tree_id(raw: &[u8]) -> Result<TreeId, HostError> {
    <[u8; 32]>::try_from(raw)
        .map(TreeId::from_bytes)
        .map_err(|_| HostError::Invalid("a tree id is 32 bytes".into()))
}

pub(super) fn switch_mode(mode: wire::SwitchMode) -> plan::Mode {
    match mode {
        wire::SwitchMode::Safe => plan::Mode::Safe,
        wire::SwitchMode::Merge => plan::Mode::Merge,
        wire::SwitchMode::Force => plan::Mode::Force,
    }
}

/// Everything a host pulls out of the opened workspace; the context itself does not leave it.
pub(super) struct Opened {
    pub objects: Arc<ObjectStack>,
    pub engine: Arc<CliEngine>,
    pub root: TreeId,
    pub local: Option<CacheDir>,
    pub deny: String,
    /// `asc.vfs.hydrationBudget`, when set.
    pub budget: Option<u64>,
    /// `asc.vfs.backend`, unparsed: each platform knows its own names.
    pub backend: Option<String>,
    pub locked: Arc<dyn Fn(&RepoPath) -> bool + Send + Sync>,
}

pub(super) async fn open(cx: &HostContext) -> Result<Opened, HostError> {
    let mount = PathBuf::from(&cx.ws.ws.mount);
    let ctx = open_ctx(&cx.home, mount).await.map_err(HostError::Failed)?;
    let ws = ctx.ws().map_err(failed)?;
    if ws.workspace.ws.id != cx.ws.ws.id {
        return Err(HostError::Precondition(
            "the directory belongs to another workspace".into(),
        ));
    }
    let head = ws.head_commit().await.map_err(failed)?;
    let root = root_of(ws, head).await.map_err(failed)?;
    let expand = |p: &str| match (p.strip_prefix("~/"), std::env::home_dir()) {
        (Some(rest), Some(h)) => h.join(rest),
        _ => PathBuf::from(p),
    };
    let cache_root = ctx
        .config
        .get("asc.cache.dir")
        .map(|p| expand(&p))
        .unwrap_or_else(|| cx.home.cache());
    let local = CacheDir::open(cache_root, cx.home.tmp()).ok();
    let refs = Arc::new(ReadOnlyRefs(cx.db.refs()));
    let engine = Arc::new(CliEngine::new(ws.objects.clone(), refs, ws.repo.key));
    let check = crate::cli::path_lock::live_check(ws);
    let locked: Arc<dyn Fn(&RepoPath) -> bool + Send + Sync> =
        Arc::new(move |p: &RepoPath| check(&String::from_utf8_lossy(p.as_bytes())));
    Ok(Opened {
        objects: ws.objects.clone(),
        engine,
        root,
        local,
        deny: ctx
            .config
            .get("asc.vfs.denyHydration")
            .unwrap_or_else(|| DEFAULT_DENY.to_owned()),
        budget: ctx
            .config
            .get("asc.vfs.hydrationBudget")
            .and_then(|t| parse_size(&t)),
        backend: ctx.config.get("asc.vfs.backend"),
        locked,
    })
}

/// The workspace's row.
pub(super) async fn row(db: &StateDb, ws: asc_state::WorkspaceId) -> Result<Workspace, HostError> {
    workspaces::get(db, ws)
        .await
        .map_err(failed)?
        .ok_or_else(|| HostError::Precondition("the workspace is gone".into()))
}

/// Whether the file at `path` differs from the outgoing tree's entry, from the disk alone.
pub(super) fn locally_modified(
    disk: &dyn DiskState,
    path: &RepoPath,
    entry: &Entry,
) -> std::io::Result<bool> {
    match disk.state(path)? {
        PathState::Tombstone => Ok(true),
        PathState::Full => {
            let Some(stat) = disk.stat(path)? else {
                return Ok(true);
            };
            if !matches!(stat.kind, DiskKind::File { .. } | DiskKind::Symlink) {
                return Ok(true);
            }
            if stat.size != entry.size && !matches!(stat.kind, DiskKind::Symlink) {
                return Ok(true);
            }
            let bytes = disk.read(path, MAX_HASH_BYTES)?;
            Ok(blob_oid(&bytes) != entry.git)
        }
        _ => Ok(false),
    }
}

/// The local state that can collide with a switch from `from`, for the paths the switch
/// touches and, for a forced switch, the journal's hints; also the hinted paths found modified.
pub(super) async fn dirty(
    engine: &CliEngine,
    disk: &(dyn DiskState + Sync),
    from: TreeId,
    touched: &[RepoPath],
    hinted: &[RepoPath],
) -> Result<(Dirty, BTreeSet<RepoPath>), HostError> {
    let mut dirty = Dirty::default();
    let mut hinted_modified = BTreeSet::new();
    let mut seen_dirs: HashSet<RepoPath> = HashSet::new();
    for path in touched.iter().chain(hinted) {
        let tracked = engine.lookup(from, path).await.map_err(failed)?;
        match tracked {
            Some(entry) if entry.kind != Kind::Dir && entry.kind != Kind::Gitlink => {
                if locally_modified(disk, path, &entry).map_err(failed)? {
                    dirty.modified.insert(path.clone());
                    if hinted.contains(path) {
                        hinted_modified.insert(path.clone());
                    }
                }
            }
            Some(_) => {}
            None => {
                // An untracked file at the path, or at an ancestor that is a file.
                let mut cur = Some(path.clone());
                while let Some(p) = cur {
                    if !seen_dirs.insert(p.clone()) {
                        break;
                    }
                    if let Some(stat) = disk.stat(&p).map_err(failed)?
                        && matches!(stat.kind, DiskKind::File { .. } | DiskKind::Symlink)
                        && engine.lookup(from, &p).await.map_err(failed)?.is_none()
                    {
                        dirty.untracked.insert(p.clone());
                    }
                    cur = p.parent();
                }
            }
        }
    }
    Ok((dirty, hinted_modified))
}

/// Hydration statistics of `core`.
pub(super) fn stats(core: &CoreFs) -> HostStats {
    let s = core.stats().snapshot();
    let name = |p: &RepoPath| String::from_utf8_lossy(p.as_bytes()).into_owned();
    HostStats {
        by_path: s
            .paths
            .iter()
            .map(|(p, v)| (name(p), v.hydrations, v.bytes))
            .collect(),
        by_process: s
            .processes
            .iter()
            .map(|(k, v)| (k.clone(), v.hydrations, v.bytes))
            .collect(),
        bytes_hydrated: s.processes.iter().map(|(_, v)| v.bytes).sum(),
    }
}

/// Fetches the content of `paths` (the whole tree when empty) into the machine cache; answers
/// `(files, bytes)`.
pub(super) async fn prefetch(core: &CoreFs, paths: &[RepoPath]) -> Result<(u64, u64), HostError> {
    let (mut files, mut bytes, mut visited) = (0u64, 0u64, 0usize);
    // `None` is the root.
    let mut work: Vec<Option<RepoPath>> = if paths.is_empty() {
        vec![None]
    } else {
        paths.iter().cloned().map(Some).collect()
    };
    while let Some(path) = work.pop() {
        visited += 1;
        if visited > MAX_PREFETCH {
            return Err(HostError::Invalid(
                "the prefetch names too many files".into(),
            ));
        }
        let node = match &path {
            Some(p) => match core.resolve(p).await.map_err(vfs)? {
                Some(n) => n,
                None => continue,
            },
            None => {
                let listing = core.read_dir(None).await.map_err(vfs)?;
                work.extend(
                    listing
                        .into_iter()
                        .flat_map(|l| l.entries)
                        .map(|n| Some(n.path)),
                );
                continue;
            }
        };
        if node.is_dir() {
            if let Some(listing) = core.read_dir(path.as_ref()).await.map_err(vfs)? {
                work.extend(listing.entries.into_iter().map(|n| Some(n.path)));
            }
        } else if let Some(c) = node.content() {
            let mut stream = core.read_content(c).await.map_err(vfs)?;
            while stream.next().await.map_err(vfs)?.is_some() {}
            files += 1;
            bytes += c.size;
        }
    }
    Ok((files, bytes))
}
