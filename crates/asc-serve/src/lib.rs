//! asc-serve: the `asc serve` server (SPEC §8).
//!
//! Later units extend the server by one registration line each in [`register_all`]: an RPC
//! handler group on `Extensions::handlers`, a router on `Extensions::http`, a service on
//! `Extensions::grpc`, or the real [`TextBudget`].

pub mod actions_store;
pub mod admin;
pub mod admin_cli;
pub mod agents;
pub mod ai;
pub mod alt_svc;
pub mod audit;
pub mod authn;
pub mod authz;
pub mod backup;
pub mod cache_role;
pub mod changes;
pub mod ci;
pub mod classify;
pub mod cloud;
pub mod codeintel;
pub mod commit_graph_markers;
pub mod commit_graph_rpc;
pub mod config;
pub mod expire;
#[cfg(feature = "forgejo")]
pub mod forgejo;
pub mod gc;
#[cfg(feature = "git-compat")]
pub mod git_http;
#[cfg(feature = "git-compat")]
pub mod git_push;
#[cfg(feature = "git-compat")]
pub mod git_view;
pub mod graph_sql;
pub mod h3_listener;
pub mod history;
pub mod http;
pub mod idmode;
pub mod import_push;
pub mod inbox;
pub mod ingest;
pub mod land;
pub mod landing;
pub mod lease;
pub mod limits;
pub mod logging;
pub mod mainline;
pub mod mcp;
pub mod migrate;
pub mod mirror;
pub mod mutations;
pub mod netguard;
pub mod objects;
pub mod obs;
pub mod outbox;
pub mod policy;
pub mod projects;
pub mod projects_warm;
pub mod read;
pub mod refauthz;
pub mod refpolicy;
pub mod reftl;
pub mod registry;
pub mod review;
pub mod secretscan;
pub mod seeds;
pub mod signing;
#[cfg(feature = "git-ssh")]
pub mod ssh;
pub mod storage;
pub mod turbo;
pub mod update;
pub mod vault;
pub mod watch_refs;
pub mod web;
pub mod workdir;
pub mod workflows;

use std::future::Future;
use std::path::Path as FsPath;
use std::sync::{Arc, OnceLock};

use asc_rpc::auth::{Access, AuthError, Authorizer, BearerToken, Principal};
use asc_rpc::server::{RepoService, ServerInfo};
use async_trait::async_trait;
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
    #[error("ssh listener: {0}")]
    Ssh(String),
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

/// The `CodeIntel` settings when `[codeintel] enabled` and `cache.disk_dir` names the disk the
/// replica keeps index segments on (`<disk_dir>/ci`).
fn codeintel_settings(cfg: &config::ServeConfig) -> Option<codeintel::rpc::Settings> {
    if !cfg.codeintel.enabled {
        return None;
    }
    let Some(dir) = cfg.cache.disk_dir.as_ref() else {
        tracing::warn!("[codeintel] enabled needs cache.disk_dir; CodeIntel is not served");
        return None;
    };
    let mut settings = codeintel::rpc::Settings::new(dir.join("ci"));
    settings.embed = codeintel::embedder::Setup::from_config(&cfg.codeintel.embed);
    Some(settings)
}

/// One registration line per unit goes here (PLAN "Stubs and extension points").
#[allow(clippy::too_many_arguments)] // one handle per subsystem, called from one place
fn register_all(
    ext: &Extensions,
    registry: &Arc<Registry>,
    limits: &config::Limits,
    import_root: Option<std::path::PathBuf>,
    cloud_cfg: &config::CloudConfig,
    codeintel_cfg: Option<codeintel::rpc::Settings>,
    embed: Option<Arc<dyn codeintel::worker::EmbedModel>>,
    security_cfg: (&secretscan::Config, &signing::Config),
) -> Result<(), ServeError> {
    limits::register(ext, limits);
    // `HasObjects`, `GetObjects` and `PutObjects`: what clone, fetch and push move objects with.
    objects::register(ext, registry);
    update::register(ext, registry);
    // `[cloud] max_user_refs` and `max_user_bytes` bind every user-ref push and cloud ingest,
    // with or without Forgejo (SPEC §21.3.7).
    if ext.pipeline.set_user_quota(cloud_cfg.user_quota()).is_err() {
        tracing::warn!("the user quota was already set");
    }
    landing::register(ext, registry);
    let hooks = read::register(ext, registry);
    classify::register(ext, registry, &hooks);
    let mutations = mutations::register(ext, registry, &hooks);
    // Repository policy, secret scanning and signed changes: the gate every ref update passes and
    // the `Policy`, `SecretScan` and `Signing` services (docs/BUILDOUT.md sections 1, 4 and 5).
    policy::register(
        ext,
        registry,
        &mutations,
        security_cfg.0.clone(),
        security_cfg.1.clone(),
    )
    .map_err(ConfigError::Invalid)?;
    mutations::rebase::register(ext, &mutations);
    mutations::land_merge::register(ext, &hooks);
    import_push::register(ext, registry, mutations, import_root);
    commit_graph_rpc::register(ext, registry, &hooks);
    changes::register(ext, registry, &hooks);
    cloud::rpc::register(ext, registry, cloud_cfg.limits());
    review::register(ext, registry);
    watch_refs::register(ext, registry, limits);
    // `CodeIntel` (I16) is served when `[codeintel] enabled` and the replica has a disk to keep
    // index segments on. `Semantic` needs `[codeintel.embed] dir`; without it, it answers
    // `FAILED_PRECONDITION` and says how to set it.
    if let Some(settings) = codeintel_cfg
        && let Err(e) = codeintel::rpc::register(ext, registry, settings.clone(), embed)
    {
        tracing::error!(error = %e, "CodeIntel is not served");
    }
    Ok(())
}

/// The gRPC services of `ext` behind `authorizer`. They are added to a copy of `ext.grpc`, so the
/// call can be made again with another authorizer (the public routes and the in-process routes
/// of the MCP backend).
pub fn build_grpc(ext: &Extensions, authorizer: Arc<dyn Authorizer>, info: ServerInfo) -> Routes {
    let mut grpc = ext.grpc.routes();
    if let Some(h) = ext.projects.get() {
        grpc = grpc.add_service(
            asc_rpc::projects::ProjectsService::new(h.clone(), authorizer.clone(), info.clone())
                .into_service(),
        );
    }
    if let Some(h) = ext.cloud.get() {
        grpc = grpc.add_service(
            cloud::rpc::CloudService::new(h.clone(), authorizer.clone(), info.clone())
                .into_service(),
        );
    }
    if let Some(h) = ext.changes.get() {
        grpc = grpc.add_service(
            changes::rpc::ChangesService::new(h.clone(), authorizer.clone(), info.clone())
                .into_service(),
        );
    }
    if let Some(h) = ext.review.get() {
        grpc = grpc
            .add_service(
                review::rpc::ReviewService::new(h.clone(), authorizer.clone(), info.clone())
                    .into_service(),
            )
            .add_service(
                inbox::rpc::InboxService::new(h.clone(), authorizer.clone(), info.clone())
                    .into_service(),
            )
            .add_service(
                ci::rpc::CiService::new(h.clone(), authorizer.clone(), info.clone()).into_service(),
            );
    }
    if let Some(h) = ext.codeintel.get() {
        grpc = grpc.add_service(
            codeintel::rpc::CodeIntelService::new(h.clone(), authorizer.clone(), info.clone())
                .into_service(),
        );
    }
    // `Storage` and `Vault` (bring-your-own storage): present once the feature is registered.
    if let Some(b) = ext.byos.get() {
        grpc = grpc
            .add_service(b.storage_service(authorizer.clone()))
            .add_service(b.vault_service(authorizer.clone()));
    }
    if let Some(h) = ext.insights.get() {
        grpc = grpc.add_service(
            ci::insights::rpc::CiInsightsService::new(h.clone(), authorizer.clone(), info.clone())
                .into_service(),
        );
    }
    if let Some(a) = ext.audit.get() {
        grpc = grpc
            .add_service(
                audit::rpc::AuditService::new(a.clone(), authorizer.clone(), info.clone())
                    .into_service(),
            )
            .add_service(
                audit::stacks_rpc::StacksService::new(a.clone(), authorizer.clone(), info.clone())
                    .into_service(),
            );
    }
    // `Policy`, `SecretScan` and `Signing`: present once the security state is registered.
    if let Some(s) = ext.security.get() {
        grpc = grpc
            .add_service(
                policy::rpc::PolicyService::new(s.policy.clone(), authorizer.clone(), info.clone())
                    .into_service(),
            )
            .add_service(
                secretscan::rpc::SecretScanService::new(
                    s.secrets.clone(),
                    authorizer.clone(),
                    info.clone(),
                )
                .into_service(),
            )
            .add_service(
                signing::rpc::SigningService::new(
                    s.signing.clone(),
                    authorizer.clone(),
                    info.clone(),
                )
                .into_service(),
            );
    }
    if let Some(h) = ext.ai.get() {
        grpc = grpc.add_service(
            ai::rpc::AiService::new(h.clone(), authorizer.clone(), info.clone()).into_service(),
        );
    }
    if let Some(h) = ext.agents.get() {
        let svc = agents::rpc::AgentService::new(h.clone(), authorizer.clone(), info.clone());
        grpc = grpc
            .add_service(svc.tokens().into_service())
            .add_service(svc.into_service());
    }
    if let Some(h) = ext.workflows.get() {
        grpc = grpc.add_service(
            workflows::rpc::WorkflowService::new(h.clone(), authorizer.clone(), info.clone())
                .into_service(),
        );
    }
    grpc.add_service(RepoService::new(ext.handlers.clone(), authorizer, info).into_service())
}

/// Builds the combined gRPC and HTTP routes for `ext`.
pub fn build_routes(ext: &Extensions, authorizer: Arc<dyn Authorizer>, info: ServerInfo) -> Routes {
    // The live git routes when the layer runs (U81), else the disabled stub (U78, SPEC §25.9).
    let git_hosts = ext
        .git
        .router(authorizer.clone())
        .unwrap_or_else(|| authn::git_router(authorizer.clone()));
    let router = build_grpc(ext, authorizer, info)
        .into_axum_router()
        .merge(ext.http.router())
        // `/<owner>/<repo>/info/refs` and the pack paths (U81; the stub of U78 when disabled).
        .merge(git_hosts)
        // tonic's own fallback answers an unknown path with `200` and a gRPC status; the public
        // listener answers a plain 404 instead (`/metrics` lives on the admin listener only).
        .fallback(|| async { axum::http::StatusCode::NOT_FOUND })
        // An agent credential reaches only what `agent_may_reach` names; new routes are closed to it.
        .layer(axum::middleware::from_fn(agents::authz::confine));
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
    // Plain http is only right behind a TLS-terminating proxy (U89).
    cfg.validate_listener()?;
    logging::starting(&cfg);
    if cfg.role == Role::Cache {
        // No SQL and no store: the cache role authorises through its upstream (U70).
        return cache_role::serve(cfg, shutdown).await;
    }
    if cfg.role != Role::Primary {
        return Err(ServeError::RoleUnavailable(cfg.role));
    }
    let (Some(db), Some(store)) = (&cfg.db, &cfg.store) else {
        return Err(ConfigError::Invalid("`store` and `db` are required".into()).into());
    };
    // Every decision passes through here, so the access log can name the caller.
    let authorizer: Arc<dyn Authorizer> = Arc::new(logging::LoggingAuthorizer::new(authorizer));
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
    // One embedder serves `Semantic` queries and the indexing workers.
    let ci_settings = codeintel_settings(&cfg);
    let ci_embed = ci_settings
        .as_ref()
        .and_then(|s| codeintel::embedder::open(s.embed.as_ref()));
    register_all(
        &ext,
        &registry,
        &cfg.limits,
        cfg.import_repo_root(),
        &cfg.cloud,
        ci_settings,
        ci_embed.clone(),
        (&cfg.secrets, &cfg.signing),
    )?;
    // Bring-your-own storage: the vault, the network guard, the router and the background work.
    let byos = storage::register(
        &ext,
        &registry,
        storage::Parts {
            vault: vault::from_config(
                &cfg.vault,
                cfg.db.as_deref(),
                cfg.cache.disk_dir.as_deref(),
                pool.clone(),
            ),
            guard: netguard::Guard::new(cfg.netguard.clone()),
            cfg: cfg.storage.clone(),
            authorizer: authorizer.clone(),
            audit: storage::default_audit(),
            authority: None,
        },
    );
    byos.start().await;
    // AI keys and agents (docs/AGENTS.md): both hubs, the agent gate of the `UpdateRefs` pipeline,
    // and the authorizer that also knows agent tokens, which every later user of it gets.
    // Working directories are private to the server's user; the default is under its data dir.
    let mut agents_cfg = cfg.agents.clone();
    let agents_dir = agents_cfg
        .work_dir
        .get_or_insert_with(|| workdir::default_dir(&cfg, "agents"))
        .clone();
    let mut workflows_cfg = cfg.workflows.clone();
    let workflows_dir = workflows_cfg
        .work_dir
        .get_or_insert_with(|| workdir::default_dir(&cfg, "workflows"))
        .clone();
    for (on, dir) in [
        (agents_cfg.enabled, &agents_dir),
        (workflows_cfg.enabled, &workflows_dir),
    ] {
        if on {
            workdir::prepare(dir).map_err(|e| ConfigError::Invalid(e.to_string()))?;
        }
    }
    let ai_agents = agents::register::register(
        &ext,
        &registry,
        agents::register::Parts {
            vault: byos.vault.clone(),
            guard: netguard::Guard::new(cfg.netguard.clone()),
            authorizer: authorizer.clone(),
            ai: cfg.aikeys.clone(),
            agents: agents_cfg,
            audit: None,
        },
    )
    .map_err(ConfigError::Invalid)?;
    let authorizer = ai_agents.agents.authorizer(authorizer);
    // Workflows: definitions in the repository, runs in the database, agents and commands as steps.
    let workflows = workflows::register(
        &ext,
        &registry,
        ai_agents.agents.clone(),
        workflows::Parts {
            vault: byos.vault.clone(),
            cfg: workflows_cfg,
            audit: None,
        },
    )
    .map_err(ConfigError::Invalid)?;
    // The audit log, the ref log and ref undo (docs/BUILDOUT.md sections 3 and 6).
    audit::register(&ext, &registry, &cfg.audit);
    ci::insights::register(&ext, &registry, &cfg.ci_insights);
    let warmer = projects::register(&ext, &registry);
    #[cfg(feature = "forgejo")]
    forgejo::register(&ext, &registry, &outbox, cfg.forgejo.as_ref())
        .map_err(|e| ConfigError::Invalid(e.to_string()))?;
    let mirror =
        mirror::register(&ext, &registry, &cfg).map_err(|e| ConfigError::Invalid(e.to_string()))?;
    #[cfg(feature = "forgejo")]
    forgejo::register_webhook(&ext, mirror.as_ref(), cfg.forgejo.as_ref())
        .map_err(|e| ConfigError::Invalid(e.to_string()))?;
    let repo_admin = admin::register(&ext, &registry, mirror.clone());
    // The git protocol layer (SPEC §25): the views, then the routes that serve them. With the
    // layer off, `build_routes` serves the disabled stub and `Version` omits `git.smart_http`.
    #[cfg(feature = "git-compat")]
    let git_services = git_view::register(&registry, &outbox, &cfg)
        .map(|views| git_http::register(&ext, views, registry.clone(), &metrics, &cfg));
    #[cfg(feature = "git-compat")]
    let git_live = git_services.is_some();
    #[cfg(not(feature = "git-compat"))]
    if cfg.git.enabled {
        tracing::warn!("git-compat is not compiled in; git protocol disabled");
    }
    let well_known = http::WellKnown {
        server: cfg.public_url.clone(),
        ..http::WellKnown::default()
    };
    let stats = obs::Stats::register(&metrics);
    let http_state = http::HttpState::new(pool, outbox.clone(), metrics.clone(), well_known)
        .with_stats(stats.clone());
    http::register(&ext, http_state.clone());
    web::register(&ext, &cfg.web)?;
    let turbo = Arc::new(
        turbo::TurboState::open(&cfg, authorizer.clone(), metrics.clone())
            .map_err(|e| ConfigError::Invalid(e.to_string()))?,
    );
    turbo::register(&ext, turbo.clone());
    let actions = actions_store::register(&ext, &turbo, &registry);
    mcp::register(&ext, &cfg);
    // Admission runs before authentication; `guarded_routes` also lets it learn which bearer
    // tokens the authorizer accepts, so that only those callers get a bucket of their own.
    let admission = Arc::new(limits::Admission::new(&cfg.limits));
    // The SSH listener (SPEC §25.11): bound only when the feature, the switch and the git layer
    // allow it; it asks the same authorizer and shares this admission controller.
    #[cfg(feature = "git-ssh")]
    let ssh = ssh::start(
        &cfg,
        git_live,
        ssh::StartDeps {
            authorizer: authorizer.clone(),
            admission: admission.clone(),
            metrics: metrics.clone(),
            // The pack commands run through the git layer, with the HTTP admission controller.
            exec: match git_services {
                Some(services) => Arc::new(ssh::GitSshExec::new(services, admission.clone())),
                None => Arc::new(ssh::Unavailable),
            },
        },
    )
    .await
    .map_err(|e| ServeError::Ssh(e.to_string()))?;
    #[cfg(not(feature = "git-ssh"))]
    if cfg.ssh.enabled {
        tracing::warn!("git-ssh is not compiled in; ssh listener not started");
    }
    #[cfg(feature = "git-compat")]
    let git_live_now = git_live;
    #[cfg(not(feature = "git-compat"))]
    let git_live_now = false;
    #[cfg(feature = "git-ssh")]
    let ssh_up = ssh.as_ref().map(|server| {
        let addr = server.local_addr().unwrap_or(cfg.ssh.listen);
        let keys = server.host_keys().keys();
        (
            addr,
            keys.iter().map(|k| k.fingerprint().to_string()).collect(),
        )
    });
    #[cfg(not(feature = "git-ssh"))]
    let ssh_up = None;
    #[cfg_attr(not(feature = "git-compat"), allow(unused_mut))]
    let mut features = vec![projects::feature()];
    features.extend(review::features());
    features.push(audit::FEATURE.to_owned());
    features.push(audit::STACKS_FEATURE.to_owned());
    features.push(ci::insights::FEATURE.to_owned());
    features.extend(codeintel::features::features(&ext.codeintel));
    features.extend(storage::features(&ext.byos));
    features.extend(ai_agents.features());
    features.extend(workflows::features(&workflows));
    if ext.security.get().is_some() {
        features.extend(policy::features());
    }
    #[cfg(feature = "git-compat")]
    if git_live {
        features.push(git_http::FEATURE.to_owned());
    }
    #[cfg(feature = "git-ssh")]
    if ssh.is_some() {
        features.push(ssh::FEATURE.to_owned());
    }
    // `/mcp` is advertised exactly when it is mounted.
    let mcp_on = mcp::enabled(&cfg);
    if mcp_on {
        features.push(mcp::FEATURE.to_owned());
    }
    let info = ServerInfo {
        features,
        ..ServerInfo::default()
    };
    if mcp_on {
        // The backend calls the gRPC services in process, behind a per-call credential only the
        // backend can mint, and charges admission itself, by principal (SPEC 22.7.2).
        let scope = mcp::backend::ServeAuthorizer::new(Arc::new(DenyAll));
        mcp::mount(
            &ext,
            mcp::Deps {
                cfg: &cfg,
                authorizer: authorizer.clone(),
                registry: registry.clone(),
                admission: admission.clone(),
                routes: build_grpc(&ext, scope.clone(), info.clone()),
                scope,
                features: &info.features,
            },
        )?;
    }
    // The agent runtime calls the server's own services through this router, as the run's agent.
    let agent_tasks = ai_agents.start(build_grpc(&ext, authorizer.clone(), info.clone()));
    let workflow_tasks = if workflows.cfg.enabled {
        workflows.start_workers()
    } else {
        Vec::new()
    };
    let routes = limits::guarded_routes(&ext, authorizer, admission, info);
    // gRPC-Web and Connect unary for browsers, in front of admission and the services.
    let routes = web::grpc_web::wrap(routes, &cfg.web, cfg.public_url.as_deref());
    // The outermost layer: the access log sees every request, whichever listener carries it.
    let routes = Routes::from(
        routes
            .into_axum_router()
            .layer(audit::meta::MetaLayer)
            .layer(axum::middleware::from_fn_with_state(stats, obs::observe))
            .layer(logging::AccessLogLayer),
    );

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
        let app = http::admin_router(http_state, admin_token).layer(logging::AccessLogLayer);
        axum::serve(admin_listener, app)
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
    // `Alt-Svc: h3=…` when an h3-capable proxy in front serves HTTP/3 (U89).
    let mut server = server.layer(alt_svc::layer(cfg.http3_port));
    // The same router over QUIC when `[tls]` and `http3_port` are set (U89b); held until return.
    let _h3 = h3_listener::start(&cfg, &routes)?;
    tracing::info!(listen = %cfg.listen, "asc serve listening");
    logging::ready(
        &cfg,
        &logging::Listeners {
            git: git_live_now,
            ssh: ssh_up,
            h3: _h3.as_ref().and(cfg.http3_port),
        },
    );
    // Purged drafts are evicted from every cache tier by the outbox (B10).
    if let Some(cloud) = ext.cloud.get() {
        outbox.effects().register(cloud.cache_evict_effect());
    }
    // A push queues its files for the indexing workers (SPEC §23.5); with `[codeintel]` off the
    // outbox keeps its no-op for the effect.
    if cfg.codeintel.enabled {
        outbox
            .effects()
            .register(codeintel::source::enqueue_effect(&registry));
    }
    let worker = tokio::spawn(outbox.run());
    // The indexing workers (SPEC §23.5) run beside `CodeIntel` whenever `[codeintel] enabled`.
    let indexers = match codeintel::runner::config(&cfg) {
        Some(c) => match codeintel::runner::start(registry.clone(), c, ci_embed) {
            Ok(i) => Some(i),
            Err(e) => {
                tracing::error!(error = %e, "codeintel workers did not start");
                None
            }
        },
        None => None,
    };
    let sweeper = turbo.spawn_sweeper();
    let actions_sweeper = actions.spawn_sweeper();
    let warming = warmer.spawn();
    let poller = mirror.as_ref().map(mirror::Mirror::spawn_poller);
    let admin_worker = repo_admin.spawn_worker();
    // Events older than `[audit] retention_days` are dropped, oldest first.
    let audit_retention = audit::retention::spawn(registry.pool().clone(), cfg.audit.clone());
    let insights_retention =
        ci::insights::retention::spawn(registry.pool().clone(), cfg.ci_insights.clone());
    // Expired agent refs and snapshots are deleted through the normal pipeline (SPEC §8.8).
    let expiry =
        Arc::new(expire::Expirer::new(registry.clone(), ext.pipeline.clone())).spawn_worker();
    // Hidden cloud heads expire and stale ones are hidden (SPEC §21.2.4).
    let cloud_expiry = Arc::new(cloud::expiry::CloudExpirer::new(
        registry.pool().clone(),
        cloud::expiry::Settings::default(),
    ))
    .spawn_worker();
    #[cfg(feature = "git-ssh")]
    let ssh_task = ssh.map(|server| tokio::spawn(server.run(std::future::pending())));
    // Collection deletes data, so it runs only when `[gc] enabled` is set.
    let collector = if cfg.gc.enabled {
        let service = Arc::new(gc::GcService::new(
            registry.clone(),
            ext.pipeline.clone(),
            cfg.gc.to_config()?,
        ));
        Some(service.spawn_worker(std::time::Duration::from_secs(cfg.gc.interval_hours * 3600)))
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
    // The indexing workers commit what they buffered before the rest stops.
    if let Some(i) = indexers {
        i.stop().await;
    }
    // Live agent runs are told to stop; a run that does not end in time is failed by the next start.
    workflows.shutdown();
    for t in &workflow_tasks {
        t.abort();
    }
    ai_agents.stop(&agent_tasks);
    // Delivery is at least once, so stopping the worker mid-row is safe.
    worker.abort();
    sweeper.abort();
    actions_sweeper.abort();
    warming.abort();
    expiry.abort();
    cloud_expiry.abort();
    if let Some(p) = poller {
        p.abort();
    }
    admin_worker.abort();
    audit_retention.abort();
    insights_retention.abort();
    #[cfg(feature = "git-ssh")]
    if let Some(t) = ssh_task {
        t.abort();
    }
    if let Some(c) = collector {
        c.abort();
    }
    admin.abort();
    match &served {
        Ok(()) => tracing::info!("asc serve stopped"),
        Err(e) => tracing::error!(error = %e, "asc serve stopped on an error"),
    }
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
