//! The repository registry and the server's extension registries (PLAN U43a).
//!
//! [`Registry`] turns a repository name into an engine over its bucket namespace
//! (`<root>/ns/<ns>`), the shared `SqlRefs` and the SQL git map. [`HttpRoutes`] and
//! [`GrpcServices`] collect what later units add; `lib.rs` merges them at startup.

use std::fmt;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use asc_core::path::RefName;
use asc_engine::derive::{Error as DeriveError, GitMapRow, GitMapSink};
use asc_rpc::auth::Authorizer;
use asc_rpc::server::{EngineBackend, RepoHandlers, RepoName};
use asc_store::bucket::BucketObjects;
use asc_store::refs::RepoKey;
use asc_store::sqlrefs::SqlRefs;
use async_trait::async_trait;
use object_store::ObjectStore;
use object_store::path::Path;
use sqlx::{AnyPool, Row};
use tonic::server::NamedService;

/// Repositories kept open at once; the least recently used is dropped beyond this.
pub const MAX_OPEN_REPOS: usize = 1024;
/// How long an open repository is trusted before its `repos` row is read again. A delete, a
/// rename or the end of a fork copy on another replica shows up here after at most this long
/// (there is no cross-replica notification). A read through a stale entry stays inside the old
/// repository id's namespace. Every mutation re-checks the row inside its own transaction
/// ([`Registry::guard`]), so this bounds only how long a read can be stale, not what a write can
/// reach. This process drops the entry at once on init, fork, rename and delete.
pub const ENTRY_TTL: Duration = Duration::from_secs(1);
/// Git-map rows written per statement batch.
const GIT_MAP_BATCH: usize = 500;

/// Why a registry call failed.
#[derive(Debug, thiserror::Error)]
pub enum RegistryError {
    #[error("repository {0} not found")]
    NotFound(String),
    #[error("repository {0} already exists")]
    Exists(String),
    #[error("invalid repository setting: {0}")]
    Invalid(String),
    #[error("store: {0}")]
    Store(String),
    #[error("database: {0}")]
    Sql(#[from] sqlx::Error),
}

/// The 16-byte random namespace id assigned at repository creation (SPEC §3.3).
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Ns([u8; 16]);

impl Ns {
    pub fn random() -> Self {
        Self(uuid::Uuid::new_v4().into_bytes())
    }

    pub fn from_bytes(b: &[u8]) -> Option<Self> {
        <[u8; 16]>::try_from(b).ok().map(Self)
    }

    pub fn as_bytes(&self) -> &[u8; 16] {
        &self.0
    }

    pub fn hex(&self) -> String {
        self.0.iter().map(|b| format!("{b:02x}")).collect()
    }
}

impl fmt::Debug for Ns {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Ns({})", self.hex())
    }
}

/// `repos.mode` (SPEC §8.6).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RepoMode {
    Mirror,
    Primary,
}

impl RepoMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Mirror => "mirror",
            Self::Primary => "primary",
        }
    }

    fn parse(s: &str) -> Result<Self, RegistryError> {
        match s {
            "mirror" => Ok(Self::Mirror),
            "primary" => Ok(Self::Primary),
            other => Err(RegistryError::Invalid(format!("repos.mode {other:?}"))),
        }
    }
}

/// Settings for a new repository.
#[derive(Clone, Debug)]
pub struct NewRepo {
    pub mode: RepoMode,
    pub default_branch: String,
}

impl Default for NewRepo {
    fn default() -> Self {
        Self {
            mode: RepoMode::Primary,
            default_branch: "master".into(),
        }
    }
}

/// The prefix of a [`RegistryError::Store`] that says a repository's storage is unavailable
/// (bring-your-own storage: the target is disabled, failed, blocked, or the vault is locked). The
/// rest of the message is safe to show.
pub const STORAGE_UNAVAILABLE: &str = "storage unavailable: ";

impl RegistryError {
    /// The user-facing reason when the repository's storage cannot be used right now.
    pub fn unavailable(&self) -> Option<&str> {
        match self {
            Self::Store(m) => m.strip_prefix(STORAGE_UNAVAILABLE),
            _ => None,
        }
    }
}

/// Where a repository's objects are when they are not in the instance bucket.
pub struct RoutedNs {
    pub store: Arc<dyn ObjectStore>,
    /// The root `BucketObjects` uses over `store`.
    pub root: Path,
    /// Changes whenever the route does; the registry rebuilds the entry then.
    pub stamp: u64,
}

/// Bring-your-own storage: maps a repository to its own bucket (`storage/`).
#[async_trait]
pub trait NsRouter: Send + Sync {
    /// The route of repository `repo_id`, `None` for the instance bucket. The error is shown to
    /// the caller as "storage unavailable".
    async fn route(&self, repo_id: i64, ns: Ns) -> Result<Option<RoutedNs>, String>;
    /// The store and root of a namespace this process has routed, for code that holds a
    /// namespace and not an entry (garbage collection, drops, index work).
    fn namespace(&self, ns: Ns) -> Option<(Arc<dyn ObjectStore>, Path)>;
}

/// One open repository.
pub struct RepoEntry {
    pub name: RepoName,
    pub key: RepoKey,
    pub ns: Ns,
    pub mode: RepoMode,
    pub default_branch: String,
    /// The engine and its RPC handlers over this namespace.
    pub backend: Arc<EngineBackend<BucketObjects, SqlRefs>>,
    /// The namespace's object store (ingest verification reads file bytes through it).
    pub objects: Arc<BucketObjects>,
    /// Receives git-map rows into `git_map` for this namespace.
    pub git_map: Arc<SqlGitMap>,
    /// The stamp of the storage route the entry was built over; 0 for the instance bucket.
    pub route_stamp: u64,
}

/// A repository kept open, and when its row was last read.
#[derive(Clone)]
struct Cached {
    entry: Arc<RepoEntry>,
    /// False while a fork copy is still running (`repos.ready`).
    ready: bool,
    checked: Instant,
}

/// Opens repositories by name.
pub struct Registry {
    pool: AnyPool,
    refs: Arc<SqlRefs>,
    store: Arc<dyn ObjectStore>,
    root: Path,
    open: quick_cache::sync::Cache<String, Cached>,
    ttl: Duration,
    router: std::sync::OnceLock<Arc<dyn NsRouter>>,
}

impl Registry {
    /// `store_url` is the bucket root (`s3://…`, `file:///…`, `memory:///`).
    pub fn new(pool: AnyPool, store_url: &str) -> Result<Self, RegistryError> {
        let url = url::Url::parse(store_url).map_err(|e| RegistryError::Store(e.to_string()))?;
        let (store, root) = if url.scheme() == "s3" {
            asc_store::s3::open_with(&url, None).map_err(|e| RegistryError::Store(e.to_string()))?
        } else {
            let (store, root) =
                object_store::parse_url(&url).map_err(|e| RegistryError::Store(e.to_string()))?;
            (Arc::from(store), root)
        };
        Ok(Self {
            refs: Arc::new(SqlRefs::new(pool.clone())),
            pool,
            store,
            root,
            open: quick_cache::sync::Cache::new(MAX_OPEN_REPOS),
            ttl: ENTRY_TTL,
            router: std::sync::OnceLock::new(),
        })
    }

    /// Installs the storage router, once. False when one is already installed.
    pub fn set_router(&self, router: Arc<dyn NsRouter>) -> bool {
        self.router.set(router).is_ok()
    }

    /// The instance bucket and its root (`<root>` of `<root>/ns/<hex>`).
    pub fn default_place(&self) -> (Arc<dyn ObjectStore>, Path) {
        (self.store.clone(), self.root.clone())
    }

    /// Reads the `repos` row again after `ttl` instead of [`ENTRY_TTL`].
    pub fn with_entry_ttl(mut self, ttl: Duration) -> Self {
        self.ttl = ttl;
        self
    }

    /// The bucket and the path prefix (`<root>/ns/<ns>`) of the namespace `ns`.
    pub(crate) fn namespace(&self, ns: Ns) -> (Arc<dyn ObjectStore>, Path) {
        if let Some(routed) = self.router.get().and_then(|r| r.namespace(ns)) {
            return routed;
        }
        (
            self.store.clone(),
            self.root.clone().join("ns").join(ns.hex()),
        )
    }

    pub fn pool(&self) -> &AnyPool {
        &self.pool
    }

    pub fn refs(&self) -> &Arc<SqlRefs> {
        &self.refs
    }

    /// Drops the cached entry for `name` (after an init, a delete or a rename).
    pub fn forget(&self, name: &RepoName) {
        self.open.remove(name.as_str());
    }

    /// [`guard_repo`], also dropping the cached entry when it is stale.
    pub async fn guard(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Any>,
        entry: &RepoEntry,
    ) -> Result<(), tonic::Status> {
        let checked = guard_repo(tx, entry).await;
        if checked.is_err() {
            self.forget(&entry.name);
        }
        checked
    }

    /// The repository `name`. A fork whose copy has not finished is not found: it is readable
    /// only once complete (SPEC §3.3).
    pub async fn open(&self, name: &RepoName) -> Result<Arc<RepoEntry>, RegistryError> {
        match self.lookup(name).await? {
            (entry, true) => Ok(entry),
            (_, false) => Err(RegistryError::NotFound(name.to_string())),
        }
    }

    /// As [`open`](Self::open), also for a fork still being copied; the flag says whether the
    /// repository is ready. For `RepoAdmin` and the copy job only.
    pub(crate) async fn open_unready(
        &self,
        name: &RepoName,
    ) -> Result<(Arc<RepoEntry>, bool), RegistryError> {
        self.lookup(name).await
    }

    async fn lookup(&self, name: &RepoName) -> Result<(Arc<RepoEntry>, bool), RegistryError> {
        let cached = self.open.get(name.as_str());
        if let Some(c) = &cached
            && c.checked.elapsed() < self.ttl
        {
            return Ok((c.entry.clone(), c.ready));
        }
        let row = sqlx::query(
            "SELECT id, ns, mode, default_branch,              CAST(CASE WHEN ready THEN 1 ELSE 0 END AS BIGINT) AS ready              FROM repos WHERE name = $1",
        )
        .bind(name.as_str())
        .fetch_optional(&self.pool)
        .await?;
        let Some(row) = row else {
            self.open.remove(name.as_str());
            return Err(RegistryError::NotFound(name.to_string()));
        };
        let id: i64 = row.try_get("id")?;
        let ns_bytes: Vec<u8> = row.try_get("ns")?;
        let mode: String = row.try_get("mode")?;
        let default_branch: String = row.try_get("default_branch")?;
        let ready = row.try_get::<i64, _>("ready")? != 0;
        let ns = Ns::from_bytes(&ns_bytes)
            .ok_or_else(|| RegistryError::Invalid("repos.ns is not 16 bytes".into()))?;
        let mode = RepoMode::parse(&mode)?;
        let routed = match self.router.get() {
            Some(r) => r
                .route(id, ns)
                .await
                .map_err(|m| RegistryError::Store(format!("{STORAGE_UNAVAILABLE}{m}")))?,
            None => None,
        };
        let stamp = routed.as_ref().map_or(0, |r| r.stamp);
        // The same repository keeps its engine (and its caches); a row that changed under the
        // name (deleted and created again, another namespace, another storage route) gets a
        // fresh entry.
        let entry = match cached {
            Some(c)
                if c.entry.key.0 == id
                    && c.entry.ns == ns
                    && c.entry.mode == mode
                    && c.entry.default_branch == default_branch
                    && c.entry.route_stamp == stamp =>
            {
                c.entry
            }
            _ => Arc::new(self.build(name, RepoKey(id), ns, mode, default_branch, routed)),
        };
        self.open.insert(
            name.to_string(),
            Cached {
                entry: entry.clone(),
                ready,
                checked: Instant::now(),
            },
        );
        Ok((entry, ready))
    }

    /// Creates the repository `name` with a fresh namespace.
    pub async fn create(
        &self,
        name: &RepoName,
        new: &NewRepo,
    ) -> Result<Arc<RepoEntry>, RegistryError> {
        let branch_ok = new.default_branch != "HEAD"
            && !new.default_branch.starts_with('-')
            && RefName::new(format!("refs/heads/{}", new.default_branch).into_bytes()).is_ok();
        if !branch_ok {
            return Err(RegistryError::Invalid("default branch name".into()));
        }
        let inserted = sqlx::query(
            "INSERT INTO repos (name, ns, mode, default_branch, created) VALUES ($1, $2, $3, $4, $5)",
        )
        .bind(name.as_str())
        .bind(Ns::random().as_bytes().to_vec())
        .bind(new.mode.as_str())
        .bind(&new.default_branch)
        .bind(now())
        .execute(&self.pool)
        .await;
        match inserted {
            Ok(_) => {
                self.forget(name);
                self.open(name).await
            }
            Err(sqlx::Error::Database(d)) if d.is_unique_violation() => {
                Err(RegistryError::Exists(name.to_string()))
            }
            Err(e) => Err(e.into()),
        }
    }

    fn build(
        &self,
        name: &RepoName,
        key: RepoKey,
        ns: Ns,
        mode: RepoMode,
        default_branch: String,
        routed: Option<RoutedNs>,
    ) -> RepoEntry {
        let route_stamp = routed.as_ref().map_or(0, |r| r.stamp);
        let (store, root) = match routed {
            Some(r) => (r.store, r.root),
            None => (
                self.store.clone(),
                self.root.clone().join("ns").join(ns.hex()),
            ),
        };
        let objects = Arc::new(BucketObjects::new(store, root));
        RepoEntry {
            name: name.clone(),
            key,
            ns,
            mode,
            default_branch,
            backend: Arc::new(EngineBackend::new(
                name.clone(),
                key,
                objects.clone(),
                self.refs.clone(),
            )),
            objects,
            git_map: Arc::new(SqlGitMap::new(self.pool.clone(), ns)),
            route_stamp,
        }
    }
}

/// Checks, inside `tx`, that the `repos` row `entry` was resolved from still exists with the same
/// id and the same name, and holds it so it cannot change before `tx` ends: `FOR SHARE` on
/// Postgres (a concurrent rename or delete waits for `tx`, or `tx` waits for it and then sees the
/// new row), the write lock of `BEGIN IMMEDIATE` on SQLite. Every mutation calls it first. A
/// missing row is `NOT_FOUND` and a renamed one `FAILED_PRECONDITION`; nothing has been written
/// by then, and the caller drops `tx`. Ids are never reused, so an equal id is the same
/// repository.
pub(crate) async fn guard_repo(
    tx: &mut sqlx::Transaction<'_, sqlx::Any>,
    entry: &RepoEntry,
) -> Result<(), tonic::Status> {
    let query = if tx.backend_name() == "PostgreSQL" {
        "SELECT name FROM repos WHERE id = $1 FOR SHARE"
    } else {
        "SELECT name FROM repos WHERE id = $1"
    };
    let name: Option<String> = sqlx::query_scalar(query)
        .bind(entry.key.0)
        .fetch_optional(&mut **tx)
        .await
        .map_err(|e| {
            tracing::error!(error = %e, "repository check failed");
            tonic::Status::internal("repository check failed")
        })?;
    match name {
        None => Err(tonic::Status::not_found("no such repository")),
        Some(n) if n != entry.name.as_str() => Err(tonic::Status::failed_precondition(
            "the repository was renamed or replaced; retry",
        )),
        Some(_) => Ok(()),
    }
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

/// `GitMapSink` over the `git_map` table (SPEC §8.2).
pub struct SqlGitMap {
    pool: AnyPool,
    ns: Ns,
}

impl SqlGitMap {
    pub fn new(pool: AnyPool, ns: Ns) -> Self {
        Self { pool, ns }
    }
}

fn kind_code(k: asc_core::git::ObjectKind) -> i16 {
    use asc_core::git::ObjectKind::{Blob, Commit, Tag, Tree};
    match k {
        Blob => 1,
        Tree => 2,
        Commit => 3,
        Tag => 4,
    }
}

#[async_trait]
impl GitMapSink for SqlGitMap {
    async fn put(&self, rows: &[GitMapRow]) -> Result<(), DeriveError> {
        let sink = |e: sqlx::Error| DeriveError::Sink(e.to_string());
        if rows.is_empty() {
            return Ok(());
        }
        let mut tx = self.pool.begin().await.map_err(sink)?;
        // One statement per chunk: a round trip per row dominates a large import.
        for chunk in rows.chunks(GIT_MAP_BATCH) {
            let mut sql = String::from("INSERT INTO git_map (ns, git, kind, asc) VALUES ");
            for i in 0..chunk.len() {
                if i > 0 {
                    sql.push_str(", ");
                }
                let b = i * 4;
                sql.push_str(&format!("(${}, ${}, ${}, ${})", b + 1, b + 2, b + 3, b + 4));
            }
            sql.push_str(" ON CONFLICT (ns, git, kind) DO NOTHING");
            let mut q = sqlx::query(sqlx::AssertSqlSafe(sql));
            for r in chunk {
                q = q
                    .bind(self.ns.as_bytes().to_vec())
                    .bind(r.git.as_bytes().to_vec())
                    .bind(i32::from(kind_code(r.kind)))
                    .bind(r.asc.to_vec());
            }
            q.execute(&mut *tx).await.map_err(sink)?;
        }
        tx.commit().await.map_err(sink)
    }
}

// ------------------------------------------------------------------ extension registries

/// Collects the `axum` routers that units contribute (health, `/mcp`, `/v8`, `/seeds`, …).
#[derive(Default)]
pub struct HttpRoutes {
    router: Mutex<axum::Router>,
}

impl HttpRoutes {
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds `router`. Two units must not claim the same path: `axum` panics on a conflict, which
    /// surfaces at startup rather than serving one of them silently.
    pub fn register(&self, router: axum::Router) {
        let mut g = self.router.lock().unwrap_or_else(PoisonError::into_inner);
        let cur = std::mem::take(&mut *g);
        *g = cur.merge(router);
    }

    /// Everything registered so far.
    pub fn router(&self) -> axum::Router {
        self.router
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

/// Collects the tonic services that units contribute.
#[derive(Default)]
pub struct GrpcServices {
    builder: Mutex<tonic::service::RoutesBuilder>,
}

impl GrpcServices {
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds a service; a second service with the same name replaces the first.
    pub fn register<S>(&self, svc: S)
    where
        S: tower::Service<axum::http::Request<tonic::body::Body>, Error = std::convert::Infallible>
            + NamedService
            + Clone
            + Send
            + Sync
            + 'static,
        S::Response: axum::response::IntoResponse,
        S::Future: Send + 'static,
    {
        self.builder
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .add_service(svc);
    }

    /// Everything registered so far.
    pub fn routes(&self) -> tonic::service::Routes {
        self.builder
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
            .routes()
    }
}

/// Builds the live git smart-HTTP routes (SPEC §25) around the server's authorizer.
pub type GitRouterFactory = Arc<dyn Fn(Arc<dyn Authorizer>) -> axum::Router + Send + Sync>;

/// Holds the live git routes, set once by `git_http::register` when the layer is running.
/// `build_routes` serves them in place of the disabled stub (`authn::git_router`) when set.
#[derive(Default)]
pub struct GitRoutesSlot {
    factory: std::sync::OnceLock<GitRouterFactory>,
}

impl GitRoutesSlot {
    /// Installs the factory; fails (returning it) when one is already set.
    pub fn set(&self, f: GitRouterFactory) -> Result<(), GitRouterFactory> {
        self.factory.set(f)
    }

    /// The live routes for `authorizer`, or `None` when the layer is not running.
    pub fn router(&self, authorizer: Arc<dyn Authorizer>) -> Option<axum::Router> {
        self.factory.get().map(|f| f(authorizer))
    }
}

/// What every unit's one-line registration receives.
pub struct Extensions {
    /// One handler per RPC group (U42).
    pub handlers: Arc<RepoHandlers>,
    pub http: HttpRoutes,
    pub grpc: GrpcServices,
    /// The `Projects` service handler (G09); `build_routes` serves it behind the authorizer.
    pub projects: asc_rpc::projects::ProjectsSlot,
    pub text_budget: crate::TextBudgetSlot,
    /// The live git smart-HTTP routes (U81); the disabled stub serves while this is empty.
    pub git: GitRoutesSlot,
    /// Ingest and `UpdateRefs` hook points (U43b).
    pub pipeline: Arc<crate::update::PipelineHooks>,
    /// The `Changes` service handler (C19); `build_routes` serves it behind the authorizer.
    pub changes: crate::changes::rpc::ChangesSlot,
    /// The `Cloud` service handler (B09); `build_routes` serves it behind the authorizer.
    pub cloud: crate::cloud::rpc::CloudSlot,
    /// The `CodeIntel` service handler (I16); `build_routes` serves it behind the authorizer.
    pub codeintel: crate::codeintel::rpc::CodeIntelSlot,
    /// The `AiProviders` hub (bring-your-own AI keys); `build_grpc` serves it behind the authorizer.
    pub ai: crate::ai::AiSlot,
    /// The `Agents` and `AgentTokens` hub; `build_grpc` serves them behind the authorizer.
    pub agents: crate::agents::AgentsSlot,
    /// The `Workflows` hub; `build_grpc` serves it behind the authorizer.
    pub workflows: crate::workflows::WorkflowsSlot,
    /// The `Review`, `Inbox` and `Ci` handlers; `build_routes` serves them behind the authorizer.
    pub review: crate::review::ReviewSlot,
    /// The `Audit` and `Stacks` (ref log, undo) state; `build_routes` serves them behind the authorizer.
    pub audit: crate::audit::AuditSlot,
    /// The `CiInsights` hub (failure groups, flaky tests); `build_routes` serves it behind the authorizer.
    pub insights: crate::ci::insights::InsightsSlot,
    /// Bring-your-own storage and the vault; `build_routes` serves `Storage` and `Vault`.
    pub byos: crate::storage::ByosSlot,
    /// Dependency scanning and update proposals; `build_grpc` serves `DepsService`.
    pub deps: crate::deps::DepsSlot,
    /// Policy, secret scanning and signing; `build_routes` serves `Policy`, `SecretScan` and `Signing`.
    pub security: crate::policy::SecuritySlot,
    /// Backups, usage, organisation settings, sessions, data export; `build_grpc` serves them.
    pub settings: crate::settings::SettingsSlot,
    /// The usage hub, which counts the bytes the server serves.
    pub usage: Arc<crate::usage::UsageSlot>,
    /// Raised while a restore is staged: no ref moves and no object is accepted until the restart.
    pub read_only: Arc<std::sync::atomic::AtomicBool>,
}

impl Extensions {
    pub fn new() -> Self {
        Self {
            handlers: Arc::new(RepoHandlers::new()),
            http: HttpRoutes::new(),
            grpc: GrpcServices::new(),
            projects: Default::default(),
            text_budget: crate::TextBudgetSlot::default(),
            git: GitRoutesSlot::default(),
            pipeline: Arc::default(),
            changes: Default::default(),
            cloud: Default::default(),
            codeintel: Default::default(),
            ai: Default::default(),
            agents: Default::default(),
            workflows: Default::default(),
            review: Default::default(),
            audit: Default::default(),
            insights: Default::default(),
            byos: Default::default(),
            security: Default::default(),
            settings: Default::default(),
            usage: Arc::default(),
            read_only: Arc::default(),
            deps: Default::default(),
        }
    }
}

impl Default for Extensions {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::migrate;
    use asc_core::git::ObjectKind;
    use asc_core::id::GitOid;
    use asc_engine::derive::GitMapRow;

    async fn registry() -> Registry {
        let pool = migrate::connect("sqlite::memory:", 1).await.unwrap();
        migrate::migrate(&pool).await.unwrap();
        Registry::new(pool, "memory:///").unwrap()
    }

    fn name(s: &str) -> RepoName {
        RepoName::parse(s).unwrap()
    }

    #[tokio::test]
    async fn create_open_and_conflict() {
        let r = registry().await;
        assert!(matches!(
            r.open(&name("asc/dds")).await,
            Err(RegistryError::NotFound(_))
        ));
        let a = r
            .create(&name("asc/dds"), &NewRepo::default())
            .await
            .unwrap();
        assert_eq!(a.default_branch, "master");
        assert_eq!(a.mode, RepoMode::Primary);
        assert!(matches!(
            r.create(&name("asc/dds"), &NewRepo::default()).await,
            Err(RegistryError::Exists(_))
        ));
        let again = r.open(&name("asc/dds")).await.unwrap();
        assert!(Arc::ptr_eq(&a, &again));
        r.forget(&name("asc/dds"));
        let fresh = r.open(&name("asc/dds")).await.unwrap();
        assert_eq!(fresh.key, a.key);
        assert_eq!(fresh.ns, a.ns);
        let bad = NewRepo {
            default_branch: "a..b".into(),
            ..NewRepo::default()
        };
        assert!(matches!(
            r.create(&name("asc/x"), &bad).await,
            Err(RegistryError::Invalid(_))
        ));
    }

    #[tokio::test]
    async fn repos_have_separate_namespaces() {
        let r = registry().await;
        let a = r.create(&name("o/a"), &NewRepo::default()).await.unwrap();
        let b = r.create(&name("o/b"), &NewRepo::default()).await.unwrap();
        assert_ne!(a.ns, b.ns);
        assert_ne!(a.key, b.key);
        assert_eq!(a.ns.hex().len(), 32);
    }

    #[tokio::test]
    async fn git_map_sink_is_idempotent_and_namespaced() {
        let r = registry().await;
        let a = r.create(&name("o/a"), &NewRepo::default()).await.unwrap();
        let b = r.create(&name("o/b"), &NewRepo::default()).await.unwrap();
        let row = GitMapRow {
            git: GitOid::from_bytes([7; 20]),
            kind: ObjectKind::Tree,
            asc: [9; 32],
        };
        a.git_map.put(&[row, row]).await.unwrap();
        a.git_map.put(&[row]).await.unwrap();
        let count = |ns: Ns| {
            let pool = r.pool().clone();
            async move {
                sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM git_map WHERE ns = $1")
                    .bind(ns.as_bytes().to_vec())
                    .fetch_one(&pool)
                    .await
                    .unwrap()
            }
        };
        assert_eq!(count(a.ns).await, 1);
        assert_eq!(count(b.ns).await, 0);
    }

    #[tokio::test]
    async fn http_and_grpc_registries_merge() {
        use tower::ServiceExt;
        let ext = Extensions::new();
        ext.http
            .register(axum::Router::new().route("/a", axum::routing::get(|| async { "a" })));
        ext.http
            .register(axum::Router::new().route("/b", axum::routing::get(|| async { "b" })));
        let app = ext.http.router();
        for (path, want) in [("/a", 200), ("/b", 200), ("/c", 404)] {
            let res = app
                .clone()
                .oneshot(
                    axum::http::Request::get(path)
                        .body(axum::body::Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(res.status().as_u16(), want, "{path}");
        }
        struct L;
        impl asc_rpc::server::LandingHandler for L {}
        ext.handlers.set_landing(Arc::new(L)).unwrap();
        assert!(ext.handlers.set_landing(Arc::new(L)).is_err());
    }

    #[tokio::test]
    async fn a_plain_http_store_endpoint_is_refused() {
        // U89c: the server's own bucket has no private-network http exception.
        let pool = migrate::connect("sqlite::memory:", 1).await.unwrap();
        let err = Registry::new(pool, "s3://b?endpoint=http://garage-abc:3900&region=garage")
            .err()
            .expect("plain http to a private host")
            .to_string();
        assert!(err.contains("http://garage-abc:3900"), "{err}");
    }
}
