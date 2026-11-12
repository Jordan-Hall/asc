//! The Windows backends of the daemon (U58; SPEC §10.2, §10.2b): hosts a virtual workspace with
//! [`asc_vfs::projfs::ProjfsProvider`] or [`asc_vfs::cloudfiles::CloudFilesProvider`] and answers
//! the daemon's `Hosted` calls with it.
//!
//! * [`available`] says whether either backend loads: ProjFS (`ProjectedFSLib.dll`, the
//!   `Client-ProjFS` feature) or Cloud Files (`cldapi.dll`, Windows 10 1709 and later).
//! * [`WinBackend::host`] opens the workspace the way the daemon's other jobs do, picks the
//!   backend (`asc.vfs.backend`; a root keeps the backend that virtualized it), builds the
//!   [`CoreFs`] over the HEAD tree, starts the provider at the workspace's mount root and replays
//!   a switch an earlier daemon left half done.
//! * [`Hosted::switch`] plans the switch from the tree diff and the on-disk state of the paths it
//!   touches (a path is modified when its file differs from the outgoing tree or a tombstone hides
//!   it, untracked when a file the outgoing tree lacks sits in the way), records the new epoch and
//!   hands the plan to the provider. Conflicts come back without touching the disk.
//!
//! The provider and engine calls block, so each runs on a blocking thread of the daemon's runtime;
//! the command layer's futures are not `Send`, so opening the workspace does too.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::SystemTime;

use asc_core::path::RepoPath;
use asc_rpc::server::wire;
use asc_state::StateDb;
use asc_store::objects::Objects;
use asc_vfs::core::{CoreFs, Options};
use asc_vfs::policy::HydrationPolicy;
use asc_vfs::win::engine::{Request, Store};
use asc_vfs::win::host::WinProvider;
use asc_vfs::win::idle::{DEFAULT_BUDGET, IDLE, Policy};
use asc_vfs::win::select::{self, Backend, Preference};
use asc_vfs::win::{Config, DbTimes};
use asc_wc::journal::Journal;
use asc_wc::redirect::Redirections;
use asc_wc::switch as plan;
use asc_wc::times::{floor_seconds, record_switch};
use asc_wc::vstatus::{DiskState, PathState};
use async_trait::async_trait;
use tokio::runtime::Handle;

use super::service::{HostContext, HostError, HostStats, Hosted, VirtualBackend};
use super::vhost::{self, blocking, failed, open, tree_id, vfs};
use crate::cli::common::CliEngine;

/// Whether a Windows backend can be loaded on this machine.
pub fn available() -> bool {
    let can = select::loadable();
    can.projfs || can.cloudfiles
}

/// The backend the daemon registers on Windows.
pub struct WinBackend;

#[async_trait]
impl VirtualBackend for WinBackend {
    async fn host(&self, cx: HostContext) -> Result<Arc<dyn Hosted>, HostError> {
        let rt = Handle::current();
        let inner_rt = rt.clone();
        tokio::task::spawn_blocking(move || rt.block_on(start(cx, inner_rt)))
            .await
            .map_err(|e| HostError::Failed(format!("the mount task failed: {e}")))?
    }
}

/// Starts the provider of `backend`; `Err` names why it cannot run here.
fn provider(backend: Backend, cfg: Config) -> Result<Arc<dyn WinProvider>, HostError> {
    let unavailable = |e: &dyn std::fmt::Display| HostError::Precondition(e.to_string());
    match backend {
        #[cfg(feature = "vfs-projfs")]
        Backend::ProjFs => asc_vfs::projfs::ProjfsProvider::new(cfg)
            .map(|p| Arc::new(p) as Arc<dyn WinProvider>)
            .map_err(|e| unavailable(&e)),
        #[cfg(feature = "vfs-cloudfiles")]
        Backend::CloudFiles => asc_vfs::cloudfiles::CloudFilesProvider::new(cfg)
            .map(|p| Arc::new(p) as Arc<dyn WinProvider>)
            .map_err(|e| unavailable(&e)),
        #[allow(unreachable_patterns)] // a build with both backends covers every variant
        other => {
            drop(cfg);
            Err(unavailable(&format!(
                "this build of asc has no {other} support"
            )))
        }
    }
}

async fn start(cx: HostContext, rt: Handle) -> Result<Arc<dyn Hosted>, HostError> {
    let root = PathBuf::from(&cx.ws.ws.mount);
    let opened = open(&cx).await?;
    let backend = {
        let root = root.clone();
        let pref: Preference = opened
            .backend
            .as_deref()
            .map(str::parse)
            .transpose()
            .map_err(HostError::Invalid)?
            .unwrap_or_default();
        blocking(move || {
            select::choose(pref, select::found(&root), select::loadable())
                .map_err(HostError::Precondition)
        })
        .await?
    };
    let objects: Arc<dyn Objects> = opened.objects.clone();
    let redirections = Redirections::load(&root).unwrap_or_default();
    let core = Arc::new(CoreFs::new(
        objects.clone(),
        opened.root,
        Options {
            redirections: redirections.clone(),
            policy: HydrationPolicy::parse(&opened.deny),
            local: opened.local.clone(),
            locked: Some(opened.locked.clone()),
            ..Options::windows()
        },
    ));
    let times = Arc::new(DbTimes {
        db: cx.db.clone(),
        ws: cx.ws.clone(),
        rt: rt.clone(),
    });
    let journal_sink: Arc<Journal> = cx.journal.clone();
    let provider = provider(
        backend,
        Config {
            workspace: cx.ws.ws.id.0.into_bytes(),
            core: core.clone(),
            objects,
            local: opened.local,
            times,
            journal: journal_sink,
            redirections,
            rt: rt.clone(),
        },
    )?;
    provider.set_store(Store {
        db: cx.db.clone(),
        ws: cx.ws.ws.id,
    });
    eprintln!(
        "asc daemon: virtual workspace {} served by {}",
        root.display(),
        provider.backend()
    );
    {
        let provider = provider.clone();
        blocking(move || provider.mount(&root).map_err(vfs)).await?;
    }
    let host = WinHost {
        provider,
        core,
        engine: opened.engine,
        db: cx.db,
        ws: cx.ws.ws.id,
        journal: cx.journal,
        budget: opened.budget.unwrap_or(DEFAULT_BUDGET),
        pending: AtomicU64::new(0),
    };
    // A switch an earlier daemon did not finish completes now; if it cannot, the periodic retry
    // keeps at it.
    if host.retry_pending().await.is_err() {
        host.pending.store(1, Ordering::SeqCst);
    }
    Ok(Arc::new(host))
}

/// One workspace served by a Windows backend.
struct WinHost {
    provider: Arc<dyn WinProvider>,
    core: Arc<CoreFs>,
    engine: Arc<CliEngine>,
    db: StateDb,
    ws: asc_state::WorkspaceId,
    journal: Arc<Journal>,
    budget: u64,
    pending: AtomicU64,
}

impl WinHost {
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
struct DiskView(Arc<dyn WinProvider>);

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
impl Hosted for WinHost {
    fn stats(&self) -> HostStats {
        vhost::stats(&self.core)
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
        let mode = vhost::switch_mode(mode);
        let diff = self.engine.leafdiff(from, to, None).await.map_err(failed)?;
        let touched: Vec<RepoPath> = diff.iter().map(|(p, _)| p.clone()).collect();
        let hinted = if mode == plan::Mode::Force {
            self.hints().await?
        } else {
            Vec::new()
        };
        let disk = self.provider_disk();
        let (dirty, hinted_modified) =
            vhost::dirty(&self.engine, disk.as_ref(), from, &touched, &hinted).await?;
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
        let row = vhost::row(&self.db, self.ws).await?;
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
        vhost::prefetch(&self.core, paths).await
    }

    async fn retry_pending(&self) -> Result<(), HostError> {
        let row = vhost::row(&self.db, self.ws).await?;
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

    async fn tombstones(&self) -> Result<Vec<Vec<u8>>, HostError> {
        let provider = self.provider.clone();
        blocking(move || {
            provider
                .tombstones()
                .map(|v| v.into_iter().map(|p| p.as_bytes().to_vec()).collect())
                .map_err(vfs)
        })
        .await
    }

    async fn unmount(&self) -> Result<(), HostError> {
        let provider = self.provider.clone();
        blocking(move || provider.unmount().map_err(vfs)).await
    }
}
