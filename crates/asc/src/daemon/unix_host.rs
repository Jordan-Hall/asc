//! The Unix backends of the daemon (SPEC §10.3, §10.4): hosts a virtual workspace with
//! [`asc_vfs::fuse::FuseProvider`] (Linux) or [`asc_vfs::nfs::NfsProvider`] (macOS, or Linux when
//! asked for) over one merged file system, and answers the daemon's `Hosted` calls with it.
//!
//! * [`available`] says whether a backend can serve here: FUSE needs `/dev/fuse`, NFS needs the
//!   system's NFS mount command.
//! * [`UnixBackend::host`] opens the workspace, picks the backend (`asc.vfs.backend`: `auto`,
//!   `fuse` or `nfs`), builds the [`CoreFs`] over the HEAD tree with the overlay at
//!   `wc/<ws>/overlay`, and mounts it at the workspace root with a read-only `.git` gitfile.
//! * [`Hosted::switch`] plans the switch like the Windows host and then moves the base tree: the
//!   core serves the new tree, and the overlay entries of the switched paths are dropped. Every
//!   write goes through the daemon, so nothing is ever left pending.
//! * Clean files hold nothing locally (they are read from the shared content cache), so
//!   dehydration only drops what the kernel cached and frees no disk.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::SystemTime;

use asc_core::id::{FileId, TreeId};
use asc_core::object::{Change, Kind as ObjKind};
use asc_core::path::RepoPath;
use asc_rpc::server::wire;
use asc_state::StateDb;
use asc_store::objects::Objects;
use asc_vfs::core::{CoreFs, Options};
use asc_vfs::policy::HydrationPolicy;
use asc_vfs::unixfs::inodes::DbInodes;
use asc_vfs::unixfs::{self, DbTimes, UnixProvider};
use asc_wc::journal::Journal;
use asc_wc::redirect::Redirections;
use asc_wc::switch::{self as plan, Plan};
use asc_wc::times::{floor_seconds, record_switch};
use asc_wc::vstatus::{DirEntry, DiskStat, DiskState, PathState};
use async_trait::async_trait;
use tokio::runtime::Handle;

use super::service::{HostContext, HostError, HostStats, Hosted, VirtualBackend};
use super::vhost::{self, blocking, failed, open, tree_id, vfs};
use crate::cli::common::CliEngine;

/// A Unix virtual file system the daemon can serve a workspace with.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Fuse,
    Nfs,
}

impl Kind {
    pub fn name(self) -> &'static str {
        match self {
            Kind::Fuse => "fuse",
            Kind::Nfs => "nfs",
        }
    }
}

/// The backends this build can serve on this machine, preferred first: FUSE (Linux) passes
/// file data through the kernel and sees every open, so it leads; NFS needs no kernel module.
pub fn loadable() -> Vec<Kind> {
    #[allow(unused_mut)] // a build without either backend lists none
    let mut can = Vec::new();
    #[cfg(all(target_os = "linux", feature = "vfs-fuse"))]
    if asc_vfs::fuse::available() {
        can.push(Kind::Fuse);
    }
    #[cfg(feature = "vfs-nfs")]
    if asc_vfs::nfs::available() {
        can.push(Kind::Nfs);
    }
    can
}

/// Whether a Unix backend can serve a workspace here.
pub fn available() -> bool {
    !loadable().is_empty()
}

/// Picks the backend for `asc.vfs.backend` (`None` or `auto`: the first that loads).
pub fn choose(pref: Option<&str>, can: &[Kind]) -> Result<Kind, HostError> {
    let want = match pref.map(str::trim) {
        None | Some("" | "auto") => {
            return can.first().copied().ok_or_else(|| {
                HostError::Precondition(
                    "no virtual file system can be served here: FUSE needs /dev/fuse, NFS needs \
                     the system's NFS client"
                        .into(),
                )
            });
        }
        Some("fuse") => Kind::Fuse,
        Some("nfs") => Kind::Nfs,
        Some(other) => {
            return Err(HostError::Invalid(format!(
                "asc.vfs.backend is '{other}'; on this system it is auto, fuse or nfs"
            )));
        }
    };
    if can.contains(&want) {
        Ok(want)
    } else {
        Err(HostError::Precondition(match want {
            Kind::Fuse => "FUSE is not available: it needs Linux, /dev/fuse and a build with the \
                           vfs-fuse feature"
                .into(),
            Kind::Nfs => "NFS is not available: it needs the system's NFS client and a build with \
                          the vfs-nfs feature"
                .into(),
        }))
    }
}

/// The backend the daemon registers on Unix.
pub struct UnixBackend;

#[async_trait]
impl VirtualBackend for UnixBackend {
    async fn host(&self, cx: HostContext) -> Result<Arc<dyn Hosted>, HostError> {
        let rt = Handle::current();
        let inner_rt = rt.clone();
        tokio::task::spawn_blocking(move || rt.block_on(start(cx, inner_rt)))
            .await
            .map_err(|e| HostError::Failed(format!("the mount task failed: {e}")))?
    }
}

/// Opens the provider of `kind` over `cfg`; `Err` names why it cannot run here.
fn provider(kind: Kind, cfg: unixfs::Config) -> Result<Arc<dyn UnixProvider>, HostError> {
    let unavailable = |e: &dyn std::fmt::Display| HostError::Precondition(e.to_string());
    match kind {
        #[cfg(all(target_os = "linux", feature = "vfs-fuse"))]
        Kind::Fuse => asc_vfs::fuse::FuseProvider::new(asc_vfs::fuse::Config::over(cfg, true))
            .map(|p| Arc::new(p) as Arc<dyn UnixProvider>)
            .map_err(|e| unavailable(&e)),
        #[cfg(feature = "vfs-nfs")]
        Kind::Nfs => asc_vfs::nfs::NfsProvider::new(cfg)
            .map(|p| Arc::new(p) as Arc<dyn UnixProvider>)
            .map_err(|e| unavailable(&e)),
        #[allow(unreachable_patterns)] // a build with both backends covers every variant
        other => {
            drop(cfg);
            Err(unavailable(&format!(
                "this build of asc has no {} support",
                other.name()
            )))
        }
    }
}

async fn start(cx: HostContext, rt: Handle) -> Result<Arc<dyn Hosted>, HostError> {
    let root = PathBuf::from(&cx.ws.ws.mount);
    let opened = open(&cx).await?;
    let kind = choose(opened.backend.as_deref(), &loadable())?;
    let objects: Arc<dyn Objects> = opened.objects.clone();
    let redirections = Redirections::load(&root).unwrap_or_default();
    let core = Arc::new(CoreFs::new(
        objects,
        opened.root,
        Options {
            redirections: redirections.clone(),
            policy: HydrationPolicy::parse(&opened.deny),
            local: opened.local.clone(),
            locked: Some(opened.locked.clone()),
            ..Options::default()
        },
    ));
    let ws_dir = cx.home.workspace_dir(cx.ws.ws.id);
    let cfg = unixfs::Config {
        core: core.clone(),
        local: opened.local,
        inodes: Arc::new(DbInodes {
            db: cx.db.clone(),
            ws: cx.ws.ws.id,
            rt: rt.clone(),
        }),
        overlay: ws_dir.join("overlay"),
        redirect: cx.home.redirect_dir(cx.ws.ws.id),
        redirections,
        times: Arc::new(DbTimes {
            db: cx.db.clone(),
            ws: cx.ws.clone(),
            rt: rt.clone(),
        }),
        journal: cx.journal.clone(),
        rt: rt.clone(),
        root_time: floor_seconds(SystemTime::now()),
        gitfile: Some(cx.home.gitdir(cx.ws.ws.id)),
    };
    let provider = {
        let root = root.clone();
        blocking(move || {
            let p = provider(kind, cfg)?;
            p.mount(&root).map_err(vfs)?;
            Ok(p)
        })
        .await?
    };
    eprintln!(
        "asc daemon: virtual workspace {} served by {}",
        root.display(),
        provider.backend()
    );
    Ok(Arc::new(UnixHost {
        provider,
        core,
        engine: opened.engine,
        db: cx.db,
        ws: cx.ws.ws.id,
        journal: cx.journal,
    }))
}

/// One workspace served by a Unix backend.
struct UnixHost {
    provider: Arc<dyn UnixProvider>,
    core: Arc<CoreFs>,
    engine: Arc<CliEngine>,
    db: StateDb,
    ws: asc_state::WorkspaceId,
    journal: Arc<Journal>,
}

impl UnixHost {
    /// The paths the journal hints at, after a reconcile when the hints are not trustworthy.
    async fn hints(&self) -> Result<Vec<RepoPath>, HostError> {
        let mut answer = self.journal.since(None).await.map_err(failed)?;
        if answer.fresh {
            let fs = self.provider.merged().clone();
            blocking(move || fs.reconcile().map(drop).map_err(failed)).await?;
            answer = self.journal.since(None).await.map_err(failed)?;
        }
        Ok(answer
            .paths
            .iter()
            .filter_map(|e| RepoPath::new(e.path.as_slice()).ok())
            .collect())
    }

    /// Adds the forced paths a diff does not touch: those `to` has are restored, the rest go.
    async fn force_extra(
        &self,
        planned: &mut Plan,
        to: TreeId,
        extra: BTreeSet<RepoPath>,
    ) -> Result<(), HostError> {
        let listed: BTreeSet<RepoPath> = planned
            .updates
            .iter()
            .map(|(p, _)| p.clone())
            .chain(planned.deletes.iter().cloned())
            .collect();
        for p in extra.into_iter().filter(|p| !listed.contains(p)) {
            let change = match self.engine.lookup(to, &p).await.map_err(failed)? {
                Some(e) if matches!(e.kind, ObjKind::File | ObjKind::Exec | ObjKind::Symlink) => {
                    Change::Write {
                        kind: e.kind,
                        file: FileId::from_bytes(e.id),
                        size: e.size,
                        git: e.git,
                        copy_from: None,
                    }
                }
                Some(e) if e.kind == ObjKind::Gitlink => Change::Gitlink { commit: e.git },
                // Gone, or a directory now: dropping the local entry shows the new tree.
                _ => {
                    planned.deletes.push(p);
                    continue;
                }
            };
            planned.updates.push((p, change));
        }
        Ok(())
    }
}

/// [`DiskState`] of the running provider, shareable across the switch's awaits.
struct DiskView(Arc<dyn UnixProvider>);

impl DiskState for DiskView {
    fn state(&self, path: &RepoPath) -> std::io::Result<PathState> {
        self.0.disk_state().state(path)
    }
    fn stat(&self, path: &RepoPath) -> std::io::Result<Option<DiskStat>> {
        self.0.disk_state().stat(path)
    }
    fn read(&self, path: &RepoPath, max: u64) -> std::io::Result<Vec<u8>> {
        self.0.disk_state().read(path, max)
    }
    fn list(&self, dir: &RepoPath) -> std::io::Result<Vec<DirEntry>> {
        self.0.disk_state().list(dir)
    }
}

#[async_trait]
impl Hosted for UnixHost {
    fn stats(&self) -> HostStats {
        vhost::stats(&self.core)
    }

    async fn dehydrate(&self, paths: &[RepoPath]) -> Result<(u64, u64), HostError> {
        let provider = self.provider.clone();
        let paths = if paths.is_empty() {
            // Everything the kernel may cache: the paths the tree lists at the top, which the
            // merged view drops with their descendants.
            let listing = self.core.read_dir(None).await.map_err(vfs)?;
            listing
                .into_iter()
                .flat_map(|l| l.entries)
                .map(|n| n.path)
                .collect()
        } else {
            paths.to_vec()
        };
        blocking(move || {
            let done = provider.dehydrate(&paths).map_err(vfs)?;
            Ok((done.dehydrated as u64, 0))
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
        let disk = DiskView(self.provider.clone());
        let (dirty, hinted_modified) =
            vhost::dirty(&self.engine, &disk, from, &touched, &hinted).await?;
        let mut planned = plan::plan(&diff, &dirty, mode);
        if !planned.conflicts.is_empty() {
            return Ok(planned
                .conflicts
                .iter()
                .map(|p| p.as_bytes().to_vec())
                .collect());
        }
        // FORCE restores every locally modified path, even one the diff does not touch
        // (`reset --hard` to the same tree).
        if mode == plan::Mode::Force {
            let mut extra: BTreeSet<RepoPath> = hinted_modified;
            extra.extend(dirty.modified.iter().cloned());
            self.force_extra(&mut planned, to, extra).await?;
        }
        let changed: Vec<RepoPath> = planned
            .updates
            .iter()
            .map(|(p, _)| p.clone())
            .chain(planned.deletes.iter().cloned())
            .collect();
        let times = record_switch(
            &self.db,
            self.ws,
            &changed,
            floor_seconds(SystemTime::now()),
        )
        .await
        .map_err(failed)?;
        let (provider, core) = (self.provider.clone(), self.core.clone());
        blocking(move || {
            core.set_root(to);
            provider.apply_switch(&planned, times).map_err(|e| {
                // The overlay is as the new tree left it up to the failure; serving the old
                // tree again keeps every untouched path as it was.
                core.set_root(from);
                vfs(e)
            })
        })
        .await?;
        Ok(Vec::new())
    }

    async fn prefetch(&self, paths: &[RepoPath]) -> Result<(u64, u64), HostError> {
        vhost::prefetch(&self.core, paths).await
    }

    async fn reconcile(&self) -> Result<u64, HostError> {
        let fs = self.provider.merged().clone();
        blocking(move || fs.reconcile().map(|r| r.examined).map_err(failed)).await
    }

    async fn tombstones(&self) -> Result<Vec<Vec<u8>>, HostError> {
        let fs = self.provider.merged().clone();
        blocking(move || {
            let mut v: Vec<Vec<u8>> = fs
                .whiteouts()
                .map_err(failed)?
                .into_iter()
                .map(|p| p.as_bytes().to_vec())
                .collect();
            v.sort();
            Ok(v)
        })
        .await
    }

    async fn unmount(&self) -> Result<(), HostError> {
        let provider = self.provider.clone();
        blocking(move || provider.unmount().map_err(vfs)).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auto_takes_the_first_backend_that_loads() {
        assert_eq!(choose(None, &[Kind::Fuse, Kind::Nfs]).unwrap(), Kind::Fuse);
        assert_eq!(choose(Some("auto"), &[Kind::Nfs]).unwrap(), Kind::Nfs);
        assert!(matches!(choose(None, &[]), Err(HostError::Precondition(_))));
    }

    #[test]
    fn a_named_backend_must_load() {
        assert_eq!(
            choose(Some("nfs"), &[Kind::Fuse, Kind::Nfs]).unwrap(),
            Kind::Nfs
        );
        assert!(matches!(
            choose(Some("fuse"), &[Kind::Nfs]),
            Err(HostError::Precondition(m)) if m.contains("/dev/fuse")
        ));
    }

    #[test]
    fn windows_names_are_refused_here() {
        for v in ["projfs", "cloudfiles", "FUSE"] {
            assert!(matches!(
                choose(Some(v), &[Kind::Fuse]),
                Err(HostError::Invalid(m)) if m.contains("auto, fuse or nfs")
            ));
        }
    }
}
