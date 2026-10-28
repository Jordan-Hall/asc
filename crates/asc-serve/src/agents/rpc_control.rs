//! The control and board calls of the `Agents` service (see `control.rs`): requests and their
//! decisions, pause, checkpoints and rollback, run limits, continue, estimate, recent tasks, run
//! log, the agents of a repository and spend. A child module of `rpc`, so it reads its private
//! helpers.
//!
//! Who may do what: reads need repo read. Deciding, pausing, rolling back, editing limits and
//! continuing need repo write and either that the caller started the run, is a policy maintainer
//! or holds repo admin ([`AgentService::may_manage`]). Monthly budgets need repo admin. A call
//! made for a person by the host carries the forwarded claim (`review::acting`), acts with the
//! role of the claim only, and is audited with the host as relay. A run of another repository
//! is "not found", never "forbidden".

use asc_rpc::auth::Access;
use asc_rpc::server::{CallCtx, wire};
use tonic::{Request, Response, Status};

use super::super::control::{
    self, ALLOWED, APPROVED, CheckpointRow, DENIED, KIND_PLAN, MAX_REASON, REJECTED, ReqRow,
};
use super::super::errors;
use super::super::repo::PolicyState;
use super::super::runs::{RunRow, RunState};
use super::super::runtime::{Requester, RunRequest};
use super::super::tokens::{self, Budget};
use super::{AgentService, db, id_of, person};
use crate::review::{handle_of, require_handle};

const DEFAULT_REQUESTS: usize = 50;
const MAX_REQUESTS: usize = 200;

fn relay(ctx: &CallCtx) -> Option<&'static str> {
    crate::review::acting::relayed(ctx).then_some("host-service")
}

impl AgentService {
    async fn note(&self, ctx: &CallCtx, action: &str, run: &RunRow, target: &str, d: &[(&str, String)]) {
        self.hub
            .audit(action, &ctx.principal.id, relay(ctx), &run.repo, target, d)
            .await;
    }

    /// The caller started the run, is a maintainer of the policy, or holds repo admin.
    pub(super) async fn may_manage(
        &self,
        ext: &tonic::Extensions,
        ctx: &CallCtx,
        run: &RunRow,
    ) -> Result<(), Status> {
        let mut allowed = Self::started(ctx, run);
        if !allowed {
            allowed = self.env.has(ext, ctx, Access::Admin).await;
        }
        if !allowed && let Some(who) = person(&ctx.principal.id) {
            let entry = self
                .hub
                .registry
                .open(&ctx.repo)
                .await
                .map_err(|e| crate::update::internal("registry", &e))?;
            if let PolicyState::Valid(p) = &*self.hub.core.view.policy(&entry).await? {
                allowed = p.is_maintainer(&who);
            }
        }
        if allowed {
            Ok(())
        } else {
            Err(Status::permission_denied(
                "only the person who started the run, a maintainer or an administrator can do this",
            ))
        }
    }

    /// The wire form of a run with the control fields (task, progress, pause, requests).
    pub(super) async fn wire_run_full(&self, r: &RunRow) -> wire::AgentRun {
        let mut w = self.wire_run(r).await;
        if let Ok(ctl) = self.hub.ctl(r.id).await {
            w.progress = Some(self.hub.progress(r, &ctl).await);
            w.paused = ctl.paused;
            w.continues_run = ctl.continues;
            w.for_change = bytes::Bytes::from(ctl.for_change);
        }
        w.pending_requests = self.hub.pending_requests(r.id).await;
        w
    }

    async fn request_wire(&self, r: &ReqRow) -> wire::AgentRequest {
        let plan = if r.kind == KIND_PLAN {
            self.hub
                .plan_of(r.run_id)
                .await
                .unwrap_or_default()
                .into_iter()
                .map(|(index, text, state)| wire::AgentPlanStep { index, text, state })
                .collect()
        } else {
            Vec::new()
        };
        wire::AgentRequest {
            id: r.id,
            run_id: r.run_id,
            kind: r.kind as i32,
            state: r.state as i32,
            summary: r.summary.clone(),
            detail: r.detail.clone(),
            plan,
            requested_at: r.requested_at,
            decided_by: r.decided_by.clone(),
            decided_at: r.decided_at,
            reason: r.reason.clone(),
            agent: r.agent.clone(),
        }
    }

    fn checkpoint_wire(c: &CheckpointRow) -> wire::AgentCheckpoint {
        wire::AgentCheckpoint {
            id: c.id,
            run_id: c.run_id,
            seq: u32::try_from(c.seq).unwrap_or(u32::MAX),
            at: c.at,
            label: c.label.clone(),
            changes: c.changes.iter().map(|x| bytes::Bytes::copy_from_slice(x)).collect(),
            tip: bytes::Bytes::copy_from_slice(&c.tip),
            rolled_back_at: c.rolled_back_at,
            rolled_back_by: c.rolled_back_by.clone(),
        }
    }

    /// The policy cap of a repository, as limits (0 = none).
    async fn policy_cap(&self, ctx: &CallCtx) -> Result<Budget, Status> {
        let entry = self
            .hub
            .registry
            .open(&ctx.repo)
            .await
            .map_err(|e| crate::update::internal("registry", &e))?;
        // The cap is the stricter of the policy and the instance defaults: an edit cannot lift a run above
        // what a new run could be given.
        let d = self.hub.core.cfg.default_limits;
        let instance = Budget {
            changes: u64::from(d.max_changes),
            tokens: d.max_tokens,
            cost: d.max_cost_micro_usd,
            paths: u64::from(d.max_paths),
            runtime: u64::from(d.max_runtime_seconds),
        };
        let policy = match &*self.hub.core.view.policy(&entry).await? {
            PolicyState::Valid(p) => {
                let b = &p.agents.budgets;
                Budget {
                    changes: b.max_changes_per_run.map_or(0, u64::from),
                    tokens: b.max_tokens.unwrap_or(0),
                    cost: b.max_cost_micro_usd.unwrap_or(0),
                    paths: b.max_paths_per_run.map_or(0, u64::from),
                    runtime: p.agents.max_runtime_seconds.map_or(0, u64::from),
                }
            }
            PolicyState::Absent => Budget::default(),
            // Fail closed: a policy that cannot be used gives no cap to edit against.
            PolicyState::Invalid(_) => {
                return Err(Status::failed_precondition(
                    "the repository policy cannot be used; fix .asc/policy.toml on trunk",
                ));
            }
        };
        Ok(policy.merge(instance))
    }

    // ---------------------------------------------------------------------------- requests

    pub(super) async fn list_requests_impl(
        &self,
        request: Request<wire::ListAgentRequestsRequest>,
    ) -> Result<Response<wire::ListAgentRequestsResponse>, Status> {
        let (meta, ext, msg) = request.into_parts();
        let ctx = self.env.context(&meta, &ext, msg.repo.as_ref(), Access::Read, false).await?;
        let limit = match msg.limit {
            0 => DEFAULT_REQUESTS,
            n => usize::try_from(n).unwrap_or(MAX_REQUESTS).min(MAX_REQUESTS),
        };
        if msg.states.len() > 8 {
            return Err(Status::invalid_argument("at most 8 states"));
        }
        let mut states = Vec::new();
        for s in &msg.states {
            let st = wire::AgentRequestState::try_from(*s)
                .map_err(|_| Status::invalid_argument("unknown state"))?;
            if st != wire::AgentRequestState::Unspecified {
                states.push(st as i64);
            }
        }
        if msg.run_id != 0 {
            self.run_in(&ctx, msg.run_id).await?;
        }
        let scope = format!("Agents.ListRequests|{}|{}", ctx.repo, msg.run_id);
        let before = crate::pagetoken::open_id(&scope, &ctx.principal.id, &msg.page_token)?
            .map(|n| u64::try_from(n).unwrap_or(0));
        let mut rows = self
            .hub
            .list_requests(ctx.repo.as_str(), msg.run_id, &states, before, limit)
            .await?;
        let next = if rows.len() > limit {
            rows.truncate(limit);
            rows.last()
                .map(|r| crate::pagetoken::seal(&scope, &ctx.principal.id, &r.id.to_string()))
                .unwrap_or_default()
        } else {
            String::new()
        };
        let mut requests = Vec::with_capacity(rows.len());
        for r in &rows {
            requests.push(self.request_wire(r).await);
        }
        Ok(Response::new(wire::ListAgentRequestsResponse { requests, next_page_token: next }))
    }

    pub(super) async fn decide_request_impl(
        &self,
        request: Request<wire::DecideAgentRequestRequest>,
    ) -> Result<Response<wire::DecideAgentRequestResponse>, Status> {
        let (meta, ext, msg) = request.into_parts();
        let ctx = self.env.context(&meta, &ext, msg.repo.as_ref(), Access::Write, true).await?;
        if msg.reason.len() > MAX_REASON {
            return Err(Status::invalid_argument("a reason is at most 512 bytes"));
        }
        let to = match wire::AgentDecision::try_from(msg.decision) {
            Ok(wire::AgentDecision::AllowOnce) => ALLOWED,
            Ok(wire::AgentDecision::Deny) => DENIED,
            Ok(wire::AgentDecision::ApprovePlan) => APPROVED,
            Ok(wire::AgentDecision::RejectPlan) => REJECTED,
            _ => return Err(Status::invalid_argument("a decision is needed")),
        };
        let id = id_of(msg.request_id)?;
        let cur = self
            .hub
            .request(id)
            .await?
            .filter(|r| r.repo.eq_ignore_ascii_case(ctx.repo.as_str()))
            .ok_or_else(|| Status::not_found("no such request"))?;
        let run = self.run_in(&ctx, cur.run_id).await?;
        self.may_manage(&ext, &ctx, &run).await?;
        let decider = handle_of(&ctx.principal.id).unwrap_or_else(|| ctx.principal.id.clone());
        let was_pending = cur.state == control::PENDING;
        let row = self.hub.decide_request(id, to, &decider, &msg.reason).await?;
        if was_pending && row.state != control::PENDING {
            self.note(
                &ctx,
                "agent.request.decided",
                &run,
                &row.id.to_string(),
                &[
                    ("run", run.id.to_string()),
                    ("decision", row.state.to_string()),
                    ("reason", row.reason.clone()),
                ],
            )
            .await;
        }
        let run = self.hub.core.runs.get(run.id).await.map_err(|e| db(&e))?.unwrap_or(run);
        Ok(Response::new(wire::DecideAgentRequestResponse {
            request: Some(self.request_wire(&row).await),
            run: Some(self.wire_run_full(&run).await),
        }))
    }

    // ---------------------------------------------------------------------------- pause, checkpoints

    pub(super) async fn pause_run_impl(
        &self,
        request: Request<wire::PauseAgentRunRequest>,
    ) -> Result<Response<wire::PauseAgentRunResponse>, Status> {
        let (meta, ext, msg) = request.into_parts();
        let ctx = self.env.context(&meta, &ext, msg.repo.as_ref(), Access::Write, false).await?;
        if msg.reason.len() > MAX_REASON {
            return Err(Status::invalid_argument("a reason is at most 512 bytes"));
        }
        let run = self.run_in(&ctx, msg.id).await?;
        self.may_manage(&ext, &ctx, &run).await?;
        let by = handle_of(&ctx.principal.id).unwrap_or_else(|| ctx.principal.id.clone());
        let before = self.hub.ctl(run.id).await?;
        self.hub.set_paused(&run, msg.paused, &by).await?;
        if before.paused != msg.paused {
            self.note(
                &ctx,
                if msg.paused { "agent.run.paused" } else { "agent.run.resumed" },
                &run,
                &run.id.to_string(),
                &[("run", run.id.to_string()), ("reason", msg.reason.clone())],
            )
            .await;
        }
        let run = self.hub.core.runs.get(run.id).await.map_err(|e| db(&e))?.unwrap_or(run);
        Ok(Response::new(wire::PauseAgentRunResponse { run: Some(self.wire_run_full(&run).await) }))
    }

    pub(super) async fn list_checkpoints_impl(
        &self,
        request: Request<wire::ListAgentCheckpointsRequest>,
    ) -> Result<Response<wire::ListAgentCheckpointsResponse>, Status> {
        let (meta, ext, msg) = request.into_parts();
        let ctx = self.env.context(&meta, &ext, msg.repo.as_ref(), Access::Read, false).await?;
        let run = self.run_in(&ctx, msg.run_id).await?;
        let cps = self.hub.checkpoints(run.id).await?;
        Ok(Response::new(wire::ListAgentCheckpointsResponse {
            checkpoints: cps.iter().map(Self::checkpoint_wire).collect(),
        }))
    }

    pub(super) async fn rollback_run_impl(
        &self,
        request: Request<wire::RollbackAgentRunRequest>,
    ) -> Result<Response<wire::RollbackAgentRunResponse>, Status> {
        let (meta, ext, msg) = request.into_parts();
        let ctx = self.env.context(&meta, &ext, msg.repo.as_ref(), Access::Write, true).await?;
        if msg.reason.len() > MAX_REASON {
            return Err(Status::invalid_argument("a reason is at most 512 bytes"));
        }
        let run = self.run_in(&ctx, msg.run_id).await?;
        self.may_manage(&ext, &ctx, &run).await?;
        let cp = self
            .hub
            .checkpoint(id_of(msg.checkpoint_id)?)
            .await?
            .filter(|c| c.run_id == run.id)
            .ok_or_else(|| Status::not_found("no such checkpoint"))?;
        let by = handle_of(&ctx.principal.id).unwrap_or_else(|| ctx.principal.id.clone());
        self.hub.request_rollback(&run, &cp).await?;
        let _ = sqlx::query("UPDATE agent_run_ctl SET paused_by = $2 WHERE run_id = $1")
            .bind(i64::try_from(run.id).unwrap_or(0))
            .bind(by)
            .execute(&self.hub.core.pool)
            .await;
        self.note(
            &ctx,
            "agent.run.rollback_requested",
            &run,
            &run.id.to_string(),
            &[
                ("run", run.id.to_string()),
                ("checkpoint", cp.seq.to_string()),
                ("reason", msg.reason.clone()),
            ],
        )
        .await;
        Ok(Response::new(wire::RollbackAgentRunResponse {
            run: Some(self.wire_run_full(&run).await),
            checkpoint: Some(Self::checkpoint_wire(&cp)),
        }))
    }

    // ---------------------------------------------------------------------------- limits, continue

    pub(super) async fn set_run_limits_impl(
        &self,
        request: Request<wire::SetAgentRunLimitsRequest>,
    ) -> Result<Response<wire::SetAgentRunLimitsResponse>, Status> {
        let (meta, ext, msg) = request.into_parts();
        let ctx = self.env.context(&meta, &ext, msg.repo.as_ref(), Access::Write, false).await?;
        let run = self.run_in(&ctx, msg.id).await?;
        self.may_manage(&ext, &ctx, &run).await?;
        if !run.state.is_active() {
            return Err(errors::run_not_active(run.state.name()));
        }
        let asked = Budget::from_wire(msg.limits.as_ref());
        let cap = self.policy_cap(&ctx).await?;
        let pick = |asked: u64, cur: u64, cap: u64| -> u64 {
            if asked == 0 {
                cur
            } else if cap > 0 && asked > cap {
                cap
            } else {
                asked
            }
        };
        let new = Budget {
            changes: pick(asked.changes, run.limits.changes, cap.changes),
            tokens: pick(asked.tokens, run.limits.tokens, cap.tokens),
            cost: pick(asked.cost, run.limits.cost, cap.cost),
            paths: pick(asked.paths, run.limits.paths, cap.paths),
            runtime: pick(asked.runtime, run.limits.runtime, cap.runtime),
        };
        let lowered = super::super::AgentsHub::lowered(asked, new);
        self.hub.core.runs.set_limits(run.id, new).await.map_err(|e| db(&e))?;
        if !run.token_id.is_empty() {
            self.hub.core.tokens.set_limits(&run.token_id, new).await.map_err(|e| db(&e))?;
        }
        self.note(
            &ctx,
            "agent.run.limits_changed",
            &run,
            &run.id.to_string(),
            &[
                ("run", run.id.to_string()),
                ("tokens", new.tokens.to_string()),
                ("cost", new.cost.to_string()),
                ("runtime", new.runtime.to_string()),
            ],
        )
        .await;
        let run = self.hub.core.runs.get(run.id).await.map_err(|e| db(&e))?.unwrap_or(run);
        Ok(Response::new(wire::SetAgentRunLimitsResponse {
            run: Some(self.wire_run_full(&run).await),
            lowered,
        }))
    }

    pub(super) async fn continue_run_impl(
        &self,
        request: Request<wire::ContinueAgentRunRequest>,
    ) -> Result<Response<wire::ContinueAgentRunResponse>, Status> {
        let (meta, ext, msg) = request.into_parts();
        let ctx = self.env.context(&meta, &ext, msg.repo.as_ref(), Access::Write, true).await?;
        let handle = require_handle(&ctx)?;
        let old = self.run_in(&ctx, msg.id).await?;
        self.may_manage(&ext, &ctx, &old).await?;
        if old.state.is_active() {
            return Err(Status::failed_precondition("the run has not finished"));
        }
        if matches!(old.state, RunState::Succeeded) {
            return Err(Status::failed_precondition("the run succeeded; start a new run instead"));
        }
        let asked = Budget::from_wire(msg.limits.as_ref());
        let limits = Budget {
            changes: if asked.changes > 0 { asked.changes } else { old.limits.changes },
            tokens: if asked.tokens > 0 { asked.tokens } else { old.limits.tokens },
            cost: if asked.cost > 0 { asked.cost } else { old.limits.cost },
            paths: if asked.paths > 0 { asked.paths } else { old.limits.paths },
            runtime: if asked.runtime > 0 { asked.runtime } else { old.limits.runtime },
        };
        let ctl = self.hub.ctl(old.id).await?;
        let task = format!(
            "{}\n\nThis run continues run #{}, which ended as {} after making {} change{}. Look at your earlier work in the repository and carry on from where it stopped.",
            old.task,
            old.id,
            old.state.name(),
            old.changes.len(),
            if old.changes.len() == 1 { "" } else { "s" }
        );
        let who = Requester { principal: ctx.principal.id.clone(), handle: Some(handle) };
        let req = RunRequest {
            repo: ctx.repo.as_str().to_owned(),
            agent: old.agent.clone(),
            title: old.title.clone(),
            task,
            target: old.target.clone(),
            base: hex(&old.base),
            paths: old.paths.clone(),
            ai_key_id: old.ai_key_id.clone(),
            model: old.model.clone(),
            limits,
            workflow_run_id: 0,
            workflow_step: String::new(),
            plan_first: ctl.plan_first,
            for_change: ctl.for_change.clone(),
            continues: old.id,
        };
        let op = ctx.op_id.ok_or_else(|| Status::invalid_argument("mutating calls need x-asc-op-id"))?;
        let hub = self.hub.clone();
        let ops = hub.core.ops.clone();
        let this = self.clone();
        let out = crate::ai::caller::once(
            &ops,
            op,
            &ctx.principal.id,
            "Agents/ContinueRun",
            true,
            || async move {
                let row = hub.create_run(&who, req).await?;
                let lowered = super::super::AgentsHub::lowered(limits, row.limits);
                Ok(wire::ContinueAgentRunResponse {
                    run: Some(this.wire_run_full(&row).await),
                    lowered,
                })
            },
        )
        .await?;
        self.note(
            &ctx,
            "agent.run.continued",
            &old,
            &old.id.to_string(),
            &[
                ("run", old.id.to_string()),
                ("new_run", out.run.as_ref().map_or(0, |r| r.id).to_string()),
            ],
        )
        .await;
        Ok(Response::new(out))
    }

    // ---------------------------------------------------------------------------- boards

    pub(super) async fn get_estimate_impl(
        &self,
        request: Request<wire::GetAgentEstimateRequest>,
    ) -> Result<Response<wire::GetAgentEstimateResponse>, Status> {
        let (meta, ext, msg) = request.into_parts();
        let ctx = self.env.context(&meta, &ext, msg.repo.as_ref(), Access::Read, false).await?;
        if !msg.agent.is_empty() && !tokens::valid_agent_name(&msg.agent) {
            return Err(Status::invalid_argument("an agent name is 1 to 40 characters of a-z, 0-9 and -"));
        }
        let (seconds, cost, toks, samples) =
            self.hub.estimate(ctx.repo.as_str(), &msg.agent, msg.task_bytes).await?;
        Ok(Response::new(wire::GetAgentEstimateResponse {
            samples,
            seconds,
            cost_micro_usd: cost,
            tokens: toks,
        }))
    }

    pub(super) async fn recent_tasks_impl(
        &self,
        request: Request<wire::RecentAgentTasksRequest>,
    ) -> Result<Response<wire::RecentAgentTasksResponse>, Status> {
        let (meta, ext, msg) = request.into_parts();
        let ctx = self.env.context(&meta, &ext, msg.repo.as_ref(), Access::Read, false).await?;
        let limit = match msg.limit {
            0 => 10,
            n => usize::try_from(n).unwrap_or(50).min(50),
        };
        // Tasks are the text of a person: a reader sees their own, an administrator everyone's.
        let by = if self.env.has(&ext, &ctx, Access::Admin).await {
            String::new()
        } else {
            ctx.principal.id.clone()
        };
        let rows = self.hub.recent_tasks(ctx.repo.as_str(), &by, limit).await?;
        Ok(Response::new(wire::RecentAgentTasksResponse {
            tasks: rows
                .into_iter()
                .map(|r| wire::AgentTaskRef {
                    task: r.task,
                    title: r.title,
                    agent: r.agent,
                    run_id: r.id,
                    at: r.created,
                })
                .collect(),
        }))
    }

    pub(super) async fn get_run_log_impl(
        &self,
        request: Request<wire::GetAgentRunLogRequest>,
    ) -> Result<Response<wire::GetAgentRunLogResponse>, Status> {
        let (meta, ext, msg) = request.into_parts();
        let ctx = self.env.context(&meta, &ext, msg.repo.as_ref(), Access::Read, false).await?;
        let run = self.run_in(&ctx, msg.id).await?;
        let (jsonl, truncated) = self.hub.run_log(&run).await?;
        let days = i64::from(self.hub.core.cfg.event_retention_days);
        let base = if run.finished > 0 { run.finished } else { crate::ai::clock::now() };
        Ok(Response::new(wire::GetAgentRunLogResponse {
            jsonl: bytes::Bytes::from(jsonl),
            truncated,
            retained_until: base + days * 86_400,
        }))
    }

    pub(super) async fn list_agents_impl(
        &self,
        request: Request<wire::ListRepoAgentsRequest>,
    ) -> Result<Response<wire::ListRepoAgentsResponse>, Status> {
        let (meta, ext, msg) = request.into_parts();
        let ctx = self.env.context(&meta, &ext, msg.repo.as_ref(), Access::Read, false).await?;
        let repo = ctx.repo.as_str().to_ascii_lowercase();
        let runs = self.hub.core.runs.list(&repo, &[], "", "", None, 1000).await.map_err(|e| db(&e))?;
        let entry = self
            .hub
            .registry
            .open(&ctx.repo)
            .await
            .map_err(|e| crate::update::internal("registry", &e))?;
        let now = crate::ai::clock::now();
        let since = now - 30 * 86_400;
        let month = control::this_month();
        let mut by: std::collections::BTreeMap<String, wire::RepoAgentInfo> = Default::default();
        let (mut running, mut landed_all, mut changes_all) = (0u32, 0u32, 0u32);
        let mut checked = 0usize;
        for r in runs.iter().take(1000) {
            let e = by.entry(r.agent.clone()).or_insert_with(|| wire::RepoAgentInfo {
                agent: r.agent.clone(),
                paths: r.paths.clone(),
                ..Default::default()
            });
            e.runs += 1;
            e.last_run = e.last_run.max(r.created);
            if r.state.is_active() {
                e.active += 1;
                running += 1;
            }
            if r.created >= since {
                for ch in &r.changes {
                    changes_all += 1;
                    if checked >= 1000 {
                        continue;
                    }
                    checked += 1;
                    let hit: Option<i64> = sqlx::query_scalar(
                        "SELECT 1 FROM changes c JOIN mainline m ON m.ns = c.ns AND m.cs = c.cs \
                         WHERE c.ns = $1 AND c.change = $2 LIMIT 1",
                    )
                    .bind(entry.ns.as_bytes().to_vec())
                    .bind(ch.clone())
                    .fetch_optional(&self.hub.core.pool)
                    .await
                    .map_err(|e| db(&e))?;
                    if hit.is_some() {
                        e.landed_30d += 1;
                        landed_all += 1;
                    }
                }
            }
        }
        for (agent, e) in &mut by {
            e.cost_micro_usd_month = self.hub.month_spend(&repo, &month, agent).await?.0;
        }
        for r in self.hub.list_requests(&repo, 0, &[control::PENDING], None, 200).await? {
            if let Some(e) = by.get_mut(&r.agent) {
                e.pending_requests += 1;
            }
        }
        Ok(Response::new(wire::ListRepoAgentsResponse {
            agents: by.into_values().collect(),
            running,
            max_concurrent: self.hub.core.cfg.max_concurrent_per_repo,
            landed_30d: landed_all,
            changes_30d: changes_all,
        }))
    }

    pub(super) async fn get_spend_impl(
        &self,
        request: Request<wire::GetAgentSpendRequest>,
    ) -> Result<Response<wire::GetAgentSpendResponse>, Status> {
        use sqlx::Row;
        let (meta, ext, msg) = request.into_parts();
        // Money: a writer sees it, a reader does not.
        let ctx = self.env.context(&meta, &ext, msg.repo.as_ref(), Access::Write, false).await?;
        let month = if msg.month.is_empty() { control::this_month() } else { msg.month.clone() };
        if !control::valid_month(&month) {
            return Err(Status::invalid_argument("a month is YYYY-MM"));
        }
        let repo = ctx.repo.as_str().to_ascii_lowercase();
        let like = format!("{month}-%");
        let u = |v: i64| u64::try_from(v).unwrap_or(0);
        let days = sqlx::query(
            "SELECT day, CAST(SUM(cost) AS BIGINT) AS c, CAST(SUM(tokens) AS BIGINT) AS t FROM agent_spend \
             WHERE repo = $1 AND day LIKE $2 GROUP BY day ORDER BY day",
        )
        .bind(repo.clone())
        .bind(like.clone())
        .fetch_all(&self.hub.core.pool)
        .await
        .map_err(|e| db(&e))?;
        let mut out_days = Vec::new();
        for r in &days {
            out_days.push(wire::SpendDay {
                day: r.try_get("day").map_err(|e| db(&e))?,
                cost_micro_usd: u(r.try_get("c").map_err(|e| db(&e))?),
                tokens: u(r.try_get("t").map_err(|e| db(&e))?),
            });
        }
        let agents = sqlx::query(
            "SELECT agent, CAST(SUM(cost) AS BIGINT) AS c, CAST(SUM(tokens) AS BIGINT) AS t, \
             CAST(SUM(requests) AS BIGINT) AS r FROM agent_spend WHERE repo = $1 AND day LIKE $2 GROUP BY agent ORDER BY agent",
        )
        .bind(repo.clone())
        .bind(like)
        .fetch_all(&self.hub.core.pool)
        .await
        .map_err(|e| db(&e))?;
        let mut out_agents = Vec::new();
        for r in &agents {
            let agent: String = r.try_get("agent").map_err(|e| db(&e))?;
            out_agents.push(wire::AgentSpend {
                monthly_cap_micro_usd: self.hub.monthly_cap(&repo, &agent).await?,
                agent,
                cost_micro_usd: u(r.try_get("c").map_err(|e| db(&e))?),
                tokens: u(r.try_get("t").map_err(|e| db(&e))?),
                requests: u(r.try_get("r").map_err(|e| db(&e))?),
            });
        }
        let (cost, toks, _) = self.hub.month_spend(&repo, &month, "").await?;
        Ok(Response::new(wire::GetAgentSpendResponse {
            month,
            cost_micro_usd: cost,
            tokens: toks,
            monthly_cap_micro_usd: self.hub.monthly_cap(&repo, "").await?,
            days: out_days,
            agents: out_agents,
        }))
    }

    pub(super) async fn set_monthly_budget_impl(
        &self,
        request: Request<wire::SetMonthlyBudgetRequest>,
    ) -> Result<Response<wire::SetMonthlyBudgetResponse>, Status> {
        let (meta, ext, msg) = request.into_parts();
        let ctx = self.env.context(&meta, &ext, msg.repo.as_ref(), Access::Admin, true).await?;
        if !msg.agent.is_empty() && !tokens::valid_agent_name(&msg.agent) {
            return Err(Status::invalid_argument("an agent name is 1 to 40 characters of a-z, 0-9 and -"));
        }
        let by = handle_of(&ctx.principal.id).unwrap_or_else(|| ctx.principal.id.clone());
        self.hub
            .set_monthly_cap(ctx.repo.as_str(), &msg.agent, msg.cap_micro_usd, &by)
            .await?;
        self.hub
            .audit(
                "agent.budget.monthly_set",
                &ctx.principal.id,
                relay(&ctx),
                ctx.repo.as_str(),
                &msg.agent,
                &[("agent", msg.agent.clone()), ("cap_micro_usd", msg.cap_micro_usd.to_string())],
            )
            .await;
        Ok(Response::new(wire::SetMonthlyBudgetResponse {
            agent: msg.agent,
            cap_micro_usd: msg.cap_micro_usd,
        }))
    }
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}
