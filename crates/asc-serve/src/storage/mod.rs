//! Bring your own storage (docs/BYOS.md, docs/BUILDOUT.md section 9).
//!
//! A user, an organisation or a repository can keep its objects in an S3-compatible bucket it
//! owns. A **target** is created `PENDING`, becomes `VERIFIED` when a probe object can be
//! written, read back and deleted through the network guard, and is used only once `ENABLED`.
//! A repository is **assigned** a target (new objects go there) or **migrated** to it (copy,
//! verify, switch, optionally delete the old objects, with reads and writes available throughout).
//!
//! The pieces:
//!
//! * [`model`], [`db`]: the fields and their rules, and the SQL;
//! * [`live`]: credentials from the [vault](crate::vault), the endpoint from the
//!   [network guard](crate::netguard), and the S3 client from `asc_store::s3::open_target`;
//!   the router that gives the registry a store per repository;
//! * [`routed`]: one namespace over a primary, a fallback and a mirror;
//! * [`service`], [`rpc`], [`authority`]: the operations, the `Storage` and `Vault` services, and
//!   the rules for who may do what;
//! * [`migrate`], [`health`]: the background work;
//! * [`audit`]: the audit seam.

pub mod audit;
pub mod authority;
pub mod db;
pub mod health;
pub mod live;
pub mod migrate;
pub mod model;
pub mod routed;
pub mod rpc;
pub mod service;

#[cfg(test)]
mod fake_s3;
#[cfg(test)]
mod tests;

use std::sync::{Arc, OnceLock};
use std::time::Duration;

use asc_rpc::auth::Authorizer;
use async_trait::async_trait;
use object_store::ObjectStore;
use object_store::path::Path;

pub use service::{StorageConfig, StorageError};

use self::audit::{Audit, TracingAudit};
use self::authority::{Authority, DefaultAuthority};
use self::db::Db;
use self::health::Health;
use self::live::{Builder, Router};
use self::migrate::{Engine, Settings};
use self::model::SECRET_KIND;
use crate::netguard::Guard;
use crate::registry::{Extensions, NsRouter, Ns, Registry, RoutedNs};
use crate::vault::{Resealer, SecretContext, Vault, VaultError};

/// How often routes are re-read so namespaces this process has not opened find their objects.
const ROUTE_REFRESH: Duration = Duration::from_secs(30);

/// Re-seals storage credentials after a vault rotation.
struct TargetResealer {
    db: Db,
}

#[async_trait]
impl Resealer for TargetResealer {
    fn kind(&self) -> &'static str {
        SECRET_KIND
    }

    async fn counts(&self) -> Result<Vec<(u32, u64)>, VaultError> {
        let rows = self
            .db
            .sealed_counts()
            .await
            .map_err(|_| VaultError::Sql("statement failed".into()))?;
        Ok(rows
            .into_iter()
            .map(|(g, n)| (u32::try_from(g).unwrap_or(0), u64::try_from(n).unwrap_or(0)))
            .collect())
    }

    async fn reseal_batch(
        &self,
        vault: &Vault,
        target: u32,
        limit: usize,
    ) -> Result<usize, VaultError> {
        let rows = self
            .db
            .not_under(target, i64::try_from(limit).unwrap_or(1))
            .await
            .map_err(|_| VaultError::Sql("statement failed".into()))?;
        let mut done = 0;
        for (id, scope, sealed, old_gen) in rows {
            let ctx = SecretContext {
                kind: SECRET_KIND,
                owner: &scope,
                id: &id,
            };
            let Some((new_gen, blob)) = vault.reseal(&ctx, &sealed)? else {
                continue;
            };
            let replaced = self
                .db
                .replace_sealed(&id, &blob, new_gen, old_gen)
                .await
                .map_err(|_| VaultError::Sql("statement failed".into()))?;
            done += usize::from(replaced);
        }
        Ok(done)
    }
}

/// The registry's view of the router.
struct RegistryRouter(Arc<Router>);

#[async_trait]
impl NsRouter for RegistryRouter {
    async fn route(&self, repo_id: i64, ns: Ns) -> Result<Option<RoutedNs>, String> {
        match self.0.route(repo_id, ns).await {
            Ok(None) => Ok(None),
            Ok(Some(r)) => Ok(Some(RoutedNs {
                store: r.store.clone(),
                root: r.root.clone(),
                stamp: r.stamp,
            })),
            // The reason is a fixed phrase; it is shown to the caller.
            Err(e) => Err(e.to_string()),
        }
    }

    fn namespace(&self, ns: Ns) -> Option<(Arc<dyn ObjectStore>, Path)> {
        self.0.namespace(ns)
    }
}

/// What `register` needs.
pub struct Parts {
    pub vault: Arc<Vault>,
    pub guard: Guard,
    pub cfg: StorageConfig,
    pub authorizer: Arc<dyn Authorizer>,
    pub audit: Arc<dyn Audit>,
    /// Who may administer what; `None` is [`DefaultAuthority`] over `authorizer`.
    pub authority: Option<Arc<dyn Authority>>,
}

/// The assembled feature.
pub struct Byos {
    pub service: Arc<service::Service>,
    pub vault: Arc<Vault>,
    pub router: Arc<Router>,
    pub engine: Arc<Engine>,
    pub health: Arc<Health>,
    authority: Arc<dyn Authority>,
    audit: Arc<dyn Audit>,
    health_interval: Duration,
}

/// Where the server's one [`Byos`] is registered, once.
#[derive(Default)]
pub struct ByosSlot(OnceLock<Arc<Byos>>);

impl ByosSlot {
    pub fn set(&self, b: Arc<Byos>) -> bool {
        self.0.set(b).is_ok()
    }

    pub fn get(&self) -> Option<&Arc<Byos>> {
        self.0.get()
    }
}

/// `Version.features` for the storage and vault services: present exactly while registered.
pub fn features(slot: &ByosSlot) -> Vec<String> {
    if slot.get().is_some() {
        vec![rpc::STORAGE_FEATURE.to_owned(), rpc::VAULT_FEATURE.to_owned()]
    } else {
        Vec::new()
    }
}

/// The unit's registration: builds the router into the registry, the vault's re-sealing of
/// storage credentials, the migration engine and the health checker, and puts the feature in
/// `ext`. Nothing runs until [`Byos::start`].
pub fn register(ext: &Extensions, registry: &Arc<Registry>, parts: Parts) -> Arc<Byos> {
    let db = Db::new(registry.pool().clone());
    let (default_store, default_root) = registry.default_place();
    let builder = Builder {
        guard: parts.guard.clone(),
        vault: parts.vault.clone(),
    };
    let router = Arc::new(Router::new(
        db.clone(),
        builder,
        default_store,
        default_root,
        Duration::from_secs(parts.cfg.repin_secs),
    ));
    if !registry.set_router(Arc::new(RegistryRouter(router.clone()))) {
        tracing::warn!("a storage router was already installed");
    }
    parts.vault.register(Arc::new(TargetResealer { db: db.clone() }));
    let engine = Engine::new(
        db.clone(),
        router.clone(),
        parts.audit.clone(),
        Settings {
            settle: Duration::from_secs(parts.cfg.migration_settle_secs),
            workers: parts.cfg.migration_workers,
            ..Settings::default()
        },
    );
    let health = Health::new(db.clone(), router.clone(), parts.audit.clone());
    let authority: Arc<dyn Authority> = match parts.authority {
        Some(a) => a,
        None => Arc::new(DefaultAuthority::new(parts.authorizer)),
    };
    let service = Arc::new(service::Service {
        db,
        vault: parts.vault.clone(),
        guard: parts.guard,
        router: router.clone(),
        engine: engine.clone(),
        audit: parts.audit.clone(),
        authority: authority.clone(),
        cfg: parts.cfg.clone(),
    });
    let byos = Arc::new(Byos {
        service,
        vault: parts.vault,
        router,
        engine,
        health,
        authority,
        audit: parts.audit,
        health_interval: Duration::from_secs(parts.cfg.health_interval_secs),
    });
    if !ext.byos.set(byos.clone()) {
        tracing::warn!("bring-your-own storage was already registered");
    }
    byos
}

impl Byos {
    /// Starts the background work: resumes a vault rotation, loads the routes, runs the
    /// migration supervisor, the health checks and the periodic route refresh. Call once, inside
    /// the runtime.
    pub async fn start(self: &Arc<Self>) {
        if let Err(e) = self.vault.resume().await {
            tracing::warn!(error = %e, "could not resume a vault rotation");
        }
        let failed = self.router.refresh().await;
        if failed > 0 {
            tracing::warn!(routes = failed, "some storage routes could not be built");
        }
        self.engine.spawn();
        if !self.health_interval.is_zero() {
            self.health.spawn(self.health_interval);
        }
        let router = self.router.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(ROUTE_REFRESH);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tick.tick().await;
                router.refresh().await;
            }
        });
    }

    /// The `Storage` service behind `authorizer`.
    pub fn storage_service(
        &self,
        authorizer: Arc<dyn Authorizer>,
    ) -> tonic::service::interceptor::InterceptedService<
        asc_rpc::services::storage_server::StorageServer<rpc::StorageService>,
        asc_rpc::auth::AuthInterceptor,
    > {
        rpc::StorageService::new(self.service.clone(), authorizer).into_service()
    }

    /// The `Vault` service behind `authorizer`.
    pub fn vault_service(
        &self,
        authorizer: Arc<dyn Authorizer>,
    ) -> tonic::service::interceptor::InterceptedService<
        asc_rpc::services::vault_server::VaultServer<rpc::VaultService>,
        asc_rpc::auth::AuthInterceptor,
    > {
        rpc::VaultService::new(
            self.vault.clone(),
            self.authority.clone(),
            authorizer,
            self.audit.clone(),
            self.service.db.pool.clone(),
        )
        .into_service()
    }
}

/// The audit sink the server uses until the audit log exists.
pub fn default_audit() -> Arc<dyn Audit> {
    Arc::new(TracingAudit)
}
