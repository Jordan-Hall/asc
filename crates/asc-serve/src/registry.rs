//! The repository registry and the server's extension registries (PLAN U43a).
//!
//! [`Registry`] turns a repository name into an engine over its bucket namespace
//! (`<root>/ns/<ns>`), the shared `SqlRefs` and the SQL git map. [`HttpRoutes`] and
//! [`GrpcServices`] collect what later units add; `lib.rs` merges them at startup.

use std::fmt;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use asc_core::path::RefName;
use asc_engine::derive::{Error as DeriveError, GitMapRow, GitMapSink};
use asc_rpc::server::{EngineBackend, RepoHandlers, RepoName};
use asc_store::bucket::BucketObjects;
use asc_store::refs::RepoKey;
use asc_store::sqlrefs::SqlRefs;
use object_store::ObjectStore;
use object_store::path::Path;
use sqlx::{AnyPool, Row};
use tonic::server::NamedService;

/// Repositories kept open at once; the least recently used is dropped beyond this.
pub const MAX_OPEN_REPOS: usize = 1024;
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
}

/// Opens repositories by name.
pub struct Registry {
    pool: AnyPool,
    refs: Arc<SqlRefs>,
    store: Arc<dyn ObjectStore>,
    root: Path,
    open: quick_cache::sync::Cache<String, Arc<RepoEntry>>,
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
        })
    }

    pub fn pool(&self) -> &AnyPool {
        &self.pool
    }

    pub fn refs(&self) -> &Arc<SqlRefs> {
        &self.refs
    }

    /// Drops the cached entry for `name` (after a delete or rename).
    pub fn forget(&self, name: &RepoName) {
        self.open.remove(name.as_str());
    }

    /// The repository `name`.
    pub async fn open(&self, name: &RepoName) -> Result<Arc<RepoEntry>, RegistryError> {
        if let Some(e) = self.open.get(name.as_str()) {
            return Ok(e);
        }
        let row = sqlx::query("SELECT id, ns, mode, default_branch FROM repos WHERE name = $1")
            .bind(name.as_str())
            .fetch_optional(&self.pool)
            .await?
            .ok_or_else(|| RegistryError::NotFound(name.to_string()))?;
        let id: i64 = row.try_get("id")?;
        let ns_bytes: Vec<u8> = row.try_get("ns")?;
        let mode: String = row.try_get("mode")?;
        let default_branch: String = row.try_get("default_branch")?;
        let ns = Ns::from_bytes(&ns_bytes)
            .ok_or_else(|| RegistryError::Invalid("repos.ns is not 16 bytes".into()))?;
        let entry = Arc::new(self.build(
            name,
            RepoKey(id),
            ns,
            RepoMode::parse(&mode)?,
            default_branch,
        ));
        self.open.insert(name.to_string(), entry.clone());
        Ok(entry)
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
            Ok(_) => self.open(name).await,
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
    ) -> RepoEntry {
        let root = self.root.clone().join("ns").join(ns.hex());
        let objects = Arc::new(BucketObjects::new(self.store.clone(), root));
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
        }
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
        let mut tx = self.pool.begin().await.map_err(sink)?;
        for chunk in rows.chunks(GIT_MAP_BATCH) {
            for r in chunk {
                sqlx::query(
                    "INSERT INTO git_map (ns, git, kind, asc) VALUES ($1, $2, $3, $4) \
                     ON CONFLICT (ns, git, kind) DO NOTHING",
                )
                .bind(self.ns.as_bytes().to_vec())
                .bind(r.git.as_bytes().to_vec())
                .bind(i32::from(kind_code(r.kind)))
                .bind(r.asc.to_vec())
                .execute(&mut *tx)
                .await
                .map_err(sink)?;
            }
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

/// What every unit's one-line registration receives.
pub struct Extensions {
    /// One handler per RPC group (U42).
    pub handlers: Arc<RepoHandlers>,
    pub http: HttpRoutes,
    pub grpc: GrpcServices,
    pub text_budget: crate::TextBudgetSlot,
    /// Ingest and `UpdateRefs` hook points (U43b).
    pub pipeline: Arc<crate::update::PipelineHooks>,
}

impl Extensions {
    pub fn new() -> Self {
        Self {
            handlers: Arc::new(RepoHandlers::new()),
            http: HttpRoutes::new(),
            grpc: GrpcServices::new(),
            text_budget: crate::TextBudgetSlot::default(),
            pipeline: Arc::default(),
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
}
