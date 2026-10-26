//! The ProjFS backend of the daemon (U58; SPEC §10.2): hosts a virtual workspace with
//! [`asc_vfs::projfs::ProjfsProvider`] and answers the daemon's `Hosted` calls with it.
//!
//! * [`available`] says whether `ProjectedFSLib.dll` loads (Windows `Client-ProjFS` enabled).
//! * [`ProjfsBackend::host`] opens the workspace the way the daemon's other jobs do, builds the
//!   [`CoreFs`] over the HEAD tree, starts the provider at the workspace's mount root and replays
//!   a switch an earlier daemon left half done.
//! * [`Hosted::switch`] plans the switch from the tree diff and the on-disk state of the paths it
//!   touches (a path is modified when its file differs from the outgoing tree or a tombstone hides
//!   it, untracked when a file the outgoing tree lacks sits in the way), records the new epoch and
//!   hands the plan to the provider. Conflicts come back without touching the disk.
//!
//! The ProjFS and engine calls block, so each runs on a blocking thread of the daemon's runtime;
//! the command layer's futures are not `Send`, so opening the workspace does too.

use std::collections::{BTreeSet, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::SystemTime;

use asc_core::git::blob_oid;
use asc_core::id::TreeId;
use asc_core::object::Kind;
use asc_core::path::RepoPath;
use asc_rpc::server::wire;
use asc_state::StateDb;
use asc_state::state::workspaces::{self, Workspace};
use asc_store::cachedir::CacheDir;
use asc_store::objects::Objects;
use asc_vfs::core::{CoreFs, Options, Provider, VfsError};
use asc_vfs::policy::{DEFAULT_DENY, HydrationPolicy};
use asc_vfs::projfs::dehydrate::{DEFAULT_BUDGET, IDLE, Policy};
use asc_vfs::projfs::switch::{Request, Store};
use asc_vfs::projfs::{Config, DbTimes, ProjfsProvider, ffi};
use asc_wc::journal::Journal;
use asc_wc::redirect::Redirections;
use asc_wc::switch::{self as plan, Dirty};
use asc_wc::times::{floor_seconds, record_switch};
use asc_wc::vstatus::{DiskKind, DiskState, MAX_HASH_BYTES, PathState};
use async_trait::async_trait;
use tokio::runtime::Handle;

use super::cloud::prod::open_ctx;
use super::scheduler::parse_size;
use super::service::{HostContext, HostError, HostStats, Hosted, VirtualBackend};
use crate::cli::common::{CliEngine, ObjectStack};
use crate::cli::restore::root_of;
use crate::refs_guard::ReadOnlyRefs;

/// Most files and directories one prefetch visits.
const MAX_PREFETCH: usize = 5_000_000;

/// Whether the Windows Projected File System can be loaded on this machine.
pub fn available() -> bool {
    ffi::Api::load().is_ok()
}

fn failed(e: impl std::fmt::Display) -> HostError {
    HostError::Failed(e.to_string())
}

fn vfs(e: VfsError) -> HostError {
    match e {
        VfsError::Conflicts => HostError::Precondition("the switch has conflicts".into()),
        other => failed(other),
    }
}

/// Runs a blocking provider or engine call off the async threads.
async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> Result<T, HostError> + Send + 'static,
) -> Result<T, HostError> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| HostError::Failed(format!("the provider task failed: {e}")))?
}

/// The backend the daemon registers on Windows.
pub struct ProjfsBackend;

#[async_trait]
impl VirtualBackend for ProjfsBackend {
    async fn host(&self, cx: HostContext) -> Result<Arc<dyn Hosted>, HostError> {
        let rt = Handle::current();
        let inner_rt = rt.clone();
        tokio::task::spawn_blocking(move || rt.block_on(start(cx, inner_rt)))
            .await
            .map_err(|e| HostError::Failed(format!("the mount task failed: {e}")))?
    }
}

/// Everything `start` pulls out of the opened workspace; the context itself does not leave it.
struct Opened {
    objects: Arc<ObjectStack>,
    engine: Arc<CliEngine>,
    root: TreeId,
    local: Option<CacheDir>,
    deny: String,
    budget: u64,
}

async fn open(cx: &HostContext) -> Result<Opened, HostError> {
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
            .and_then(|t| parse_size(&t))
            .unwrap_or(DEFAULT_BUDGET),
    })
}

async fn start(cx: HostContext, rt: Handle) -> Result<Arc<dyn Hosted>, HostError> {
    let root = PathBuf::from(&cx.ws.ws.mount);
    let opened = open(&cx).await?;
    let objects: Arc<dyn Objects> = opened.objects.clone();
    let redirections = Redirections::load(&root).unwrap_or_default();
    let core = Arc::new(CoreFs::new(
        objects.clone(),
        opened.root,
        Options {
            redirections: redirections.clone(),
            policy: HydrationPolicy::parse(&opened.deny),
            local: opened.local.clone(),
            ..Options::windows()
        },
    ));
    let times = Arc::new(DbTimes {
        db: cx.db.clone(),
        ws: cx.ws.clone(),
        rt: rt.clone(),
    });
    let journal_sink: Arc<Journal> = cx.journal.clone();
    let provider = ProjfsProvider::new(Config {
        workspace: cx.ws.ws.id.0.into_bytes(),
        core: core.clone(),
        objects,
        local: opened.local,
        times,
        journal: journal_sink,
        redirections,
        rt: rt.clone(),
    })
    .map_err(|e| HostError::Precondition(e.to_string()))?;
    provider.set_store(Store {
        db: cx.db.clone(),
        ws: cx.ws.ws.id,
    });
    let provider = Arc::new(provider);
    {
        let provider = provider.clone();
        blocking(move || provider.mount(&root).map_err(vfs)).await?;
    }
    let host = ProjfsHost {
        provider,
        core,
        engine: opened.engine,
        db: cx.db,
        ws: cx.ws.ws.id,
        journal: cx.journal,
        budget: opened.budget,
        pending: AtomicU64::new(0),
    };
    // A switch an earlier daemon did not finish completes now; if it cannot, the periodic retry
    // keeps at it.
    if host.retry_pending().await.is_err() {
        host.pending.store(1, Ordering::SeqCst);
    }
    Ok(Arc::new(host))
}

/// One workspace served by ProjFS.
struct ProjfsHost {
    provider: Arc<ProjfsProvider>,
    core: Arc<CoreFs>,
    engine: Arc<CliEngine>,
    db: StateDb,
    ws: asc_state::WorkspaceId,
    journal: Arc<Journal>,
    budget: u64,
    pending: AtomicU64,
}

fn tree_id(raw: &[u8]) -> Result<TreeId, HostError> {
    <[u8; 32]>::try_from(raw)
        .map(TreeId::from_bytes)
        .map_err(|_| HostError::Invalid("a tree id is 32 bytes".into()))
}

impl ProjfsHost {
    async fn row(&self) -> Result<Workspace, HostError> {
        workspaces::get(&self.db, self.ws)
            .await
            .map_err(failed)?
            .ok_or_else(|| HostError::Precondition("the workspace is gone".into()))
    }

    /// Whether the file at `path` differs from the outgoing tree's entry, from the disk alone.
    fn locally_modified(
        disk: &dyn DiskState,
        path: &RepoPath,
        entry: &asc_core::object::Entry,
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

    /// The local state that can collide with a switch from `from`, for the paths in `candidates`
    /// (the paths the switch touches and, for a forced switch, the journal's hints).
    async fn dirty(
        &self,
        from: TreeId,
        touched: &[RepoPath],
        hinted: &[RepoPath],
    ) -> Result<(Dirty, BTreeSet<RepoPath>), HostError> {
        let mut dirty = Dirty::default();
        let mut hinted_modified = BTreeSet::new();
        let disk = self.provider_disk();
        let mut seen_dirs: HashSet<RepoPath> = HashSet::new();
        for path in touched.iter().chain(hinted) {
            let tracked = self.engine.lookup(from, path).await.map_err(failed)?;
            match tracked {
                Some(entry) if entry.kind != Kind::Dir && entry.kind != Kind::Gitlink => {
                    let modified =
                        Self::locally_modified(disk.as_ref(), path, &entry).map_err(failed)?;
                    if modified {
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
                            && self
                                .engine
                                .lookup(from, &p)
                                .await
                                .map_err(failed)?
                                .is_none()
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

    fn provider_disk(&self) -> Arc<DiskView> {
        Arc::new(DiskView(self.provider.clone()))
    }

    /// The paths the journal hints at, after a reconcile when the hints are not trustworthy.
    async fn hints(&self) -> Result<Vec<RepoPath>, HostError> {
        let mut answer = self.journal.since(None).await.map_err(failed)?;
        if answer.fresh {
            let provider = self.provider.clone();
            blocking(move || provider.reconcile().map(drop).map_err(vfs)).await?;
            answer = self.journal.since(None).await.map_err(failed)?;
        }
        Ok(answer
            .paths
            .iter()
            .filter_map(|e| RepoPath::new(e.path.as_slice()).ok())
            .collect())
    }
}

/// [`DiskState`] of the running provider, shareable with blocking threads.
struct DiskView(Arc<ProjfsProvider>);

impl DiskState for DiskView {
    fn state(&self, path: &RepoPath) -> std::io::Result<PathState> {
        self.0.disk_state().state(path)
    }
    fn stat(&self, path: &RepoPath) -> std::io::Result<Option<asc_wc::vstatus::DiskStat>> {
        self.0.disk_state().stat(path)
    }
    fn read(&self, path: &RepoPath, max: u64) -> std::io::Result<Vec<u8>> {
        self.0.disk_state().read(path, max)
    }
    fn list(&self, dir: &RepoPath) -> std::io::Result<Vec<asc_wc::vstatus::DirEntry>> {
        self.0.disk_state().list(dir)
    }
}

#[async_trait]
impl Hosted for ProjfsHost {
    fn stats(&self) -> HostStats {
        let s = self.core.stats().snapshot();
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

    fn pending(&self) -> u64 {
        self.pending.load(Ordering::SeqCst)
    }

    async fn dehydrate(&self, paths: &[RepoPath]) -> Result<(u64, u64), HostError> {
        let provider = self.provider.clone();
        let paths = paths.to_vec();
        blocking(move || {
            let before = provider.hydrated_bytes().map_err(vfs)?;
            let done = provider.dehydrate(&paths).map_err(vfs)?;
            let after = provider.hydrated_bytes().map_err(vfs)?;
            Ok((done.dehydrated as u64, before.saturating_sub(after)))
        })
        .await
    }

    async fn switch(
        &self,
        from: &[u8],
        to: &[u8],
        mode: wire::SwitchMode,
    ) -> Result<Vec<Vec<u8>>, HostError> {
        let (from, to) = (tree_id(from)?, tree_id(to)?);
        let mode = match mode {
            wire::SwitchMode::Safe => plan::Mode::Safe,
            wire::SwitchMode::Merge => plan::Mode::Merge,
            wire::SwitchMode::Force => plan::Mode::Force,
        };
        let diff = self.engine.leafdiff(from, to, None).await.map_err(failed)?;
        let touched: Vec<RepoPath> = diff.iter().map(|(p, _)| p.clone()).collect();
        let hinted = if mode == plan::Mode::Force {
            self.hints().await?
        } else {
            Vec::new()
        };
        let (dirty, hinted_modified) = self.dirty(from, &touched, &hinted).await?;
        let planned = plan::plan(&diff, &dirty, mode);
        if !planned.conflicts.is_empty() {
            return Ok(planned
                .conflicts
                .iter()
                .map(|p| p.as_bytes().to_vec())
                .collect());
        }
        // FORCE restores every locally modified path the target tree has, even one the diff does
        // not touch (`reset --hard` to the same tree).
        let mut extra: BTreeSet<RepoPath> = BTreeSet::new();
        if mode == plan::Mode::Force {
            extra.extend(hinted_modified);
            extra.extend(dirty.modified.iter().cloned());
        }
        let row = self.row().await?;
        let changed: Vec<RepoPath> = planned
            .updates
            .iter()
            .map(|(p, _)| p.clone())
            .chain(planned.deletes.iter().cloned())
            .chain(extra.iter().cloned())
            .collect();
        let times = record_switch(
            &self.db,
            self.ws,
            &changed,
            floor_seconds(SystemTime::now()),
        )
        .await
        .map_err(failed)?;
        let provider = self.provider.clone();
        let to_head = row.ws.head.clone();
        let report = blocking(move || {
            provider
                .switch(
                    &Request {
                        mode,
                        plan: &planned,
                        extra: &extra,
                        from_root: from,
                        to_root: to,
                        to_head: &to_head,
                    },
                    times,
                )
                .map_err(vfs)
        })
        .await?;
        self.pending
            .store(report.pending.len() as u64, Ordering::SeqCst);
        Ok(Vec::new())
    }

    async fn prefetch(&self, paths: &[RepoPath]) -> Result<(u64, u64), HostError> {
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
                Some(p) => match self.core.resolve(p).await.map_err(vfs)? {
                    Some(n) => n,
                    None => continue,
                },
                None => {
                    let listing = self.core.read_dir(None).await.map_err(vfs)?;
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
                if let Some(listing) = self.core.read_dir(path.as_ref()).await.map_err(vfs)? {
                    work.extend(listing.entries.into_iter().map(|n| Some(n.path)));
                }
            } else if let Some(c) = node.content() {
                let mut stream = self.core.read_content(c).await.map_err(vfs)?;
                while stream.next().await.map_err(vfs)?.is_some() {}
                files += 1;
                bytes += c.size;
            }
        }
        Ok((files, bytes))
    }

    async fn retry_pending(&self) -> Result<(), HostError> {
        let row = self.row().await?;
        let provider = self.provider.clone();
        let replayed = blocking(move || provider.resume(row.epoch_time).map_err(vfs)).await?;
        self.pending
            .store(replayed.map_or(0, |r| r.pending as u64), Ordering::SeqCst);
        Ok(())
    }

    async fn reconcile(&self) -> Result<u64, HostError> {
        let provider = self.provider.clone();
        blocking(move || provider.reconcile().map(|r| r.examined).map_err(vfs)).await
    }

    async fn dehydrate_idle(&self) -> Result<(u64, u64), HostError> {
        let provider = self.provider.clone();
        let policy = Policy {
            idle: IDLE,
            budget: self.budget,
        };
        blocking(move || {
            provider
                .dehydrate_idle(&policy, SystemTime::now())
                .map(|r| (r.files, r.bytes))
                .map_err(vfs)
        })
        .await
    }

    async fn unmount(&self) -> Result<(), HostError> {
        let provider = self.provider.clone();
        blocking(move || provider.unmount().map_err(vfs)).await
    }
}
