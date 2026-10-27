//! Assembling the AI-key and agent features at start-up.
//!
//! [`register`] builds the [`AiHub`] and the [`AgentsHub`] over the server's vault and network
//! guard, installs the agent gate into the `UpdateRefs` pipeline and fills the two extension
//! slots. [`Registered::start`] then starts the dispatcher and the workers once the final router
//! exists (the runtime calls the server's own services through it, as the run's agent).

use std::sync::Arc;

use asc_rpc::auth::Authorizer;
use tokio::task::JoinHandle;

use super::{AgentsConfig, AgentsHub};
use crate::ai::audit::{Audit, PoolAudit};
use crate::ai::owner::AuthorizerOwners;
use crate::ai::prices::Prices;
use crate::ai::{AiHub, AiKeysConfig};
use crate::netguard::Guard;
use crate::registry::{Extensions, Registry};
use crate::vault::Vault;

/// What [`register`] needs.
pub struct Parts {
    pub vault: Arc<Vault>,
    pub guard: Guard,
    /// The server's own authorizer: it decides who administers an owner's keys.
    pub authorizer: Arc<dyn Authorizer>,
    pub ai: AiKeysConfig,
    pub agents: AgentsConfig,
    /// `None`: the audit log of the registry's database.
    pub audit: Option<Arc<dyn Audit>>,
}

/// The hubs [`register`] made.
pub struct Registered {
    pub ai: Arc<AiHub>,
    pub agents: Arc<AgentsHub>,
}

/// Builds and installs both features. An invalid configuration, an unreadable price file or a
/// second registration is an error the server refuses to start with.
pub fn register(
    ext: &Extensions,
    registry: &Arc<Registry>,
    parts: Parts,
) -> Result<Registered, String> {
    parts.ai.validate()?;
    parts.agents.validate()?;
    let prices = match &parts.ai.price_file {
        Some(p) => Prices::load(p)?,
        None => Prices::default(),
    };
    let pool = registry.pool().clone();
    let audit: Arc<dyn Audit> = parts
        .audit
        .unwrap_or_else(|| Arc::new(PoolAudit::new(pool.clone())));
    let ai = AiHub::new(
        pool.clone(),
        parts.vault,
        parts.guard,
        parts.ai,
        prices,
        Arc::new(AuthorizerOwners::new(parts.authorizer)),
    );
    ai.set_audit(audit.clone());
    let agents = AgentsHub::new(
        pool,
        registry.clone(),
        ai.clone(),
        parts.agents,
        Some(audit),
    );
    if ext.pipeline.set_agent_gate(agents.gate()).is_err() {
        return Err("the agent gate was already set".into());
    }
    if !ext.ai.set(ai.clone()) {
        return Err("the AI key service was already registered".into());
    }
    if !ext.agents.set(agents.clone()) {
        return Err("the agent services were already registered".into());
    }
    Ok(Registered { ai, agents })
}

impl Registered {
    /// What `Version.features` adds: the keys always, the agent services when they are switched on.
    pub fn features(&self) -> Vec<String> {
        let mut v = vec![crate::ai::FEATURE.to_owned()];
        if self.agents.core.cfg.enabled {
            v.push(super::FEATURE_RUNS.to_owned());
            v.push(super::FEATURE_TOKENS.to_owned());
        }
        v
    }

    /// Starts the AI-key sweeper and, when agents are enabled, the dispatcher and the reaper.
    /// `routes` are the server's services behind the authorizer that knows agent tokens.
    pub fn start(&self, routes: tonic::service::Routes) -> Vec<JoinHandle<()>> {
        let mut tasks = vec![self.ai.spawn_sweeper()];
        if self.agents.core.cfg.enabled {
            tasks.extend(self.agents.start(routes));
        }
        tasks
    }

    /// Tells live runs to stop; the server calls it on shutdown.
    pub fn stop(&self, tasks: &[JoinHandle<()>]) {
        self.agents.shutdown();
        for t in tasks {
            t.abort();
        }
    }
}
