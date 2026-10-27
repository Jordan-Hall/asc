//! The `Agents` and `AgentTokens` tonic services.
//!
//! Authorisation, call by call (every decision is made by the server's [`Authorizer`] before a
//! handler runs; a handler that cannot decide refuses):
//!
//! | call | needs |
//! |---|---|
//! | `Agents.CreateRun` | write on the repository, as a person (an agent cannot start runs) |
//! | `ListRuns`, `GetRun`, `WatchRun` | read |
//! | `CancelRun` | the person who started the run, a maintainer of the policy, or repository admin |
//! | `SendInput` | the person who started the run |
//! | `AgentTokens.CreateToken`, `ListTokens`, `RevokeToken`, `SetBudget` | repository admin |
//! | `GetToken` | repository admin, or the token itself |
//! | `ReportUsage` | the agent token itself |
//!
//! A token secret is in exactly one response, `CreateToken`'s, and that response is not stored for
//! a retry of the same operation id.

use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use asc_core::policy::{Kind, PathGlob, Who};
use asc_rpc::auth::{Access, AuthInterceptor, Authorizer};
use asc_rpc::server::{CallCtx, MAX_MESSAGE, ServerInfo, wire};
use asc_rpc::services::agent_tokens_server::{AgentTokens, AgentTokensServer};
use asc_rpc::services::agents_server::{Agents, AgentsServer};
use futures::Stream;
use prost::Message as _;
use tonic::service::interceptor::InterceptedService;
use tonic::{Request, Response, Status};

use super::errors;
use super::repo::PolicyState;
use super::runs::{RunRow, RunState};
use super::runtime::{Requester, RunRequest, example_of};
use super::tokens::{self, Budget, NewToken, Refusal, Spent, TokenRow};
use super::{AgentsHub, event_to_wire, run_to_wire};
use crate::ai::clock;
use crate::ai::ops::OpState;
use crate::review::{Env, handle_of, require_handle};

/// Runs in one `ListRuns` page when the request names no limit, and at most.
const DEFAULT_RUNS: usize = 30;
const MAX_RUNS_PAGE: usize = 100;
/// Tokens in one `ListTokens` page when the request names no limit, and at most.
const DEFAULT_TOKENS: usize = 50;
const MAX_TOKENS_PAGE: usize = 200;
/// Events sent in one batch of a watch.
const WATCH_BATCH: usize = 200;
/// Longest a watch stream lives; the client resumes with `after_seq`.
const WATCH_LIFETIME: Duration = Duration::from_secs(10 * 60);
/// Open watch streams a server holds at once.
const MAX_WATCHERS: usize = 256;

fn db(e: &sqlx::Error) -> Status {
    tracing::error!(class = %crate::ai::store::class(e), "agent services database error");
    Status::internal("internal error")
}

fn id_of(n: u64) -> Result<u64, Status> {
    if n == 0 || i64::try_from(n).is_err() {
        Err(Status::invalid_argument("invalid id"))
    } else {
        Ok(n)
    }
}

/// The policy `Who` of a person.
fn person(principal: &str) -> Option<Who> {
    handle_of(principal).map(|h| Who::new(Kind::User, h))
}

type WatchStream =
    Pin<Box<dyn Stream<Item = Result<wire::AgentRunEvent, Status>> + Send + 'static>>;

/// The `asc.v1.Agents` and `asc.v1.AgentTokens` services.
#[derive(Clone)]
pub struct AgentService {
    hub: Arc<AgentsHub>,
    env: Env,
    watchers: Arc<tokio::sync::Semaphore>,
}

impl AgentService {
    pub fn new(hub: Arc<AgentsHub>, authorizer: Arc<dyn Authorizer>, info: ServerInfo) -> Self {
        Self {
            hub,
            env: Env { authorizer, info },
            watchers: Arc::new(tokio::sync::Semaphore::new(MAX_WATCHERS)),
        }
    }

    pub fn into_service(self) -> InterceptedService<AgentsServer<Self>, AuthInterceptor> {
        let server = AgentsServer::new(self)
            .max_decoding_message_size(MAX_MESSAGE)
            .max_encoding_message_size(MAX_MESSAGE)
            .accept_compressed(tonic::codec::CompressionEncoding::Zstd);
        InterceptedService::new(server, AuthInterceptor)
    }

    /// The token service over the same hub.
    pub fn tokens(&self) -> TokenService {
        TokenService {
            hub: self.hub.clone(),
            env: self.env.clone(),
        }
    }

    async fn run_in(&self, ctx: &CallCtx, id: u64) -> Result<RunRow, Status> {
        let id = id_of(id)?;
        let run = self.hub.core.runs.get(id).await.map_err(|e| db(&e))?;
        // A run of another repository is "not found", never "forbidden".
        run.filter(|r| r.repo.eq_ignore_ascii_case(ctx.repo.as_str()))
            .ok_or_else(|| Status::not_found("no such run"))
    }

    async fn wire_run(&self, r: &RunRow) -> wire::AgentRun {
        let token = if r.token_id.is_empty() {
            None
        } else {
            self.hub.core.tokens.get(&r.token_id).await.ok().flatten()
        };
        run_to_wire(r, token.as_ref(), clock::now())
    }

    /// Whether `ctx`'s caller started the run.
    fn started(ctx: &CallCtx, r: &RunRow) -> bool {
        r.created_by == ctx.principal.id
            || handle_of(&ctx.principal.id)
                .is_some_and(|h| !r.starter.is_empty() && h.eq_ignore_ascii_case(&r.starter))
    }
}

#[tonic::async_trait]
impl Agents for AgentService {
    type WatchRunStream = WatchStream;

    async fn create_run(
        &self,
        request: Request<wire::CreateAgentRunRequest>,
    ) -> Result<Response<wire::CreateAgentRunResponse>, Status> {
        let (meta, ext, msg) = request.into_parts();
        let ctx = self
            .env
            .context(&meta, &ext, msg.repo.as_ref(), Access::Write, true)
            .await?;
        // Only a person starts a run: an agent token cannot start agents of its own.
        let handle = require_handle(&ctx)?;
        let op = ctx
            .op_id
            .ok_or_else(|| Status::invalid_argument("mutating calls need x-asc-op-id"))?;
        let who = Requester {
            principal: ctx.principal.id.clone(),
            handle: Some(handle),
        };
        let req = RunRequest {
            repo: ctx.repo.as_str().to_owned(),
            agent: msg.agent,
            title: msg.title,
            task: msg.task,
            target: msg.target,
            base: msg.base,
            paths: msg.paths,
            ai_key_id: msg.ai_key_id,
            model: msg.model,
            limits: Budget::from_wire(msg.limits.as_ref()),
            workflow_run_id: 0,
            workflow_step: String::new(),
        };
        let hub = self.hub.clone();
        let ops = hub.core.ops.clone();
        let this = self.clone();
        crate::ai::caller::once(
            &ops,
            op,
            &ctx.principal.id,
            "Agents/CreateRun",
            true,
            || async move {
                let row = hub.create_run(&who, req).await?;
                Ok(wire::CreateAgentRunResponse {
                    run: Some(this.wire_run(&row).await),
                })
            },
        )
        .await
        .map(Response::new)
    }

    async fn list_runs(
        &self,
        request: Request<wire::ListAgentRunsRequest>,
    ) -> Result<Response<wire::ListAgentRunsResponse>, Status> {
        let (meta, ext, msg) = request.into_parts();
        let ctx = self
            .env
            .context(&meta, &ext, msg.repo.as_ref(), Access::Read, false)
            .await?;
        let limit = match msg.limit {
            0 => DEFAULT_RUNS,
            n => usize::try_from(n)
                .unwrap_or(MAX_RUNS_PAGE)
                .min(MAX_RUNS_PAGE),
        };
        let mut states = Vec::new();
        for s in &msg.states {
            let st = wire::AgentRunState::try_from(*s)
                .map_err(|_| Status::invalid_argument("unknown state"))?;
            if let Some(s) = RunState::from_wire(st) {
                states.push(s);
            }
        }
        if !msg.agent.is_empty() && !tokens::valid_agent_name(&msg.agent) {
            return Err(Status::invalid_argument(
                "an agent name is 1 to 40 characters of a-z, 0-9 and -",
            ));
        }
        let before = if msg.page_token.is_empty() {
            None
        } else {
            Some(
                msg.page_token
                    .parse::<u64>()
                    .ok()
                    .filter(|_| {
                        msg.page_token.len() <= 19
                            && msg.page_token.bytes().all(|b| b.is_ascii_digit())
                    })
                    .ok_or_else(|| Status::invalid_argument("malformed page token"))?,
            )
        };
        let mut rows = self
            .hub
            .core
            .runs
            .list(
                &ctx.repo.as_str().to_ascii_lowercase(),
                &states,
                &msg.agent,
                &msg.created_by,
                before,
                limit,
            )
            .await
            .map_err(|e| db(&e))?;
        let next = if rows.len() > limit {
            rows.truncate(limit);
            rows.last().map(|r| r.id.to_string()).unwrap_or_default()
        } else {
            String::new()
        };
        let mut runs = Vec::with_capacity(rows.len());
        for r in &rows {
            runs.push(self.wire_run(r).await);
        }
        Ok(Response::new(wire::ListAgentRunsResponse {
            runs,
            next_page_token: next,
        }))
    }

    async fn get_run(
        &self,
        request: Request<wire::GetAgentRunRequest>,
    ) -> Result<Response<wire::GetAgentRunResponse>, Status> {
        let (meta, ext, msg) = request.into_parts();
        let ctx = self
            .env
            .context(&meta, &ext, msg.repo.as_ref(), Access::Read, false)
            .await?;
        let run = self.run_in(&ctx, msg.id).await?;
        Ok(Response::new(wire::GetAgentRunResponse {
            run: Some(self.wire_run(&run).await),
        }))
    }

    async fn cancel_run(
        &self,
        request: Request<wire::CancelAgentRunRequest>,
    ) -> Result<Response<wire::CancelAgentRunResponse>, Status> {
        let (meta, ext, msg) = request.into_parts();
        let ctx = self
            .env
            .context(&meta, &ext, msg.repo.as_ref(), Access::Write, false)
            .await?;
        if msg.reason.len() > 256 {
            return Err(Status::invalid_argument("a reason is at most 256 bytes"));
        }
        let run = self.run_in(&ctx, msg.id).await?;
        let mut allowed = Self::started(&ctx, &run);
        if !allowed {
            allowed = self.env.has(&ext, &ctx, Access::Admin).await;
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
        if !allowed {
            return Err(Status::permission_denied(
                "only the person who started the run, a maintainer or an administrator can cancel it",
            ));
        }
        let row = self
            .hub
            .cancel_run(run.id, &msg.reason, &ctx.principal.id)
            .await?;
        Ok(Response::new(wire::CancelAgentRunResponse {
            run: Some(self.wire_run(&row).await),
        }))
    }

    async fn send_input(
        &self,
        request: Request<wire::SendAgentInputRequest>,
    ) -> Result<Response<wire::SendAgentInputResponse>, Status> {
        let (meta, ext, msg) = request.into_parts();
        let ctx = self
            .env
            .context(&meta, &ext, msg.repo.as_ref(), Access::Write, true)
            .await?;
        let run = self.run_in(&ctx, msg.id).await?;
        if !Self::started(&ctx, &run) {
            return Err(Status::permission_denied(
                "only the person who started the run can send it a message",
            ));
        }
        let op = ctx
            .op_id
            .ok_or_else(|| Status::invalid_argument("mutating calls need x-asc-op-id"))?;
        let hub = self.hub.clone();
        let ops = hub.core.ops.clone();
        let this = self.clone();
        let text = msg.text;
        crate::ai::caller::once(
            &ops,
            op,
            &ctx.principal.id,
            "Agents/SendInput",
            true,
            || async move {
                let row = hub.send_input(run.id, &text).await?;
                Ok(wire::SendAgentInputResponse {
                    run: Some(this.wire_run(&row).await),
                })
            },
        )
        .await
        .map(Response::new)
    }

    async fn watch_run(
        &self,
        request: Request<wire::WatchAgentRunRequest>,
    ) -> Result<Response<Self::WatchRunStream>, Status> {
        let (meta, ext, msg) = request.into_parts();
        let ctx = self
            .env
            .context(&meta, &ext, msg.repo.as_ref(), Access::Read, false)
            .await?;
        let run = self.run_in(&ctx, msg.id).await?;
        let permit = self
            .watchers
            .clone()
            .try_acquire_owned()
            .map_err(|_| Status::resource_exhausted("too many open watches; retry shortly"))?;
        let hub = self.hub.clone();
        let started = std::time::Instant::now();
        let state = (
            hub,
            run.id,
            msg.after_seq,
            std::collections::VecDeque::<wire::AgentRunEvent>::new(),
            false,
            permit,
        );
        let stream = futures::stream::unfold(
            state,
            move |(hub, id, mut after, mut buf, mut done, permit)| async move {
                loop {
                    if let Some(ev) = buf.pop_front() {
                        after = ev.seq;
                        return Some((Ok(ev), (hub, id, after, buf, done, permit)));
                    }
                    if done || started.elapsed() > WATCH_LIFETIME {
                        return None;
                    }
                    match hub.core.runs.events_after(id, after, WATCH_BATCH).await {
                        Ok(rows) if !rows.is_empty() => {
                            buf.extend(rows.iter().map(event_to_wire));
                            continue;
                        }
                        Ok(_) => {}
                        Err(e) => return Some((Err(db(&e)), (hub, id, after, buf, true, permit))),
                    }
                    // No new events: the stream ends when the run is over and everything was sent.
                    match hub.core.runs.get(id).await {
                        Ok(Some(r)) if !r.state.is_active() => {
                            // One more read, in case the closing events were written after the last one.
                            match hub.core.runs.events_after(id, after, WATCH_BATCH).await {
                                Ok(rows) if !rows.is_empty() => {
                                    buf.extend(rows.iter().map(event_to_wire));
                                    done = true;
                                    continue;
                                }
                                _ => return None,
                            }
                        }
                        Ok(Some(_)) => {}
                        _ => return None,
                    }
                    tokio::time::sleep(Duration::from_millis(250)).await;
                }
            },
        );
        Ok(Response::new(Box::pin(stream)))
    }
}

// ------------------------------------------------------------------------------ tokens

/// The `asc.v1.AgentTokens` service.
#[derive(Clone)]
pub struct TokenService {
    hub: Arc<AgentsHub>,
    env: Env,
}

impl TokenService {
    pub fn into_service(self) -> InterceptedService<AgentTokensServer<Self>, AuthInterceptor> {
        let server = AgentTokensServer::new(self)
            .max_decoding_message_size(MAX_MESSAGE)
            .max_encoding_message_size(MAX_MESSAGE)
            .accept_compressed(tonic::codec::CompressionEncoding::Zstd);
        InterceptedService::new(server, AuthInterceptor)
    }

    async fn token_in(&self, ctx: &CallCtx, id: &str) -> Result<TokenRow, Status> {
        let row = self.hub.core.tokens.get(id).await.map_err(|e| db(&e))?;
        row.filter(|t| t.repo.eq_ignore_ascii_case(ctx.repo.as_str()))
            .ok_or_else(|| Status::not_found("no such token"))
    }

    /// The limits a token or run gets: the request, the policy and the server defaults, the
    /// strictest of the three in each dimension.
    pub async fn effective_limits(
        &self,
        entry: &crate::registry::RepoEntry,
        asked: Budget,
    ) -> Result<(Budget, Option<Arc<asc_core::policy::Policy>>), Status> {
        let policy = match &*self.hub.core.view.policy(entry).await? {
            PolicyState::Absent => None,
            PolicyState::Valid(p) => Some(p.clone()),
            PolicyState::Invalid(why) => {
                return Err(errors::policy_denied(
                    format!(
                        "the repository policy cannot be used: {why}; fix .asc/policy.toml on trunk"
                    ),
                    vec![],
                ));
            }
        };
        let d = self.hub.core.cfg.default_limits;
        let mut from_policy = Budget::default();
        if let Some(p) = &policy {
            let b = &p.agents.budgets;
            from_policy = Budget {
                changes: b.max_changes_per_run.map_or(0, u64::from),
                tokens: b.max_tokens.unwrap_or(0),
                cost: b.max_cost_micro_usd.unwrap_or(0),
                paths: b.max_paths_per_run.map_or(0, u64::from),
                runtime: p.agents.max_runtime_seconds.map_or(0, u64::from),
            };
        }
        let defaults = Budget {
            changes: u64::from(d.max_changes),
            tokens: d.max_tokens,
            cost: d.max_cost_micro_usd,
            paths: u64::from(d.max_paths),
            runtime: u64::from(d.max_runtime_seconds),
        };
        Ok((asked.merge(from_policy).merge(defaults), policy))
    }
}

#[tonic::async_trait]
impl AgentTokens for TokenService {
    async fn create_token(
        &self,
        request: Request<wire::CreateAgentTokenRequest>,
    ) -> Result<Response<wire::CreateAgentTokenResponse>, Status> {
        let (meta, ext, msg) = request.into_parts();
        let ctx = self
            .env
            .context(&meta, &ext, msg.repo.as_ref(), Access::Admin, true)
            .await?;
        let cfg = &self.hub.core.cfg;
        if !cfg.enabled {
            return Err(errors::disabled("instance"));
        }
        let bad = |m: &str| Err(Status::invalid_argument(m.to_owned()));
        if msg.name.is_empty()
            || msg.name.len() > tokens::MAX_NAME
            || msg.name.chars().any(char::is_control)
        {
            return bad("a name is 1 to 64 bytes of text");
        }
        if !tokens::valid_agent_name(&msg.agent) {
            return bad("an agent name is 1 to 40 characters of a-z, 0-9 and -");
        }
        if msg.paths.len() > tokens::MAX_PATHS {
            return bad("a token has at most 32 paths");
        }
        let ttl = match u64::from(msg.ttl_seconds) {
            0 => tokens::DEFAULT_TTL_SECS,
            n => n,
        };
        if ttl > cfg.max_token_ttl_secs {
            return bad(&format!(
                "a token lives at most {} seconds",
                cfg.max_token_ttl_secs
            ));
        }
        let entry = self
            .hub
            .registry
            .open(&ctx.repo)
            .await
            .map_err(|e| crate::update::internal("registry", &e))?;
        let (limits, policy) = self
            .effective_limits(&entry, Budget::from_wire(msg.limits.as_ref()))
            .await?;
        if policy.as_ref().is_some_and(|p| !p.agents.enabled) {
            return Err(errors::disabled("repository policy"));
        }
        // Paths: each glob must be one the policy admits an agent of this name to write; none
        // given means the policy's own default scope.
        let who = Who::new(Kind::Agent, msg.agent.clone());
        let mut paths = Vec::new();
        for p in &msg.paths {
            PathGlob::parse(p)
                .map_err(|e| Status::invalid_argument(format!("path {p:?}: {}", e.0)))?;
            if let Some(pol) = &policy
                && pol.check_write(&who, example_of(p).as_bytes()).is_err()
            {
                return Err(errors::scope_denied(vec![p.clone()]));
            }
            paths.push(p.clone());
        }
        let op = ctx
            .op_id
            .ok_or_else(|| Status::invalid_argument("mutating calls need x-asc-op-id"))?;
        let creator = ctx.principal.id.clone();
        let hub = self.hub.clone();
        let repo = ctx.repo.as_str().to_ascii_lowercase();
        let new = NewToken {
            repo: repo.clone(),
            name: msg.name,
            agent: msg.agent,
            paths,
            max_changes_per_run: msg.max_changes_per_run,
            created_by: creator.clone(),
            owner_id: tokens::owner_id_for(&creator),
            expires_at: clock::now() + i64::try_from(ttl).unwrap_or(i64::MAX),
            limits,
            run_id: 0,
        };
        // The answer holds the secret, so it is not kept for a retry of the same operation.
        crate::ai::caller::once(
            &hub.core.ops,
            op,
            &creator,
            "AgentTokens/CreateToken",
            false,
            || async {
                let secret = tokens::new_secret();
                let row = hub
                    .core
                    .tokens
                    .create(&new, &secret)
                    .await
                    .map_err(|e| db(&e))?
                    .ok_or_else(|| {
                        Status::resource_exhausted(
                            "this repository has too many live agent tokens; revoke some",
                        )
                    })?;
                hub.audit(
                    "agent.token.created",
                    &creator,
                    None,
                    &repo,
                    &row.id,
                    &[
                        ("token", row.id.clone()),
                        ("agent", row.agent.clone()),
                        ("expires_at", row.expires_at.to_string()),
                        ("max_changes_per_run", row.max_changes_per_run.to_string()),
                    ],
                )
                .await;
                Ok(wire::CreateAgentTokenResponse {
                    token: Some(row.to_wire(clock::now())),
                    secret: secret.to_string(),
                })
            },
        )
        .await
        .map(Response::new)
    }

    async fn list_tokens(
        &self,
        request: Request<wire::ListAgentTokensRequest>,
    ) -> Result<Response<wire::ListAgentTokensResponse>, Status> {
        let (meta, ext, msg) = request.into_parts();
        let ctx = self
            .env
            .context(&meta, &ext, msg.repo.as_ref(), Access::Admin, false)
            .await?;
        let limit = match msg.limit {
            0 => DEFAULT_TOKENS,
            n => usize::try_from(n)
                .unwrap_or(MAX_TOKENS_PAGE)
                .min(MAX_TOKENS_PAGE),
        };
        let mut states = Vec::new();
        for s in &msg.states {
            states.push(
                wire::AgentTokenState::try_from(*s)
                    .map_err(|_| Status::invalid_argument("unknown state"))?,
            );
        }
        let after = if msg.page_token.is_empty() {
            None
        } else {
            let (c, id) = msg
                .page_token
                .split_once(':')
                .and_then(|(c, id)| Some((c.parse::<i64>().ok()?, id.to_owned())))
                .filter(|(_, id)| id.len() <= 32 && id.starts_with("at_"))
                .ok_or_else(|| Status::invalid_argument("malformed page token"))?;
            Some((c, id))
        };
        let now = clock::now();
        let mut rows = self
            .hub
            .core
            .tokens
            .list(
                &ctx.repo.as_str().to_ascii_lowercase(),
                &states,
                after,
                limit,
                now,
            )
            .await
            .map_err(|e| db(&e))?;
        let next = if rows.len() > limit {
            rows.truncate(limit);
            rows.last()
                .map(|t| format!("{}:{}", t.created, t.id))
                .unwrap_or_default()
        } else {
            String::new()
        };
        Ok(Response::new(wire::ListAgentTokensResponse {
            tokens: rows.iter().map(|t| t.to_wire(now)).collect(),
            next_page_token: next,
        }))
    }

    async fn get_token(
        &self,
        request: Request<wire::GetAgentTokenRequest>,
    ) -> Result<Response<wire::GetAgentTokenResponse>, Status> {
        let (meta, ext, msg) = request.into_parts();
        // The token itself reads its own row; anyone else needs admin.
        let ctx = self
            .env
            .context(&meta, &ext, msg.repo.as_ref(), Access::Read, false)
            .await?;
        let itself =
            tokens::parse_principal(&ctx.principal.id).is_some_and(|p| p.token_id == msg.id);
        if !itself && !self.env.has(&ext, &ctx, Access::Admin).await {
            // The same answer as for a token that does not exist.
            return Err(Status::not_found("no such token"));
        }
        let row = self.token_in(&ctx, &msg.id).await?;
        let now = clock::now();
        let remaining = Spent::remaining(row.limits, row.spent, 0);
        Ok(Response::new(wire::GetAgentTokenResponse {
            token: Some(row.to_wire(now)),
            remaining: Some(remaining),
        }))
    }

    async fn revoke_token(
        &self,
        request: Request<wire::RevokeAgentTokenRequest>,
    ) -> Result<Response<wire::RevokeAgentTokenResponse>, Status> {
        let (meta, ext, msg) = request.into_parts();
        let ctx = self
            .env
            .context(&meta, &ext, msg.repo.as_ref(), Access::Admin, false)
            .await?;
        if msg.reason.len() > 256 {
            return Err(Status::invalid_argument("a reason is at most 256 bytes"));
        }
        let row = self.token_in(&ctx, &msg.id).await?;
        let reason: String = msg.reason.chars().filter(|c| !c.is_control()).collect();
        if self
            .hub
            .core
            .tokens
            .revoke(&row.id, &reason)
            .await
            .map_err(|e| db(&e))?
        {
            self.hub
                .audit(
                    "agent.token.revoked",
                    &ctx.principal.id,
                    None,
                    &row.repo,
                    &row.id,
                    &[("token", row.id.clone()), ("agent", row.agent.clone())],
                )
                .await;
        }
        let row = self.token_in(&ctx, &msg.id).await?;
        Ok(Response::new(wire::RevokeAgentTokenResponse {
            token: Some(row.to_wire(clock::now())),
        }))
    }

    async fn set_budget(
        &self,
        request: Request<wire::SetAgentTokenBudgetRequest>,
    ) -> Result<Response<wire::SetAgentTokenBudgetResponse>, Status> {
        let (meta, ext, msg) = request.into_parts();
        let ctx = self
            .env
            .context(&meta, &ext, msg.repo.as_ref(), Access::Admin, false)
            .await?;
        let row = self.token_in(&ctx, &msg.id).await?;
        if row.revoked_at > 0 {
            return Err(errors::token_revoked());
        }
        let entry = self
            .hub
            .registry
            .open(&ctx.repo)
            .await
            .map_err(|e| crate::update::internal("registry", &e))?;
        let (limits, _) = self
            .effective_limits(&entry, Budget::from_wire(msg.limits.as_ref()))
            .await?;
        self.hub
            .core
            .tokens
            .set_limits(&row.id, limits)
            .await
            .map_err(|e| db(&e))?;
        let row = self.token_in(&ctx, &msg.id).await?;
        Ok(Response::new(wire::SetAgentTokenBudgetResponse {
            token: Some(row.to_wire(clock::now())),
        }))
    }

    async fn report_usage(
        &self,
        request: Request<wire::ReportAgentUsageRequest>,
    ) -> Result<Response<wire::ReportAgentUsageResponse>, Status> {
        let (meta, ext, msg) = request.into_parts();
        // Reading is enough to authenticate: an exhausted token must still be able to report.
        let ctx = self
            .env
            .context(&meta, &ext, msg.repo.as_ref(), Access::Read, true)
            .await?;
        let Some(me) = tokens::parse_principal(&ctx.principal.id) else {
            return Err(Status::permission_denied(
                "only an agent token reports usage",
            ));
        };
        let op = ctx
            .op_id
            .ok_or_else(|| Status::invalid_argument("mutating calls need x-asc-op-id"))?;
        let row = self.token_in(&ctx, &me.token_id).await?;
        let core = &self.hub.core;
        let answer = |t: &TokenRow| wire::ReportAgentUsageResponse {
            usage: Some(t.spent.to_wire(0)),
            remaining: Some(Spent::remaining(t.limits, t.spent, 0)),
        };
        match core
            .ops
            .begin(op, &ctx.principal.id, "AgentTokens/ReportUsage")
            .await?
        {
            OpState::Done(bytes) => {
                let r = wire::ReportAgentUsageResponse::decode(bytes.as_slice())
                    .map_err(|_| Status::internal("internal error"))?;
                let t = self.token_in(&ctx, &row.id).await?;
                return match over(&t) {
                    Some(dim) => Err(errors::budget_exceeded(dim)),
                    None => Ok(Response::new(r)),
                };
            }
            OpState::Consumed => {
                return Err(Status::already_exists("this operation already happened"));
            }
            OpState::New => {}
        }
        let total = msg.input_tokens.saturating_add(msg.output_tokens);
        let charged = core
            .tokens
            .charge_usage(&row.id, total, msg.cost_micro_usd, msg.requests.max(1))
            .await;
        match charged {
            Ok(Ok(t)) => {
                let r = answer(&t);
                core.ops.finish(op, Some(r.encode_to_vec())).await;
                Ok(Response::new(r))
            }
            Ok(Err((t, dim))) => {
                // Recorded in full; the token is exhausted from now on.
                core.ops.finish(op, Some(answer(&t).encode_to_vec())).await;
                self.hub
                    .audit(
                        "agent.budget.exceeded",
                        &ctx.principal.id,
                        None,
                        &t.repo,
                        &t.id,
                        &[
                            ("token", t.id.clone()),
                            ("budget", dim.to_owned()),
                            (
                                "limit",
                                match dim {
                                    "cost" => t.limits.cost,
                                    _ => t.limits.tokens,
                                }
                                .to_string(),
                            ),
                        ],
                    )
                    .await;
                Err(errors::budget_exceeded(dim))
            }
            Err(Refusal::Gone) => {
                core.ops.abandon(op).await;
                Err(errors::token_revoked())
            }
            Err(Refusal::Budget(d)) => Err(errors::budget_exceeded(d)),
            Err(Refusal::Sql(c)) => {
                core.ops.abandon(op).await;
                Err(crate::update::internal("report usage", &c))
            }
        }
    }
}

/// The dimension a token has passed its limit in, if it has.
fn over(t: &TokenRow) -> Option<&'static str> {
    if t.limits.tokens > 0 && t.spent.tokens > t.limits.tokens {
        Some("tokens")
    } else if t.limits.cost > 0 && t.spent.cost > t.limits.cost {
        Some("cost")
    } else {
        None
    }
}
