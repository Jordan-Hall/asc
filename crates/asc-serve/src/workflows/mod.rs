//! Repository workflows (`workflows.proto`, docs/BUILDOUT.md section 13, docs/WORKFLOWS.md).
//!
//! A workflow is agent work, commands, approvals and check waits that start on an event and run
//! in order. Its definition is a file of the repository, `.asc/workflows/<name>.toml` on trunk,
//! parsed strictly by `asc_core::workflow`. The server keeps whether it is switched on, its runs,
//! and the secrets its command steps read. The rules, in the order they matter:
//!
//! * **Trunk decides.** The definition that runs is the one on the trunk tip when the run is
//!   made, and the run keeps its own copy: a change that edits a workflow, or an agent that
//!   rewrites it, affects no run that exists. A change cannot give itself a workflow.
//! * **Optional and off by default.** `[workflows] enabled` switches the feature on for the
//!   instance and a maintainer switches each workflow on; agent steps also need `[agents]`.
//! * **Nothing a file says widens a limit.** Agent steps are runs of the agent runtime with their
//!   own token, scope and budget; the workflow's `[budget]` caps the sum over its steps and the
//!   repository policy still applies to every step. A command step runs a fixed argument list in
//!   the operator's sandbox; text from events and inputs reaches it only as environment
//!   variables, never as an argument.
//! * **Approvals are people.** Only a person admitted by the step's `approvers` decides, an agent
//!   never does, and the person who started a run does not approve it unless the step names them.
//! * **At most once.** An event fires a workflow once however many servers see it (a slot per
//!   workflow and event); a workflow's own agents cannot start it again, and a chain of
//!   workflows started by workflows' agents stops at `max_chain_depth`.
//! * **Results are checks.** A command step reports its result to the CI service against the
//!   exact change version it ran on, so reviews and the merge rules see it.
//!
//! Modules: [`defs`] (reading definitions), [`events`] (what starts a run), [`engine`] (the
//! dispatcher and the driver of a run), [`steps`] (what each kind of step does), [`store`] and
//! [`secrets`] (SQL), [`rpc`] (the service).

pub mod config;
pub mod defs;
pub mod engine;
pub mod events;
pub mod rpc;
pub mod secrets;
pub mod steps;
pub mod store;

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock, PoisonError};

use asc_ci::sandbox::Sandbox;
use asc_core::workflow::{self as core, Step, StepKind, TriggerKind, Workflow};
use asc_rpc::server::wire;
use prost::Message as _;
use tokio::sync::{Notify, Semaphore};
use tokio_util::sync::CancellationToken;

pub use config::WorkflowsConfig;
use defs::{Def, Defs};
use secrets::Secrets;
use store::{RunRow, StepRow, Store};

use crate::agents::AgentsHub;
use crate::ai::audit::{Audit, Entry};
use crate::ai::clock;
use crate::registry::{Registry, RepoEntry};
use crate::review::Reviews;
use crate::vault::Vault;

/// What `Version.features` adds.
pub const FEATURE: &str = "workflows.v1";
/// The principal that events and schedules act as.
pub const SYSTEM_ACTOR: &str = "system:workflows";
/// The `source` of the checks a command step reports.
pub const CHECK_SOURCE: &str = "asc-workflow";

/// A run in this process.
pub(crate) struct LiveRun {
    /// Cancelled when the run is told to stop: cancelled by a person, out of time, or the
    /// server is shutting down. Every step of the run watches it.
    pub stop: CancellationToken,
    /// Why, once it was cancelled.
    pub why: Mutex<Option<engine::StopWhy>>,
    /// Woken when a step changed state or an approval was decided.
    pub changed: Notify,
    /// Agent steps of one run take turns, so the budget a step starts with is exact.
    pub agent_turn: tokio::sync::Mutex<()>,
}

/// The workflow services' shared state.
pub struct WorkflowsHub {
    pub cfg: WorkflowsConfig,
    pub store: Store,
    pub secrets: Secrets,
    pub agents: Arc<AgentsHub>,
    pub(crate) registry: Arc<Registry>,
    pub(crate) reviews: Option<Arc<Reviews>>,
    pub(crate) sandbox: Sandbox,
    pub(crate) work_dir: PathBuf,
    /// Names this process in the lease of a run.
    pub(crate) instance: String,
    pub(crate) live: Mutex<HashMap<u64, Arc<LiveRun>>>,
    pub(crate) wake: Notify,
    pub(crate) permits: Arc<Semaphore>,
    pub(crate) shutdown: CancellationToken,
    pub(crate) audit: Option<Arc<dyn Audit>>,
    defs: Mutex<DefsCache>,
}

/// Parsed definitions by repository and file path.
type DefsCache = HashMap<(i64, Vec<u8>), Arc<Defs>>;

/// The place the server's single [`WorkflowsHub`] is registered, once.
#[derive(Default)]
pub struct WorkflowsSlot(OnceLock<Arc<WorkflowsHub>>);

impl WorkflowsSlot {
    pub fn set(&self, hub: Arc<WorkflowsHub>) -> bool {
        self.0.set(hub).is_ok()
    }

    pub fn get(&self) -> Option<&Arc<WorkflowsHub>> {
        self.0.get()
    }
}

impl WorkflowsHub {
    pub fn new(
        registry: Arc<Registry>,
        agents: Arc<AgentsHub>,
        vault: Arc<Vault>,
        reviews: Option<Arc<Reviews>>,
        cfg: WorkflowsConfig,
        audit: Option<Arc<dyn Audit>>,
    ) -> Arc<Self> {
        let pool = registry.pool().clone();
        let secrets = Secrets::new(pool.clone(), vault);
        secrets.register();
        let s = &cfg.sandbox;
        let sandbox = Sandbox::new(asc_ci::sandbox::Config {
            allow: s.allow.clone(),
            path_dirs: s.path_dirs.clone(),
            pass_env: s.pass_env.clone(),
            wrapper: s.wrapper.clone(),
            allow_unisolated: s.allow_unisolated,
            max_output: s.max_output_bytes,
            max_wall: std::time::Duration::from_secs(s.max_command_seconds),
        });
        let work_dir = cfg
            .work_dir
            .clone()
            .unwrap_or_else(|| std::env::temp_dir().join("asc-workflows"));
        let permits = Arc::new(Semaphore::new(cfg.max_concurrent_runs as usize));
        Arc::new(Self {
            store: Store::new(pool),
            secrets,
            agents,
            registry,
            reviews,
            sandbox,
            work_dir,
            instance: crate::ai::rand_id("wf_", 12),
            live: Mutex::new(HashMap::new()),
            wake: Notify::new(),
            permits,
            shutdown: CancellationToken::new(),
            audit,
            defs: Mutex::new(HashMap::new()),
            cfg,
        })
    }

    /// The definitions of the trunk tip of `entry`, read once per tip.
    pub async fn defs_of(&self, entry: &RepoEntry) -> Result<Arc<Defs>, tonic::Status> {
        let view = &self.agents.core.view;
        let Some(tip) = view.trunk_tip(entry).await? else {
            return Ok(Arc::new(Defs::default()));
        };
        let key = (entry.key.0, tip.as_bytes().to_vec());
        if let Some(hit) = self
            .defs
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&key)
        {
            return Ok(hit.clone());
        }
        let loaded = Arc::new(defs::load_at(view, entry, tip, &self.cfg).await?);
        let mut cache = self.defs.lock().unwrap_or_else(PoisonError::into_inner);
        if cache.len() > 512 {
            cache.clear();
        }
        cache.insert(key, loaded.clone());
        Ok(loaded)
    }

    pub(crate) async fn audit(&self, e: Entry<'_>) {
        if let Some(a) = &self.audit {
            a.record(e).await;
        }
    }

    pub(crate) fn live(&self, id: u64) -> Option<Arc<LiveRun>> {
        self.live
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&id)
            .cloned()
    }

    /// Records the run as it is now in its event stream, and wakes the process that drives it.
    pub(crate) async fn emit(&self, run_id: u64, step: &str) {
        let Ok(Some(run)) = self.store.get_run(run_id).await else {
            return;
        };
        let Ok(steps) = self.store.steps(run_id).await else {
            return;
        };
        let snapshot = run_to_wire(&run, &steps, false).encode_to_vec();
        if let Err(e) = self.store.append_event(run_id, step, &snapshot).await {
            tracing::error!(class = %crate::ai::store::class(&e), "workflow event not recorded");
        }
        if let Some(live) = self.live(run_id) {
            live.changed.notify_one();
        }
    }
}

// ------------------------------------------------------------------------------ wire forms

/// The wire form of a trigger kind.
pub fn trigger_to_wire(k: TriggerKind) -> wire::WorkflowTriggerKind {
    match k {
        TriggerKind::Manual => wire::WorkflowTriggerKind::Manual,
        TriggerKind::ChangeOpened => wire::WorkflowTriggerKind::ChangeOpened,
        TriggerKind::ChangeUpdated => wire::WorkflowTriggerKind::ChangeUpdated,
        TriggerKind::CheckFailed => wire::WorkflowTriggerKind::CheckFailed,
        TriggerKind::CommentMention => wire::WorkflowTriggerKind::CommentMention,
        TriggerKind::Land => wire::WorkflowTriggerKind::Land,
        TriggerKind::Schedule => wire::WorkflowTriggerKind::Schedule,
    }
}

/// The trigger kind of a wire value.
pub fn trigger_from_wire(n: i64) -> Option<TriggerKind> {
    let k = wire::WorkflowTriggerKind::try_from(i32::try_from(n).ok()?).ok()?;
    Some(match k {
        wire::WorkflowTriggerKind::Unspecified => return None,
        wire::WorkflowTriggerKind::Manual => TriggerKind::Manual,
        wire::WorkflowTriggerKind::ChangeOpened => TriggerKind::ChangeOpened,
        wire::WorkflowTriggerKind::ChangeUpdated => TriggerKind::ChangeUpdated,
        wire::WorkflowTriggerKind::CheckFailed => TriggerKind::CheckFailed,
        wire::WorkflowTriggerKind::CommentMention => TriggerKind::CommentMention,
        wire::WorkflowTriggerKind::Schedule => TriggerKind::Schedule,
        wire::WorkflowTriggerKind::Land => TriggerKind::Land,
    })
}

fn limits_to_wire(l: &core::Limits) -> wire::AgentBudgetLimits {
    wire::AgentBudgetLimits {
        max_changes: l.max_changes,
        max_tokens: l.max_tokens,
        max_cost_micro_usd: l.max_cost_micro_usd,
        max_paths: l.max_paths,
        max_runtime_seconds: l.max_runtime_seconds,
    }
}

fn step_kind_to_wire(k: StepKind) -> wire::WorkflowStepKind {
    match k {
        StepKind::Agent => wire::WorkflowStepKind::Agent,
        StepKind::Command => wire::WorkflowStepKind::Command,
        StepKind::Approval => wire::WorkflowStepKind::Approval,
        StepKind::Checks => wire::WorkflowStepKind::Checks,
    }
}

fn step_to_wire(s: &Step) -> wire::WorkflowStep {
    wire::WorkflowStep {
        id: s.id.clone(),
        kind: step_kind_to_wire(s.kind) as i32,
        needs: s.needs.clone(),
        agent: s.agent.clone(),
        prompt: s.prompt.clone(),
        paths: s.paths.clone(),
        limits: Some(limits_to_wire(&s.limits)),
        approvers: s.approvers.iter().map(core::Approver::render).collect(),
        checks: s.checks.clone(),
        timeout_seconds: s.timeout_seconds,
        continue_on_failure: s.continue_on_failure,
        command: s.command.clone(),
        secrets: s
            .secrets
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect(),
    }
}

/// The wire form of a definition. `enabled` is the server's state; `source` the trunk changeset.
pub fn def_to_wire(d: &Def, enabled: bool, source: Option<&[u8]>) -> wire::WorkflowDefinition {
    let mut out = wire::WorkflowDefinition {
        name: d.name.clone(),
        path: d.path.clone(),
        source: bytes::Bytes::copy_from_slice(source.unwrap_or_default()),
        enabled,
        valid: d.parsed.is_ok(),
        error: d.error(),
        ..wire::WorkflowDefinition::default()
    };
    if let Some(w) = d.workflow() {
        out.description = w.description.clone();
        out.triggers = w
            .triggers
            .iter()
            .map(|t| wire::WorkflowTrigger {
                kind: trigger_to_wire(t.kind) as i32,
                branches: t.branches.clone(),
                paths: t.paths.iter().map(|p| p.as_str().to_owned()).collect(),
                cron: t
                    .cron
                    .as_ref()
                    .map(|c| c.as_str().to_owned())
                    .unwrap_or_default(),
                agent: t.agent.clone().unwrap_or_default(),
            })
            .collect();
        out.inputs = w
            .inputs
            .iter()
            .map(|i| wire::WorkflowInput {
                name: i.name.clone(),
                description: i.description.clone(),
                required: i.required,
                default_value: i.default.clone(),
            })
            .collect();
        out.steps = w.steps.iter().map(step_to_wire).collect();
        out.limits = Some(limits_to_wire(&w.limits));
        out.max_parallel_runs = w.max_parallel_runs;
    }
    out
}

/// What a step used, from its stored JSON.
pub(crate) fn usage_of(json: &str) -> wire::AgentBudgetUsage {
    let v: serde_json::Value = serde_json::from_str(json).unwrap_or(serde_json::Value::Null);
    let n = |k: &str| v.get(k).and_then(serde_json::Value::as_u64).unwrap_or(0);
    wire::AgentBudgetUsage {
        changes: u32::try_from(n("changes")).unwrap_or(u32::MAX),
        tokens: n("tokens"),
        cost_micro_usd: n("cost"),
        paths: u32::try_from(n("paths")).unwrap_or(u32::MAX),
        runtime_seconds: u32::try_from(n("runtime")).unwrap_or(u32::MAX),
        requests: n("requests"),
    }
}

/// The wire form of a run. `with_output` includes the output of command steps (a snapshot in an
/// event stream leaves it out; `GetRun` has it).
pub fn run_to_wire(r: &RunRow, steps: &[StepRow], with_output: bool) -> wire::WorkflowRun {
    let mut usage = wire::AgentBudgetUsage::default();
    let steps_wire: Vec<wire::WorkflowStepRun> = steps
        .iter()
        .map(|s| {
            let u = usage_of(&s.usage_json);
            usage.changes = usage.changes.saturating_add(u.changes);
            usage.tokens = usage.tokens.saturating_add(u.tokens);
            usage.cost_micro_usd = usage.cost_micro_usd.saturating_add(u.cost_micro_usd);
            usage.paths = usage.paths.saturating_add(u.paths);
            usage.requests = usage.requests.saturating_add(u.requests);
            usage.runtime_seconds = usage.runtime_seconds.saturating_add(u.runtime_seconds);
            wire::WorkflowStepRun {
                id: s.step.clone(),
                state: s.state.to_wire() as i32,
                agent_run_id: s.agent_run_id,
                started: s.started,
                finished: s.finished,
                decided_by: s.decided_by.clone(),
                comment: s.comment.clone(),
                error: s.error.clone(),
                exit_code: s.exit_code,
                timed_out: s.timed_out,
                output: if with_output {
                    s.output.clone()
                } else {
                    String::new()
                },
                ci_run_id: s.ci_run_id,
                usage: (s.agent_run_id > 0).then_some(u),
            }
        })
        .collect();
    let end = if r.finished > 0 {
        r.finished
    } else {
        clock::now()
    };
    if r.started > 0 {
        usage.runtime_seconds = u32::try_from((end - r.started).max(0)).unwrap_or(u32::MAX);
    }
    wire::WorkflowRun {
        id: r.id,
        repo: r.repo.clone(),
        workflow: r.workflow.clone(),
        source: bytes::Bytes::copy_from_slice(&r.source),
        state: r.state.to_wire() as i32,
        trigger: trigger_from_wire(r.trigger).map_or(0, |k| trigger_to_wire(k) as i32),
        started_by: r.started_by.clone(),
        change: bytes::Bytes::copy_from_slice(&r.change),
        inputs: r
            .inputs
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect::<std::collections::HashMap<_, _>>(),
        steps: steps_wire,
        usage: Some(usage),
        created: r.created,
        finished: r.finished,
        error: r.error.clone(),
    }
}

/// Parses the workflow a run was made from.
pub(crate) fn workflow_of(r: &RunRow) -> Result<Workflow, String> {
    Workflow::parse(&r.workflow, r.definition.as_bytes()).map_err(|e| e.message)
}

/// Text made safe to store and show: control characters other than newline and tab are dropped,
/// and it is cut to `max` bytes at a character boundary.
pub(crate) fn clean(s: &str, max: usize) -> String {
    let mut out: String = s
        .chars()
        .filter(|c| !c.is_control() || matches!(c, '\n' | '\t'))
        .collect();
    if out.len() > max {
        let mut cut = max;
        while !out.is_char_boundary(cut) {
            cut -= 1;
        }
        out.truncate(cut);
    }
    out
}

/// The event fields of a run as the template context reads them.
pub(crate) fn template_of(run: &RunRow, change_hex: String) -> core::TemplateContext {
    core::TemplateContext {
        inputs: run.inputs.clone(),
        change: change_hex,
        event: run.event.clone(),
    }
}

#[allow(dead_code)]
fn _types(_: BTreeMap<String, String>) {}

/// What [`register`] needs.
pub struct Parts {
    pub vault: Arc<Vault>,
    pub cfg: WorkflowsConfig,
    /// `None`: the audit log of the registry's database.
    pub audit: Option<Arc<dyn Audit>>,
}

/// Builds the workflow hub and fills its extension slot. An invalid configuration or a second
/// registration is an error the server refuses to start with.
pub fn register(
    ext: &crate::registry::Extensions,
    registry: &Arc<Registry>,
    agents: Arc<AgentsHub>,
    parts: Parts,
) -> Result<Arc<WorkflowsHub>, String> {
    parts.cfg.validate()?;
    let audit: Arc<dyn Audit> = parts
        .audit
        .unwrap_or_else(|| Arc::new(crate::ai::audit::PoolAudit::new(registry.pool().clone())));
    let hub = WorkflowsHub::new(
        registry.clone(),
        agents,
        parts.vault,
        ext.review.get().cloned(),
        parts.cfg,
        Some(audit),
    );
    if !ext.workflows.set(hub.clone()) {
        return Err("the workflow service was already registered".into());
    }
    Ok(hub)
}

/// What `Version.features` adds when the service is mounted.
pub fn features(hub: &WorkflowsHub) -> Vec<String> {
    if hub.cfg.enabled {
        vec![FEATURE.to_owned()]
    } else {
        Vec::new()
    }
}

#[cfg(test)]
mod tests;
#[cfg(test)]
mod tests_runs;
