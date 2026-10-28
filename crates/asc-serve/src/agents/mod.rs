//! Agents: tokens, runs and the runtime that executes them (`agents.proto`, docs/AGENTS.md).
//!
//! An agent is a model working through a task on a copy of a repository and leaving a *stack of
//! changes* for people to review. The design rules, in the order they matter:
//!
//! * **Never trunk.** An agent's only writable refs are `refs/heads/agent/<its name>/...`
//!   ([`gate`]); landing is a person's decision through the normal review and merge path.
//! * **Server-side limits.** Scope (token paths intersected with the repository policy) and
//!   budget (changes, paths, tokens, cost, wall time) are enforced by the server on every ref
//!   update and every model call ([`gate`], [`tokens`], [`runtime`]), never by anything the model
//!   or a file says.
//! * **A token per run.** A run gets its own `ascat_` token that expires with the run and is
//!   revoked when it ends; an agent outside the server (a developer's own, CI) uses the same
//!   kind of token, made with `AgentTokens.CreateToken`.
//! * **Optional.** Everything here is off unless `[agents] enabled` is set and the repository's
//!   policy does not switch agents off.
//!
//! Modules: [`tokens`] (secret, rows, counters), [`authz`] (the credential), [`gate`] (the ref
//! update hook), [`repo`] (reading a repository), [`runs`] (run rows and events), [`workspace`]
//! and [`tools`] (what a run can do), [`runtime`] (the loop), [`rpc`] (the services).

pub mod authz;
pub mod config;
pub mod control;
pub mod errors;
pub mod gate;
pub mod inproc;
pub mod mask;
pub mod register;
pub mod repo;
pub mod rpc;
pub mod runs;
pub mod runtime;
pub mod tokens;
pub mod tools;
pub mod workspace;

#[cfg(test)]
pub(crate) mod tests;
#[cfg(test)]
mod tests_control;
#[cfg(test)]
mod tests_flow;
#[cfg(test)]
pub(crate) mod tests_tokens;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};

use asc_ci::sandbox::{self, Sandbox};
use asc_rpc::server::wire;
use sqlx::AnyPool;
use tokio::sync::{Notify, Semaphore};
use tokio_util::sync::CancellationToken;

pub use config::AgentsConfig;
use inproc::InProc;
use repo::RepoView;
use runs::{EventRow, RunRow, RunStore};
use tokens::{Spent, TokenRow, TokenStore};

use crate::ai::AiHub;
pub use crate::ai::audit::{Audit, Entry};
use crate::ai::ops::Ops;
use crate::registry::Registry;

/// What `Version.features` adds for runs.
pub const FEATURE_RUNS: &str = "agents.v1";
/// What `Version.features` adds for tokens.
pub const FEATURE_TOKENS: &str = "agent-tokens.v1";
/// Active runs (queued or running) one repository may have.
pub const MAX_ACTIVE_PER_REPO: usize = 50;

/// What the gate, the authorizer and the runtime share.
pub struct Core {
    pub pool: AnyPool,
    pub tokens: TokenStore,
    pub runs: RunStore,
    pub view: Arc<RepoView>,
    pub cfg: AgentsConfig,
    pub ops: Ops,
}

/// A run in this process.
pub(crate) struct Live {
    pub cancel: CancellationToken,
    /// Why the run was told to stop, when it was.
    pub reason: Mutex<Option<runtime::StopReason>>,
}

/// The agent services' shared state.
pub struct AgentsHub {
    pub core: Arc<Core>,
    pub ai: Arc<AiHub>,
    pub(crate) registry: Arc<Registry>,
    pub(crate) sandbox: Sandbox,
    pub(crate) inproc: OnceLock<InProc>,
    pub(crate) live: Mutex<HashMap<u64, Arc<Live>>>,
    pub(crate) wake: Notify,
    pub(crate) permits: Arc<Semaphore>,
    pub(crate) work_dir: PathBuf,
    pub(crate) shutdown: CancellationToken,
    pub(crate) audit: Option<Arc<dyn Audit>>,
}

/// The place the server's single [`AgentsHub`] is registered, once.
#[derive(Default)]
pub struct AgentsSlot(OnceLock<Arc<AgentsHub>>);

impl AgentsSlot {
    pub fn set(&self, hub: Arc<AgentsHub>) -> bool {
        self.0.set(hub).is_ok()
    }

    pub fn get(&self) -> Option<&Arc<AgentsHub>> {
        self.0.get()
    }
}

/// The sandbox a configuration describes.
pub fn sandbox_of(cfg: &AgentsConfig) -> Sandbox {
    let s = &cfg.sandbox;
    Sandbox::new(sandbox::Config {
        allow: s.allow.clone(),
        path_dirs: s.path_dirs.clone(),
        pass_env: s.pass_env.clone(),
        wrapper: s.wrapper.clone(),
        restricted_wrapper: s.restricted_wrapper.clone(),
        allow_unisolated: s.allow_unisolated,
        max_output: s.max_output_bytes,
        max_wall: std::time::Duration::from_secs(s.max_command_seconds),
    })
}

impl AgentsHub {
    pub fn new(
        pool: AnyPool,
        registry: Arc<Registry>,
        ai: Arc<AiHub>,
        cfg: AgentsConfig,
        audit: Option<Arc<dyn Audit>>,
    ) -> Arc<Self> {
        let work_dir = cfg
            .work_dir
            .clone()
            .unwrap_or_else(|| crate::workdir::fallback("agents"));
        let sandbox = sandbox_of(&cfg);
        let permits = Arc::new(Semaphore::new(cfg.max_concurrent_runs as usize));
        let core = Arc::new(Core {
            pool: pool.clone(),
            tokens: TokenStore::new(pool.clone()),
            runs: RunStore::new(pool.clone()),
            view: Arc::new(RepoView::new(registry.clone())),
            cfg,
            ops: Ops::new(pool),
        });
        Arc::new(Self {
            core,
            ai,
            registry,
            sandbox,
            inproc: OnceLock::new(),
            live: Mutex::new(HashMap::new()),
            wake: Notify::new(),
            permits,
            work_dir,
            shutdown: CancellationToken::new(),
            audit,
        })
    }

    /// The gate to install in the `UpdateRefs` pipeline.
    pub fn gate(&self) -> Arc<dyn gate::AgentGate> {
        Arc::new(gate::ServerAgentGate::new(self.core.clone()))
    }

    /// The authorizer that recognises agent tokens, wrapped around `inner`.
    pub fn authorizer(
        &self,
        inner: Arc<dyn asc_rpc::auth::Authorizer>,
    ) -> Arc<dyn asc_rpc::auth::Authorizer> {
        authz::AgentAuthorizer::new(inner, self.core.tokens.clone(), self.core.cfg.enabled)
    }

    pub(crate) async fn audit(
        &self,
        action: &str,
        actor: &str,
        on_behalf: Option<&str>,
        repo: &str,
        target: &str,
        details: &[(&str, String)],
    ) {
        if let Some(a) = &self.audit {
            a.record(Entry {
                action,
                actor,
                on_behalf,
                repo,
                owner: "",
                target,
                denied: false,
                details,
            })
            .await;
        }
    }
}

// ------------------------------------------------------------------------------ wire forms

/// The wire form of a run. `token` is the run's token row, which holds its usage.
pub fn run_to_wire(r: &RunRow, token: Option<&TokenRow>, now: i64) -> wire::AgentRun {
    let spent = token.map_or_else(Spent::default, |t| t.spent);
    let end = if r.finished > 0 { r.finished } else { now };
    let runtime_seconds = if r.started > 0 {
        u32::try_from((end - r.started).max(0)).unwrap_or(u32::MAX)
    } else {
        0
    };
    wire::AgentRun {
        id: r.id,
        repo: r.repo.clone(),
        agent: r.agent.clone(),
        title: r.title.clone(),
        state: r.state.to_wire() as i32,
        target: r.target.clone(),
        base: bytes::Bytes::copy_from_slice(&r.base),
        ai_key_id: r.ai_key_id.clone(),
        model: r.model.clone(),
        limits: Some(r.limits.to_wire()),
        usage: Some(spent.to_wire(runtime_seconds)),
        changes: r
            .changes
            .iter()
            .map(|c| bytes::Bytes::copy_from_slice(c))
            .collect(),
        created_by: if r.starter.is_empty() {
            r.created_by.clone()
        } else {
            r.starter.clone()
        },
        created: r.created,
        started: r.started,
        finished: r.finished,
        failure_code: r.failure_code.clone(),
        failure_message: r.failure_message.clone(),
        token_id: r.token_id.clone(),
        workflow_run_id: r.workflow_run_id,
        workflow_step: r.workflow_step.clone(),
        task: r.task.clone(),
        ..Default::default()
    }
}

/// The wire form of an event.
pub fn event_to_wire(e: &EventRow) -> wire::AgentRunEvent {
    let usage: Option<wire::AgentBudgetUsage> =
        serde_json::from_str::<serde_json::Value>(&e.usage_json)
            .ok()
            .map(|v| {
                let n = |k: &str| v.get(k).and_then(serde_json::Value::as_u64).unwrap_or(0);
                wire::AgentBudgetUsage {
                    changes: u32::try_from(n("changes")).unwrap_or(u32::MAX),
                    tokens: n("tokens"),
                    cost_micro_usd: n("cost"),
                    paths: u32::try_from(n("paths")).unwrap_or(u32::MAX),
                    runtime_seconds: u32::try_from(n("runtime")).unwrap_or(u32::MAX),
                    requests: n("requests"),
                }
            });
    wire::AgentRunEvent {
        run_id: e.run_id,
        seq: e.seq,
        kind: i32::try_from(e.kind).unwrap_or(0),
        time_ms: e.time_ms,
        state: i32::try_from(e.state).unwrap_or(0),
        text: e.text.clone(),
        tool: e.tool.clone(),
        change: bytes::Bytes::copy_from_slice(&e.change),
        usage: usage.filter(|_| !e.usage_json.is_empty()),
    }
}
