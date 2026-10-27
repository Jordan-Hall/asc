//! The `DepsService` tonic service.
//!
//! Authorisation, call by call (the server's authorizer decides before a handler runs; a handler that
//! cannot decide refuses):
//!
//! | call | needs |
//! |---|---|
//! | `ListFindings`, `ListUpdates`, `GetConfig` | read |
//! | `Scan`, `ProposeUpdate`, `SetConfig`, `Dismiss` | write, and a maintainer of the policy or repository admin |
//!
//! An agent credential reaches none of them (`agents::authz` closes every service it does not name).
//! A proposal is a change on a draft ref under `agent/deps/`; landing it is a person's decision.

use std::sync::Arc;

use asc_core::policy::{Kind, Who};
use asc_rpc::auth::{Access, AuthInterceptor, Authorizer};
use asc_rpc::server::{CallCtx, MAX_MESSAGE, ServerInfo, wire};
use asc_rpc::services::deps_service_server::{DepsService, DepsServiceServer};
use tonic::service::interceptor::InterceptedService;
use tonic::{Request, Response, Status};

use super::hub::DepsHub;
use crate::agents::repo::PolicyState;
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
