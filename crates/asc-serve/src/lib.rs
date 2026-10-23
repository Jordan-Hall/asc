//! asc-serve: the `asc serve` server (SPEC §8).
//!
//! Later units extend the server by one registration line each in [`register_all`]: an RPC
//! handler group on `Extensions::handlers`, a router on `Extensions::http`, a service on
//! `Extensions::grpc`, or the real [`TextBudget`].

pub mod config;
pub mod forgejo;
pub mod gc;
pub mod graph_sql;
pub mod http;
pub mod idmode;
pub mod ingest;
pub mod land;
pub mod limits;
pub mod mainline;
pub mod migrate;
pub mod mirror;
pub mod outbox;
pub mod read;
pub mod registry;
pub mod seeds;
pub mod turbo;
pub mod update;

use std::future::Future;
use std::path::Path as FsPath;
use std::sync::{Arc, OnceLock};

use async_trait::async_trait;
use asc_rpc::auth::{Access, AuthError, Authorizer, BearerToken, Principal};
use asc_rpc::server::{RepoService, ServerInfo};
use tonic::service::Routes;
use tonic::transport::{Identity, Server, ServerTlsConfig};

pub use config::{ConfigError, Role, ServeConfig};
pub use registry::{Extensions, Registry};

/// Pool size of the server's SQL connections.
const DB_CONNECTIONS: u32 = 16;
/// Largest accepted TLS PEM file.
const MAX_PEM_BYTES: u64 = 1024 * 1024;

/// Why the server could not start or stopped.
#[derive(Debug, thiserror::Error)]
pub enum ServeError {
    #[error(transparent)]
    Config(#[from] ConfigError),
    #[error(transparent)]
    Migrate(#[from] migrate::MigrateError),
    #[error(transparent)]
    Registry(#[from] registry::RegistryError),
    #[error("database: {0}")]
    Sql(#[from] sqlx::Error),
    #[error("tls: {0}")]
    Tls(String),
    #[error("admin listener: {0}")]
    Admin(String),
    #[error("role {0:?} is not served by this build yet")]
    RoleUnavailable(Role),
    #[error("transport: {0}")]
    Transport(#[from] tonic::transport::Error),
}

// ------------------------------------------------------------------------------ text budget

/// Held while a whole-file buffer is alive; releasing it returns the bytes to the budget.
pub struct TextGuard(#[allow(dead_code)] Option<Box<dyn Send + Sync>>);

impl TextGuard {
    /// A guard that holds nothing (the unlimited default).
    pub fn unlimited() -> Self {
        Self(None)
    }

    /// A guard that keeps `permit` alive until dropped.
    pub fn holding(permit: impl Send + Sync + 'static) -> Self {
        Self(Some(Box::new(permit)))
    }
}

/// The budget was exhausted and the wait expired (maps to `RESOURCE_EXHAUSTED`).
#[derive(Debug, thiserror::Error)]
#[error("text memory budget exhausted")]
pub struct TextBudgetExhausted;

/// Bounds the bytes of whole files the server buffers at once (`limits.text_memory`, SPEC §16.4).
/// U47, U48a, U48b and U67 call [`acquire`](Self::acquire) before buffering a whole file.
#[async_trait]
pub trait TextBudget: Send + Sync {
    async fn acquire(&self, bytes: u64) -> Result<TextGuard, TextBudgetExhausted>;
}

struct Unlimited;

#[async_trait]
impl TextBudget for Unlimited {
    async fn acquire(&self, _bytes: u64) -> Result<TextGuard, TextBudgetExhausted> {
        Ok(TextGuard::unlimited())
    }
}

/// Holds the process-wide [`TextBudget`]: unlimited until U69 sets the real one, once.
#[derive(Default)]
pub struct TextBudgetSlot {
    budget: OnceLock<Arc<dyn TextBudget>>,
}

impl TextBudgetSlot {
    /// Installs the budget; fails (returning it) when one is already set.
    pub fn set(&self, b: Arc<dyn TextBudget>) -> Result<(), Arc<dyn TextBudget>> {
        self.budget.set(b)
    }

    pub async fn acquire(&self, bytes: u64) -> Result<TextGuard, TextBudgetExhausted> {
        match self.budget.get() {
            Some(b) => b.acquire(bytes).await,
            None => Unlimited.acquire(bytes).await,
        }
    }
}

// ------------------------------------------------------------------------------ authz

/// Refuses everything. The server uses it until a real authorizer (Forgejo, U44) is supplied,
/// so a misconfigured deployment fails closed.
pub struct DenyAll;

#[async_trait]
impl Authorizer for DenyAll {
    async fn authorize(
        &self,
        token: Option<&BearerToken>,
        _repo: &str,
        _access: Access,
    ) -> Result<Principal, AuthError> {
        Err(if token.is_some() {
            AuthError::Forbidden
        } else {
            AuthError::Unauthenticated
        })
    }
}

// ------------------------------------------------------------------------------ startup

/// One registration line per unit goes here (PLAN "Stubs and extension points").
fn register_all(ext: &Extensions, registry: &Arc<Registry>, limits: &config::Limits) {
    limits::register(ext, limits);
    update::register(ext, registry);
    read::register(ext, registry);
}

/// Builds the combined gRPC and HTTP routes for `ext`.
pub fn build_routes(ext: &Extensions, authorizer: Arc<dyn Authorizer>, info: ServerInfo) -> Routes {
    ext.grpc
        .register(RepoService::new(ext.handlers.clone(), authorizer, info).into_service());
    let router = ext
        .grpc
        .routes()
        .into_axum_router()
        .merge(ext.http.router())
        // tonic's own fallback answers an unknown path with `200` and a gRPC status; the public
        // listener answers a plain 404 instead (`/metrics` lives on the admin listener only).
        .fallback(|| async { axum::http::StatusCode::NOT_FOUND });
    Routes::from(router)
}

fn read_pem(path: &FsPath) -> Result<Vec<u8>, ServeError> {
    let tls = |e: std::io::Error| ServeError::Tls(format!("{}: {e}", path.display()));
    let file = std::fs::File::open(path).map_err(tls)?;
    let mut buf = Vec::new();
    std::io::Read::read_to_end(&mut std::io::Read::take(file, MAX_PEM_BYTES + 1), &mut buf)
        .map_err(tls)?;
    if buf.len() as u64 > MAX_PEM_BYTES {
        return Err(ServeError::Tls(format!("{} is too large", path.display())));
    }
    Ok(buf)
}

/// Reads the admin bearer token: the first line of the file, trimmed, never empty.
fn read_token(path: &FsPath) -> Result<Arc<str>, ServeError> {
    let bytes =
        read_pem(path).map_err(|_| ServeError::Admin(format!("{}: unreadable", path.display())))?;
    let text = String::from_utf8(bytes)
        .map_err(|_| ServeError::Admin(format!("{}: not UTF-8", path.display())))?;
    let token = text.lines().next().unwrap_or("").trim();
    if token.is_empty() {
        return Err(ServeError::Admin(format!(
            "{}: empty token",
            path.display()
        )));
    }
    Ok(Arc::from(token))
}

/// Runs the primary server until `shutdown` resolves. The schema must already be migrated.
pub async fn serve(
    cfg: ServeConfig,
    authorizer: Arc<dyn Authorizer>,
    shutdown: impl Future<Output = ()>,
) -> Result<(), ServeError> {
    cfg.validate()?;
    if cfg.role != Role::Primary {
        return Err(ServeError::RoleUnavailable(cfg.role));
    }
    let (Some(db), Some(store)) = (&cfg.db, &cfg.store) else {
        return Err(ConfigError::Invalid("`store` and `db` are required".into()).into());
    };
    let pool = migrate::connect(db, DB_CONNECTIONS).await?;
    migrate::ensure_current(&pool).await?;
    let registry = Arc::new(Registry::new(pool.clone(), store)?);
    let metrics = Arc::new(http::Metrics::new());
    let outbox = Arc::new(outbox::Outbox::new(
        pool.clone(),
        metrics.clone(),
        outbox::OutboxConfig::default(),
    ));
    let ext = Extensions::new();
    register_all(&ext, &registry, &cfg.limits);
    forgejo::register(&ext, &registry, &outbox, cfg.forgejo.as_ref())
        .map_err(|e| ConfigError::Invalid(e.to_string()))?;
    let mirror = mirror::register(&ext, &registry, &cfg)
        .map_err(|e| ConfigError::Invalid(e.to_string()))?;
    let well_known = http::WellKnown {
        server: cfg.public_url.clone(),
        ..http::WellKnown::default()
    };
    let http_state = http::HttpState::new(pool, outbox.clone(), metrics.clone(), well_known);
    http::register(&ext, http_state.clone());
    let turbo = Arc::new(
        turbo::TurboState::open(&cfg, authorizer.clone(), metrics)
            .map_err(|e| ConfigError::Invalid(e.to_string()))?,
    );
    turbo::register(&ext, turbo.clone());
    // Admission runs before authentication; `guarded_routes` also lets it learn which bearer
    // tokens the authorizer accepts, so that only those callers get a bucket of their own.
    let admission = Arc::new(limits::Admission::new(&cfg.limits));
    let routes = limits::guarded_routes(&ext, authorizer, admission, ServerInfo::default());

    // The admin listener is bound before the public one so a bad address fails startup.
    let admin_token = match &cfg.admin.token_file {
        Some(p) => Some(read_token(p)?),
        None => None,
    };
    let admin_listener = tokio::net::TcpListener::bind(cfg.admin.listen)
        .await
        .map_err(|e| ServeError::Admin(format!("{}: {e}", cfg.admin.listen)))?;
    let (admin_stop, admin_stopped) = tokio::sync::oneshot::channel::<()>();
    let admin = tokio::spawn(async move {
        axum::serve(admin_listener, http::admin_router(http_state, admin_token))
            .with_graceful_shutdown(async {
                let _ = admin_stopped.await;
            })
            .await
    });
    tracing::info!(listen = %cfg.admin.listen, "asc serve admin listening");

    let mut server = Server::builder().accept_http1(true);
    if let Some(tls) = &cfg.tls {
        let identity = Identity::from_pem(read_pem(&tls.cert)?, read_pem(&tls.key)?);
        server = server.tls_config(ServerTlsConfig::new().identity(identity))?;
    }
    tracing::info!(listen = %cfg.listen, "asc serve listening");
    let worker = tokio::spawn(outbox.run());
    let sweeper = turbo.spawn_sweeper();
    let poller = mirror.as_ref().map(mirror::Mirror::spawn_poller);
    // Collection deletes data, so it runs only when `[gc] enabled` is set.
    let collector = if cfg.gc.enabled {
        let service = Arc::new(gc::GcService::new(
            registry.clone(),
            ext.pipeline.clone(),
            cfg.gc.to_config()?,
        ));
        Some(service.spawn_worker(std::time::Duration::from_secs(
            cfg.gc.interval_hours * 3600,
        )))
    } else {
        None
    };
    let served = server
        .add_routes(routes)
        .serve_with_shutdown(cfg.listen, async move {
            shutdown.await;
            let _ = admin_stop.send(());
        })
        .await;
    // Delivery is at least once, so stopping the worker mid-row is safe.
    worker.abort();
    sweeper.abort();
    if let Some(p) = poller {
        p.abort();
    }
    if let Some(c) = collector {
        c.abort();
    }
    admin.abort();
    served?;
    Ok(())
}

/// `asc serve migrate`: applies the schema, then exits.
pub async fn run_migrate(cfg: &ServeConfig) -> Result<(), ServeError> {
    let Some(db) = &cfg.db else {
        return Err(ConfigError::Invalid("`db` is required".into()).into());
    };
    let pool = migrate::connect(db, 2).await?;
    migrate::migrate(&pool).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn text_budget_is_unlimited_until_set_and_set_once() {
        let slot = TextBudgetSlot::default();
        slot.acquire(u64::MAX).await.unwrap();
        struct Never;
        #[async_trait]
        impl TextBudget for Never {
            async fn acquire(&self, _: u64) -> Result<TextGuard, TextBudgetExhausted> {
                Err(TextBudgetExhausted)
            }
        }
        assert!(slot.set(Arc::new(Never)).is_ok());
        assert!(slot.set(Arc::new(Never)).is_err());
        assert!(slot.acquire(1).await.is_err());
    }

    #[tokio::test]
    async fn deny_all_fails_closed() {
        let t = BearerToken::new("x").unwrap();
        assert_eq!(
            DenyAll
                .authorize(Some(&t), "a/b", Access::Read)
                .await
                .unwrap_err(),
            AuthError::Forbidden
        );
        assert_eq!(
            DenyAll
                .authorize(None, "a/b", Access::Read)
                .await
                .unwrap_err(),
            AuthError::Unauthenticated
        );
    }

    #[tokio::test]
    async fn migrate_then_serve_answers_health_and_denies_rpcs() {
        use tower::ServiceExt;
        let dir = tempfile::tempdir().unwrap();
        let db = format!(
            "sqlite:{}?mode=rwc",
            dir.path()
                .join("asc.db")
                .display()
                .to_string()
                .replace('\\', "/")
        );
        let store = url::Url::from_directory_path(dir.path().join("bucket"))
            .map(|u| u.to_string())
            .unwrap_or_default();
        std::fs::create_dir_all(dir.path().join("bucket")).unwrap();
        let cfg = ServeConfig::parse(&format!("db = {db:?}\nstore = {store:?}")).unwrap();
        // Serving before migrating is refused.
        let pool = migrate::connect(&db, 1).await.unwrap();
        assert!(migrate::ensure_current(&pool).await.is_err());
        run_migrate(&cfg).await.unwrap();
        migrate::ensure_current(&pool).await.unwrap();

        let ext = Extensions::new();
        let metrics = Arc::new(http::Metrics::new());
        let outbox = Arc::new(outbox::Outbox::new(
            pool.clone(),
            metrics.clone(),
            outbox::OutboxConfig::default(),
        ));
        http::register(
            &ext,
            http::HttpState::new(pool, outbox, metrics, http::WellKnown::default()),
        );
        let routes = build_routes(&ext, Arc::new(DenyAll), ServerInfo::default());
        let app = routes.into_axum_router();
        let res = app
            .clone()
            .oneshot(
                axum::http::Request::get("/healthz")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status().as_u16(), 200);
        let res = app
            .oneshot(
                axum::http::Request::get("/metrics")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            res.status().as_u16(),
            404,
            "metrics are not on the public router"
        );
    }

    #[test]
    fn admin_token_file_is_trimmed_and_nonempty() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("t");
        std::fs::write(&p, "tok\n").unwrap();
        assert_eq!(&*read_token(&p).unwrap(), "tok");
        std::fs::write(&p, "\n").unwrap();
        assert!(read_token(&p).is_err());
        assert!(read_token(&dir.path().join("none")).is_err());
    }
}
