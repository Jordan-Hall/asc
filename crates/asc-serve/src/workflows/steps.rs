//! What each kind of step does.
//!
//! * **agent**: an agent run of the agent runtime, started for the person who switched the
//!   workflow on, with the limits of the step cut to what is left of the workflow's budget. The
//!   changes it makes are a stack under review; a later step starts from its top.
//! * **command**: a fixed argument list in the operator's sandbox over a working copy of the
//!   change. Event and input text reach the process only as environment variables; secrets are
//!   opened from the vault for this one process and scrubbed from the output. The result is
//!   reported as a check against the exact version of the change.
//! * **approval**: waits for one of the approvers; only [`WorkflowsHub::decide_approval`] ends it,
//!   or the timeout.
//! * **checks**: waits until each named check has succeeded on the newest version of the change.
//!
//! A step ends in `Done`, `Failed(why)` or `Stopped` (the run was told to stop). `why` is text
//! the server wrote, so it holds no secret, and is cleaned and cut before it is stored.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use asc_ci::sandbox::{SandboxError, Spec};
use asc_core::id::ChangesetId;
use asc_core::workflow::{self as core, Step, StepKind, Workflow};
use asc_rpc::server::{CallCtx, RepoName, wire};
use sqlx::Row;
use tokio_util::sync::CancellationToken;

use super::engine::StopWhy;
use super::store::{RunRow, StepRow, StepState};
use super::{CHECK_SOURCE, LiveRun, SYSTEM_ACTOR, WorkflowsHub, clean, template_of, usage_of};
use crate::agents::mask::Masker;
use crate::agents::repo::{PolicyState, changeset_id};
use crate::agents::runs::RunState as AgentState;
use crate::agents::runtime::{Requester, RunRequest};
use crate::agents::tokens::Budget;
use crate::agents::workspace::{Limits, Workspace};
use crate::ai::clock;
use crate::registry::RepoEntry;
use crate::review::handle_of;

/// How a step ended.
pub(crate) enum StepEnd {
    Done,
    Failed(String),
    /// The run was told to stop.
    Stopped,
    /// The step's row already says how it ended (an approval decided by a person).
    Recorded,
}

/// The change a step works on: the top of an earlier agent step's stack, else the change the
/// event was about.
#[derive(Clone, Debug, Default)]
struct Tip {
    cs: Option<ChangesetId>,
    change: Vec<u8>,
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// The last `max` bytes of `s`, from a character boundary, saying so.
fn tail(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_owned();
    }
    let mut from = s.len() - max;
    while !s.is_char_boundary(from) {
        from += 1;
    }
    format!("[... output cut]\n{}", &s[from..])
}

impl WorkflowsHub {
    /// Runs one step to its end and records how it ended.
    pub(crate) async fn run_step(
        self: Arc<Self>,
        run_id: u64,
        wf: Arc<Workflow>,
        step: String,
        live: Arc<LiveRun>,
        resume: bool,
    ) {
        let Ok(Some(run)) = self.store.get_run(run_id).await else {
            return;
        };
        let Some(def) = wf.step(&step) else { return };
        let end = match def.kind {
            StepKind::Agent => self.agent_step(&run, &wf, def, &live, resume).await,
            StepKind::Command => self.command_step(&run, def, &live).await,
            StepKind::Approval => self.approval_step(&run, def, &live).await,
            StepKind::Checks => self.checks_step(&run, &wf, def, &live).await,
        };
        let (to, error) = match end {
            StepEnd::Done => (StepState::Succeeded, String::new()),
            StepEnd::Failed(why) => (StepState::Failed, clean(&why, 500)),
            StepEnd::Stopped => {
                // A server that is shutting down leaves the step as it is; the next one resumes it.
                if matches!(
                    *live
                        .why
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner),
                    Some(StopWhy::Shutdown)
                ) {
                    return;
                }
                (StepState::Cancelled, String::new())
            }
            StepEnd::Recorded => {
                self.emit(run_id, &step).await;
                live.changed.notify_one();
                return;
            }
        };
        let _ = self
            .store
            .step_state(
                run_id,
                &step,
                &[StepState::Running, StepState::Waiting],
                to,
                &error,
            )
            .await;
        self.emit(run_id, &step).await;
        live.changed.notify_one();
    }

    async fn entry_of(&self, repo: &str) -> Result<Arc<RepoEntry>, String> {
        let name =
            RepoName::parse(repo).map_err(|_| "the repository name is not valid".to_owned())?;
        self.registry
            .open(&name)
            .await
            .map_err(|_| "the repository no longer exists".to_owned())
    }

    /// Where a step works: see [`Tip`].
    async fn work_tip(
        &self,
        run: &RunRow,
        wf: &Workflow,
        this: &StepRow,
        steps: &[StepRow],
        entry: &RepoEntry,
    ) -> Tip {
        for s in steps
            .iter()
            .rev()
            .filter(|s| s.ord < this.ord && s.agent_run_id > 0)
        {
            if wf.step(&s.step).is_none_or(|d| d.kind != StepKind::Agent)
                || s.state != StepState::Succeeded
            {
                continue;
            }
            let Ok(Some(ar)) = self.agents.core.runs.get(s.agent_run_id).await else {
                continue;
            };
            let Some(last) = ar.changes.last() else {
                continue;
            };
            if let Ok(Some(cs)) = self.head_of(entry, last).await {
                return Tip {
                    cs: Some(cs),
                    change: last.clone(),
                };
            }
        }
        Tip {
            cs: changeset_id(&run.change_cs),
            change: run.change.clone(),
        }
    }

    // -------------------------------------------------------------------------- agent

    /// The limits a new agent run of `def` gets: its own, cut to what the workflow has left.
    fn step_budget(
        wf: &Workflow,
        def: &Step,
        steps: &[StepRow],
        run: &RunRow,
    ) -> Result<Budget, &'static str> {
        let (mut changes, mut tokens, mut cost, mut paths) = (0u64, 0u64, 0u64, 0u64);
        for s in steps.iter().filter(|s| s.agent_run_id > 0) {
            let u = usage_of(&s.usage_json);
            changes += u64::from(u.changes);
            tokens += u.tokens;
            cost += u.cost_micro_usd;
            paths += u64::from(u.paths);
        }
        let cut =
            |whole: u64, own: u64, used: u64, name: &'static str| -> Result<u64, &'static str> {
                let left = if whole > 0 {
                    match whole.checked_sub(used) {
                        Some(l) if l > 0 => Some(l),
                        _ => return Err(name),
                    }
                } else {
                    None
                };
                Ok(match (own > 0, left) {
                    (true, Some(l)) => own.min(l),
                    (true, None) => own,
                    (false, Some(l)) => l,
                    (false, None) => 0,
                })
            };
        let l = &wf.limits;
        let own = &def.limits;
        let elapsed = u64::try_from((clock::now() - run.started.max(1)).max(0)).unwrap_or(0);
        let runtime = cut(
            u64::from(l.max_runtime_seconds),
            u64::from(own.max_runtime_seconds),
            elapsed,
            "runtime",
        )?;
        // The step's timeout also bounds the run of its agent.
        let timeout = u64::from(def.effective_timeout());
        Ok(Budget {
            changes: cut(
                u64::from(l.max_changes),
                u64::from(own.max_changes),
                changes,
                "changes",
            )?,
            tokens: cut(l.max_tokens, own.max_tokens, tokens, "tokens")?,
            cost: cut(l.max_cost_micro_usd, own.max_cost_micro_usd, cost, "cost")?,
            paths: cut(
                u64::from(l.max_paths),
                u64::from(own.max_paths),
                paths,
                "paths",
            )?,
            runtime: if runtime > 0 {
                runtime.min(timeout)
            } else {
                timeout
            },
        })
    }

    async fn agent_step(
        &self,
        run: &RunRow,
        wf: &Workflow,
        def: &Step,
        live: &Arc<LiveRun>,
        resume: bool,
    ) -> StepEnd {
        // Agent steps of a run take turns, so each starts with an exact budget.
        let _turn = tokio::select! {
            t = live.agent_turn.lock() => t,
            () = live.stop.cancelled() => return StepEnd::Stopped,
        };
        let cfg = &self.agents.core.cfg;
        if !cfg.enabled {
            return StepEnd::Failed(
                "AGENTS_DISABLED: agents are switched off on this server".into(),
            );
        }
        let steps = match self.store.steps(run.id).await {
            Ok(s) => s,
            Err(e) => {
                return StepEnd::Failed(format!("database error: {}", crate::ai::store::class(&e)));
            }
        };
        let Some(this) = steps.iter().find(|s| s.step == def.id) else {
            return StepEnd::Failed("internal error".into());
        };
        let existing = this.agent_run_id;
        let started = clock::now();
        let agent_run_id = if existing > 0 && resume {
            existing
        } else {
            let entry = match self.entry_of(&run.repo).await {
                Ok(e) => e,
                Err(m) => return StepEnd::Failed(m),
            };
            let limits = match Self::step_budget(wf, def, &steps, run) {
                Ok(l) => l,
                Err(dim) => {
                    return StepEnd::Failed(format!(
                        "BUDGET_EXCEEDED: the {dim} budget of the workflow is used up"
                    ));
                }
            };
            let tip = self.work_tip(run, wf, this, &steps, &entry).await;
            let ctx = template_of(run, hex(&tip.change));
            let prompt = core::render(&def.prompt, &ctx);
            let who = Requester {
                principal: run.requester.clone(),
                handle: handle_of(&run.requester),
            };
            let req = RunRequest {
                repo: run.repo.clone(),
                agent: def.agent.clone(),
                title: clean(&format!("{}/{}", run.workflow, def.id), 200),
                task: prompt,
                target: String::new(),
                base: tip.cs.map(|c| hex(c.as_bytes())).unwrap_or_default(),
                paths: def.paths.clone(),
                ai_key_id: String::new(),
                model: String::new(),
                limits,
                workflow_run_id: run.id,
                workflow_step: def.id.clone(),
            };
            match self.agents.create_run(&who, req).await {
                Ok(r) => {
                    let _ = self.store.step_agent_run(run.id, &def.id, r.id).await;
                    r.id
                }
                Err(st) => return StepEnd::Failed(status_text(&st)),
            }
        };
        self.emit(run.id, &def.id).await;

        let timeout = Duration::from_secs(u64::from(def.effective_timeout()));
        let began = std::time::Instant::now();
        let ar = loop {
            let Ok(Some(ar)) = self.agents.core.runs.get(agent_run_id).await else {
                return StepEnd::Failed("the agent run is gone".into());
            };
            if !ar.state.is_active() {
                break ar;
            }
            if live.stop.is_cancelled() {
                let _ = self
                    .agents
                    .cancel_run(agent_run_id, "the workflow was stopped", SYSTEM_ACTOR)
                    .await;
                self.record_usage(run.id, &def.id, agent_run_id, started)
                    .await;
                return StepEnd::Stopped;
            }
            if began.elapsed() >= timeout {
                let _ = self
                    .agents
                    .cancel_run(agent_run_id, "the step timed out", SYSTEM_ACTOR)
                    .await;
                self.record_usage(run.id, &def.id, agent_run_id, started)
                    .await;
                return StepEnd::Failed(format!(
                    "the agent step timed out after {} seconds",
                    timeout.as_secs()
                ));
            }
            tokio::select! {
                () = tokio::time::sleep(Duration::from_millis(300)) => {}
                () = live.stop.cancelled() => {}
            }
        };
        self.record_usage(run.id, &def.id, agent_run_id, started)
            .await;
        match ar.state {
            AgentState::Succeeded => StepEnd::Done,
            AgentState::BudgetExceeded => {
                StepEnd::Failed(format!("BUDGET_EXCEEDED: {}", ar.failure_message))
            }
            // The run was told to stop and its agent run ended first: the step is stopped, not failed.
            AgentState::Cancelled if live.stop.is_cancelled() => StepEnd::Stopped,
            AgentState::Cancelled => StepEnd::Failed("the agent run was cancelled".into()),
            AgentState::Expired => StepEnd::Failed("the agent run ran out of time".into()),
            _ => StepEnd::Failed(if ar.failure_code.is_empty() {
                ar.failure_message
            } else {
                format!("{}: {}", ar.failure_code, ar.failure_message)
            }),
        }
    }

    /// Stores what an agent run used, from the counters of its token.
    async fn record_usage(&self, run_id: u64, step: &str, agent_run: u64, started: i64) {
        let Ok(Some(ar)) = self.agents.core.runs.get(agent_run).await else {
            return;
        };
        let spent = match self.agents.core.tokens.get(&ar.token_id).await {
            Ok(Some(t)) if !ar.token_id.is_empty() => t.spent,
            _ => Default::default(),
        };
        let end = if ar.finished > 0 {
            ar.finished
        } else {
            clock::now()
        };
        let json = format!(
            "{{\"changes\":{},\"tokens\":{},\"cost\":{},\"paths\":{},\"requests\":{},\"runtime\":{}}}",
            spent.changes,
            spent.tokens,
            spent.cost,
            spent.paths,
            spent.requests,
            (end - started).max(0)
        );
        let _ = self.store.step_usage(run_id, step, &json).await;
    }

    // -------------------------------------------------------------------------- command

    async fn command_step(&self, run: &RunRow, def: &Step, live: &Arc<LiveRun>) -> StepEnd {
        if !self.sandbox.enabled() {
            return StepEnd::Failed("commands are not enabled on this server: the operator has not set [workflows.sandbox]".into());
        }
        let wf = match super::workflow_of(run) {
            Ok(w) => w,
            Err(m) => return StepEnd::Failed(m),
        };
        let entry = match self.entry_of(&run.repo).await {
            Ok(e) => e,
            Err(m) => return StepEnd::Failed(m),
        };
        let steps = self.store.steps(run.id).await.unwrap_or_default();
        let Some(this) = steps.iter().find(|s| s.step == def.id) else {
            return StepEnd::Failed("internal error".into());
        };
        let tip = self.work_tip(run, &wf, this, &steps, &entry).await;
        // Secrets go only to code that is already trusted: trunk content, or the exact version of
        // a change a reviewer approved. Anything else (a change nobody approved, an agent's
        // output) could print or send them, and masking the output does not stop that.
        if !def.secrets.is_empty() && !self.tip_trusted(&entry, &tip).await {
            return StepEnd::Failed(
                "this step holds secrets and its working copy is neither landed nor an approved version of a change; approve the change first".into(),
            );
        }
        // The environment: what the event says first, then the secrets, which nothing may override.
        let ctx = template_of(run, hex(&run.change));
        let mut env: BTreeMap<String, String> = core::command_env(&ctx);
        let mut secret_values: Vec<zeroize::Zeroizing<String>> = Vec::new();
        for (var, name) in &def.secrets {
            match self.secrets.open(&run.repo, name).await {
                Ok(Some(v)) => {
                    env.insert(var.clone(), v.to_string());
                    secret_values.push(v);
                }
                Ok(None) => {
                    return StepEnd::Failed(format!(
                        "the secret {name:?} is not set for this repository"
                    ));
                }
                Err(super::secrets::SecretError::Vault(crate::vault::VaultError::Locked(_))) => {
                    return StepEnd::Failed(
                        "the vault is locked, so the secrets of this step cannot be opened".into(),
                    );
                }
                Err(_) => return StepEnd::Failed(format!("the secret {name:?} could not be read")),
            }
        }
        let masker = Masker::new(secret_values.iter().map(|s| s.to_string()));
        let view = &self.agents.core.view;
        let root_cs = match tip.cs {
            Some(c) => Some(c),
            None => match view.trunk_tip(&entry).await {
                Ok(t) => t,
                Err(_) => return StepEnd::Failed("the repository could not be read".into()),
            },
        };
        let base_root = match root_cs {
            Some(cs) => match view.root_of(&entry, cs).await {
                Ok(r) => Some(r),
                Err(_) => return StepEnd::Failed("the working copy could not be read".into()),
            },
            None => None,
        };
        let limits = Limits {
            max_files: self.cfg.max_work_files,
            max_bytes: self.cfg.max_work_bytes,
            max_file_bytes: self.cfg.max_file_bytes,
        };
        let ws = match Workspace::create(
            view.clone(),
            entry.clone(),
            base_root,
            &self.work_dir,
            run.id,
            limits,
        )
        .await
        {
            Ok(w) => w,
            Err(s) => {
                return StepEnd::Failed(format!(
                    "the working copy could not be made: {}",
                    s.message()
                ));
            }
        };
        let started = clock::now();
        let (ci_run, job) = self
            .report_check(&entry, run, def, &tip, wire::CiStatus::Running, started, 0)
            .await;
        let timeout = Duration::from_secs(
            u64::from(def.effective_timeout()).min(self.cfg.sandbox.max_command_seconds),
        );
        let spec = Spec {
            argv: def.command.clone(),
            cwd: ws.tree().to_path_buf(),
            root: ws.dir().to_path_buf(),
            env,
            secrets: secret_values.iter().map(|s| s.to_string()).collect(),
            // A step with secrets has no network beyond the hosts its trunk definition names.
            egress: (!secret_values.is_empty()).then(|| def.egress.clone()),
            timeout,
        };
        let result = self.sandbox.run(&spec, &live.stop).await;
        ws.cleanup().await;
        let finished = clock::now();
        let (end, status, step_result) = match result {
            Ok(o) => {
                let text = tail(&masker.mask(&o.output), self.cfg.max_output_bytes);
                let ok = o.success();
                let head = if o.timed_out {
                    format!("the command timed out after {} seconds", timeout.as_secs())
                } else if o.cancelled {
                    "the command was cancelled".to_owned()
                } else {
                    format!(
                        "exit code {}",
                        o.code
                            .map_or_else(|| "none (killed)".to_owned(), |c| c.to_string())
                    )
                };
                let res = super::store::StepResult {
                    exit_code: o.code.unwrap_or(-1),
                    timed_out: o.timed_out,
                    output: text.clone(),
                    ci_run_id: ci_run,
                };
                let end = if ok {
                    StepEnd::Done
                } else if o.cancelled && live.stop.is_cancelled() {
                    StepEnd::Stopped
                } else {
                    StepEnd::Failed(head)
                };
                (
                    end,
                    if ok {
                        wire::CiStatus::Success
                    } else if o.cancelled {
                        wire::CiStatus::Cancelled
                    } else {
                        wire::CiStatus::Failure
                    },
                    res,
                )
            }
            Err(e) => {
                let why = match e {
                    SandboxError::NotAllowed | SandboxError::NotBare | SandboxError::Empty => {
                        format!(
                            "this command is not allowed on this server; it must start with one of: {}",
                            self.sandbox.allowed_text().join("; ")
                        )
                    }
                    SandboxError::NoEgressControl => {
                        "this step holds secrets and the server has no restricted wrapper ([workflows.sandbox] restricted_wrapper) to deny it the network"
                            .to_owned()
                    }
                    SandboxError::NotIsolated | SandboxError::Disabled => {
                        "commands cannot run on this server: no isolation wrapper is configured"
                            .to_owned()
                    }
                    other => other.to_string(),
                };
                let res = super::store::StepResult {
                    exit_code: -1,
                    timed_out: false,
                    output: why.clone(),
                    ci_run_id: ci_run,
                };
                (StepEnd::Failed(why), wire::CiStatus::Failure, res)
            }
        };
        let _ = self.store.step_result(run.id, &def.id, &step_result).await;
        self.finish_check(
            &entry,
            run,
            def,
            &tip,
            status,
            started,
            finished,
            ci_run,
            job,
            &step_result.output,
        )
        .await;
        end
    }

    /// Reports (or updates) the check of a command step; returns the CI run and job ids, 0 when
    /// there is no CI service or the report was refused.
    #[allow(clippy::too_many_arguments)]
    async fn report_check(
        &self,
        entry: &RepoEntry,
        run: &RunRow,
        def: &Step,
        tip: &Tip,
        status: wire::CiStatus,
        started: i64,
        finished: i64,
    ) -> (u64, u64) {
        let Some(reviews) = &self.reviews else {
            return (0, 0);
        };
        let Ok(repo) = RepoName::parse(&run.repo) else {
            return (0, 0);
        };
        let ctx = CallCtx {
            repo,
            id_mode: asc_rpc::IdMode::Asc,
            principal: asc_rpc::auth::Principal {
                id: SYSTEM_ACTOR.to_owned(),
            },
            op_id: Some(uuid::Uuid::new_v4()),
            client: None,
            admin: Default::default(),
        };
        // The exact version this command ran on.
        let version = if tip.change.is_empty() {
            0
        } else {
            self.version_of(entry, &tip.change, tip.cs).await
        };
        let req = wire::ReportRunRequest {
            repo: Some(wire::RepoRef {
                name: run.repo.clone(),
                id_mode: asc_rpc::IdMode::Asc as i32,
            }),
            source: CHECK_SOURCE.to_owned(),
            external_id: format!("{}/{}/{}", run.workflow, run.id, def.id),
            name: clean(&format!("{}/{}", run.workflow, def.id), 255),
            change: bytes::Bytes::copy_from_slice(&tip.change),
            version,
            status: status as i32,
            started,
            finished,
            url: String::new(),
            jobs: vec![wire::ReportJob {
                name: clean(&def.id, 255),
                status: status as i32,
                started,
                finished,
                url: String::new(),
            }],
        };
        // Steps of one run report at the same time, and a writer that loses a race for the
        // database answers with a transient error: report again, a few times.
        let mut last = tonic::Code::Ok;
        for attempt in 0..5u64 {
            match reviews.ci_report_run(&ctx, req.clone()).await {
                Ok(r) => {
                    let run_ = r.run.unwrap_or_default();
                    return (run_.id, run_.jobs.first().map_or(0, |j| j.id));
                }
                Err(e) => {
                    last = e.code();
                    if !matches!(
                        last,
                        tonic::Code::Internal | tonic::Code::Aborted | tonic::Code::Unavailable
                    ) {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(50 * (attempt + 1))).await;
                }
            }
        }
        tracing::warn!(code = ?last, "a workflow check could not be reported");
        (0, 0)
    }

    #[allow(clippy::too_many_arguments)]
    async fn finish_check(
        &self,
        entry: &RepoEntry,
        run: &RunRow,
        def: &Step,
        tip: &Tip,
        status: wire::CiStatus,
        started: i64,
        finished: i64,
        ci_run: u64,
        job: u64,
        output: &str,
    ) {
        let (id, job_id) = self
            .report_check(entry, run, def, tip, status, started, finished)
            .await;
        let (id, job_id) = if id > 0 { (id, job_id) } else { (ci_run, job) };
        let Some(reviews) = &self.reviews else { return };
        if id == 0 || job_id == 0 || output.is_empty() {
            return;
        }
        let Ok(repo) = RepoName::parse(&run.repo) else {
            return;
        };
        let ctx = CallCtx {
            repo,
            id_mode: asc_rpc::IdMode::Asc,
            principal: asc_rpc::auth::Principal {
                id: SYSTEM_ACTOR.to_owned(),
            },
            op_id: Some(uuid::Uuid::new_v4()),
            client: None,
            admin: Default::default(),
        };
        let data = tail(output, crate::ci::MAX_APPEND);
        let _ = reviews
            .ci_append_log(
                &ctx,
                wire::AppendJobLogRequest {
                    repo: None,
                    run_id: id,
                    job_id,
                    data: bytes::Bytes::from(data.into_bytes()),
                },
            )
            .await;
    }

    /// Whether a working copy at `tip` is trusted with secrets: trunk's own content, or the exact
    /// changeset of a change version that carries an approval for that version.
    async fn tip_trusted(&self, entry: &RepoEntry, tip: &Tip) -> bool {
        let Some(cs) = tip.cs else {
            return true; // no change: the step works on the trunk tip
        };
        if let Ok(Some(trunk)) = self.agents.core.view.trunk_tip(entry).await {
            if trunk == cs {
                return true;
            }
            let graph = crate::policy::graph_of(&self.registry, entry);
            if graph.is_ancestor(cs, trunk).await.unwrap_or(false) {
                return true;
            }
        }
        self.approved_exactly(entry, &tip.change, cs).await
    }

    /// Whether `cs` is version `v` of `change` and the review state holds an approval for exactly
    /// version `v`. Read at the time the step runs, never remembered from when the run began.
    async fn approved_exactly(&self, entry: &RepoEntry, change: &[u8], cs: ChangesetId) -> bool {
        let version = self.version_of(entry, change, Some(cs)).await;
        if version == 0 {
            return false;
        }
        let state: Option<Vec<u8>> =
            sqlx::query_scalar("SELECT state FROM review_state WHERE ns = $1 AND change = $2")
                .bind(entry.ns.as_bytes().to_vec())
                .bind(change.to_vec())
                .fetch_optional(self.store.pool())
                .await
                .ok()
                .flatten();
        let review = state
            .and_then(|raw| <wire::ReviewState as prost::Message>::decode(raw.as_slice()).ok());
        // The approvals the policy asks of a landing (at least one) must be for exactly this version.
        let need = match self.agents.core.view.policy(entry).await {
            Ok(state) => match &*state {
                PolicyState::Valid(p) => p.land_requirements().approvals,
                _ => 0,
            },
            Err(_) => 0,
        };
        review.is_some_and(|r| approval_holds(&r, version, need))
    }

    /// The version of `change` that is changeset `cs`, 0 when the version log does not know it
    /// (the check then lands on the newest version).
    async fn version_of(&self, entry: &RepoEntry, change: &[u8], cs: Option<ChangesetId>) -> u32 {
        let Some(cs) = cs else { return 0 };
        let v: Option<i64> = sqlx::query_scalar(
            "SELECT CAST(v AS BIGINT) FROM change_versions WHERE ns = $1 AND change = $2 AND cs = $3",
        )
        .bind(entry.ns.as_bytes().to_vec())
        .bind(change.to_vec())
        .bind(cs.as_bytes().to_vec())
        .fetch_optional(self.store.pool())
        .await
        .ok()
        .flatten();
        v.and_then(|v| u32::try_from(v).ok()).unwrap_or(0)
    }

    // -------------------------------------------------------------------------- approval

    async fn approval_step(&self, run: &RunRow, def: &Step, live: &Arc<LiveRun>) -> StepEnd {
        // The step is Running when it starts; it waits for a person.
        let _ = self
            .store
            .step_state(
                run.id,
                &def.id,
                &[StepState::Running],
                StepState::Waiting,
                "",
            )
            .await;
        self.emit(run.id, &def.id).await;
        let timeout = Duration::from_secs(u64::from(def.effective_timeout()));
        let waited_from = self
            .store
            .step(run.id, &def.id)
            .await
            .ok()
            .flatten()
            .map_or_else(clock::now, |s| s.started.max(1));
        loop {
            let Ok(Some(row)) = self.store.step(run.id, &def.id).await else {
                return StepEnd::Failed("the step is gone".into());
            };
            if row.state != StepState::Waiting {
                return StepEnd::Recorded;
            }
            if live.stop.is_cancelled() {
                return StepEnd::Stopped;
            }
            let waited = u64::try_from((clock::now() - waited_from).max(0)).unwrap_or(0);
            if waited >= timeout.as_secs() {
                return StepEnd::Failed(format!(
                    "nobody decided within {} seconds",
                    timeout.as_secs()
                ));
            }
            tokio::select! {
                () = live.changed.notified() => {}
                () = tokio::time::sleep(Duration::from_millis(500)) => {}
                () = live.stop.cancelled() => {}
            }
        }
    }

    // -------------------------------------------------------------------------- checks

    async fn checks_step(
        &self,
        run: &RunRow,
        wf: &Workflow,
        def: &Step,
        live: &Arc<LiveRun>,
    ) -> StepEnd {
        let entry = match self.entry_of(&run.repo).await {
            Ok(e) => e,
            Err(m) => return StepEnd::Failed(m),
        };
        let steps = self.store.steps(run.id).await.unwrap_or_default();
        let Some(this) = steps.iter().find(|s| s.step == def.id) else {
            return StepEnd::Failed("internal error".into());
        };
        let tip = self.work_tip(run, wf, this, &steps, &entry).await;
        if tip.change.is_empty() {
            return StepEnd::Failed("there is no change to wait for checks on".into());
        }
        let timeout = Duration::from_secs(u64::from(def.effective_timeout()));
        let began = std::time::Instant::now();
        loop {
            if live.stop.is_cancelled() {
                return StepEnd::Stopped;
            }
            let latest = self.latest_version(&entry, &tip.change).await.unwrap_or(0);
            let mut waiting: Vec<&str> = Vec::new();
            for name in &def.checks {
                let row = sqlx::query(
                    "SELECT status, version FROM check_runs WHERE ns = $1 AND change = $2 AND name = $3 ORDER BY id DESC LIMIT 1",
                )
                .bind(entry.ns.as_bytes().to_vec())
                .bind(tip.change.clone())
                .bind(name.clone())
                .fetch_optional(self.store.pool())
                .await;
                match row {
                    Ok(None) => waiting.push(name),
                    Ok(Some(r)) => {
                        let status: i64 = r.try_get("status").unwrap_or(0);
                        let version: i64 = r.try_get("version").unwrap_or(0);
                        if latest > 0 && version < latest {
                            // A result for an older version says nothing about this one.
                            waiting.push(name);
                            continue;
                        }
                        match status {
                            3 => {}
                            4 | 5 => {
                                return StepEnd::Failed(format!(
                                    "the check {name:?} did not succeed"
                                ));
                            }
                            6 => return StepEnd::Failed(format!("the check {name:?} was skipped")),
                            _ => waiting.push(name),
                        }
                    }
                    Err(e) => {
                        return StepEnd::Failed(format!(
                            "database error: {}",
                            crate::ai::store::class(&e)
                        ));
                    }
                }
            }
            if waiting.is_empty() {
                return StepEnd::Done;
            }
            if began.elapsed() >= timeout {
                return StepEnd::Failed(format!(
                    "timed out after {} seconds waiting for: {}",
                    timeout.as_secs(),
                    waiting
                        .iter()
                        .map(|w| format!("{w:?}"))
                        .collect::<Vec<_>>()
                        .join(", ")
                ));
            }
            tokio::select! {
                () = tokio::time::sleep(Duration::from_secs(2)) => {}
                () = live.stop.cancelled() => {}
                () = live.changed.notified() => {}
            }
        }
    }
}

/// A refusal of an internal service as one line of text for a step: the typed code when there is
/// one, then the message. The services' messages hold no secret.
fn status_text(s: &tonic::Status) -> String {
    let code = asc_rpc::server::status_rpc_error(s)
        .map(|e| e.code().to_owned())
        .unwrap_or_default();
    let msg = clean(s.message(), 300);
    if code.is_empty() || msg.starts_with(&code) {
        msg
    } else {
        format!("{code}: {msg}")
    }
}

#[allow(dead_code)]
fn _unused(_: CancellationToken, _: &Option<&Arc<()>>) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_is_cut_from_the_front_on_a_character_boundary() {
        assert_eq!(tail("short", 100), "short");
        let t = tail(&"é".repeat(100), 51);
        assert!(t.starts_with("[... output cut]\n"));
        assert!(t.len() < 100);
        assert!(t.ends_with('é'));
    }
}

/// Whether `review` holds the approvals a landing needs (the policy's count, never fewer than
/// one) for exactly `version`: an approval of an older or a newer version is not one.
fn approval_holds(review: &wire::ReviewState, version: u32, policy_need: u32) -> bool {
    version != 0 && review.reviewed_version == version && review.approvals >= policy_need.max(1)
}

#[cfg(test)]
mod approval_tests {
    use super::*;

    fn review(approvals: u32, reviewed_version: u32) -> wire::ReviewState {
        wire::ReviewState {
            approvals,
            reviewed_version,
            ..wire::ReviewState::default()
        }
    }

    #[test]
    fn secrets_need_the_policys_count_of_approvals_for_exactly_the_running_version() {
        assert!(
            approval_holds(&review(1, 3), 3, 0),
            "no policy: one approval"
        );
        assert!(approval_holds(&review(1, 3), 3, 1));
        assert!(
            !approval_holds(&review(1, 3), 3, 2),
            "the policy asks for two"
        );
        assert!(approval_holds(&review(2, 3), 3, 2));
        assert!(!approval_holds(&review(2, 2), 3, 2), "an older version");
        assert!(!approval_holds(&review(2, 4), 3, 2), "a newer version");
        assert!(!approval_holds(&review(0, 3), 3, 0), "no approval");
        assert!(!approval_holds(&review(5, 0), 0, 0), "an unknown version");
    }
}
