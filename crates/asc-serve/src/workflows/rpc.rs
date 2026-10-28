//! The `Workflows` tonic service.
//!
//! Authorisation, call by call (the server's [`Authorizer`] decides before a handler runs; a
//! handler that cannot decide refuses):
//!
//! | call | needs |
//! |---|---|
//! | `ListDefinitions`, `GetDefinition`, `Validate`, `ListRuns`, `GetRun`, `WatchRun` | read |
//! | `SetEnabled` | a maintainer of the policy, or repository admin |
//! | `Trigger` | write, as a person (an agent cannot start workflows) |
//! | `CancelRun` | the person who started the run, a maintainer, or repository admin |
//! | `DecideApproval` | a person admitted by the step's `approvers` |
//! | `ListSecrets`, `SetSecret`, `DeleteSecret` | repository admin |
//!
//! A secret value is in exactly one request (`SetSecret`) and in no response. Nothing here puts
//! a secret, a key or a token in an error or a log line.

use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use asc_core::policy::{Kind, Who};
use asc_core::workflow::{TriggerKind, valid_name};
use asc_rpc::auth::{Access, AuthInterceptor, Authorizer};
use asc_rpc::errors::{ErrorInfo, RpcError};
use asc_rpc::server::{CallCtx, MAX_MESSAGE, ServerInfo, rpc_status, wire};
use asc_rpc::services::workflows_server::{Workflows, WorkflowsServer};
use futures::Stream;
use prost::Message as _;
use tonic::service::interceptor::InterceptedService;
use tonic::{Request, Response, Status};
use zeroize::Zeroizing;

use super::defs::{self, Def};
use super::engine::{Start, StartError};
use super::secrets::SecretError;
use super::store::{RunRow, RunState};
use super::{WorkflowsHub, clean, def_to_wire, run_to_wire};
use crate::agents::errors;
use crate::agents::repo::PolicyState;
use crate::ai::audit::Entry;
use crate::ai::caller::once;
use crate::review::{Env, handle_of, require_handle};

const DEFAULT_RUNS: usize = 30;
const MAX_RUNS_PAGE: usize = 100;
const WATCH_BATCH: usize = 100;
const WATCH_LIFETIME: Duration = Duration::from_secs(10 * 60);
const MAX_WATCHERS: usize = 256;
/// Largest definition text `Validate` takes.
const MAX_VALIDATE: usize = asc_core::workflow::MAX_WORKFLOW_BYTES;

fn sql(e: &sqlx::Error) -> Status {
    crate::update::internal("workflows", &crate::ai::store::class(e))
}

fn id_of(n: u64) -> Result<u64, Status> {
    if n == 0 || i64::try_from(n).is_err() {
        Err(Status::invalid_argument("invalid id"))
    } else {
        Ok(n)
    }
}

fn disabled(why: &str) -> Status {
    rpc_status(&RpcError::WorkflowDisabled(ErrorInfo {
        paths: vec![why.to_owned()],
        ..ErrorInfo::default()
    }))
}

fn invalid(why: &str) -> Status {
    rpc_status(&RpcError::WorkflowInvalid(ErrorInfo {
        paths: vec![why.to_owned()],
        ..ErrorInfo::default()
    }))
}

type WatchStream =
    Pin<Box<dyn Stream<Item = Result<wire::WorkflowRunEvent, Status>> + Send + 'static>>;

/// The `asc.v1.Workflows` service.
#[derive(Clone)]
pub struct WorkflowService {
    hub: Arc<WorkflowsHub>,
    env: Env,
    watchers: Arc<tokio::sync::Semaphore>,
}

impl WorkflowService {
    pub fn new(hub: Arc<WorkflowsHub>, authorizer: Arc<dyn Authorizer>, info: ServerInfo) -> Self {
        Self {
            hub,
            env: Env { authorizer, info },
            watchers: Arc::new(tokio::sync::Semaphore::new(MAX_WATCHERS)),
        }
    }

    pub fn into_service(self) -> InterceptedService<WorkflowsServer<Self>, AuthInterceptor> {
        let server = WorkflowsServer::new(self)
            .max_decoding_message_size(MAX_MESSAGE)
            .max_encoding_message_size(MAX_MESSAGE)
            .accept_compressed(tonic::codec::CompressionEncoding::Zstd);
        InterceptedService::new(server, AuthInterceptor)
    }

    async fn entry(&self, ctx: &CallCtx) -> Result<Arc<crate::registry::RepoEntry>, Status> {
        self.hub
            .registry
            .open(&ctx.repo)
            .await
            .map_err(|e| match e {
                crate::registry::RegistryError::NotFound(_) => {
                    Status::not_found("no such repository")
                }
                e => crate::update::internal("registry", &e),
            })
    }

    async fn run_in(&self, ctx: &CallCtx, id: u64) -> Result<RunRow, Status> {
        let id = id_of(id)?;
        let run = self.hub.store.get_run(id).await.map_err(|e| sql(&e))?;
        // A run of another repository is "not found", never "forbidden".
        run.filter(|r| r.repo.eq_ignore_ascii_case(ctx.repo.as_str()))
            .ok_or_else(|| Status::not_found("no such run"))
    }

    async fn wire_run(&self, run: &RunRow, output: bool) -> Result<wire::WorkflowRun, Status> {
        let steps = self.hub.store.steps(run.id).await.map_err(|e| sql(&e))?;
        Ok(run_to_wire(run, &steps, output))
    }

    /// Whether the caller maintains the repository: a maintainer of the policy, or an administrator.
    async fn maintains(
        &self,
        ext: &tonic::Extensions,
        ctx: &CallCtx,
        entry: &crate::registry::RepoEntry,
    ) -> Result<bool, Status> {
        if self.env.has(ext, ctx, Access::Admin).await {
            return Ok(true);
        }
        let Some(h) = handle_of(&ctx.principal.id) else {
            return Ok(false);
        };
        Ok(match &*self.hub.agents.core.view.policy(entry).await? {
            PolicyState::Valid(p) => p.is_maintainer(&Who::new(Kind::User, h)),
            _ => false,
        })
    }

    fn started(ctx: &CallCtx, run: &RunRow) -> bool {
        let handle = handle_of(&ctx.principal.id);
        run.requester == ctx.principal.id
            || handle.is_some_and(|h| {
                !run.started_by.is_empty() && h.eq_ignore_ascii_case(&run.started_by)
            })
    }

    async fn def_wire(
        &self,
        entry: &crate::registry::RepoEntry,
        repo: &str,
        d: &Def,
        source: Option<&[u8]>,
    ) -> Result<wire::WorkflowDefinition, Status> {
        let enabled = self
            .hub
            .store
            .is_enabled(repo, &d.name)
            .await
            .map_err(|e| sql(&e))?;
        let _ = entry;
        Ok(def_to_wire(d, enabled, source))
    }
}

#[tonic::async_trait]
impl Workflows for WorkflowService {
    type WatchRunStream = WatchStream;

    async fn list_definitions(
        &self,
        request: Request<wire::ListWorkflowDefinitionsRequest>,
    ) -> Result<Response<wire::ListWorkflowDefinitionsResponse>, Status> {
        let (meta, ext, msg) = request.into_parts();
        let ctx = self
            .env
            .context(&meta, &ext, msg.repo.as_ref(), Access::Read, false)
            .await?;
        let entry = self.entry(&ctx).await?;
        let defs = self.hub.defs_of(&entry).await?;
        let repo = ctx.repo.as_str().to_ascii_lowercase();
        let src = defs.source.map(|s| s.as_bytes().to_vec());
        let mut out = Vec::with_capacity(defs.list.len());
        for d in &defs.list {
            out.push(self.def_wire(&entry, &repo, d, src.as_deref()).await?);
        }
        Ok(Response::new(wire::ListWorkflowDefinitionsResponse {
            definitions: out,
        }))
    }

    async fn get_definition(
        &self,
        request: Request<wire::GetWorkflowDefinitionRequest>,
    ) -> Result<Response<wire::GetWorkflowDefinitionResponse>, Status> {
        let (meta, ext, msg) = request.into_parts();
        let ctx = self
            .env
            .context(&meta, &ext, msg.repo.as_ref(), Access::Read, false)
            .await?;
        let entry = self.entry(&ctx).await?;
        let defs = self.hub.defs_of(&entry).await?;
        let d = defs
            .get(&msg.name)
            .ok_or_else(|| Status::not_found("no such workflow"))?;
        let repo = ctx.repo.as_str().to_ascii_lowercase();
        let src = defs.source.map(|s| s.as_bytes().to_vec());
        Ok(Response::new(wire::GetWorkflowDefinitionResponse {
            definition: Some(self.def_wire(&entry, &repo, d, src.as_deref()).await?),
            text: bytes::Bytes::copy_from_slice(&d.text),
        }))
    }

    async fn validate(
        &self,
        request: Request<wire::ValidateWorkflowRequest>,
    ) -> Result<Response<wire::ValidateWorkflowResponse>, Status> {
        let (meta, ext, msg) = request.into_parts();
        let _ctx = self
            .env
            .context(&meta, &ext, msg.repo.as_ref(), Access::Read, false)
            .await?;
        if !valid_name(&msg.name) {
            return Err(Status::invalid_argument(
                "a workflow is named with [a-z0-9_-], 1 to 64 bytes",
            ));
        }
        if msg.text.len() > MAX_VALIDATE {
            return Err(Status::invalid_argument(format!(
                "a workflow file is at most {MAX_VALIDATE} bytes"
            )));
        }
        let parsed = defs::parse(&msg.name, &msg.text, &self.hub.cfg);
        let d = Def {
            name: msg.name.clone(),
            path: format!("{}/{}.toml", asc_core::workflow::WORKFLOW_DIR, msg.name),
            text: msg.text.to_vec(),
            parsed,
        };
        let diagnostics = match &d.parsed {
            Ok(_) => Vec::new(),
            Err(e) => vec![wire::WorkflowDiagnostic {
                code: "WORKFLOW_INVALID".into(),
                message: clean(&e.message, 500),
                line: e.line,
                column: e.column,
            }],
        };
        Ok(Response::new(wire::ValidateWorkflowResponse {
            valid: d.parsed.is_ok(),
            diagnostics,
            definition: Some(def_to_wire(&d, false, None)),
        }))
    }

    async fn set_enabled(
        &self,
        request: Request<wire::SetWorkflowEnabledRequest>,
    ) -> Result<Response<wire::SetWorkflowEnabledResponse>, Status> {
        let (meta, ext, msg) = request.into_parts();
        let ctx = self
            .env
            .context(&meta, &ext, msg.repo.as_ref(), Access::Write, false)
            .await?;
        let entry = self.entry(&ctx).await?;
        if !self.maintains(&ext, &ctx, &entry).await? {
            return Err(Status::permission_denied(
                "only a maintainer or an administrator switches a workflow on or off",
            ));
        }
        if !self.hub.cfg.enabled {
            return Err(disabled("instance"));
        }
        let defs = self.hub.defs_of(&entry).await?;
        let d = defs
            .get(&msg.name)
            .ok_or_else(|| Status::not_found("no such workflow"))?;
        let repo = ctx.repo.as_str().to_ascii_lowercase();
        if msg.enabled {
            let Some(wf) = d.workflow() else {
                return Err(invalid(&d.error()));
            };
            if wf
                .steps
                .iter()
                .any(|s| s.kind == asc_core::workflow::StepKind::Agent)
            {
                if !self.hub.agents.core.cfg.enabled {
                    return Err(errors::disabled("instance"));
                }
                if let PolicyState::Valid(p) = &*self.hub.agents.core.view.policy(&entry).await?
                    && !p.agents.enabled
                {
                    return Err(errors::disabled("repository policy"));
                }
            }
            let handle = require_handle(&ctx).ok();
            if handle.is_none() && !crate::authz::is_host_service(&ctx.principal.id) {
                return Err(Status::permission_denied(
                    "a workflow is switched on by a person",
                ));
            }
            // Switching on the first workflow of a repository starts its event cursors at "now".
            if self
                .hub
                .store
                .enabled_names(&repo)
                .await
                .map_err(|e| sql(&e))?
                .is_empty()
            {
                self.hub
                    .store
                    .clear_cursors(&repo)
                    .await
                    .map_err(|e| sql(&e))?;
            }
        }
        let changed = self
            .hub
            .store
            .set_enabled(&repo, &d.name, msg.enabled, &ctx.principal.id)
            .await
            .map_err(|e| sql(&e))?;
        if changed {
            self.hub
                .audit(Entry {
                    action: "workflow.enabled",
                    repo: &repo,
                    target: &d.name,
                    details: &[
                        ("workflow", d.name.clone()),
                        ("enabled", msg.enabled.to_string()),
                    ],
                    ..Entry::new("workflow.enabled", &ctx.principal.id)
                })
                .await;
        }
        let src = defs.source.map(|s| s.as_bytes().to_vec());
        Ok(Response::new(wire::SetWorkflowEnabledResponse {
            definition: Some(self.def_wire(&entry, &repo, d, src.as_deref()).await?),
        }))
    }

    async fn trigger(
        &self,
        request: Request<wire::TriggerWorkflowRequest>,
    ) -> Result<Response<wire::TriggerWorkflowResponse>, Status> {
        let (meta, ext, msg) = request.into_parts();
        let ctx = self
            .env
            .context(&meta, &ext, msg.repo.as_ref(), Access::Write, true)
            .await?;
        let handle = require_handle(&ctx)?;
        let op = ctx
            .op_id
            .ok_or_else(|| Status::invalid_argument("mutating calls need x-asc-op-id"))?;
        if !self.hub.cfg.enabled {
            return Err(disabled("instance"));
        }
        let entry = self.entry(&ctx).await?;
        let defs = self.hub.defs_of(&entry).await?;
        let d = defs
            .get(&msg.name)
            .ok_or_else(|| Status::not_found("no such workflow"))?
            .clone();
        let repo = ctx.repo.as_str().to_ascii_lowercase();
        let Some(wf) = d.workflow() else {
            return Err(invalid(&d.error()));
        };
        if !self
            .hub
            .store
            .is_enabled(&repo, &d.name)
            .await
            .map_err(|e| sql(&e))?
        {
            return Err(disabled("the workflow is not switched on"));
        }
        if !wf.triggers.iter().any(|t| t.kind == TriggerKind::Manual) {
            return Err(Status::failed_precondition(
                "this workflow has no manual trigger",
            ));
        }
        if wf
            .steps
            .iter()
            .any(|s| s.kind == asc_core::workflow::StepKind::Agent)
            && !self.hub.agents.core.cfg.enabled
        {
            return Err(errors::disabled("instance"));
        }
        let given: std::collections::BTreeMap<String, String> = msg
            .inputs
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        let inputs = wf
            .resolve_inputs(&given)
            .map_err(|e| Status::invalid_argument(clean(&e.message, 300)))?;
        let (change, change_cs) = match msg.change.len() {
            0 => (Vec::new(), Vec::new()),
            16 => {
                let head = self.hub.head_of(&entry, &msg.change).await?;
                let Some(head) = head else {
                    return Err(Status::not_found("unknown change"));
                };
                (msg.change.to_vec(), head.as_bytes().to_vec())
            }
            _ => return Err(Status::invalid_argument("a change is 16 bytes")),
        };
        let mut event = std::collections::BTreeMap::new();
        event.insert("actor".to_owned(), handle.clone());
        event.insert("branch".to_owned(), entry.default_branch.clone());
        if !change.is_empty() {
            event.insert(
                "change".to_owned(),
                change.iter().map(|b| format!("{b:02x}")).collect(),
            );
        }
        let hub = self.hub.clone();
        let this = self.clone();
        let principal = ctx.principal.id.clone();
        let start = Start {
            repo: repo.clone(),
            def: d.clone(),
            source: defs
                .source
                .map(|s| s.as_bytes().to_vec())
                .unwrap_or_default(),
            trigger: TriggerKind::Manual,
            started_by: handle,
            requester: principal.clone(),
            change,
            change_cs,
            event,
            inputs,
            depth: 0,
            origin: String::new(),
        };
        let ops = hub.agents.core.ops.clone();
        once(
            &ops,
            op,
            &principal,
            "Workflows/Trigger",
            true,
            || async move {
                let run = hub.start(start).await.map_err(|e: StartError| e.status())?;
                Ok(wire::TriggerWorkflowResponse {
                    run: Some(this.wire_run(&run, false).await?),
                })
            },
        )
        .await
        .map(Response::new)
    }

    async fn list_runs(
        &self,
        request: Request<wire::ListWorkflowRunsRequest>,
    ) -> Result<Response<wire::ListWorkflowRunsResponse>, Status> {
        let (meta, ext, msg) = request.into_parts();
        let ctx = self
            .env
            .context(&meta, &ext, msg.repo.as_ref(), Access::Read, false)
            .await?;
        if !msg.name.is_empty() && !valid_name(&msg.name) {
            return Err(Status::invalid_argument(
                "a workflow is named with [a-z0-9_-], 1 to 64 bytes",
            ));
        }
        let limit = match msg.limit {
            0 => DEFAULT_RUNS,
            n => usize::try_from(n)
                .unwrap_or(MAX_RUNS_PAGE)
                .min(MAX_RUNS_PAGE),
        };
        let mut states = Vec::new();
        for s in &msg.states {
            let st = wire::WorkflowRunState::try_from(*s)
                .map_err(|_| Status::invalid_argument("unknown state"))?;
            if let Some(s) = RunState::from_wire(st) {
                states.push(s);
            }
        }
        let scope = format!("Workflows.ListRuns|{}", ctx.repo);
        let before = crate::pagetoken::open_id(&scope, &ctx.principal.id, &msg.page_token)?
            .map(|n| u64::try_from(n).unwrap_or(0));
        let mut rows = self
            .hub
            .store
            .list_runs(
                &ctx.repo.as_str().to_ascii_lowercase(),
                &msg.name,
                &states,
                before,
                limit,
            )
            .await
            .map_err(|e| sql(&e))?;
        let next = if rows.len() > limit {
            rows.truncate(limit);
            rows.last()
                .map(|r| crate::pagetoken::seal(&scope, &ctx.principal.id, &r.id.to_string()))
                .unwrap_or_default()
        } else {
            String::new()
        };
        let mut runs = Vec::with_capacity(rows.len());
        for r in &rows {
            runs.push(self.wire_run(r, false).await?);
        }
        Ok(Response::new(wire::ListWorkflowRunsResponse {
            runs,
            next_page_token: next,
        }))
    }

    async fn get_run(
        &self,
        request: Request<wire::GetWorkflowRunRequest>,
    ) -> Result<Response<wire::GetWorkflowRunResponse>, Status> {
        let (meta, ext, msg) = request.into_parts();
        let ctx = self
            .env
            .context(&meta, &ext, msg.repo.as_ref(), Access::Read, false)
            .await?;
        let run = self.run_in(&ctx, msg.id).await?;
        Ok(Response::new(wire::GetWorkflowRunResponse {
            run: Some(self.wire_run(&run, true).await?),
        }))
    }

    async fn cancel_run(
        &self,
        request: Request<wire::CancelWorkflowRunRequest>,
    ) -> Result<Response<wire::CancelWorkflowRunResponse>, Status> {
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
            let entry = self.entry(&ctx).await?;
            allowed = self.maintains(&ext, &ctx, &entry).await?;
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
        Ok(Response::new(wire::CancelWorkflowRunResponse {
            run: Some(self.wire_run(&row, false).await?),
        }))
    }

    async fn decide_approval(
        &self,
        request: Request<wire::DecideWorkflowApprovalRequest>,
    ) -> Result<Response<wire::DecideWorkflowApprovalResponse>, Status> {
        let (meta, ext, msg) = request.into_parts();
        let ctx = self
            .env
            .context(&meta, &ext, msg.repo.as_ref(), Access::Write, true)
            .await?;
        // An agent never decides: only a person has a handle.
        let handle = require_handle(&ctx)?;
        let op = ctx
            .op_id
            .ok_or_else(|| Status::invalid_argument("mutating calls need x-asc-op-id"))?;
        let run = self.run_in(&ctx, msg.id).await?;
        let entry = self.entry(&ctx).await?;
        let admin = self.env.has(&ext, &ctx, Access::Admin).await;
        let policy = match &*self.hub.agents.core.view.policy(&entry).await? {
            PolicyState::Valid(p) => Some(p.clone()),
            _ => None,
        };
        let who = Who::new(Kind::User, handle.clone());
        let in_role = move |role: &str| -> bool {
            policy.as_ref().is_some_and(|p| p.is_member(&who, role))
                || (role == "maintainer" && admin)
        };
        let hub = self.hub.clone();
        let this = self.clone();
        let principal = ctx.principal.id.clone();
        let step = msg.step.clone();
        let (approve, comment) = (msg.approve, msg.comment.clone());
        let ops = hub.agents.core.ops.clone();
        once(
            &ops,
            op,
            &principal.clone(),
            "Workflows/DecideApproval",
            true,
            || async move {
                let row = hub
                    .decide_approval(
                        run.id, &step, &principal, &handle, approve, &comment, in_role,
                    )
                    .await?;
                Ok(wire::DecideWorkflowApprovalResponse {
                    run: Some(this.wire_run(&row, false).await?),
                })
            },
        )
        .await
        .map(Response::new)
    }

    async fn watch_run(
        &self,
        request: Request<wire::WatchWorkflowRunRequest>,
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
            std::collections::VecDeque::<wire::WorkflowRunEvent>::new(),
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
                    match hub.store.events_after(id, after, WATCH_BATCH).await {
                        Ok(rows) if !rows.is_empty() => {
                            for (seq, time_ms, step, snapshot) in rows {
                                buf.push_back(wire::WorkflowRunEvent {
                                    seq,
                                    time_ms,
                                    run: wire::WorkflowRun::decode(snapshot.as_slice()).ok(),
                                    step,
                                });
                            }
                            continue;
                        }
                        Ok(_) => {}
                        Err(e) => return Some((Err(sql(&e)), (hub, id, after, buf, true, permit))),
                    }
                    match hub.store.get_run(id).await {
                        Ok(Some(r)) if !r.state.is_active() => {
                            match hub.store.events_after(id, after, WATCH_BATCH).await {
                                Ok(rows) if !rows.is_empty() => {
                                    for (seq, time_ms, step, snapshot) in rows {
                                        buf.push_back(wire::WorkflowRunEvent {
                                            seq,
                                            time_ms,
                                            run: wire::WorkflowRun::decode(snapshot.as_slice())
                                                .ok(),
                                            step,
                                        });
                                    }
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

    async fn list_secrets(
        &self,
        request: Request<wire::ListWorkflowSecretsRequest>,
    ) -> Result<Response<wire::ListWorkflowSecretsResponse>, Status> {
        let (meta, ext, msg) = request.into_parts();
        let ctx = self
            .env
            .context(&meta, &ext, msg.repo.as_ref(), Access::Admin, false)
            .await?;
        let list = self
            .hub
            .secrets
            .list(ctx.repo.as_str())
            .await
            .map_err(secret_status)?;
        Ok(Response::new(wire::ListWorkflowSecretsResponse {
            secrets: list.into_iter().map(secret_wire).collect(),
        }))
    }

    async fn set_secret(
        &self,
        request: Request<wire::SetWorkflowSecretRequest>,
    ) -> Result<Response<wire::SetWorkflowSecretResponse>, Status> {
        let (meta, ext, msg) = request.into_parts();
        // The value is wrapped at once and is never formatted.
        let value = Zeroizing::new(msg.value);
        let ctx = self
            .env
            .context(&meta, &ext, msg.repo.as_ref(), Access::Admin, true)
            .await?;
        let op = ctx
            .op_id
            .ok_or_else(|| Status::invalid_argument("mutating calls need x-asc-op-id"))?;
        let hub = self.hub.clone();
        let repo = ctx.repo.as_str().to_ascii_lowercase();
        let (name, by) = (msg.name, ctx.principal.id.clone());
        let ops = hub.agents.core.ops.clone();
        once(
            &ops,
            op,
            &by.clone(),
            "Workflows/SetSecret",
            true,
            || async move {
                let info = hub
                    .secrets
                    .set(&repo, &name, &value, &by)
                    .await
                    .map_err(secret_status)?;
                Ok(wire::SetWorkflowSecretResponse {
                    secret: Some(secret_wire(info)),
                })
            },
        )
        .await
        .map(Response::new)
    }

    async fn delete_secret(
        &self,
        request: Request<wire::DeleteWorkflowSecretRequest>,
    ) -> Result<Response<wire::DeleteWorkflowSecretResponse>, Status> {
        let (meta, ext, msg) = request.into_parts();
        let ctx = self
            .env
            .context(&meta, &ext, msg.repo.as_ref(), Access::Admin, false)
            .await?;
        if !super::secrets::valid_name(&msg.name) {
            return Err(Status::invalid_argument(
                "a secret name is 1 to 64 bytes of A-Z, a-z, 0-9, _, . and -",
            ));
        }
        self.hub
            .secrets
            .delete(ctx.repo.as_str(), &msg.name)
            .await
            .map_err(secret_status)?;
        Ok(Response::new(wire::DeleteWorkflowSecretResponse {}))
    }
}

fn secret_wire(i: super::secrets::SecretInfo) -> wire::WorkflowSecret {
    wire::WorkflowSecret {
        name: i.name,
        hint: i.hint,
        created: i.created,
        updated: i.updated,
        updated_by: i.updated_by,
    }
}

fn secret_status(e: SecretError) -> Status {
    match e {
        SecretError::Invalid(m) => Status::invalid_argument(m),
        SecretError::TooMany => Status::resource_exhausted(format!(
            "a repository holds at most {} secrets",
            super::secrets::MAX_PER_REPO
        )),
        SecretError::Vault(crate::vault::VaultError::Locked(_)) => {
            rpc_status(&RpcError::VaultLocked(ErrorInfo {
                paths: vec!["locked".into()],
                ..ErrorInfo::default()
            }))
        }
        SecretError::Vault(_) => Status::internal("internal error"),
        SecretError::Sql(c) => crate::update::internal("workflow secrets", &c),
    }
}
