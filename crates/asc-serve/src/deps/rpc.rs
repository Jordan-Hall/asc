//! The `DepsService` tonic service.
//!
//! Authorisation, call by call (the server's authorizer decides before a handler runs; a handler that
//! cannot decide refuses):
//!
//! | call | needs |
//! |---|---|
//! | `ListFindings`, `ListUpdates`, `GetConfig`, `ScanStatus`, `Sbom`, `Status`, `Statuses` | read |
//! | `Scan`, `ProposeUpdate`, `SetConfig`, `Dismiss` | write, and a maintainer of the policy or repository admin |
//!
//! An agent credential reaches none of them (`agents::authz` closes every service it does not name).
//! A proposal is a change on a draft ref under `agent/deps/`; landing it is a person's decision.

use std::sync::Arc;

use asc_core::policy::{Kind, Who};
use asc_rpc::IdMode;
use asc_rpc::auth::{Access, AdminProof, AuthInterceptor, Authorizer, Credential, Principal};
use asc_rpc::forwarded::{Claim, Role};
use asc_rpc::server::{CallCtx, MAX_MESSAGE, RepoName, ServerInfo, wire};
use asc_rpc::services::deps_service_server::{DepsService, DepsServiceServer};
use tonic::service::interceptor::InterceptedService;
use tonic::{Code, Request, Response, Status};

use super::hub::{DepsHub, MAX_STATUSES};
use super::status::{RepoStatus, Verdict};
use crate::agents::repo::PolicyState;
use crate::authz::claim::{HostCall, classify, subject};
use crate::review::{Env, handle_of};

pub struct DepsRpc {
    hub: Arc<DepsHub>,
    env: Env,
}

impl DepsRpc {
    pub fn new(hub: Arc<DepsHub>, authorizer: Arc<dyn Authorizer>, info: ServerInfo) -> Self {
        Self {
            hub,
            env: Env { authorizer, info },
        }
    }

    pub fn into_service(self) -> InterceptedService<DepsServiceServer<Self>, AuthInterceptor> {
        let server = DepsServiceServer::new(self)
            .max_decoding_message_size(MAX_MESSAGE)
            .max_encoding_message_size(MAX_MESSAGE)
            .accept_compressed(tonic::codec::CompressionEncoding::Zstd);
        InterceptedService::new(server, AuthInterceptor)
    }

    /// Whether the caller maintains the repository: an administrator, or a maintainer of the policy.
    async fn require_maintainer(
        &self,
        ext: &tonic::Extensions,
        ctx: &CallCtx,
    ) -> Result<(), Status> {
        let denied = || {
            Status::permission_denied(
                "only a maintainer of the repository policy or an administrator can do this",
            )
        };
        if self.env.has(ext, ctx, Access::Admin).await {
            return Ok(());
        }
        let handle = handle_of(&ctx.principal.id).ok_or_else(denied)?;
        let entry = self.hub.entry(ctx).await?;
        match &*self.hub.view.policy(&entry).await? {
            PolicyState::Valid(p) if p.is_maintainer(&Who::new(Kind::User, handle)) => Ok(()),
            _ => Err(denied()),
        }
    }
}

/// The wire form of a status.
fn status_wire(s: RepoStatus) -> wire::DepStatusResponse {
    let status = match s.verdict {
        Verdict::Unknown => wire::DepStatus::Unknown,
        Verdict::Clean => wire::DepStatus::Clean,
        Verdict::Findings => wire::DepStatus::Findings,
    };
    wire::DepStatusResponse {
        status: status as i32,
        open_by_severity: s.open_by_severity.into_iter().collect(),
        moves: s
            .moves
            .into_iter()
            .map(|m| wire::DepMove {
                ecosystem: m.ecosystem,
                name: m.name,
                from: m.from,
                to: m.to,
            })
            .collect(),
        scanned_at: s.scanned_at,
        scanned: s.scanned.into(),
        reason: s.reason.text().to_owned(),
    }
}

/// Longest owner name.
const MAX_OWNER: usize = 64;

/// The claim subject of the owner of a `Statuses` call, or `None` for a call without an owner. The
/// owner and its kind are checked, and every repository must be one of that owner's (compared lower
/// case); this holds whoever the caller is, so a claim for one owner never reaches another's.
fn owner_subject(req: &wire::DepStatusesRequest) -> Result<Option<String>, Status> {
    let kind = wire::OwnerKind::try_from(req.kind).unwrap_or(wire::OwnerKind::Unspecified);
    if req.owner.is_empty() && kind == wire::OwnerKind::Unspecified {
        return Ok(None);
    }
    let owner = req.owner.to_ascii_lowercase();
    let owner_ok = !owner.is_empty()
        && owner.len() <= MAX_OWNER
        && !owner.starts_with(['.', '-'])
        && owner
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'));
    if !owner_ok {
        return Err(Status::invalid_argument(
            "an owner is 1 to 64 characters of [A-Za-z0-9._-]",
        ));
    }
    let subject = match kind {
        wire::OwnerKind::Org => subject::org(&owner),
        wire::OwnerKind::User => subject::user(&owner),
        _ => return Err(Status::invalid_argument("the owner is a user or an org")),
    };
    for repo in &req.repos {
        let name = RepoName::parse(&repo.name)?;
        let lower = name.as_str().to_ascii_lowercase();
        if lower.split_once('/').map(|(o, _)| o) != Some(owner.as_str()) {
            return Err(Status::invalid_argument(
                "every repository is one of the owner's",
            ));
        }
    }
    Ok(Some(subject))
}

/// The context of one repository of a call made on a verified owner claim: the person of the claim
/// acts, with the claim as the whole of the authority.
fn claimed_context(claim: &Claim, repo: &wire::RepoRef) -> Result<CallCtx, Status> {
    Ok(CallCtx {
        repo: RepoName::parse(&repo.name)?,
        id_mode: IdMode::from_wire(repo.id_mode)
            .map_err(|e| Status::invalid_argument(e.to_string()))?,
        principal: Principal {
            id: claim.user.clone(),
        },
        op_id: None,
        client: None,
        admin: AdminProof::default().with_forwarded(Some(claim.clone())),
    })
}

#[tonic::async_trait]
impl DepsService for DepsRpc {
    async fn scan(
        &self,
        request: Request<wire::ScanDepsRequest>,
    ) -> Result<Response<wire::ScanDepsResponse>, Status> {
        let (meta, ext, msg) = request.into_parts();
        let ctx = self
            .env
            .context(&meta, &ext, msg.repo.as_ref(), Access::Write, false)
            .await?;
        self.require_maintainer(&ext, &ctx).await?;
        self.hub.scan(&ctx, msg).await.map(Response::new)
    }

    async fn scan_status(
        &self,
        request: Request<wire::DepScanStatusRequest>,
    ) -> Result<Response<wire::DepScanStatusResponse>, Status> {
        let (meta, ext, msg) = request.into_parts();
        let ctx = self
            .env
            .context(&meta, &ext, msg.repo.as_ref(), Access::Read, false)
            .await?;
        self.hub.scan_status(&ctx).await.map(Response::new)
    }

    async fn sbom(
        &self,
        request: Request<wire::SbomRequest>,
    ) -> Result<Response<wire::SbomResponse>, Status> {
        let (meta, ext, msg) = request.into_parts();
        let ctx = self
            .env
            .context(&meta, &ext, msg.repo.as_ref(), Access::Read, false)
            .await?;
        self.hub.sbom(&ctx, msg).await.map(Response::new)
    }

    async fn status(
        &self,
        request: Request<wire::DepStatusRequest>,
    ) -> Result<Response<wire::DepStatusResponse>, Status> {
        let (meta, ext, msg) = request.into_parts();
        let ctx = self
            .env
            .context(&meta, &ext, msg.repo.as_ref(), Access::Read, false)
            .await?;
        self.hub
            .status_for(&ctx, &msg.change)
            .await
            .map(|s| Response::new(status_wire(s)))
    }

    async fn statuses(
        &self,
        request: Request<wire::DepStatusesRequest>,
    ) -> Result<Response<wire::DepStatusesResponse>, Status> {
        let (meta, ext, msg) = request.into_parts();
        if msg.repos.len() > MAX_STATUSES {
            return Err(Status::invalid_argument("too many repositories"));
        }
        // With an owner, the host service's read claim for the owner covers every repository of it.
        let claim = match owner_subject(&msg)? {
            None => None,
            Some(subject) => {
                let id = self
                    .env
                    .authorizer
                    .authenticate(ext.get::<Credential>())
                    .await
                    .map(|p| p.id)
                    .unwrap_or_default();
                match classify(&*self.env.authorizer, &id, &meta, &subject) {
                    HostCall::Person(c) if c.allows(Role::Read) => Some(c),
                    HostCall::Person(_) | HostCall::Refused => {
                        return Err(Status::permission_denied(
                            "the claim the host service forwarded does not cover this call",
                        ));
                    }
                    HostCall::System | HostCall::NotHost => None,
                }
            }
        };
        let mut allowed = Vec::with_capacity(msg.repos.len());
        for repo in msg.repos {
            let made = match &claim {
                Some(c) => claimed_context(c, &repo),
                None => {
                    self.env
                        .context(&meta, &ext, Some(&repo), Access::Read, false)
                        .await
                }
            };
            match made {
                Ok(ctx) => allowed.push((repo, ctx)),
                // Not readable and not there look alike: the repository is left out.
                Err(e) if matches!(e.code(), Code::PermissionDenied | Code::NotFound) => {}
                Err(e) => return Err(e),
            }
        }
        let statuses = self
            .hub
            .statuses(allowed)
            .await?
            .into_iter()
            .map(|(repo, s)| {
                let w = status_wire(s);
                wire::DepRepoStatus {
                    repo: Some(repo),
                    status: w.status,
                    open_by_severity: w.open_by_severity,
                    scanned_at: w.scanned_at,
                    scanned: w.scanned,
                    reason: w.reason,
                }
            })
            .collect();
        Ok(Response::new(wire::DepStatusesResponse { statuses }))
    }

    async fn list_findings(
        &self,
        request: Request<wire::ListDepFindingsRequest>,
    ) -> Result<Response<wire::ListDepFindingsResponse>, Status> {
        let (meta, ext, msg) = request.into_parts();
        let ctx = self
            .env
            .context(&meta, &ext, msg.repo.as_ref(), Access::Read, false)
            .await?;
        self.hub.list_findings(&ctx, msg).await.map(Response::new)
    }

    async fn list_updates(
        &self,
        request: Request<wire::ListDepUpdatesRequest>,
    ) -> Result<Response<wire::ListDepUpdatesResponse>, Status> {
        let (meta, ext, msg) = request.into_parts();
        let ctx = self
            .env
            .context(&meta, &ext, msg.repo.as_ref(), Access::Read, false)
            .await?;
        self.hub.list_updates(&ctx, msg).await.map(Response::new)
    }

    async fn propose_update(
        &self,
        request: Request<wire::ProposeDepUpdateRequest>,
    ) -> Result<Response<wire::ProposeDepUpdateResponse>, Status> {
        let (meta, ext, msg) = request.into_parts();
        let ctx = self
            .env
            .context(&meta, &ext, msg.repo.as_ref(), Access::Write, !msg.dry_run)
            .await?;
        self.require_maintainer(&ext, &ctx).await?;
        self.hub.propose_update(&ctx, msg).await.map(Response::new)
    }

    async fn set_config(
        &self,
        request: Request<wire::SetDepsConfigRequest>,
    ) -> Result<Response<wire::SetDepsConfigResponse>, Status> {
        let (meta, ext, msg) = request.into_parts();
        let ctx = self
            .env
            .context(&meta, &ext, msg.repo.as_ref(), Access::Write, !msg.dry_run)
            .await?;
        self.require_maintainer(&ext, &ctx).await?;
        self.hub.set_config(&ctx, msg).await.map(Response::new)
    }

    async fn get_config(
        &self,
        request: Request<wire::GetDepsConfigRequest>,
    ) -> Result<Response<wire::GetDepsConfigResponse>, Status> {
        let (meta, ext, msg) = request.into_parts();
        let ctx = self
            .env
            .context(&meta, &ext, msg.repo.as_ref(), Access::Read, false)
            .await?;
        self.hub.get_config(&ctx, msg).await.map(Response::new)
    }

    async fn dismiss(
        &self,
        request: Request<wire::DismissDepFindingRequest>,
    ) -> Result<Response<wire::DismissDepFindingResponse>, Status> {
        let (meta, ext, msg) = request.into_parts();
        let ctx = self
            .env
            .context(&meta, &ext, msg.repo.as_ref(), Access::Write, true)
            .await?;
        self.require_maintainer(&ext, &ctx).await?;
        self.hub.dismiss(&ctx, msg).await.map(Response::new)
    }
}
