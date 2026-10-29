//! The `asc.v1.Insights` tonic service. Every call needs repo read; the answer is about the
//! repository of the call only (docs/BUILDOUT.md section 24).

use std::sync::Arc;

use asc_rpc::auth::{Access, AuthInterceptor, Authorizer};
use asc_rpc::server::{MAX_MESSAGE, ServerInfo, wire};
use asc_rpc::services::insights_server::{Insights, InsightsServer};
use tonic::service::interceptor::InterceptedService;
use tonic::{Request, Response, Status};

use crate::review::{Env, Reviews};

pub struct InsightsService {
    hub: Arc<Reviews>,
    env: Env,
}

impl InsightsService {
    pub fn new(hub: Arc<Reviews>, authorizer: Arc<dyn Authorizer>, info: ServerInfo) -> Self {
        Self {
            hub,
            env: Env { authorizer, info },
        }
    }

    /// The tonic service with message limits, zstd request decoding and [`AuthInterceptor`].
    pub fn into_service(self) -> InterceptedService<InsightsServer<Self>, AuthInterceptor> {
        let server = InsightsServer::new(self)
            .max_decoding_message_size(MAX_MESSAGE)
            .max_encoding_message_size(MAX_MESSAGE)
            .accept_compressed(tonic::codec::CompressionEncoding::Zstd);
        InterceptedService::new(server, AuthInterceptor)
    }
}

#[tonic::async_trait]
impl Insights for InsightsService {
    async fn get_stats(
        &self,
        request: Request<wire::InsightsStatsRequest>,
    ) -> Result<Response<wire::InsightsStatsResponse>, Status> {
        let (meta, ext, msg) = request.into_parts();
        let ctx = self
            .env
            .context(&meta, &ext, msg.repo.as_ref(), Access::Read, false)
            .await?;
        self.hub.stats_get(&ctx, msg).await.map(Response::new)
    }

    async fn list_contributors(
        &self,
        request: Request<wire::ListContributorsRequest>,
    ) -> Result<Response<wire::ListContributorsResponse>, Status> {
        let (meta, ext, msg) = request.into_parts();
        let ctx = self
            .env
            .context(&meta, &ext, msg.repo.as_ref(), Access::Read, false)
            .await?;
        self.hub
            .stats_contributors(&ctx, msg)
            .await
            .map(Response::new)
    }
}
