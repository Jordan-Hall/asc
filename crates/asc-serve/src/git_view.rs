//! The git view of a repo for the git protocol layer (SPEC §25.2, §25.8; PLAN U80). Only built
//! with the Cargo feature `git-compat`.
//!
//! [`GitViewService`] owns what the HTTP routes of U81 and U82 need and nothing else of git: the
//! one [`ServerGit`] (the only way a git child starts), the per-repo [`GitView`]s, the per-replica
//! build slots, and the singleflight that makes N simultaneous requests for one repo do one sync.
//! A [`SqlRefSource`] feeds a view from the repo's shared refs (`refs/heads/*`, `refs/tags/*`)
//! and the deterministic git ids of U34; private refs (`refs/users/*`, drafts, snapshots) never
//! enter a view.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use asc_core::codec::decode_envelope;
use asc_core::git::{ObjectKind, export_tag, object_oid};
use asc_core::id::{GitOid, RefTarget, TagId};
use asc_core::object::TagRecord;
use asc_core::path::RefName;
use asc_git::compat::view::{RefSource, SourceError};
use asc_git::compat::{
    BuildSlots, FsckMode, GitView, Readiness, RefUpdate, ServerGit, ServerGitConfig,
    ServerGitError, SourceRef, SyncReport, ViewError,
};
use asc_git::export::{ExportReport, ExportRoot, Exporter};
use asc_git::odb::Odb;
use asc_git::realgit::{RealGit, RealGitError};
use asc_rpc::coalesce::Coalesce;
use asc_store::key::ObjKey;
use asc_store::objects::Objects;
use asc_store::refs::Refs;
use asc_store::sqlrefs::SqlRefs;
use sqlx::{AnyPool, Row};

use crate::config::{GitFsck, ServeConfig};
use crate::outbox::{Effect, EffectError, Job};
use crate::registry::{RepoEntry, RepoMode};

/// Views kept open at once; a request for another repo past this is refused (`503`), so one
/// server cannot be made to hold unbounded handles.
pub const MAX_OPEN_VIEWS: usize = 4096;
/// Objects a changeset exports on average (blob, a few trees, the commit): the build-size
/// estimate is the changeset count times this.
pub const OBJECTS_PER_CHANGESET: u64 = 10;
/// The outbox effect that brings a view in step after shared refs moved.
pub const EFFECT: &str = "git_view_sync";
/// Directory under the views' parent holding the asc-owned hooks and private state.
const COMPAT_DIR: &str = "git-compat";

/// Why a view operation failed.
#[derive(Debug, thiserror::Error)]
pub enum GitViewError {
    /// The layer is off, with the reason to log once (SPEC §25.9: "disables the layer with a
    /// logged reason").
    #[error("git protocol disabled: {0}")]
    Disabled(String),
    #[error(transparent)]
    View(#[from] ViewError),
    #[error(transparent)]
    Git(#[from] ServerGitError),
    #[error(transparent)]
    RealGit(#[from] RealGitError),
    #[error("database: {0}")]
    Sql(#[from] sqlx::Error),
    #[error("{0}")]
    Repo(String),
    #[error("more than {MAX_OPEN_VIEWS} views are open")]
    TooManyViews,
    /// A sync that other requests shared failed.
    #[error("{0}")]
    Shared(String),
}

/// Runs a future that is not `Send` (the export holds a `gix` object database handle across its
/// awaits) on a blocking thread of the current runtime. The future is built on that thread and
/// never leaves it. The git children it starts block their thread, so this is also where they
/// belong.
async fn on_blocking_thread<F, Fut, T>(make: F) -> Result<T, GitViewError>
where
    F: FnOnce() -> Fut + Send + 'static,
    Fut: std::future::Future<Output = T>,
    T: Send + 'static,
{
    let handle = tokio::runtime::Handle::current();
    tokio::task::spawn_blocking(move || handle.block_on(make()))
        .await
        .map_err(|e| GitViewError::Shared(format!("the view task failed: {e}")))
}

/// What a request learns about a repo's view.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ViewAnswer {
    /// The view is built and matches asc's refs.
    Ready(SyncReport),
    /// A build runs: answer `503` with `Retry-After`.
    Building { retry_after: Duration },
}

/// The git views of a server.
pub struct GitViewService {
    server: ServerGit,
    git: RealGit,
    view_dir: PathBuf,
    /// `<cache disk_dir>/mirrors`, where mirror-mode repos keep their bare mirror.
    mirror_dir: Option<PathBuf>,
    slots: Arc<BuildSlots>,
    views: Mutex<HashMap<i64, GitView>>,
    flights: Coalesce<i64, Result<ViewAnswer, String>>,
    pool: AnyPool,
}

impl GitViewService {
    /// Starts the layer from `[git]` and `[limits.git]`: resolves real git, checks its version,
    /// prepares the asc-owned directories. `Err(Disabled)` carries the reason when the layer
    /// cannot run (no directory, git older than 2.40 or missing); the caller logs it once and
    /// serves the disabled stub. `Ok(None)` is `[git] enabled = false`.
    pub fn start(
        cfg: &ServeConfig,
        pool: AnyPool,
        mirror_dir: Option<PathBuf>,
    ) -> Result<Option<Arc<Self>>, GitViewError> {
        if !cfg.git.enabled {
            return Ok(None);
        }
        let disabled = |why: &str| GitViewError::Disabled(why.to_owned());
        let view_dir = cfg
            .git
            .view_dir(&cfg.cache)
            .ok_or_else(|| disabled("neither git.view_dir nor cache.disk_dir is set"))?;
        if !view_dir.is_absolute() {
            return Err(disabled("the git view directory must be an absolute path"));
        }
        let compat = view_dir
            .parent()
            .ok_or_else(|| disabled("the git view directory has no parent"))?
            .join(COMPAT_DIR);
        let git = RealGit::resolve(cfg.git.git_path(), None)
            .map_err(|e| GitViewError::Disabled(e.to_string()))?;
        let limits = cfg.limits.git.to_limits();
        let slots = BuildSlots::new(limits.view_builds_per_replica);
        let server = ServerGit::new(
            &git,
            ServerGitConfig {
                hooks_dir: compat.join("hooks"),
                state_dir: compat.join("state"),
                limits,
                fsck: match cfg.git.fsck {
                    GitFsck::Strict => FsckMode::Strict,
                    GitFsck::Migration => FsckMode::Migration,
                },
            },
        )
        .map_err(|e| GitViewError::Disabled(e.to_string()))?;
        Ok(Some(Arc::new(Self {
            server,
            git,
            view_dir,
            mirror_dir,
            slots,
            views: Mutex::new(HashMap::new()),
            flights: Coalesce::new(),
            pool,
        })))
    }

    /// The only handle on git children for this server.
    pub fn server(&self) -> &ServerGit {
        &self.server
    }

    pub fn view_dir(&self) -> &Path {
        &self.view_dir
    }

    /// The (opened) view of `repo`; mirror-mode repos use their bare mirror.
    pub fn view(&self, repo: &RepoEntry) -> Result<GitView, GitViewError> {
        let id = repo.key.0;
        let mut views = self.views.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(v) = views.get(&id) {
            return Ok(v.clone());
        }
        if views.len() >= MAX_OPEN_VIEWS {
            return Err(GitViewError::TooManyViews);
        }
        let view = if repo.mode == RepoMode::Mirror {
            let dir = self
                .mirror_dir
                .as_ref()
                .ok_or_else(|| GitViewError::Repo("the server keeps no mirrors".into()))?;
            GitView::open_mirror(&self.server, &dir.join(format!("{id}.git")))?
        } else {
            let v = GitView::open(&self.server, &self.view_dir, id)?;
            v.set_build_slots(self.slots.clone());
            v
        };
        views.insert(id, view.clone());
        Ok(view)
    }

    fn source(&self, repo: &Arc<RepoEntry>, refs: &Arc<SqlRefs>) -> SqlRefSource {
        SqlRefSource {
            repo: repo.clone(),
            refs: refs.clone(),
            pool: self.pool.clone(),
        }
    }

    /// Brings `repo`'s view in step with its refs for a request. Concurrent requests for one
    /// repo share one run. A view that must be built in the background starts that build and
    /// answers [`ViewAnswer::Building`].
    pub async fn ensure(
        self: &Arc<Self>,
        repo: &Arc<RepoEntry>,
        refs: &Arc<SqlRefs>,
    ) -> Result<ViewAnswer, GitViewError> {
        let (this, repo2, refs2) = (self.clone(), repo.clone(), refs.clone());
        self.flights
            .run(repo.key.0, move || async move {
                let answer =
                    on_blocking_thread(
                        move || async move { this.ensure_once(&repo2, &refs2).await },
                    )
                    .await;
                match answer {
                    Ok(Ok(a)) => Ok(a),
                    Ok(Err(e)) => Err(e.to_string()),
                    Err(e) => Err(e.to_string()),
                }
            })
            .await
            .map_err(GitViewError::Shared)
    }

    async fn ensure_once(
        self: &Arc<Self>,
        repo: &Arc<RepoEntry>,
        refs: &Arc<SqlRefs>,
    ) -> Result<ViewAnswer, GitViewError> {
        let view = self.view(repo)?;
        let source = self.source(repo, refs);
        let exporter = view.exporter(repo.backend.engine(), &*repo.objects, &self.git);
        match view.ensure(&source, &exporter).await? {
            Readiness::Ready(r) => Ok(ViewAnswer::Ready(r)),
            Readiness::Building { retry_after } => Ok(ViewAnswer::Building { retry_after }),
            Readiness::StartBackgroundBuild {
                ticket,
                retry_after,
            } => {
                let (this, repo, refs) = (self.clone(), repo.clone(), refs.clone());
                // The build outlives this request; its result is only logged.
                tokio::spawn(async move {
                    let built = on_blocking_thread(move || async move {
                        let source = this.source(&repo, &refs);
                        let exporter =
                            view.exporter(repo.backend.engine(), &*repo.objects, &this.git);
                        let started = std::time::Instant::now();
                        match view.build(ticket, &source, &exporter).await {
                            Ok(r) => tracing::info!(
                                repo = %repo.name,
                                refs = r.refs_set,
                                written = r.written,
                                seconds = started.elapsed().as_secs_f64(),
                                "git view built"
                            ),
                            Err(e) => {
                                tracing::warn!(repo = %repo.name, error = %e, "git view build failed");
                            }
                        }
                    })
                    .await;
                    if let Err(e) = built {
                        tracing::warn!(error = %e, "git view build task failed");
                    }
                });
                Ok(ViewAnswer::Building { retry_after })
            }
        }
    }

    /// `asc serve admin git-view warm`: builds or syncs the view now, however large it is.
    pub async fn warm(
        self: &Arc<Self>,
        repo: &Arc<RepoEntry>,
        refs: &Arc<SqlRefs>,
    ) -> Result<SyncReport, GitViewError> {
        let (this, repo, refs) = (self.clone(), repo.clone(), refs.clone());
        on_blocking_thread(move || async move {
            let view = this.view(&repo)?;
            let source = this.source(&repo, &refs);
            let exporter = view.exporter(repo.backend.engine(), &*repo.objects, &this.git);
            match view.ensure(&source, &exporter).await? {
                Readiness::Ready(r) => Ok(r),
                Readiness::Building { .. } => Err(GitViewError::Repo(
                    "a build is already running; try again when it finishes".into(),
                )),
                Readiness::StartBackgroundBuild { ticket, .. } => {
                    Ok(view.build(ticket, &source, &exporter).await?)
                }
            }
        })
        .await?
    }

    /// `asc serve admin git-view rebuild`: discards the view and derives it again.
    pub async fn rebuild(
        self: &Arc<Self>,
        repo: &Arc<RepoEntry>,
        refs: &Arc<SqlRefs>,
    ) -> Result<SyncReport, GitViewError> {
        let (this, repo, refs) = (self.clone(), repo.clone(), refs.clone());
        on_blocking_thread(move || async move {
            let view = this.view(&repo)?;
            let source = this.source(&repo, &refs);
            let exporter = view.exporter(repo.backend.engine(), &*repo.objects, &this.git);
            Ok(view.rebuild(&source, &exporter).await?)
        })
        .await?
    }

    /// Brings the host's git directory `dir` (the bridge) in step with `plan`: exports what the
    /// new tips need that `dir` lacks, then sets the planned refs in one `update-ref`
    /// transaction. Refs outside the plan are not touched, and no git hook runs. Repeating a
    /// plan changes nothing.
    pub async fn promote(
        self: &Arc<Self>,
        repo: &Arc<RepoEntry>,
        dir: PathBuf,
        plan: PromotePlan,
    ) -> Result<SyncReport, GitViewError> {
        let (this, repo) = (self.clone(), repo.clone());
        on_blocking_thread(move || async move {
            let mut exported = ExportReport::default();
            if !plan.sets.is_empty() {
                let roots: Vec<ExportRoot> = plan.sets.iter().map(|s| s.root).collect();
                let odb = Odb::open(&dir).map_err(ViewError::from)?;
                let exporter =
                    Exporter::new(repo.backend.engine(), &*repo.objects, &this.git, &dir);
                exported = exporter
                    .export(&roots, &odb)
                    .await
                    .map_err(ViewError::from)?;
                for (set, tip) in plan.sets.iter().zip(&exported.tips) {
                    if *tip != set.git {
                        return Err(ViewError::TipMismatch {
                            name: set.name.clone(),
                            expected: set.git,
                            actual: *tip,
                        }
                        .into());
                    }
                }
            }
            let mut updates: Vec<RefUpdate> = plan
                .sets
                .iter()
                .map(|s| RefUpdate::Set {
                    name: s.name.clone(),
                    oid: s.git,
                })
                .collect();
            updates.extend(
                plan.deletes
                    .iter()
                    .map(|name| RefUpdate::Delete { name: name.clone() }),
            );
            this.server.update_refs(&dir, &updates)?;
            Ok(SyncReport {
                noop: false,
                refs_set: plan.sets.len(),
                refs_deleted: plan.deletes.len(),
                head_changed: false,
                written: exported.written,
                skipped: exported.skipped,
            })
        })
        .await?
    }

    /// `asc serve admin git-view prune`: removes the views of repos that no longer exist; returns
    /// the repo ids removed.
    pub async fn prune(&self) -> Result<Vec<i64>, GitViewError> {
        let rows = sqlx::query("SELECT id FROM repos")
            .fetch_all(&self.pool)
            .await?;
        let mut live = HashSet::with_capacity(rows.len());
        for r in &rows {
            live.insert(r.try_get::<i64, _>("id")?);
        }
        let removed = GitView::prune(&self.view_dir, &live)?;
        let mut views = self.views.lock().unwrap_or_else(PoisonError::into_inner);
        for id in &removed {
            views.remove(id);
        }
        Ok(removed)
    }
}

/// Starts the views at server start: registers the `git_view_sync` effect and logs why the layer
/// is off when it is. The returned service is what the git routes (U81, U82) are given; `None`
/// means they answer with the disabled stub (SPEC §25.9).
pub fn register(
    registry: &Arc<crate::registry::Registry>,
    outbox: &crate::outbox::Outbox,
    cfg: &ServeConfig,
) -> Option<Arc<GitViewService>> {
    let mirror_dir = cfg.cache.disk_dir.as_ref().map(|d| d.join("mirrors"));
    let promote_root = cfg.bridge_promote_root();
    match GitViewService::start(cfg, registry.pool().clone(), mirror_dir) {
        Ok(Some(service)) => {
            GitViewSyncEffect::new(service.clone(), registry.clone()).register(outbox);
            if let Some(root) = promote_root {
                BridgePromoteEffect::new(service.clone(), registry.clone(), root).register(outbox);
            }
            tracing::info!(view_dir = %service.view_dir().display(), "git protocol views are ready");
            Some(service)
        }
        Ok(None) => {
            tracing::info!("git protocol disabled: [git] enabled = false");
            fail_promotions(promote_root, outbox, "[git] enabled = false");
            None
        }
        Err(e) => {
            tracing::warn!("{e}");
            fail_promotions(promote_root, outbox, &e.to_string());
            None
        }
    }
}

/// With a bridge to keep but no git layer, promotion rows must fail rather than pass as done.
fn fail_promotions(root: Option<PathBuf>, outbox: &crate::outbox::Outbox, why: &str) {
    if root.is_some() {
        outbox
            .effects()
            .register(Arc::new(PromoteUnavailable(why.to_owned())));
    }
}

/// A repo's shared refs as a [`RefSource`].
pub struct SqlRefSource {
    repo: Arc<RepoEntry>,
    refs: Arc<SqlRefs>,
    pool: AnyPool,
}

fn source_err(e: impl std::fmt::Display) -> SourceError {
    e.to_string().into()
}

/// The git id of a tag object: what the export renders for it.
async fn tag_git(repo: &RepoEntry, id: TagId) -> Result<GitOid, SourceError> {
    let data = repo
        .objects
        .get(&ObjKey::Tag(id))
        .await
        .map_err(source_err)?
        .ok_or_else(|| source_err(format!("tag {id} is missing")))?;
    let rec: TagRecord = decode_envelope(&data).map_err(source_err)?;
    let target = repo
        .backend
        .engine()
        .git_commit_of(rec.target)
        .await
        .map_err(source_err)?;
    let raw = export_tag(&rec, target).map_err(source_err)?;
    Ok(object_oid(ObjectKind::Tag, &raw))
}

/// What to export for a ref's target, and the git id asc's map records for it.
async fn export_target(
    repo: &RepoEntry,
    target: RefTarget,
) -> Result<(ExportRoot, GitOid), SourceError> {
    Ok(match target {
        RefTarget::Changeset(c) => (
            ExportRoot::Changeset(c),
            repo.backend
                .engine()
                .git_commit_of(c)
                .await
                .map_err(source_err)?,
        ),
        RefTarget::Tag(t) => (ExportRoot::Tag(t), tag_git(repo, t).await?),
    })
}

#[async_trait]
impl RefSource for SqlRefSource {
    async fn refs(&self) -> Result<Vec<SourceRef>, SourceError> {
        let mut out = Vec::new();
        for prefix in ["refs/heads/", "refs/tags/"] {
            for e in self
                .refs
                .list(self.repo.key, prefix)
                .await
                .map_err(source_err)?
            {
                let name = String::from_utf8(e.name.as_bytes().to_vec())
                    .map_err(|_| source_err("a ref name is not UTF-8"))?;
                let (target, git) = export_target(&self.repo, e.target).await?;
                out.push(SourceRef { name, target, git });
            }
        }
        Ok(out)
    }

    async fn default_branch(&self) -> Result<Option<String>, SourceError> {
        Ok(Some(format!("refs/heads/{}", self.repo.default_branch)))
    }

    async fn estimated_objects(&self) -> Result<Option<u64>, SourceError> {
        let row = sqlx::query("SELECT COUNT(*) AS n FROM cs_graph WHERE ns = $1")
            .bind(self.repo.ns.as_bytes().to_vec())
            .fetch_one(&self.pool)
            .await
            .map_err(source_err)?;
        let n: i64 = row.try_get("n").map_err(source_err)?;
        Ok(Some(
            u64::try_from(n)
                .unwrap_or(0)
                .saturating_mul(OBJECTS_PER_CHANGESET),
        ))
    }
}

/// The `git_view_sync` outbox effect: after shared refs moved, bring the view in step so the next
/// fetch finds it ready. A build in progress is not an error: the next request converges it.
pub struct GitViewSyncEffect {
    service: Arc<GitViewService>,
    registry: Arc<crate::registry::Registry>,
}

impl GitViewSyncEffect {
    pub fn new(service: Arc<GitViewService>, registry: Arc<crate::registry::Registry>) -> Self {
        Self { service, registry }
    }

    /// Registers the effect, replacing the placeholder.
    pub fn register(self, outbox: &crate::outbox::Outbox) {
        outbox.effects().register(Arc::new(self));
    }
}

#[async_trait]
impl Effect for GitViewSyncEffect {
    fn kind(&self) -> &'static str {
        EFFECT
    }

    async fn run(&self, job: &Job) -> Result<(), EffectError> {
        let fail = |e: &dyn std::fmt::Display| EffectError(e.to_string());
        let row = sqlx::query("SELECT name FROM repos WHERE id = $1")
            .bind(job.repo_id)
            .fetch_optional(self.registry.pool())
            .await
            .map_err(|e| fail(&e))?;
        // A deleted repo has nothing to sync; `prune` removes its view.
        let Some(row) = row else {
            return Ok(());
        };
        let name: String = row.try_get("name").map_err(|e| fail(&e))?;
        let name = asc_rpc::server::RepoName::parse(&name).map_err(|e| fail(&e))?;
        let repo = self.registry.open(&name).await.map_err(|e| fail(&e))?;
        self.service
            .ensure(&repo, self.registry.refs())
            .await
            .map(|_| ())
            .map_err(|e| fail(&e))
    }
}

// ------------------------------------------------------------------------------ bridge_promote

/// The outbox effect that keeps the host's git directory current (SPEC §8.3 `bridge_promote`).
pub const PROMOTE_EFFECT: &str = "bridge_promote";
/// Ref namespaces the host's git directory holds: shared branches, tags and the pull heads the
/// host writes. Private refs (`refs/users/*`, drafts, snapshots) never leave asc.
const BRIDGE_NAMESPACES: [&str; 3] = ["refs/heads/", "refs/tags/", "refs/pull/"];
/// Refs one promotion moves: an `UpdateRefs` request cannot name more.
const MAX_PROMOTE_REFS: usize = asc_rpc::server::MAX_REF_UPDATES;

/// One ref to set in the bridge: the export that produces its target and the git id that must
/// come out of it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromoteSet {
    pub name: String,
    pub root: ExportRoot,
    pub git: GitOid,
}

/// What a promotion does to the bridge, from asc's current refs.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PromotePlan {
    pub sets: Vec<PromoteSet>,
    pub deletes: Vec<String>,
}

/// The `<root>/<owner>/<name>.git` directory of `repo` (the host lowercases both parts). It
/// must be a plain directory inside `root` that holds a bare repository.
fn bridge_dir(root: &Path, repo: &str) -> Result<PathBuf, String> {
    if !root.is_absolute() {
        return Err("the bridge root must be an absolute path".into());
    }
    let (owner, name) = repo
        .split_once('/')
        .ok_or_else(|| "invalid repository name".to_owned())?;
    for part in [owner, name] {
        if part.is_empty() || part == "." || part == ".." || part.contains(['/', '\\', '\0']) {
            return Err("invalid repository name".into());
        }
    }
    let dir = root
        .join(owner.to_lowercase())
        .join(format!("{}.git", name.to_lowercase()));
    let meta = std::fs::symlink_metadata(&dir)
        .map_err(|e| format!("the bridge directory of {repo} is not readable: {e}"))?;
    if !meta.file_type().is_dir() {
        return Err(format!("the bridge of {repo} is not a plain directory"));
    }
    let inside = match (std::fs::canonicalize(&dir), std::fs::canonicalize(root)) {
        (Ok(d), Ok(r)) => d.starts_with(r),
        _ => false,
    };
    if !inside {
        return Err(format!("the bridge of {repo} is outside the bridge root"));
    }
    if !dir.join("HEAD").is_file() || !dir.join("objects").is_dir() {
        return Err(format!("the bridge of {repo} is not a bare git repository"));
    }
    Ok(dir)
}

/// The ref names of a row the bridge holds, sorted and without repeats.
fn promoted_names(doc: &serde_json::Value) -> Result<Vec<String>, EffectError> {
    let refs = doc
        .get("refs")
        .and_then(|r| r.as_array())
        .ok_or_else(|| EffectError("outbox row has no refs".into()))?;
    if refs.len() > MAX_PROMOTE_REFS {
        return Err(EffectError("outbox row lists too many refs".into()));
    }
    let mut names: Vec<String> = refs
        .iter()
        .filter_map(|r| r.get("name").and_then(|n| n.as_str()))
        .filter(|n| BRIDGE_NAMESPACES.iter().any(|ns| n.starts_with(ns)))
        .map(str::to_owned)
        .collect();
    names.sort();
    names.dedup();
    Ok(names)
}

/// `bridge_promote`: after a host writer moved shared refs, exports the new tips into the host's
/// git directory and sets its refs to **asc's current** values, so a plain `git clone` of the host
/// sees them. Idempotent (a repeat finds every object and ref in place), bounded by the row's ref
/// list, and silent about everything but names and counts. A missing or unusable directory is an
/// error: the row is retried with backoff until the host makes the directory right.
///
/// The `ImportGitPush` rows are skipped: the host's git moves its refs itself when its hook
/// returns ([`crate::import_push::HOST_PUSH_VIA`]).
pub struct BridgePromoteEffect {
    service: Arc<GitViewService>,
    registry: Arc<crate::registry::Registry>,
    root: PathBuf,
}

impl BridgePromoteEffect {
    pub fn new(
        service: Arc<GitViewService>,
        registry: Arc<crate::registry::Registry>,
        root: PathBuf,
    ) -> Self {
        Self {
            service,
            registry,
            root,
        }
    }

    /// Registers the effect, replacing the placeholder.
    pub fn register(self, outbox: &crate::outbox::Outbox) {
        outbox.effects().register(Arc::new(self));
    }

    /// Whether the host's own git wrote this row's refs.
    async fn host_wrote_it(&self, job: &Job) -> Result<bool, sqlx::Error> {
        let row = sqlx::query("SELECT via FROM ref_log WHERE repo_id = $1 AND op_id = $2 LIMIT 1")
            .bind(job.repo_id)
            .bind(job.op_id.clone())
            .fetch_optional(self.registry.pool())
            .await?;
        match row {
            Some(r) => Ok(r.try_get::<String, _>("via")? == crate::import_push::HOST_PUSH_VIA),
            None => Ok(false),
        }
    }

    /// asc's current value of every named ref, as the bridge should hold it.
    async fn plan(&self, repo: &RepoEntry, names: &[String]) -> Result<PromotePlan, EffectError> {
        let fail = |e: &dyn std::fmt::Display| EffectError(e.to_string());
        let mut plan = PromotePlan::default();
        for name in names {
            let ref_name = RefName::new(name.as_bytes().to_vec()).map_err(|e| fail(&e))?;
            match self
                .registry
                .refs()
                .get(repo.key, &ref_name)
                .await
                .map_err(|e| fail(&e))?
            {
                Some(entry) => {
                    let (root, git) = export_target(repo, entry.target)
                        .await
                        .map_err(|e| fail(&e))?;
                    plan.sets.push(PromoteSet {
                        name: name.clone(),
                        root,
                        git,
                    });
                }
                None => plan.deletes.push(name.clone()),
            }
        }
        Ok(plan)
    }
}

#[async_trait]
impl Effect for BridgePromoteEffect {
    fn kind(&self) -> &'static str {
        PROMOTE_EFFECT
    }

    async fn run(&self, job: &Job) -> Result<(), EffectError> {
        let fail = |e: &dyn std::fmt::Display| EffectError(e.to_string());
        let names = promoted_names(&job.doc)?;
        if names.is_empty() {
            return Ok(());
        }
        let row = sqlx::query("SELECT name FROM repos WHERE id = $1")
            .bind(job.repo_id)
            .fetch_optional(self.registry.pool())
            .await
            .map_err(|e| fail(&e))?;
        // A deleted repo has no bridge to keep.
        let Some(row) = row else {
            return Ok(());
        };
        let name: String = row.try_get("name").map_err(|e| fail(&e))?;
        let name = asc_rpc::server::RepoName::parse(&name).map_err(|e| fail(&e))?;
        let repo = self.registry.open(&name).await.map_err(|e| fail(&e))?;
        // A mirror repo is written by its source, never by a host writer.
        if repo.mode == RepoMode::Mirror || self.host_wrote_it(job).await.map_err(|e| fail(&e))? {
            return Ok(());
        }
        let plan = self.plan(&repo, &names).await?;
        let (root, repo_name) = (self.root.clone(), repo.name.as_str().to_owned());
        let dir = tokio::task::spawn_blocking(move || bridge_dir(&root, &repo_name))
            .await
            .map_err(|e| fail(&e))?
            .map_err(EffectError)?;
        let report = self
            .service
            .promote(&repo, dir, plan)
            .await
            .map_err(|e| fail(&e))?;
        tracing::info!(
            event = "bridge.promoted",
            repo = %repo.name,
            refs_set = report.refs_set,
            refs_deleted = report.refs_deleted,
            written = report.written,
            "bridge promoted"
        );
        Ok(())
    }
}

/// Stands in for [`BridgePromoteEffect`] when the git layer cannot start: the rows fail and are
/// retried (the outbox lag alarm fires) instead of being accepted and dropped.
struct PromoteUnavailable(String);

#[async_trait]
impl Effect for PromoteUnavailable {
    fn kind(&self) -> &'static str {
        PROMOTE_EFFECT
    }

    async fn run(&self, _job: &Job) -> Result<(), EffectError> {
        Err(EffectError(format!(
            "the bridge cannot be promoted: {}",
            self.0
        )))
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use asc_core::content::{Body, ingest};
    use asc_core::id::ChangesetId;
    use asc_core::object::{Change, Kind};
    use asc_core::path::{RefName, RepoPath};
    use asc_core::sig::Signature;
    use asc_engine::commit::{NewChangeset, PathPolicy};
    use asc_rpc::server::RepoName;
    use asc_store::codec::encode_stored;
    use asc_store::refs::{Actor, Expect, RefTxn, RefUpdate};
    use uuid::Uuid;

    use super::*;
    use crate::migrate;
    use crate::registry::{NewRepo, Registry};

    struct Env {
        _dir: tempfile::TempDir,
        registry: Arc<Registry>,
        service: Arc<GitViewService>,
        repo: Arc<RepoEntry>,
    }

    fn config(dir: &Path, extra: &str) -> ServeConfig {
        let disk = dir.to_string_lossy().replace('\\', "/");
        ServeConfig::parse(&format!(
            "store = \"memory:///\"\ndb = \"sqlite::memory:\"\ncache = {{ disk_dir = \"{disk}\", disk = \"1GiB\" }}\n{extra}"
        ))
        .expect("config")
    }

    async fn env() -> Env {
        let dir = tempfile::tempdir().unwrap();
        let pool = migrate::connect("sqlite::memory:", 1).await.unwrap();
        migrate::migrate(&pool).await.unwrap();
        let registry = Arc::new(Registry::new(pool.clone(), "memory:///").unwrap());
        let repo = registry
            .create(&RepoName::parse("o/r").unwrap(), &NewRepo::default())
            .await
            .unwrap();
        let service = GitViewService::start(&config(dir.path(), ""), pool, None)
            .unwrap()
            .expect("enabled");
        Env {
            _dir: dir,
            registry,
            service,
            repo,
        }
    }

    fn sig() -> Signature {
        Signature::new(b"A".to_vec(), b"a@x".to_vec(), 1_700_000_000, 0).unwrap()
    }

    async fn commit(
        repo: &RepoEntry,
        parent: Option<ChangesetId>,
        file: &str,
        body: &[u8],
    ) -> ChangesetId {
        let engine = repo.backend.engine();
        let got = ingest(Cursor::new(body)).unwrap();
        let Body::Inline(b) = got.body else {
            unreachable!("small test file")
        };
        repo.objects
            .put(&ObjKey::File(got.file), encode_stored(&b))
            .await
            .unwrap();
        let change = Change::write(Kind::File, got.file, got.size, got.git, None).unwrap();
        let base = match parent {
            Some(p) => engine.get_changeset(p).await.unwrap().root,
            None => asc_engine::Engine::<asc_store::bucket::BucketObjects, SqlRefs>::empty_tree_id(),
        };
        let root = engine
            .apply(base, &[(RepoPath::new(file).unwrap(), change)])
            .await
            .unwrap();
        engine
            .create_changeset(
                NewChangeset {
                    parents: parent.into_iter().collect(),
                    author: sig(),
                    committer: sig(),
                    message: b"msg\n".to_vec(),
                    extra: vec![],
                    root,
                    copies: vec![],
                },
                PathPolicy::Native { windows_safe: true },
            )
            .await
            .unwrap()
    }

    async fn set_ref(env: &Env, name: &str, target: ChangesetId) {
        let txn = RefTxn {
            updates: vec![RefUpdate {
                name: RefName::new(name.as_bytes().to_vec()).unwrap(),
                expect: Expect::Any,
                new: Some(RefTarget::Changeset(target)),
                expires_at: None,
            }],
            actor: Actor {
                id: "test".into(),
                via: "test".into(),
            },
            reason: "test".into(),
            op_id: Uuid::new_v4(),
        };
        env.registry
            .refs()
            .update(env.repo.key, &txn)
            .await
            .unwrap();
    }

    fn view_refs(env: &Env) -> Vec<String> {
        let view = env.service.view(&env.repo).unwrap();
        let mut names: Vec<String> = env
            .service
            .server()
            .list_refs(view.path())
            .unwrap()
            .into_iter()
            .map(|(n, _)| n)
            .collect();
        names.sort();
        names
    }

    #[tokio::test]
    async fn a_view_follows_the_shared_refs_and_never_holds_private_ones() {
        let env = env().await;
        let a = commit(&env.repo, None, "f.txt", b"one").await;
        let b = commit(&env.repo, Some(a), "f.txt", b"two").await;
        set_ref(&env, "refs/heads/master", b).await;
        set_ref(&env, "refs/heads/topic", a).await;
        set_ref(&env, "refs/users/7/heads/private", b).await;
        set_ref(&env, "refs/pull/1/head", b).await;

        let ViewAnswer::Ready(first) = env
            .service
            .ensure(&env.repo, env.registry.refs())
            .await
            .unwrap()
        else {
            panic!("a small repo builds inline");
        };
        assert_eq!(first.refs_set, 2);
        assert_eq!(
            view_refs(&env),
            vec!["refs/heads/master", "refs/heads/topic"]
        );
        let view = env.service.view(&env.repo).unwrap();
        assert_eq!(
            env.service
                .server()
                .read_head(view.path())
                .unwrap()
                .as_deref(),
            Some("refs/heads/master")
        );
        // The view's ids are the export's ids.
        let want = env.repo.backend.engine().git_commit_of(b).await.unwrap();
        let got = env.service.server().list_refs(view.path()).unwrap();
        assert!(got.contains(&("refs/heads/master".to_owned(), want)));

        let ViewAnswer::Ready(again) = env
            .service
            .ensure(&env.repo, env.registry.refs())
            .await
            .unwrap()
        else {
            panic!("ready");
        };
        assert!(again.noop);

        // A ref move is picked up by the next request.
        let c = commit(&env.repo, Some(b), "f.txt", b"three").await;
        set_ref(&env, "refs/heads/master", c).await;
        let ViewAnswer::Ready(moved) = env
            .service
            .ensure(&env.repo, env.registry.refs())
            .await
            .unwrap()
        else {
            panic!("ready");
        };
        assert_eq!((moved.refs_set, moved.written), (1, 3));
    }

    #[tokio::test]
    async fn simultaneous_requests_share_one_sync() {
        let env = env().await;
        let a = commit(&env.repo, None, "f", b"one").await;
        set_ref(&env, "refs/heads/master", a).await;
        let refs = env.registry.refs().clone();
        let calls: Vec<_> = (0..6)
            .map(|_| {
                let (s, r, refs) = (env.service.clone(), env.repo.clone(), refs.clone());
                tokio::spawn(async move { s.ensure(&r, &refs).await })
            })
            .collect();
        let mut built = 0;
        for c in calls {
            match c.await.unwrap().unwrap() {
                ViewAnswer::Ready(r) if !r.noop => built += 1,
                ViewAnswer::Ready(_) => {}
                ViewAnswer::Building { .. } => panic!("small repos never answer 503"),
            }
        }
        assert!(built >= 1, "someone built the view");
        assert_eq!(view_refs(&env), vec!["refs/heads/master"]);
    }

    #[tokio::test]
    async fn a_large_repo_builds_in_the_background_and_requests_get_503_meanwhile() {
        let env = env().await;
        let a = commit(&env.repo, None, "f", b"one").await;
        set_ref(&env, "refs/heads/master", a).await;
        // Enough graph rows that the estimate passes the inline threshold.
        let rows = asc_git::compat::INLINE_BUILD_OBJECTS / OBJECTS_PER_CHANGESET + 1;
        let mut tx = env.registry.pool().begin().await.unwrap();
        for i in 0..rows {
            let mut cs = [0u8; 32];
            cs[..8].copy_from_slice(&i.to_be_bytes());
            sqlx::query("INSERT INTO cs_graph (ns, cs, gen, parents) VALUES ($1, $2, 0, $3)")
                .bind(env.repo.ns.as_bytes().to_vec())
                .bind(cs.to_vec())
                .bind(Vec::<u8>::new())
                .execute(&mut *tx)
                .await
                .unwrap();
        }
        tx.commit().await.unwrap();

        let first = env
            .service
            .ensure(&env.repo, env.registry.refs())
            .await
            .unwrap();
        let ViewAnswer::Building { retry_after } = first else {
            panic!("a big view answers 503 while it builds, got {first:?}");
        };
        assert!(retry_after.as_secs() > 0);
        let mut ready = false;
        for _ in 0..200 {
            tokio::time::sleep(Duration::from_millis(100)).await;
            if let ViewAnswer::Ready(r) = env
                .service
                .ensure(&env.repo, env.registry.refs())
                .await
                .unwrap()
            {
                assert!(r.noop, "the background build already did the work");
                ready = true;
                break;
            }
        }
        assert!(ready, "the background build finishes");
        assert_eq!(view_refs(&env), vec!["refs/heads/master"]);
    }

    #[tokio::test]
    async fn warm_rebuild_and_prune() {
        let env = env().await;
        let a = commit(&env.repo, None, "f", b"one").await;
        set_ref(&env, "refs/heads/master", a).await;
        let warmed = env
            .service
            .warm(&env.repo, env.registry.refs())
            .await
            .unwrap();
        assert_eq!(warmed.refs_set, 1);
        let rebuilt = env
            .service
            .rebuild(&env.repo, env.registry.refs())
            .await
            .unwrap();
        assert_eq!((rebuilt.refs_set, rebuilt.written), (1, 3));
        // A view of a repo that is gone is pruned; the live one stays.
        let ghost = env.service.view_dir().join("9999.git");
        std::fs::create_dir_all(&ghost).unwrap();
        let removed = env.service.prune().await.unwrap();
        assert_eq!(removed, vec![9999]);
        assert_eq!(view_refs(&env), vec!["refs/heads/master"]);
    }

    #[tokio::test]
    async fn the_effect_syncs_the_view() {
        let env = env().await;
        let a = commit(&env.repo, None, "f", b"one").await;
        set_ref(&env, "refs/heads/master", a).await;
        let effect = GitViewSyncEffect::new(env.service.clone(), env.registry.clone());
        let job = Job {
            id: 1,
            repo_id: env.repo.key.0,
            op_id: Uuid::new_v4().as_bytes().to_vec(),
            doc: serde_json::json!({ "effects": [EFFECT] }),
        };
        assert_eq!(effect.kind(), EFFECT);
        effect.run(&job).await.unwrap();
        assert_eq!(view_refs(&env), vec!["refs/heads/master"]);
        // A repo that no longer exists is not an error: nothing to sync.
        let gone = Job {
            repo_id: 4242,
            ..job
        };
        effect.run(&gone).await.unwrap();
    }

    #[tokio::test]
    async fn the_layer_starts_only_when_enabled_and_runnable() {
        let dir = tempfile::tempdir().unwrap();
        let pool = migrate::connect("sqlite::memory:", 1).await.unwrap();
        let off = config(dir.path(), "[git]\nenabled = false\n");
        assert!(
            GitViewService::start(&off, pool.clone(), None)
                .unwrap()
                .is_none()
        );

        let no_dir =
            ServeConfig::parse("store = \"memory:///\"\ndb = \"sqlite::memory:\"").unwrap();
        let err = GitViewService::start(&no_dir, pool.clone(), None)
            .err()
            .unwrap();
        assert!(matches!(err, GitViewError::Disabled(_)), "{err}");

        let missing = dir.path().join("no-such-git").join("git");
        let text = format!(
            "[git]\ngit_path = \"{}\"\n",
            missing.to_string_lossy().replace('\\', "/")
        );
        let err = GitViewService::start(&config(dir.path(), &text), pool, None)
            .err()
            .unwrap();
        assert!(matches!(err, GitViewError::Disabled(_)), "{err}");
    }
}
