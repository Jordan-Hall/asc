//! The `Policy` service (`policy.proto`) and `Repo.GetPolicy`.
//!
//! Reading the policy needs repo read; proposing a change needs repo write and makes an ordinary
//! change on a draft ref of the caller, which only a maintainer named by the policy can land (the
//! path rules refuse anyone else, whoever the proposer is). Evaluating the policy for another
//! principal needs a maintainer, because it tells how someone else is bound.

use std::sync::Arc;

use asc_core::id::ChangesetId;
use asc_core::policy::{Diagnostic, MAX_POLICY_BYTES, POLICY_PATH, POLICY_VERSION, Policy, Who};
use asc_rpc::auth::{Access, AuthInterceptor, Authorizer};
use asc_rpc::errors::RpcError;
use asc_rpc::server::{CallCtx, MAX_MESSAGE, MAX_STRING, MutationsHandler, ServerInfo, wire};
use asc_rpc::services::policy_server::{Policy as PolicyApi, PolicyServer};
use async_trait::async_trait;
use bytes::Bytes;
use tonic::service::interceptor::InterceptedService;
use tonic::{Request, Response, Status};

use super::{Loaded, MAX_EVAL_PATHS, PolicyStore, State, who_from_ref, who_of};
use crate::audit::NewEvent;
use crate::audit::actor::actor_of;
use crate::idmode::IdCodec;
use crate::mutations::ServerMutations;
use crate::registry::{Registry, RepoEntry};
use crate::review::{Env, handle_of, internal, now, registry_status, sql};
use crate::update::{info, rpc};

/// Longest commit message of a proposal, in bytes.
const MAX_MESSAGE_BYTES: usize = 4096;
/// The message of a proposal that names none.
const DEFAULT_MESSAGE: &str = "policy: update .asc/policy.toml";
/// Longest repository path a decision is asked for, in bytes.
const MAX_PATH_BYTES: usize = 4096;

/// The state the `Policy` service shares.
pub struct PolicyHub {
    registry: Arc<Registry>,
    policies: Arc<PolicyStore>,
    commits: Arc<ServerMutations>,
}

fn subject_list(subjects: &[asc_core::policy::Subject]) -> Vec<String> {
    subjects.iter().map(|s| s.render()).collect()
}

fn globs(g: &[asc_core::policy::PathGlob]) -> Vec<String> {
    g.iter().map(|g| g.as_str().to_owned()).collect()
}

fn diagnostic(d: &Diagnostic) -> wire::PolicyDiagnostic {
    wire::PolicyDiagnostic {
        severity: wire::PolicySeverity::Error as i32,
        code: d.code.to_owned(),
        message: d.message.clone(),
        line: d.line,
        column: d.column,
    }
}

fn warning(code: &str, message: &str) -> wire::PolicyDiagnostic {
    wire::PolicyDiagnostic {
        severity: wire::PolicySeverity::Warning as i32,
        code: code.to_owned(),
        message: message.to_owned(),
        line: 0,
        column: 0,
    }
}

fn kind_name(who: &Who) -> String {
    who.kind.map_or(String::new(), |k| k.as_str().to_owned())
}

/// The paths of a request, checked.
fn paths_of(paths: &[Bytes]) -> Result<(), Status> {
    if paths.len() > MAX_EVAL_PATHS {
        return Err(Status::invalid_argument(format!(
            "at most {MAX_EVAL_PATHS} paths in one call"
        )));
    }
    for p in paths {
        if p.is_empty() || p.len() > MAX_PATH_BYTES || p.contains(&0) {
            return Err(Status::invalid_argument("invalid path"));
        }
    }
    Ok(())
}

/// `GetPolicyResponse` of `loaded` for `who`, with a decision per path.
fn describe(
    default_branch: &str,
    loaded: &Loaded,
    source: Bytes,
    who: &Who,
    paths: &[Bytes],
) -> wire::GetPolicyResponse {
    let mut out = wire::GetPolicyResponse {
        source,
        principal_kind: kind_name(who),
        ..wire::GetPolicyResponse::default()
    };
    match &loaded.state {
        State::Absent => {
            out.decisions = paths
                .iter()
                .map(|p| wire::PolicyDecision {
                    path: p.clone(),
                    allowed: true,
                    reason: String::new(),
                })
                .collect();
        }
        State::Invalid(d) => {
            out.present = true;
            out.error = d.message.clone();
            // Unusable: nothing is allowed until trunk is repaired.
            out.decisions = paths
                .iter()
                .map(|p| wire::PolicyDecision {
                    path: p.clone(),
                    allowed: false,
                    reason: "the repository policy cannot be used".into(),
                })
                .collect();
        }
        State::Valid(p) => {
            out.present = true;
            out.version = POLICY_VERSION;
            out.maintainer = p.is_maintainer(who);
            out.roles = p
                .roles
                .iter()
                .map(|(name, members)| wire::PolicyRole {
                    name: name.clone(),
                    members: subject_list(members),
                })
                .collect();
            out.trunk = Some(wire::PolicyTrunk {
                branch: default_branch.to_owned(),
                require_approvals: p.trunk.require_approvals,
                require_checks: p.trunk.require_checks.clone(),
                allow_force: p.trunk.allow_force,
                require_signed: p.signing.require_signed,
            });
            out.paths = p
                .paths
                .iter()
                .map(|r| wire::PolicyPathRule {
                    glob: r.glob.as_str().to_owned(),
                    write: subject_list(&r.write),
                })
                .collect();
            out.agents = Some(wire::PolicyAgents {
                default_scope: globs(&p.agents.default_scope),
                denied_paths: globs(&p.agents.denied_paths),
                max_changes_per_run: p.agents.budgets.max_changes_per_run.unwrap_or(0),
                max_paths_per_run: p.agents.budgets.max_paths_per_run.unwrap_or(0),
                enabled: p.agents.enabled,
                max_runtime_seconds: p.agents.max_runtime_seconds.unwrap_or(0),
                max_tokens: p.agents.budgets.max_tokens.unwrap_or(0),
                max_cost_micro_usd: p.agents.budgets.max_cost_micro_usd.unwrap_or(0),
            });
            out.review = Some(wire::PolicyReview {
                author_may_approve: p.review.author_may_approve,
                human_approval: p.review.human_approval,
                keep_approvals_on_restack: p.review.keep_approvals_on_restack,
            });
            out.merge = Some(wire::PolicyMerge {
                queue: p.merge.queue,
                revalidate: p.merge.revalidate,
                batch: p.merge.batch,
                post_merge_checks: p.merge.post_merge_checks,
            });
            out.stacks = Some(wire::PolicyStacks {
                max_depth: p.stacks.max_depth.unwrap_or(0),
                cleanup_days: p.stacks.cleanup_days.unwrap_or(0),
                restack_on_main_move: p.stacks.restack_on_main_move,
                collaborators_may_push: p.stacks.collaborators_may_push,
            });
            out.merge_strategy = p.strategy.as_str().to_owned();
            out.decisions = paths
                .iter()
                .map(|path| match p.check_write(who, path) {
                    Ok(()) => wire::PolicyDecision {
                        path: path.clone(),
                        allowed: true,
                        reason: String::new(),
                    },
                    Err(d) => wire::PolicyDecision {
                        path: path.clone(),
                        allowed: false,
                        reason: d.reason,
                    },
                })
                .collect();
        }
    }
    out
}

impl PolicyHub {
    pub fn new(
        registry: Arc<Registry>,
        policies: Arc<PolicyStore>,
        commits: Arc<ServerMutations>,
    ) -> Self {
        Self {
            registry,
            policies,
            commits,
        }
    }

    pub fn policies(&self) -> &Arc<PolicyStore> {
        &self.policies
    }

    async fn open(&self, ctx: &CallCtx) -> Result<Arc<RepoEntry>, Status> {
        self.registry.open(&ctx.repo).await.map_err(registry_status)
    }

    fn codec(&self, ctx: &CallCtx, entry: &RepoEntry) -> IdCodec {
        IdCodec::new(ctx.id_mode, &self.registry, entry).for_viewer(&ctx.principal)
    }

    async fn source_out(&self, ids: &IdCodec, loaded: &Loaded) -> Result<Bytes, Status> {
        match loaded.source {
            Some(cs) => ids.changeset_out(cs).await,
            None => Ok(Bytes::new()),
        }
    }

    /// `Repo.GetPolicy`: the policy in force, as the caller is bound by it.
    pub async fn get_policy(
        &self,
        ctx: &CallCtx,
        req: wire::GetPolicyRequest,
    ) -> Result<wire::GetPolicyResponse, Status> {
        paths_of(&req.paths)?;
        let entry = self.open(ctx).await?;
        let ids = self.codec(ctx, &entry);
        let loaded = self.policies.in_force(&entry).await?;
        let who = who_of(&ctx.principal.id);
        let source = self.source_out(&ids, &loaded).await?;
        Ok(describe(
            &entry.default_branch,
            &loaded,
            source,
            &who,
            &req.paths,
        ))
    }

    /// `Policy.GetSource`.
    pub async fn get_source(
        &self,
        ctx: &CallCtx,
        req: wire::GetPolicySourceRequest,
    ) -> Result<wire::GetPolicySourceResponse, Status> {
        if req.rev.len() > MAX_STRING {
            return Err(Status::invalid_argument("revision is too long"));
        }
        let entry = self.open(ctx).await?;
        let ids = self.codec(ctx, &entry);
        let loaded = if req.rev.is_empty() {
            self.policies.in_force(&entry).await?
        } else {
            let cs = ids
                .resolve(&req.rev, &entry.default_branch, |e| internal("history", &e))
                .await?
                .ok_or_else(|| Status::not_found(format!("unknown revision {:?}", req.rev)))?;
            self.policies.at(&entry, cs).await?
        };
        let who = who_of(&ctx.principal.id);
        let source = self.source_out(&ids, &loaded).await?;
        let parsed = describe(&entry.default_branch, &loaded, source.clone(), &who, &[]);
        Ok(wire::GetPolicySourceResponse {
            present: loaded.present(),
            source,
            text: loaded.text.clone(),
            parsed: Some(parsed),
        })
    }

    /// The verdict on `text`, with the warnings an editor shows. `current` is the policy in force.
    fn judge(
        &self,
        default_branch: &str,
        text: &[u8],
        who: &Who,
        current: &Loaded,
    ) -> wire::ValidatePolicyResponse {
        match Policy::diagnose(text) {
            Err(d) => wire::ValidatePolicyResponse {
                valid: false,
                diagnostics: vec![diagnostic(&d)],
                parsed: None,
                caller_stays_maintainer: false,
            },
            Ok(p) => {
                let stays = p.is_maintainer(who);
                let mut diagnostics = Vec::new();
                if p.maintainers_include_agents() {
                    diagnostics.push(warning(
                        "maintainer_includes_agents",
                        "an agent named as a maintainer can change this policy and so lift every rule that binds it",
                    ));
                }
                let was_maintainer = match &current.state {
                    State::Valid(c) => c.is_maintainer(who),
                    // No policy: everyone who can write is effectively a maintainer.
                    State::Absent => true,
                    State::Invalid(_) => false,
                };
                if was_maintainer && !stays {
                    diagnostics.push(warning(
                        "lockout",
                        "you are not a maintainer in this file: after it lands you could not change the policy again",
                    ));
                }
                let loaded = Loaded {
                    source: None,
                    text: Bytes::copy_from_slice(text),
                    state: State::Valid(Arc::new(p)),
                };
                wire::ValidatePolicyResponse {
                    valid: true,
                    diagnostics,
                    parsed: Some(describe(default_branch, &loaded, Bytes::new(), who, &[])),
                    caller_stays_maintainer: stays,
                }
            }
        }
    }

    /// `Policy.Validate`.
    pub async fn validate(
        &self,
        ctx: &CallCtx,
        req: wire::ValidatePolicyRequest,
    ) -> Result<wire::ValidatePolicyResponse, Status> {
        if req.text.len() > MAX_POLICY_BYTES {
            // Reported as a diagnostic like any other refusal of the file.
            let d = Diagnostic {
                code: "too_large",
                message: format!("the policy file is larger than {MAX_POLICY_BYTES} bytes"),
                line: 0,
                column: 0,
            };
            return Ok(wire::ValidatePolicyResponse {
                valid: false,
                diagnostics: vec![diagnostic(&d)],
                parsed: None,
                caller_stays_maintainer: false,
            });
        }
        let entry = self.open(ctx).await?;
        let current = self.policies.in_force(&entry).await?;
        let who = who_of(&ctx.principal.id);
        Ok(self.judge(&entry.default_branch, &req.text, &who, &current))
    }

    /// `Policy.Propose`: a change on top of `base` that writes the file, on a draft ref of the
    /// caller. It never writes trunk.
    pub async fn propose(
        &self,
        ctx: &CallCtx,
        req: wire::ProposePolicyRequest,
    ) -> Result<wire::ProposePolicyResponse, Status> {
        if req.text.len() > MAX_POLICY_BYTES {
            return Err(rpc(RpcError::PolicyInvalid(info(vec![format!(
                "the policy file is larger than {MAX_POLICY_BYTES} bytes"
            )]))));
        }
        if req.message.len() > MAX_MESSAGE_BYTES || req.message.contains(&0) {
            return Err(Status::invalid_argument("invalid message"));
        }
        let entry = self.open(ctx).await?;
        let ids = self.codec(ctx, &entry);
        let who = who_of(&ctx.principal.id);
        let current = self.policies.in_force(&entry).await?;
        let validation = self.judge(&entry.default_branch, &req.text, &who, &current);
        if !validation.valid {
            let d = validation
                .diagnostics
                .first()
                .map(|d| {
                    if d.line > 0 {
                        format!("line {}: {}", d.line, d.message)
                    } else {
                        d.message.clone()
                    }
                })
                .unwrap_or_default();
            return Err(rpc(RpcError::PolicyInvalid(info(vec![d]))));
        }
        if validation.diagnostics.iter().any(|d| d.code == "lockout") {
            return Err(rpc(RpcError::PolicyLockout(info(vec![
                "the proposed file does not name you as a maintainer".into(),
            ]))));
        }
        // The edit starts from trunk as it is now: a moved trunk is a conflict, not a silent merge.
        let base = ids.changeset_in(&req.base).await?;
        let tip = self.policies.trunk_tip(&entry).await?;
        if tip != Some(base) {
            let mut i = info(vec!["trunk moved; start again from its tip".into()]);
            if let Some(t) = tip {
                i.actual = ids.changeset_out(t).await?;
            }
            return Err(rpc(RpcError::VersionConflict(i)));
        }
        let handle = handle_of(&ctx.principal.id);
        let name = handle.clone().unwrap_or_else(|| "asc".to_owned());
        let at = now();
        let signature = wire::Signature {
            name: name.clone().into_bytes().into(),
            email: format!("{name}@asc.invalid").into_bytes().into(),
            time: at,
            tz_min: 0,
        };
        let mut suffix = [0u8; 4];
        getrandom::fill(&mut suffix).map_err(|e| internal("random", &e))?;
        let update_ref = format!(
            "refs/heads/asc/policy-{at}-{}",
            suffix
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<String>()
        );
        let message = if req.message.is_empty() {
            Bytes::from_static(DEFAULT_MESSAGE.as_bytes())
        } else {
            req.message.clone()
        };
        let commit = wire::CommitRequest {
            repo: Some(wire::RepoRef {
                name: ctx.repo.to_string(),
                id_mode: ctx.id_mode.to_wire(),
            }),
            parent: ids.changeset_out(base).await.map(hex_of)?,
            ops: vec![wire::FileOp {
                path: Bytes::from_static(POLICY_PATH.as_bytes()),
                op: Some(wire::file_op::Op::PutContent(req.text.clone())),
                mode: 0,
            }],
            message,
            author: Some(signature.clone()),
            committer: Some(signature),
            update_ref: update_ref.clone(),
            expect: None,
            sign: false,
        };
        let made = self.commits.commit(ctx, commit).await?;
        let cs = ids.changeset_in(&made.id).await?;
        let change = change_of(self.registry.pool(), &entry, &cs).await?;
        let event = NewEvent::new("policy.proposed", actor_of(&ctx.principal.id))
            .repo(ctx.repo.as_str())
            .target(&update_ref)
            .detail("change", hex_of(change.clone()))
            .detail("summary", "policy file proposed");
        crate::audit::store::record_best_effort(self.registry.pool(), &event).await;
        Ok(wire::ProposePolicyResponse {
            change,
            changeset: made.id,
            validation: Some(validation),
        })
    }

    /// `Policy.Evaluate`.
    pub async fn evaluate(
        &self,
        ctx: &CallCtx,
        req: wire::EvaluatePolicyRequest,
    ) -> Result<wire::EvaluatePolicyResponse, Status> {
        use wire::PolicyAction as A;
        paths_of(&req.paths)?;
        if req.rev.len() > MAX_STRING {
            return Err(Status::invalid_argument("revision is too long"));
        }
        let action =
            A::try_from(req.action).map_err(|_| Status::invalid_argument("unknown action"))?;
        if action == A::Unspecified {
            return Err(Status::invalid_argument("name the action to evaluate"));
        }
        let entry = self.open(ctx).await?;
        let ids = self.codec(ctx, &entry);
        let loaded = if req.rev.is_empty() {
            self.policies.in_force(&entry).await?
        } else {
            let cs = ids
                .resolve(&req.rev, &entry.default_branch, |e| internal("history", &e))
                .await?
                .ok_or_else(|| Status::not_found(format!("unknown revision {:?}", req.rev)))?;
            self.policies.at(&entry, cs).await?
        };
        let me = who_of(&ctx.principal.id);
        let target = match &req.principal {
            None => me.clone(),
            Some(p) if p.kind.is_empty() && p.name.is_empty() => me.clone(),
            Some(p) => {
                if p.kind.len() > 16 || p.name.len() > 255 || p.name.chars().any(char::is_control) {
                    return Err(Status::invalid_argument("invalid principal"));
                }
                who_from_ref(&p.kind, &p.name)
            }
        };
        // How someone else is bound is for maintainers to see. With no policy in force there is
        // nobody to name, and with an unusable one, nobody can be told apart: refuse (not found,
        // so principals cannot be probed).
        if target != me {
            let allowed = loaded.policy().is_some_and(|p| p.is_maintainer(&me));
            if !allowed {
                return Err(Status::not_found("no such principal"));
            }
        }
        let principal = Some(wire::PrincipalRef {
            kind: kind_name(&target),
            name: target.name.clone(),
        });
        let mut out = wire::EvaluatePolicyResponse {
            principal,
            ..wire::EvaluatePolicyResponse::default()
        };
        let policy = loaded.policy();
        match action {
            A::Unspecified => {}
            A::Write => {
                let described = describe(
                    &entry.default_branch,
                    &loaded,
                    Bytes::new(),
                    &target,
                    &req.paths,
                );
                out.decisions = described.decisions;
            }
            A::Run => match (&loaded.state, policy) {
                (State::Invalid(_), _) => {
                    out.run_allowed = false;
                    out.run_reason = "the repository policy cannot be used".into();
                }
                (_, Some(p)) => match p.check_run(
                    &target,
                    usize::try_from(req.run_changes).unwrap_or(usize::MAX),
                    usize::try_from(req.run_paths).unwrap_or(usize::MAX),
                ) {
                    Ok(()) => out.run_allowed = true,
                    Err(d) => {
                        out.run_allowed = false;
                        out.run_reason = d.reason;
                    }
                },
                (_, None) => out.run_allowed = true,
            },
            A::Land => {
                let r = policy.map(|p| p.land_requirements());
                out.land = Some(match r {
                    Some(r) => wire::PolicyLandRequirements {
                        approvals: r.approvals,
                        checks: r.checks,
                        signed: r.signed,
                        merge_strategy: r.strategy.as_str().to_owned(),
                    },
                    None => wire::PolicyLandRequirements {
                        merge_strategy: asc_core::policy::Strategy::default().as_str().to_owned(),
                        ..wire::PolicyLandRequirements::default()
                    },
                });
            }
        }
        Ok(out)
    }
}

fn hex_of(b: Bytes) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// The change id of `cs` in the change index (empty when the index does not know it).
async fn change_of(
    pool: &sqlx::AnyPool,
    entry: &RepoEntry,
    cs: &ChangesetId,
) -> Result<Bytes, Status> {
    let row: Option<Vec<u8>> =
        sqlx::query_scalar("SELECT change FROM changes WHERE ns = $1 AND cs = $2")
            .bind(entry.ns.as_bytes().to_vec())
            .bind(cs.as_bytes().to_vec())
            .fetch_optional(pool)
            .await
            .map_err(sql)?;
    Ok(row.map(Bytes::from).unwrap_or_default())
}

/// `Repo.GetPolicy` is served by the same hub.
#[async_trait]
impl asc_rpc::server::PolicyHandler for PolicyHub {
    async fn get_policy(
        &self,
        ctx: &CallCtx,
        req: wire::GetPolicyRequest,
    ) -> Result<wire::GetPolicyResponse, Status> {
        PolicyHub::get_policy(self, ctx, req).await
    }
}

// ------------------------------------------------------------------------------ service

/// The `asc.v1.Policy` tonic service.
pub struct PolicyService {
    hub: Arc<PolicyHub>,
    env: Env,
}

impl PolicyService {
    pub fn new(hub: Arc<PolicyHub>, authorizer: Arc<dyn Authorizer>, info: ServerInfo) -> Self {
        Self {
            hub,
            env: Env { authorizer, info },
        }
    }

    /// The tonic service with message limits, zstd request decoding and [`AuthInterceptor`].
    pub fn into_service(self) -> InterceptedService<PolicyServer<Self>, AuthInterceptor> {
        let server = PolicyServer::new(self)
            .max_decoding_message_size(MAX_MESSAGE)
            .max_encoding_message_size(MAX_MESSAGE)
            .accept_compressed(tonic::codec::CompressionEncoding::Zstd);
        InterceptedService::new(server, AuthInterceptor)
    }
}

#[tonic::async_trait]
impl PolicyApi for PolicyService {
    async fn get_source(
        &self,
        request: Request<wire::GetPolicySourceRequest>,
    ) -> Result<Response<wire::GetPolicySourceResponse>, Status> {
        let (meta, ext, msg) = request.into_parts();
        let ctx = self
            .env
            .context(&meta, &ext, msg.repo.as_ref(), Access::Read, false)
            .await?;
        self.hub.get_source(&ctx, msg).await.map(Response::new)
    }

    async fn validate(
        &self,
        request: Request<wire::ValidatePolicyRequest>,
    ) -> Result<Response<wire::ValidatePolicyResponse>, Status> {
        let (meta, ext, msg) = request.into_parts();
        let ctx = self
            .env
            .context(&meta, &ext, msg.repo.as_ref(), Access::Read, false)
            .await?;
        self.hub.validate(&ctx, msg).await.map(Response::new)
    }

    async fn propose(
        &self,
        request: Request<wire::ProposePolicyRequest>,
    ) -> Result<Response<wire::ProposePolicyResponse>, Status> {
        let (meta, ext, msg) = request.into_parts();
        let ctx = self
            .env
            .context(&meta, &ext, msg.repo.as_ref(), Access::Write, true)
            .await?;
        self.hub.propose(&ctx, msg).await.map(Response::new)
    }

    async fn evaluate(
        &self,
        request: Request<wire::EvaluatePolicyRequest>,
    ) -> Result<Response<wire::EvaluatePolicyResponse>, Status> {
        let (meta, ext, msg) = request.into_parts();
        let ctx = self
            .env
            .context(&meta, &ext, msg.repo.as_ref(), Access::Read, false)
            .await?;
        self.hub.evaluate(&ctx, msg).await.map(Response::new)
    }
}
