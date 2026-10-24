//! The git view (SPEC §25.2, §25.3): a bare repository per repo, derived from asc's refs by the
//! deterministic export (U34) and served by real git through [`ServerGit`].
//!
//! The view is a cache, never the source of truth. Losing it loses nothing: [`GitView::rebuild`]
//! derives it again. It holds the shared refs and tags only (`refs/heads/*`, `refs/tags/*`), so
//! one view serves every principal. Its `config` is rewritten from a fixed template before any
//! child starts when it differs, and a mirror-mode repo (SPEC §8.6) is served from its bare
//! mirror, read-only, after its `config` passes an allow-list.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU8, AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use async_trait::async_trait;
use asc_core::id::GitOid;
use asc_engine::Engine;
use asc_store::objects::Objects;
use asc_store::refs::Refs;
use futures::lock::Mutex;

use super::limits::INLINE_BUILD_OBJECTS;
use super::pktline::GitProtocol;
use super::server_git::{
    PackRequest, PackService, QuarantineStats, RefUpdate, ServerChild, ServerGit, ServerGitError,
    check_repo_config, private_dir, valid_ref_name,
};
use crate::export::{ExportError, ExportReport, ExportRoot, Exporter};
use crate::odb::{Odb, OdbError};
use crate::realgit::RealGit;

/// Refs one sync may set or delete.
pub const MAX_SYNC_REFS: usize = 200_000;
/// Largest `config` read from a view or a mirror; a larger file is refused.
pub const MAX_VIEW_CONFIG_BYTES: u64 = 1024 * 1024;
/// `Retry-After` of the `503` answered while a view builds.
pub const RETRY_AFTER_BUILDING: Duration = Duration::from_secs(5);
/// The marker file of a view whose first full sync finished.
const READY_MARKER: &str = "asc-view-ready";
/// The shared-ref namespaces a view holds.
const VIEW_NAMESPACES: [&str; 2] = ["refs/heads/", "refs/tags/"];
/// A view directory is `<repo id>.git`.
const VIEW_SUFFIX: &str = ".git";

/// The `config` of every derived view; anything else found there is overwritten.
pub const VIEW_CONFIG_TEMPLATE: &str = "[core]\n\trepositoryformatversion = 0\n\tfilemode = false\n\tbare = true\n\tlogallrefupdates = false\n";

/// Why a view operation failed.
#[derive(Debug, thiserror::Error)]
pub enum ViewError {
    #[error(transparent)]
    Git(#[from] ServerGitError),
    #[error(transparent)]
    Export(#[from] ExportError),
    #[error(transparent)]
    Odb(#[from] OdbError),
    #[error("reading the repo's refs: {0}")]
    Source(SourceError),
    #[error("{what}: {source}")]
    Io {
        what: &'static str,
        source: io::Error,
    },
    #[error("view directory {0} must be absolute")]
    NotAbsolute(PathBuf),
    #[error("repo id {0} is negative")]
    BadRepoId(i64),
    #[error("{} is a symbolic link or not a directory", .0.display())]
    NotADirectory(PathBuf),
    #[error("ref {0:?} is not a shared head or tag name")]
    BadRef(String),
    #[error("ref {0:?} is listed twice")]
    DuplicateRef(String),
    #[error("{0} refs exceed the limit of {MAX_SYNC_REFS}")]
    TooManyRefs(usize),
    #[error("ref {name} exports as {actual}, but asc's git map says {expected}")]
    TipMismatch {
        name: String,
        expected: GitOid,
        actual: GitOid,
    },
    #[error("{} is not a bare git repository", .0.display())]
    NotARepository(PathBuf),
    #[error("this view is a read-only mirror")]
    ReadOnlyMirror,
    #[error("that build ticket belongs to another view")]
    ForeignTicket,
}

fn io_err(what: &'static str) -> impl FnOnce(io::Error) -> ViewError {
    move |source| ViewError::Io { what, source }
}

/// The error of a [`RefSource`].
pub type SourceError = Box<dyn std::error::Error + Send + Sync>;

/// One ref of the repo's shared set (SPEC §21.5.1 policy), as the view should serve it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceRef {
    /// `refs/heads/<name>` or `refs/tags/<name>`.
    pub name: String,
    /// What to export for it.
    pub target: ExportRoot,
    /// The git id asc's `git_map` records for the target. The export must produce exactly this.
    pub git: GitOid,
}

/// asc's side of a sync: the refs to serve. Implemented by `asc-serve` over `ListRefs` and
/// `git_map`.
#[async_trait]
pub trait RefSource: Send + Sync {
    /// The shared refs and tags.
    async fn refs(&self) -> Result<Vec<SourceRef>, SourceError>;

    /// The default branch as `refs/heads/<name>`, for the view's `HEAD`.
    async fn default_branch(&self) -> Result<Option<String>, SourceError> {
        Ok(None)
    }

    /// An estimate of the objects a first build must export. `None` means unknown, which builds
    /// in the background.
    async fn estimated_objects(&self) -> Result<Option<u64>, SourceError> {
        Ok(None)
    }
}

/// What a sync did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SyncReport {
    /// Every ref and `HEAD` already matched; nothing was exported or changed.
    pub noop: bool,
    pub refs_set: usize,
    pub refs_deleted: usize,
    pub head_changed: bool,
    /// Objects the export wrote and skipped.
    pub written: u64,
    pub skipped: u64,
}

/// Where a view's data comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ViewKind {
    /// Built by the export.
    Derived,
    /// The repo's bare mirror, served read-only without any export.
    Mirror,
}

/// The build state machine of a derived view.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ViewState {
    Absent,
    Building,
    Ready,
}

const ABSENT: u8 = 0;
const BUILDING: u8 = 1;
const READY: u8 = 2;

fn state_of(code: u8) -> ViewState {
    match code {
        BUILDING => ViewState::Building,
        READY => ViewState::Ready,
        _ => ViewState::Absent,
    }
}

/// The per-replica cap on view builds (`view_builds_per_replica`), shared by every view.
#[derive(Debug)]
pub struct BuildSlots {
    max: usize,
    used: AtomicUsize,
}

impl BuildSlots {
    pub fn new(max: u32) -> Arc<Self> {
        Arc::new(Self {
            max: max as usize,
            used: AtomicUsize::new(0),
        })
    }

    fn try_acquire(self: &Arc<Self>) -> Option<SlotGuard> {
        let mut cur = self.used.load(Ordering::SeqCst);
        loop {
            if cur >= self.max {
                return None;
            }
            match self
                .used
                .compare_exchange(cur, cur + 1, Ordering::SeqCst, Ordering::SeqCst)
            {
                Ok(_) => return Some(SlotGuard(self.clone())),
                Err(now) => cur = now,
            }
        }
    }

    /// Builds running now.
    pub fn in_use(&self) -> usize {
        self.used.load(Ordering::SeqCst)
    }
}

struct SlotGuard(Arc<BuildSlots>);

impl Drop for SlotGuard {
    fn drop(&mut self) {
        self.0.used.fetch_sub(1, Ordering::SeqCst);
    }
}

/// The right to build a view in a background job. Dropping it unused returns the view to
/// `Absent`.
pub struct BuildTicket {
    view: Arc<ViewInner>,
    _slot: Option<SlotGuard>,
    done: bool,
}

impl Drop for BuildTicket {
    fn drop(&mut self) {
        if !self.done {
            let _ = self.view.state.compare_exchange(
                BUILDING,
                ABSENT,
                Ordering::SeqCst,
                Ordering::SeqCst,
            );
        }
    }
}

/// The answer of [`GitView::ensure`].
pub enum Readiness {
    /// The view is built and now matches asc's refs.
    Ready(SyncReport),
    /// A build is running; answer `503` with `Retry-After`.
    Building { retry_after: Duration },
    /// The view must be built in the background: answer `503` with `Retry-After` and run
    /// [`GitView::build`] with this ticket in a job.
    StartBackgroundBuild {
        ticket: BuildTicket,
        retry_after: Duration,
    },
}

struct ViewInner {
    server: ServerGit,
    dir: PathBuf,
    kind: ViewKind,
    state: AtomicU8,
    sync_lock: Mutex<()>,
    slots: OnceLock<Arc<BuildSlots>>,
}

/// One repo's git view.
#[derive(Clone)]
pub struct GitView {
    inner: Arc<ViewInner>,
}

impl std::fmt::Debug for GitView {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GitView")
            .field("dir", &self.inner.dir)
            .field("kind", &self.inner.kind)
            .field("state", &self.state())
            .finish()
    }
}

/// `<view_dir>/<repo id>.git`: built from the number only, never from a name.
fn view_path(view_dir: &Path, repo_id: i64) -> Result<PathBuf, ViewError> {
    if !view_dir.is_absolute() {
        return Err(ViewError::NotAbsolute(view_dir.to_owned()));
    }
    if repo_id < 0 {
        return Err(ViewError::BadRepoId(repo_id));
    }
    Ok(view_dir.join(format!("{repo_id}{VIEW_SUFFIX}")))
}

fn check_plain_dir(path: &Path) -> Result<bool, ViewError> {
    match fs::symlink_metadata(path) {
        Ok(m) if m.file_type().is_dir() => Ok(true),
        Ok(_) => Err(ViewError::NotADirectory(path.to_owned())),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(io_err("inspecting the view directory")(e)),
    }
}

fn read_bounded(path: &Path) -> io::Result<Option<Vec<u8>>> {
    let f = match fs::File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    let mut buf = Vec::new();
    f.take(MAX_VIEW_CONFIG_BYTES + 1).read_to_end(&mut buf)?;
    Ok(Some(buf))
}

impl GitView {
    /// Opens (creating when missing) the derived view of repo `repo_id` under `view_dir`.
    pub fn open(server: &ServerGit, view_dir: &Path, repo_id: i64) -> Result<Self, ViewError> {
        let dir = view_path(view_dir, repo_id)?;
        private_dir(view_dir).map_err(io_err("creating the view directory"))?;
        if check_plain_dir(&dir)? && !dir.join("HEAD").is_file() {
            // A build that died before `git init` finished.
            fs::remove_dir_all(&dir).map_err(io_err("removing a half-made view"))?;
        }
        if !dir.exists() {
            private_dir(&dir).map_err(io_err("creating the view"))?;
            server.init_bare(&dir)?;
        }
        write_template_if_different(&dir)?;
        let ready = dir.join(READY_MARKER).is_file();
        Ok(Self::new_inner(
            server,
            dir,
            ViewKind::Derived,
            if ready { READY } else { ABSENT },
        ))
    }

    /// Uses the repo's bare mirror as its view (SPEC §25.2 "Reads", item 3): no export, no
    /// pushes. The mirror's `config` must pass the allow-list.
    pub fn open_mirror(server: &ServerGit, mirror_dir: &Path) -> Result<Self, ViewError> {
        if !mirror_dir.is_absolute() {
            return Err(ViewError::NotAbsolute(mirror_dir.to_owned()));
        }
        if !check_plain_dir(mirror_dir)?
            || !mirror_dir.join("HEAD").is_file()
            || !mirror_dir.join("objects").is_dir()
        {
            return Err(ViewError::NotARepository(mirror_dir.to_owned()));
        }
        check_repo_config(mirror_dir)?;
        Ok(Self::new_inner(
            server,
            mirror_dir.to_owned(),
            ViewKind::Mirror,
            READY,
        ))
    }

    fn new_inner(server: &ServerGit, dir: PathBuf, kind: ViewKind, state: u8) -> Self {
        Self {
            inner: Arc::new(ViewInner {
                server: server.clone(),
                dir,
                kind,
                state: AtomicU8::new(state),
                sync_lock: Mutex::new(()),
                slots: OnceLock::new(),
            }),
        }
    }

    /// Shares the per-replica build cap with this view. Set once, before the view is used.
    pub fn set_build_slots(&self, slots: Arc<BuildSlots>) {
        let _ = self.inner.slots.set(slots);
    }

    pub fn path(&self) -> &Path {
        &self.inner.dir
    }

    pub fn kind(&self) -> ViewKind {
        self.inner.kind
    }

    pub fn state(&self) -> ViewState {
        state_of(self.inner.state.load(Ordering::SeqCst))
    }

    /// An exporter that writes into this view.
    pub fn exporter<'a, O: Objects, R: Refs>(
        &self,
        engine: &'a Engine<O, R>,
        objects: &'a O,
        git: &'a RealGit,
    ) -> Exporter<'a, O, R> {
        Exporter::new(engine, objects, git, &self.inner.dir)
    }

    /// Re-checks the view's `config` (rewriting it from the template when it differs; refusing a
    /// mirror whose config is outside the allow-list).
    pub fn verify_config(&self) -> Result<(), ViewError> {
        match self.inner.kind {
            ViewKind::Derived => write_template_if_different(&self.inner.dir),
            ViewKind::Mirror => Ok(check_repo_config(&self.inner.dir)?),
        }
    }

    /// Starts `git upload-pack` or `git receive-pack` on the view, after [`GitView::verify_config`].
    /// A mirror refuses `receive-pack`.
    pub fn spawn_pack(
        &self,
        service: PackService,
        req: &PackRequest,
    ) -> Result<ServerChild, ViewError> {
        if self.inner.kind == ViewKind::Mirror && service == PackService::ReceivePack {
            return Err(ViewError::ReadOnlyMirror);
        }
        self.verify_config()?;
        Ok(self.inner.server.pack(service, &self.inner.dir, req)?)
    }

    /// [`GitView::spawn_pack`] with extra `ASC_*` environment variables for the child and the
    /// hook it starts ([`ServerGit::pack_with_env`]).
    pub fn spawn_pack_with_env(
        &self,
        service: PackService,
        req: &PackRequest,
        env: &[(&str, &str)],
    ) -> Result<ServerChild, ViewError> {
        if self.inner.kind == ViewKind::Mirror && service == PackService::ReceivePack {
            return Err(ViewError::ReadOnlyMirror);
        }
        self.verify_config()?;
        Ok(self
            .inner
            .server
            .pack_with_env(service, &self.inner.dir, req, env)?)
    }

    /// [`GitView::spawn_pack_with_env`] in session mode ([`ServerGit::session_with_env`]): the
    /// child holds one full-duplex stream open, as SSH needs.
    pub fn spawn_session_with_env(
        &self,
        service: PackService,
        protocol: Option<&GitProtocol>,
        env: &[(&str, &str)],
    ) -> Result<ServerChild, ViewError> {
        if self.inner.kind == ViewKind::Mirror && service == PackService::ReceivePack {
            return Err(ViewError::ReadOnlyMirror);
        }
        self.verify_config()?;
        Ok(self
            .inner
            .server
            .session_with_env(service, &self.inner.dir, protocol, env)?)
    }

    /// Counts and sizes the objects of a push's quarantine ([`ServerGit::quarantine_stats`]).
    pub fn quarantine_stats(&self, quarantine: &Path) -> Result<QuarantineStats, ViewError> {
        Ok(self
            .inner
            .server
            .quarantine_stats(&self.inner.dir, quarantine)?)
    }

    /// Brings the view in step with `source`: exports the missing closure of the shared refs and
    /// tags, sets the refs in one transaction and points `HEAD` at the default branch. A no-op
    /// (one `for-each-ref`) when every ref already matches. Concurrent syncs run one at a time;
    /// the later ones find nothing to do.
    ///
    /// `exporter` must come from [`GitView::exporter`]. The call blocks on short git children, so
    /// run it where a brief block is acceptable.
    pub async fn sync<O: Objects, R: Refs>(
        &self,
        source: &dyn RefSource,
        exporter: &Exporter<'_, O, R>,
    ) -> Result<SyncReport, ViewError> {
        if self.inner.kind == ViewKind::Mirror {
            return Ok(SyncReport {
                noop: true,
                ..SyncReport::default()
            });
        }
        let _guard = self.inner.sync_lock.lock().await;
        let report = self.sync_locked(source, exporter).await?;
        self.mark_ready();
        Ok(report)
    }

    fn mark_ready(&self) {
        if fs::write(self.inner.dir.join(READY_MARKER), b"").is_ok() {
            self.inner.state.store(READY, Ordering::SeqCst);
        }
    }

    async fn sync_locked<O: Objects, R: Refs>(
        &self,
        source: &dyn RefSource,
        exporter: &Exporter<'_, O, R>,
    ) -> Result<SyncReport, ViewError> {
        let server = &self.inner.server;
        let dir = &self.inner.dir;
        self.verify_config()?;
        let wanted = source.refs().await.map_err(ViewError::Source)?;
        check_source_refs(&wanted)?;
        let default_branch = source.default_branch().await.map_err(ViewError::Source)?;
        if let Some(b) = &default_branch
            && !(valid_ref_name(b) && b.starts_with("refs/heads/"))
        {
            return Err(ViewError::BadRef(b.clone()));
        }

        let current: HashMap<String, GitOid> = server.list_refs(dir)?.into_iter().collect();
        let to_set: Vec<&SourceRef> = wanted
            .iter()
            .filter(|r| current.get(&r.name) != Some(&r.git))
            .collect();
        let wanted_names: HashSet<&str> = wanted.iter().map(|r| r.name.as_str()).collect();
        let mut to_delete: Vec<&String> = current
            .keys()
            .filter(|n| !wanted_names.contains(n.as_str()))
            .collect();
        to_delete.sort();
        if to_set.len() + to_delete.len() > MAX_SYNC_REFS {
            return Err(ViewError::TooManyRefs(to_set.len() + to_delete.len()));
        }
        let head_change = match &default_branch {
            Some(b) if server.read_head(dir)?.as_deref() != Some(b.as_str()) => Some(b.clone()),
            _ => None,
        };
        if to_set.is_empty() && to_delete.is_empty() && head_change.is_none() {
            return Ok(SyncReport {
                noop: true,
                ..SyncReport::default()
            });
        }

        let mut exported = ExportReport::default();
        if !to_set.is_empty() {
            let roots: Vec<ExportRoot> = to_set.iter().map(|r| r.target).collect();
            let odb = Odb::open(dir)?;
            exported = exporter.export(&roots, &odb).await?;
            for (r, tip) in to_set.iter().zip(&exported.tips) {
                if *tip != r.git {
                    return Err(ViewError::TipMismatch {
                        name: r.name.clone(),
                        expected: r.git,
                        actual: *tip,
                    });
                }
            }
        }
        let mut updates: Vec<RefUpdate> = to_set
            .iter()
            .map(|r| RefUpdate::Set {
                name: r.name.clone(),
                oid: r.git,
            })
            .collect();
        updates.extend(
            to_delete
                .iter()
                .map(|n| RefUpdate::Delete { name: (*n).clone() }),
        );
        server.update_refs(dir, &updates)?;
        let head_changed = if let Some(b) = head_change {
            server.set_head(dir, &b)?;
            true
        } else {
            false
        };
        Ok(SyncReport {
            noop: false,
            refs_set: to_set.len(),
            refs_deleted: to_delete.len(),
            head_changed,
            written: exported.written,
            skipped: exported.skipped,
        })
    }

    /// The request-time entry point: syncs a ready view, builds a small absent one inline, and
    /// otherwise answers that the view is building (SPEC §25.2 "Reads", item 1). A repo with
    /// more than [`INLINE_BUILD_OBJECTS`] estimated objects, or an unknown estimate, builds in a
    /// background job: the caller runs [`GitView::build`] with the returned ticket.
    pub async fn ensure<O: Objects, R: Refs>(
        &self,
        source: &dyn RefSource,
        exporter: &Exporter<'_, O, R>,
    ) -> Result<Readiness, ViewError> {
        match self.state() {
            ViewState::Ready => return Ok(Readiness::Ready(self.sync(source, exporter).await?)),
            ViewState::Building => {
                return Ok(Readiness::Building {
                    retry_after: RETRY_AFTER_BUILDING,
                });
            }
            ViewState::Absent => {}
        }
        let estimate = source
            .estimated_objects()
            .await
            .map_err(ViewError::Source)?;
        let inline = estimate.is_some_and(|n| n <= INLINE_BUILD_OBJECTS);
        let busy = Readiness::Building {
            retry_after: RETRY_AFTER_BUILDING,
        };
        let slot = match self.inner.slots.get() {
            Some(slots) => match slots.try_acquire() {
                Some(s) => Some(s),
                None => return Ok(busy),
            },
            None => None,
        };
        if self
            .inner
            .state
            .compare_exchange(ABSENT, BUILDING, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return Ok(busy);
        }
        let ticket = BuildTicket {
            view: self.inner.clone(),
            _slot: slot,
            done: false,
        };
        if inline {
            return Ok(Readiness::Ready(
                self.build(ticket, source, exporter).await?,
            ));
        }
        Ok(Readiness::StartBackgroundBuild {
            ticket,
            retry_after: RETRY_AFTER_BUILDING,
        })
    }

    /// Runs the build a [`BuildTicket`] stands for. On failure the view returns to `Absent`.
    pub async fn build<O: Objects, R: Refs>(
        &self,
        mut ticket: BuildTicket,
        source: &dyn RefSource,
        exporter: &Exporter<'_, O, R>,
    ) -> Result<SyncReport, ViewError> {
        if !Arc::ptr_eq(&ticket.view, &self.inner) {
            return Err(ViewError::ForeignTicket);
        }
        let report = {
            let _guard = self.inner.sync_lock.lock().await;
            self.sync_locked(source, exporter).await?
        };
        self.mark_ready();
        ticket.done = true;
        Ok(report)
    }

    /// Discards the view and derives it again (`asc serve admin git-view rebuild`). A pack child
    /// running on the view at that moment fails; the operator chooses the moment.
    pub async fn rebuild<O: Objects, R: Refs>(
        &self,
        source: &dyn RefSource,
        exporter: &Exporter<'_, O, R>,
    ) -> Result<SyncReport, ViewError> {
        if self.inner.kind == ViewKind::Mirror {
            return Err(ViewError::ReadOnlyMirror);
        }
        let _guard = self.inner.sync_lock.lock().await;
        self.inner.state.store(BUILDING, Ordering::SeqCst);
        let result = async {
            let dir = &self.inner.dir;
            fs::remove_dir_all(dir).map_err(io_err("removing the view"))?;
            private_dir(dir).map_err(io_err("creating the view"))?;
            self.inner.server.init_bare(dir)?;
            write_template_if_different(dir)?;
            self.sync_locked(source, exporter).await
        }
        .await;
        match result {
            Ok(report) => {
                self.mark_ready();
                Ok(report)
            }
            Err(e) => {
                self.inner.state.store(ABSENT, Ordering::SeqCst);
                Err(e)
            }
        }
    }

    /// Deletes the views of repos not in `live` (`asc serve admin git-view prune`); returns the
    /// repo ids removed. Only `<number>.git` directories are touched.
    pub fn prune(view_dir: &Path, live: &HashSet<i64>) -> Result<Vec<i64>, ViewError> {
        let mut removed = Vec::new();
        let entries = match fs::read_dir(view_dir) {
            Ok(e) => e,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(removed),
            Err(e) => return Err(io_err("listing the views")(e)),
        };
        for entry in entries {
            let entry = entry.map_err(io_err("listing the views"))?;
            let name = entry.file_name();
            let Some(id) = name
                .to_str()
                .and_then(|n| n.strip_suffix(VIEW_SUFFIX))
                .filter(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
                .and_then(|n| n.parse::<i64>().ok())
            else {
                continue;
            };
            let is_real_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
            if is_real_dir && !live.contains(&id) {
                fs::remove_dir_all(entry.path()).map_err(io_err("removing a stale view"))?;
                removed.push(id);
            }
        }
        removed.sort_unstable();
        Ok(removed)
    }
}

/// Every wanted ref is a shared head or tag with a valid name, listed once.
fn check_source_refs(refs: &[SourceRef]) -> Result<(), ViewError> {
    if refs.len() > MAX_SYNC_REFS {
        return Err(ViewError::TooManyRefs(refs.len()));
    }
    let mut seen = HashSet::with_capacity(refs.len());
    for r in refs {
        if !valid_ref_name(&r.name) || !VIEW_NAMESPACES.iter().any(|p| r.name.starts_with(p)) {
            return Err(ViewError::BadRef(r.name.clone()));
        }
        if !seen.insert(r.name.as_str()) {
            return Err(ViewError::DuplicateRef(r.name.clone()));
        }
    }
    Ok(())
}

/// Rewrites `<dir>/config` from [`VIEW_CONFIG_TEMPLATE`] when it differs.
fn write_template_if_different(dir: &Path) -> Result<(), ViewError> {
    let path = dir.join("config");
    let current = read_bounded(&path).map_err(io_err("reading the view config"))?;
    if current.as_deref() == Some(VIEW_CONFIG_TEMPLATE.as_bytes()) {
        return Ok(());
    }
    let tmp = dir.join("config.asc-new");
    fs::write(&tmp, VIEW_CONFIG_TEMPLATE).map_err(io_err("writing the view config"))?;
    fs::rename(&tmp, &path).map_err(io_err("replacing the view config"))?;
    Ok(())
}
