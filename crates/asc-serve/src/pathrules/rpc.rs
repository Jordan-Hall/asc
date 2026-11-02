//! The `PathRules` tonic service.

use std::sync::Arc;

use asc_core::pathacl::{Level, Rule};
use asc_rpc::auth::{Access, AuthInterceptor, Authorizer};
use asc_rpc::forwarded::Role;
use asc_rpc::server::{MAX_MESSAGE, ServerInfo, wire};
use asc_rpc::services::path_rules_server::{PathRules, PathRulesServer};
use tonic::metadata::MetadataMap;
use tonic::service::interceptor::InterceptedService;
use tonic::{Request, Response, Status};

use super::{MAX_CHECK_PATHS, PathRulesHub, RulesError, parse_rule};
use crate::ai::audit::Entry;
use crate::ai::caller::{once, op_id};
use crate::settings::access::{Actor, Gate, Subject};

/// The `asc.v1.PathRules` service.
pub struct PathRulesService {
    hub: Arc<PathRulesHub>,
    gate: Gate,
    authorizer: Arc<dyn Authorizer>,
}

fn fail(e: RulesError) -> Status {
    e.status()
}

fn wire_rule(r: &Rule) -> wire::PathRule {
    wire::PathRule {
        dir: r.dir.clone(),
        subject: r.subject.render(),
        level: r.level.as_str().to_owned(),
    }
}

/// A viewer principal of `CheckPathAccess`: `user:<login>`, `agent:<name>`, `ci:<job>` or
/// `deploy:<key>`.
fn parse_principal(s: &str) -> Result<asc_core::policy::Who, Status> {
    use asc_core::policy::{Kind, Who};
    let bad = || Status::invalid_argument("principal is user:, agent:, ci: or deploy: and a name");
    let (k, name) = s.split_once(':').ok_or_else(bad)?;
    if !super::name_ok(name) {
        return Err(bad());
    }
    let kind = match k {
        "user" => Kind::User,
        "agent" => Kind::Agent,
        "ci" => Kind::Ci,
        "deploy" => Kind::Deploy,
        _ => return Err(bad()),
    };
    Ok(Who::new(kind, name))
}

impl PathRulesService {
    pub fn new(hub: Arc<PathRulesHub>, authorizer: Arc<dyn Authorizer>, _info: ServerInfo) -> Self {
        Self {
            hub,
            gate: Gate::new(authorizer.clone()),
            authorizer,
        }
    }

    pub fn into_service(self) -> InterceptedService<PathRulesServer<Self>, AuthInterceptor> {
        let server = PathRulesServer::new(self)
            .max_decoding_message_size(MAX_MESSAGE)
            .max_encoding_message_size(MAX_MESSAGE)
            .accept_compressed(tonic::codec::CompressionEncoding::Zstd);
        InterceptedService::new(server, AuthInterceptor)
    }

    /// The repository (`owner/name`, lower case) and the administrator acting on it: the hub's
    /// claim for the repository with role admin, or a credential with admin access to it.
    async fn repo_admin(
        &self,
        ext: &tonic::Extensions,
        meta: &MetadataMap,
        repo: &str,
    ) -> Result<(String, Actor), Status> {
        let subject = Subject::repo(repo)?;
        let Subject::Repo(name) = subject.clone() else {
            return Err(Gate::denied());
        };
        let caller = self.gate.caller(ext).await?;
        if let Some(actor) = self.gate.allow(&caller, meta, &subject, Role::Admin) {
            return Ok((name, actor));
        }
        if crate::authz::is_host_service(&caller.principal.id) {
            return Err(Gate::denied());
        }
        let p = self
            .authorizer
            .authorize_credential(caller.credential.as_ref(), &name, Access::Admin)
            .await
            .map_err(|_| Gate::denied())?;
        Ok((
            name,
            Actor {
                replay_key: p.id.clone(),
                handle: crate::review::handle_of(&p.id),
                id: p.id,
                relay: None,
                system: false,
            },
        ))
    }

    /// The repository and the caller's own viewer identity and level on it, the highest of
    /// admin, write and read it holds (the hub's claim for a person, else its credential).
    async fn repo_reader(
        &self,
        ext: &tonic::Extensions,
        meta: &MetadataMap,
        repo: &str,
    ) -> Result<(String, asc_core::policy::Who, Level), Status> {
        use asc_core::policy::{Kind, Who};
        let subject = Subject::repo(repo)?;
        let Subject::Repo(name) = subject.clone() else {
            return Err(Gate::denied());
        };
        let caller = self.gate.caller(ext).await?;
        let levels = [
            (Role::Admin, Access::Admin, Level::Admin),
            (Role::Write, Access::Write, Level::Write),
            (Role::Read, Access::Read, Level::Read),
        ];
        if crate::authz::is_host_service(&caller.principal.id) {
            for (role, _, level) in levels {
                if let Some(actor) = self.gate.allow(&caller, meta, &subject, role) {
                    // The host's own work has no person, so no rule can name it.
                    let handle = actor.handle.ok_or_else(Gate::denied)?;
                    return Ok((name, Who::new(Kind::User, &handle), level));
                }
            }
            return Err(Gate::denied());
        }
        for (_, access, level) in levels {
            if self
                .authorizer
                .authorize_credential(caller.credential.as_ref(), &name, access)
                .await
                .is_ok()
            {
                return Ok((name, crate::policy::who_of(&caller.principal.id), level));
            }
        }
        Err(Gate::denied())
    }

    /// The organisation and the administrator acting on it (the hub's claim, role admin).
    async fn org_admin(
        &self,
        ext: &tonic::Extensions,
        meta: &MetadataMap,
        org: &str,
    ) -> Result<(String, Actor), Status> {
        let subject = Subject::org(org)?;
        let Subject::Org(name) = subject.clone() else {
            return Err(Gate::denied());
        };
        let caller = self.gate.caller(ext).await?;
        let actor = self.gate.require(&caller, meta, &subject, Role::Admin)?;
        Ok((name, actor))
    }
}

#[tonic::async_trait]
impl PathRules for PathRulesService {
    async fn list_path_rules(
        &self,
        request: Request<wire::ListPathRulesRequest>,
    ) -> Result<Response<wire::ListPathRulesResponse>, Status> {
        let (meta, ext, msg) = request.into_parts();
        let (repo, _) = self.repo_admin(&ext, &meta, &msg.repo).await?;
        let set = self.hub.get(&repo).await.map_err(fail)?;
        Ok(Response::new(wire::ListPathRulesResponse {
            rules: set.rules.iter().map(wire_rule).collect(),
            revision: set.revision,
            updated: set.updated,
            updated_by: set.updated_by,
        }))
    }

    async fn set_path_rules(
        &self,
        request: Request<wire::SetPathRulesRequest>,
    ) -> Result<Response<wire::SetPathRulesResponse>, Status> {
        let (meta, ext, msg) = request.into_parts();
        let (repo, actor) = self.repo_admin(&ext, &meta, &msg.repo).await?;
        let op = op_id(&meta)?;
        let mut rules = Vec::with_capacity(msg.rules.len());
        for (i, r) in msg.rules.iter().enumerate() {
            rules.push(
                parse_rule(&r.dir, &r.subject, &r.level)
                    .map_err(|e| match e {
                        RulesError::Invalid(m) => {
                            RulesError::Invalid(format!("rule {}: {m}", i + 1))
                        }
                        e => e,
                    })
                    .map_err(fail)?,
            );
        }
        let hub = self.hub.clone();
        once(
            &hub.ops,
            op,
            &actor.replay_key,
            "PathRules/SetPathRules",
            true,
            || async {
                let revision = hub
                    .set(&repo, &rules, msg.expected_revision, &actor.id)
                    .await
                    .map_err(fail)?;
                hub.audit
                    .record(Entry {
                        repo: &repo,
                        target: &repo,
                        on_behalf: actor.relay.as_deref(),
                        details: &[
                            ("revision", revision.to_string()),
                            ("rules", rules.len().to_string()),
                        ],
                        ..Entry::new("pathrules.set", &actor.id)
                    })
                    .await;
                Ok(wire::SetPathRulesResponse { revision })
            },
        )
        .await
        .map(Response::new)
    }

    async fn check_path_access(
        &self,
        request: Request<wire::CheckPathAccessRequest>,
    ) -> Result<Response<wire::CheckPathAccessResponse>, Status> {
        let (meta, ext, msg) = request.into_parts();
        let (repo, _) = self.repo_admin(&ext, &meta, &msg.repo).await?;
        if msg.paths.len() > MAX_CHECK_PATHS {
            return Err(Status::invalid_argument(format!(
                "at most {MAX_CHECK_PATHS} paths per call"
            )));
        }
        let who = parse_principal(&msg.principal)?;
        let repo_level = match Level::parse(&msg.repo_level) {
            Some(l @ (Level::None | Level::Read | Level::Write | Level::Admin)) => l,
            _ => {
                return Err(Status::invalid_argument(
                    "repo_level is none, read, write or admin",
                ));
            }
        };
        let acl = self.hub.acl(&repo).await.map_err(fail)?;
        let viewer = self
            .hub
            .viewer(&repo, who, repo_level)
            .await
            .map_err(fail)?;
        let mut access = Vec::with_capacity(msg.paths.len());
        for p in &msg.paths {
            let path = p.trim_matches('/');
            if path.len() > asc_core::pathacl::MAX_DIR_LEN * 4 {
                return Err(Status::invalid_argument("a path is too long"));
            }
            let (level, by) = acl.decide(&viewer, path.as_bytes());
            access.push(wire::PathAccess {
                path: p.clone(),
                level: level.as_str().to_owned(),
                decided_by: by
                    .map(|d| String::from_utf8_lossy(d).into_owned())
                    .unwrap_or_default(),
            });
        }
        Ok(Response::new(wire::CheckPathAccessResponse {
            access,
            teams: viewer.teams,
        }))
    }

    async fn get_my_path_access(
        &self,
        request: Request<wire::GetMyPathAccessRequest>,
    ) -> Result<Response<wire::GetMyPathAccessResponse>, Status> {
        let (meta, ext, msg) = request.into_parts();
        let (repo, who, repo_level) = self.repo_reader(&ext, &meta, &msg.repo).await?;
        let set = self.hub.get(&repo).await.map_err(fail)?;
        let acl = self.hub.acl(&repo).await.map_err(fail)?;
        let viewer = self
            .hub
            .viewer(&repo, who, repo_level)
            .await
            .map_err(fail)?;
        let dirs = acl
            .decided_dirs(&viewer)
            .into_iter()
            .map(|(dir, level)| {
                let dir = String::from_utf8_lossy(&dir).into_owned();
                wire::PathAccess {
                    path: dir.clone(),
                    level: level.as_str().to_owned(),
                    decided_by: dir,
                }
            })
            .collect();
        Ok(Response::new(wire::GetMyPathAccessResponse {
            repo_level: repo_level.as_str().to_owned(),
            dirs,
            revision: set.revision,
        }))
    }

    async fn list_teams(
        &self,
        request: Request<wire::ListTeamsRequest>,
    ) -> Result<Response<wire::ListTeamsResponse>, Status> {
        let (meta, ext, msg) = request.into_parts();
        let (org, _) = self.org_admin(&ext, &meta, &msg.org).await?;
        let teams = self.hub.teams(&org).await.map_err(fail)?;
        Ok(Response::new(wire::ListTeamsResponse {
            teams: teams
                .into_iter()
                .map(|(name, members)| wire::Team { name, members })
                .collect(),
        }))
    }

    async fn set_team_members(
        &self,
        request: Request<wire::SetTeamMembersRequest>,
    ) -> Result<Response<wire::SetTeamMembersResponse>, Status> {
        let (meta, ext, msg) = request.into_parts();
        let (org, actor) = self.org_admin(&ext, &meta, &msg.org).await?;
        let op = op_id(&meta)?;
        let hub = self.hub.clone();
        once(
            &hub.ops,
            op,
            &actor.replay_key,
            "PathRules/SetTeamMembers",
            true,
            || async {
                let n = hub
                    .set_team(&org, &msg.team, &msg.members)
                    .await
                    .map_err(fail)?;
                hub.audit
                    .record(Entry {
                        owner: &org,
                        target: &msg.team,
                        on_behalf: actor.relay.as_deref(),
                        details: &[("members", n.to_string()), ("team", msg.team.clone())],
                        ..Entry::new("pathrules.team_set", &actor.id)
                    })
                    .await;
                Ok(wire::SetTeamMembersResponse {})
            },
        )
        .await
        .map(Response::new)
    }
}
