//! The `OrgSettings` tonic service.

use std::sync::Arc;

use asc_rpc::auth::{AuthInterceptor, Authorizer};
use asc_rpc::forwarded::Role;
use asc_rpc::server::{MAX_MESSAGE, ServerInfo, wire};
use asc_rpc::services::org_settings_server::{OrgSettings, OrgSettingsServer};
use tonic::service::interceptor::InterceptedService;
use tonic::{Request, Response, Status};

use super::policy::{self, MergeDefaults};
use super::{AgentPolicy, AgentRow, InstanceAgents, OrgHub, OrgRow};
use crate::ai::audit::Entry;
use crate::ai::caller::{once, op_id};
use crate::settings::access::{Actor, Gate, Subject};

/// The `asc.v1.OrgSettings` service.
pub struct OrgService {
    hub: Arc<OrgHub>,
    gate: Gate,
}

fn fail(e: super::OrgError) -> Status {
    e.status()
}

fn wire_merge(d: &MergeDefaults, set: bool) -> wire::MergeDefaults {
    wire::MergeDefaults {
        require_approvals: d.require_approvals,
        require_checks: d.require_checks.clone(),
        allow_force: d.allow_force,
        require_signed: d.require_signed,
        human_approval: d.human_approval,
        author_may_approve: d.author_may_approve,
        queue: d.queue,
        revalidate: d.revalidate,
        strategy: d.strategy.clone(),
        set,
    }
}

fn merge_of(w: &wire::MergeDefaults) -> MergeDefaults {
    MergeDefaults {
        require_approvals: w.require_approvals,
        require_checks: w.require_checks.clone(),
        allow_force: w.allow_force,
        require_signed: w.require_signed,
        human_approval: w.human_approval,
        author_may_approve: w.author_may_approve,
        queue: w.queue,
        revalidate: w.revalidate,
        strategy: w.strategy.clone(),
    }
}

fn wire_agent(a: &AgentRow) -> wire::OrgAgent {
    wire::OrgAgent {
        name: a.name.clone(),
        display_name: a.display.clone(),
        run_by: a.run_by.clone(),
        paths: a.paths.clone(),
        repos: a.repos.clone(),
        state: if a.paused {
            wire::OrgAgentState::Paused
        } else {
            wire::OrgAgentState::Active
        } as i32,
        created: a.created,
        created_by: a.created_by.clone(),
        updated: a.updated,
    }
}

impl OrgService {
    pub fn new(hub: Arc<OrgHub>, authorizer: Arc<dyn Authorizer>, _info: ServerInfo) -> Self {
        Self {
            hub,
            gate: Gate::new(authorizer),
        }
    }

    pub fn into_service(self) -> InterceptedService<OrgSettingsServer<Self>, AuthInterceptor> {
        let server = OrgSettingsServer::new(self)
            .max_decoding_message_size(MAX_MESSAGE)
            .max_encoding_message_size(MAX_MESSAGE)
            .accept_compressed(tonic::codec::CompressionEncoding::Zstd);
        InterceptedService::new(server, AuthInterceptor)
    }

    /// The organisation and the actor, provided the caller holds `needed` on it.
    async fn org(
        &self,
        ext: &tonic::Extensions,
        meta: &tonic::metadata::MetadataMap,
        org: &str,
        needed: Role,
    ) -> Result<(Subject, Actor), Status> {
        let subject = Subject::org(org)?;
        let caller = self.gate.caller(ext).await?;
        let actor = self.gate.require(&caller, meta, &subject, needed)?;
        Ok((subject, actor))
    }

    async fn settings(
        &self,
        org: &str,
        row: &OrgRow,
    ) -> Result<wire::GetOrgSettingsResponse, Status> {
        let inst = self.hub.instance().await.map_err(fail)?;
        let count = self.hub.agent_count(org).await.map_err(fail)?;
        let gate = self.hub.agent_gate(org).await.map_err(fail)?;
        Ok(wire::GetOrgSettingsResponse {
            agents: Some(wire::OrgAgentPolicy {
                allow_agents: row.agents.allow_agents,
                agent_changesets_need_person: row.agents.need_person,
                monthly_spend_ceiling_micro_usd: row.agents.ceiling,
            }),
            merge: Some(wire_merge(
                &row.merge.clone().unwrap_or_default(),
                row.merge.is_some(),
            )),
            agent_count: u32::try_from(count).unwrap_or(u32::MAX),
            updated: row.updated,
            updated_by: row.updated_by.clone(),
            instance_allows_agents: inst.allow_agents,
            instance_default_budget_micro_usd: inst.default_budget,
            effective_spend_ceiling_micro_usd: gate.ceiling,
        })
    }

    async fn audit(
        &self,
        action: &str,
        actor: &Actor,
        org: &str,
        target: &str,
        details: &[(&str, String)],
    ) {
        let details = details.to_vec();
        self.hub
            .audit
            .record(Entry {
                owner: org,
                target,
                on_behalf: actor.relay.as_deref(),
                details: &details,
                ..Entry::new(action, &actor.id)
            })
            .await;
    }
}

#[tonic::async_trait]
impl OrgSettings for OrgService {
    async fn get_org_settings(
        &self,
        request: Request<wire::GetOrgSettingsRequest>,
    ) -> Result<Response<wire::GetOrgSettingsResponse>, Status> {
        let (meta, ext, msg) = request.into_parts();
        let (s, _) = self.org(&ext, &meta, &msg.org, Role::Read).await?;
        let org = s.audit_owner().to_owned();
        let row = self.hub.row(&org).await.map_err(fail)?;
        Ok(Response::new(self.settings(&org, &row).await?))
    }

    async fn set_agent_policy(
        &self,
        request: Request<wire::SetOrgAgentPolicyRequest>,
    ) -> Result<Response<wire::SetOrgAgentPolicyResponse>, Status> {
        let (meta, ext, msg) = request.into_parts();
        let (s, actor) = self.org(&ext, &meta, &msg.org, Role::Admin).await?;
        let op = op_id(&meta)?;
        let w = msg
            .agents
            .ok_or_else(|| Status::invalid_argument("missing agents"))?;
        let org = s.audit_owner().to_owned();
        let hub = self.hub.clone();
        once(
            &hub.ops,
            op,
            &actor.replay_key,
            "OrgSettings/SetAgentPolicy",
            true,
            || async {
                let old = hub.row(&org).await.map(|r| r.agents).unwrap_or_default();
                let row = hub
                    .set_agents(
                        &org,
                        AgentPolicy {
                            allow_agents: w.allow_agents,
                            need_person: w.agent_changesets_need_person,
                            ceiling: w.monthly_spend_ceiling_micro_usd,
                        },
                        &actor.id,
                    )
                    .await
                    .map_err(fail)?;
                self.audit(
                    "org.agents.updated",
                    &actor,
                    &org,
                    &org,
                    &[
                        ("allow_agents", w.allow_agents.to_string()),
                        ("need_person", w.agent_changesets_need_person.to_string()),
                        ("ceiling", w.monthly_spend_ceiling_micro_usd.to_string()),
                        ("old_allow_agents", old.allow_agents.to_string()),
                        ("old_need_person", old.need_person.to_string()),
                        ("old_ceiling", old.ceiling.to_string()),
                    ],
                )
                .await;
                Ok(wire::SetOrgAgentPolicyResponse {
                    settings: Some(self.settings(&org, &row).await?),
                })
            },
        )
        .await
        .map(Response::new)
    }

    async fn set_merge_defaults(
        &self,
        request: Request<wire::SetOrgMergeDefaultsRequest>,
    ) -> Result<Response<wire::SetOrgMergeDefaultsResponse>, Status> {
        let (meta, ext, msg) = request.into_parts();
        let (s, actor) = self.org(&ext, &meta, &msg.org, Role::Admin).await?;
        let op = op_id(&meta)?;
        let w = msg
            .merge
            .ok_or_else(|| Status::invalid_argument("missing merge defaults"))?;
        let org = s.audit_owner().to_owned();
        let hub = self.hub.clone();
        once(
            &hub.ops,
            op,
            &actor.replay_key,
            "OrgSettings/SetMergeDefaults",
            true,
            || async {
                let d = merge_of(&w);
                let row = hub
                    .set_merge(&org, d.clone(), &actor.id)
                    .await
                    .map_err(fail)?;
                self.audit(
                    "org.merge.updated",
                    &actor,
                    &org,
                    &org,
                    &[
                        ("approvals", d.require_approvals.to_string()),
                        ("checks", d.require_checks.len().to_string()),
                        ("strategy", d.strategy.clone()),
                    ],
                )
                .await;
                Ok(wire::SetOrgMergeDefaultsResponse {
                    settings: Some(self.settings(&org, &row).await?),
                })
            },
        )
        .await
        .map(Response::new)
    }

    async fn resolve_merge_policy(
        &self,
        request: Request<wire::ResolveMergePolicyRequest>,
    ) -> Result<Response<wire::ResolveMergePolicyResponse>, Status> {
        let (meta, ext, msg) = request.into_parts();
        let (s, _) = self.org(&ext, &meta, &msg.org, Role::Read).await?;
        let org = s.audit_owner().to_owned();
        // The repository must belong to the organisation named: nothing else is resolved here.
        let repo = msg.repo.to_ascii_lowercase();
        if repo
            .strip_prefix(&format!("{org}/"))
            .is_none_or(|n| n.is_empty() || n.contains('/'))
        {
            return Err(Status::invalid_argument("repo is <organisation>/<name>"));
        }
        let own = msg.repo_values.as_ref().map(merge_of).unwrap_or_default();
        own.validate().map_err(Status::invalid_argument)?;
        let defaults = self.hub.merge_defaults(&org).await;
        let (effective, tightened) = match &defaults {
            Some(d) => policy::tighten(d, &own),
            None => (own, Vec::new()),
        };
        Ok(Response::new(wire::ResolveMergePolicyResponse {
            effective: Some(wire_merge(&effective, true)),
            tightened: tightened.into_iter().map(str::to_owned).collect(),
            org: Some(wire_merge(
                &defaults.clone().unwrap_or_default(),
                defaults.is_some(),
            )),
        }))
    }

    async fn list_agents(
        &self,
        request: Request<wire::ListOrgAgentsRequest>,
    ) -> Result<Response<wire::ListOrgAgentsResponse>, Status> {
        let (meta, ext, msg) = request.into_parts();
        let (s, _) = self.org(&ext, &meta, &msg.org, Role::Read).await?;
        let agents = self.hub.agents(s.audit_owner()).await.map_err(fail)?;
        Ok(Response::new(wire::ListOrgAgentsResponse {
            agents: agents.iter().map(wire_agent).collect(),
        }))
    }

    async fn create_agent(
        &self,
        request: Request<wire::CreateOrgAgentRequest>,
    ) -> Result<Response<wire::CreateOrgAgentResponse>, Status> {
        let (meta, ext, msg) = request.into_parts();
        let (s, actor) = self.org(&ext, &meta, &msg.org, Role::Admin).await?;
        let op = op_id(&meta)?;
        let w = msg
            .agent
            .ok_or_else(|| Status::invalid_argument("missing agent"))?;
        let org = s.audit_owner().to_owned();
        let hub = self.hub.clone();
        once(
            &hub.ops,
            op,
            &actor.replay_key,
            "OrgSettings/CreateAgent",
            true,
            || async {
                let row = AgentRow {
                    name: w.name,
                    display: w.display_name,
                    run_by: w.run_by,
                    paths: w.paths,
                    repos: w.repos,
                    paused: false,
                    created: 0,
                    created_by: String::new(),
                    updated: 0,
                };
                let a = hub.create_agent(&org, row, &actor.id).await.map_err(fail)?;
                self.audit(
                    "org.agent.created",
                    &actor,
                    &org,
                    &a.name,
                    &[("run_by", a.run_by.clone())],
                )
                .await;
                Ok(wire::CreateOrgAgentResponse {
                    agent: Some(wire_agent(&a)),
                })
            },
        )
        .await
        .map(Response::new)
    }

    async fn update_agent(
        &self,
        request: Request<wire::UpdateOrgAgentRequest>,
    ) -> Result<Response<wire::UpdateOrgAgentResponse>, Status> {
        let (meta, ext, msg) = request.into_parts();
        let (s, actor) = self.org(&ext, &meta, &msg.org, Role::Admin).await?;
        let op = op_id(&meta)?;
        let org = s.audit_owner().to_owned();
        let hub = self.hub.clone();
        once(
            &hub.ops,
            op,
            &actor.replay_key,
            "OrgSettings/UpdateAgent",
            true,
            || async {
                let mut a = hub.agent(&org, &msg.name).await.map_err(fail)?;
                if let Some(d) = msg.display_name {
                    a.display = d;
                }
                if let Some(r) = msg.run_by {
                    a.run_by = r.to_ascii_lowercase();
                }
                if let Some(st) = msg.state {
                    a.paused = wire::OrgAgentState::try_from(st) == Ok(wire::OrgAgentState::Paused);
                }
                if msg.replace_paths {
                    a.paths = msg.paths;
                }
                if msg.replace_repos {
                    a.repos = msg.repos.iter().map(|r| r.to_ascii_lowercase()).collect();
                }
                let a = hub.update_agent(&org, a).await.map_err(fail)?;
                self.audit(
                    "org.agent.updated",
                    &actor,
                    &org,
                    &a.name,
                    &[("paused", a.paused.to_string())],
                )
                .await;
                Ok(wire::UpdateOrgAgentResponse {
                    agent: Some(wire_agent(&a)),
                })
            },
        )
        .await
        .map(Response::new)
    }

    async fn delete_agent(
        &self,
        request: Request<wire::DeleteOrgAgentRequest>,
    ) -> Result<Response<wire::DeleteOrgAgentResponse>, Status> {
        let (meta, ext, msg) = request.into_parts();
        let (s, actor) = self.org(&ext, &meta, &msg.org, Role::Admin).await?;
        let op = op_id(&meta)?;
        let org = s.audit_owner().to_owned();
        let hub = self.hub.clone();
        once(
            &hub.ops,
            op,
            &actor.replay_key,
            "OrgSettings/DeleteAgent",
            true,
            || async {
                hub.delete_agent(&org, &msg.name).await.map_err(fail)?;
                self.audit("org.agent.deleted", &actor, &org, &msg.name, &[])
                    .await;
                Ok(wire::DeleteOrgAgentResponse {})
            },
        )
        .await
        .map(Response::new)
    }

    async fn get_instance_agents(
        &self,
        request: Request<wire::GetInstanceAgentsRequest>,
    ) -> Result<Response<wire::GetInstanceAgentsResponse>, Status> {
        let (meta, ext, _) = request.into_parts();
        let caller = self.gate.caller(&ext).await?;
        self.gate
            .require(&caller, &meta, &Subject::Instance, Role::Admin)?;
        let i = self.hub.instance().await.map_err(fail)?;
        Ok(Response::new(wire::GetInstanceAgentsResponse {
            allow_agents: i.allow_agents,
            default_budget_micro_usd: i.default_budget,
        }))
    }

    async fn set_instance_agents(
        &self,
        request: Request<wire::SetInstanceAgentsRequest>,
    ) -> Result<Response<wire::SetInstanceAgentsResponse>, Status> {
        let (meta, ext, msg) = request.into_parts();
        let caller = self.gate.caller(&ext).await?;
        let actor = self
            .gate
            .require(&caller, &meta, &Subject::Instance, Role::Admin)?;
        let op = op_id(&meta)?;
        let hub = self.hub.clone();
        once(
            &hub.ops,
            op,
            &actor.replay_key,
            "OrgSettings/SetInstanceAgents",
            true,
            || async {
                let old = hub.instance().await.map_err(fail)?;
                let i = hub
                    .set_instance(
                        InstanceAgents {
                            allow_agents: msg.allow_agents,
                            default_budget: msg.default_budget_micro_usd,
                        },
                        &actor.id,
                    )
                    .await
                    .map_err(fail)?;
                self.audit(
                    "instance.agents.updated",
                    &actor,
                    "",
                    "instance",
                    &[
                        ("allow_agents", i.allow_agents.to_string()),
                        ("ceiling", i.default_budget.to_string()),
                        ("old_allow_agents", old.allow_agents.to_string()),
                        ("old_ceiling", old.default_budget.to_string()),
                    ],
                )
                .await;
                Ok(wire::SetInstanceAgentsResponse {
                    settings: Some(wire::GetInstanceAgentsResponse {
                        allow_agents: i.allow_agents,
                        default_budget_micro_usd: i.default_budget,
                    }),
                })
            },
        )
        .await
        .map(Response::new)
    }
}
