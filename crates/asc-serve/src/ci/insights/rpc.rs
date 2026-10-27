//! The `CiInsights` tonic service. Reading needs repository read, reporting needs repository admin
//! (the runner's service credential), changing a group needs repository write, and quarantining
//! a maintainer: repository admin, or a member of the policy role `maintainer`.

use std::sync::Arc;

use asc_rpc::auth::{Access, AuthInterceptor, Authorizer};
use asc_rpc::server::{CallCtx, MAX_MESSAGE, ServerInfo, wire};
use asc_rpc::services::ci_insights_server::{CiInsights, CiInsightsServer};
use tonic::service::interceptor::InterceptedService;
use tonic::{Request, Response, Status};

use super::Insights;
use crate::review::Env;

/// The `asc.v1.CiInsights` tonic service.
pub struct CiInsightsService {
    hub: Arc<Insights>,
    env: Env,
}

impl CiInsightsService {
    pub fn new(hub: Arc<Insights>, authorizer: Arc<dyn Authorizer>, info: ServerInfo) -> Self {
        Self {
            hub,
            env: Env { authorizer, info },
        }
    }

    /// The tonic service with message limits, zstd request decoding and [`AuthInterceptor`].
    pub fn into_service(self) -> InterceptedService<CiInsightsServer<Self>, AuthInterceptor> {
        let server = CiInsightsServer::new(self)
            .max_decoding_message_size(MAX_MESSAGE)
            .max_encoding_message_size(MAX_MESSAGE)
            .accept_compressed(tonic::codec::CompressionEncoding::Zstd);
        InterceptedService::new(server, AuthInterceptor)
    }

    /// Whether the caller is a maintainer: repository admin, or a member of the policy role.
    async fn maintainer(&self, ext: &tonic::Extensions, ctx: &CallCtx) -> bool {
        if self.env.has(ext, ctx, Access::Admin).await {
            return true;
        }
        let Ok(entry) = self.hub.registry.open(&ctx.repo).await else {
            return false;
        };
        match self.hub.policy.in_force(&entry).await {
            Ok(loaded) => loaded
                .policy()
                .is_some_and(|p| p.is_maintainer(&crate::policy::who_of(&ctx.principal.id))),
            Err(_) => false,
        }
    }
}

#[tonic::async_trait]
impl CiInsights for CiInsightsService {
    async fn report_test_cases(
        &self,
        request: Request<wire::ReportTestCasesRequest>,
    ) -> Result<Response<wire::ReportTestCasesResponse>, Status> {
        let (meta, ext, msg) = request.into_parts();
        let ctx = self
            .env
            .context(&meta, &ext, msg.repo.as_ref(), Access::Admin, false)
            .await?;
        self.hub.report(&ctx, msg).await.map(Response::new)
    }

    async fn list_failure_groups(
        &self,
        request: Request<wire::ListFailureGroupsRequest>,
    ) -> Result<Response<wire::ListFailureGroupsResponse>, Status> {
        let (meta, ext, msg) = request.into_parts();
        let ctx = self
            .env
            .context(&meta, &ext, msg.repo.as_ref(), Access::Read, false)
            .await?;
        self.hub.list_groups(&ctx, msg).await.map(Response::new)
    }

    async fn get_failure_group(
        &self,
        request: Request<wire::GetFailureGroupRequest>,
    ) -> Result<Response<wire::GetFailureGroupResponse>, Status> {
        let (meta, ext, msg) = request.into_parts();
        let ctx = self
            .env
            .context(&meta, &ext, msg.repo.as_ref(), Access::Read, false)
            .await?;
        self.hub.get_group(&ctx, msg).await.map(Response::new)
    }

    async fn set_failure_group_state(
        &self,
        request: Request<wire::SetFailureGroupStateRequest>,
    ) -> Result<Response<wire::SetFailureGroupStateResponse>, Status> {
        let (meta, ext, msg) = request.into_parts();
        let ctx = self
            .env
            .context(&meta, &ext, msg.repo.as_ref(), Access::Write, true)
            .await?;
        let quarantine = msg.state == wire::FailureGroupState::Quarantined as i32;
        let maintainer = quarantine && self.maintainer(&ext, &ctx).await;
        self.hub
            .set_state(&ctx, maintainer, msg)
            .await
            .map(Response::new)
    }

    async fn flaky_history(
        &self,
        request: Request<wire::FlakyHistoryRequest>,
    ) -> Result<Response<wire::FlakyHistoryResponse>, Status> {
        let (meta, ext, msg) = request.into_parts();
        let ctx = self
            .env
            .context(&meta, &ext, msg.repo.as_ref(), Access::Read, false)
            .await?;
        self.hub.flaky_history(&ctx, msg).await.map(Response::new)
    }

    async fn list_flaky_tests(
        &self,
        request: Request<wire::ListFlakyTestsRequest>,
    ) -> Result<Response<wire::ListFlakyTestsResponse>, Status> {
        let (meta, ext, msg) = request.into_parts();
        let ctx = self
            .env
            .context(&meta, &ext, msg.repo.as_ref(), Access::Read, false)
            .await?;
        self.hub.list_flaky(&ctx, msg).await.map(Response::new)
    }
}
