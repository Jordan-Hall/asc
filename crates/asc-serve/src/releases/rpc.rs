//! The `asc.v1.Releases` tonic service. Reads need repo read (drafts need repo write), `Create` and
//! `Publish` need repo write and an operation id.

use std::sync::Arc;

use asc_rpc::auth::{Access, AuthInterceptor, Authorizer};
use asc_rpc::server::{MAX_MESSAGE, ServerInfo, wire};
use asc_rpc::services::releases_server::{Releases, ReleasesServer};
use tonic::service::interceptor::InterceptedService;
use tonic::{Request, Response, Status};

use crate::review::{Env, Reviews};

pub struct ReleasesService {
    hub: Arc<Reviews>,
    env: Env,
}

impl ReleasesService {
    pub fn new(hub: Arc<Reviews>, authorizer: Arc<dyn Authorizer>, info: ServerInfo) -> Self {
        Self {
            hub,
            env: Env { authorizer, info },
        }
    }

    /// The tonic service with message limits, zstd request decoding and [`AuthInterceptor`].
    pub fn into_service(self) -> InterceptedService<ReleasesServer<Self>, AuthInterceptor> {
        let server = ReleasesServer::new(self)
            .max_decoding_message_size(MAX_MESSAGE)
            .max_encoding_message_size(MAX_MESSAGE)
            .accept_compressed(tonic::codec::CompressionEncoding::Zstd);
        InterceptedService::new(server, AuthInterceptor)
    }
}

#[tonic::async_trait]
impl Releases for ReleasesService {
    async fn draft_notes(
        &self,
        request: Request<wire::DraftReleaseNotesRequest>,
    ) -> Result<Response<wire::DraftReleaseNotesResponse>, Status> {
        let (meta, ext, msg) = request.into_parts();
        let ctx = self
            .env
            .context(&meta, &ext, msg.repo.as_ref(), Access::Read, false)
            .await?;
        self.hub.release_notes(&ctx, msg).await.map(Response::new)
    }

    async fn next(
        &self,
        request: Request<wire::NextReleaseRequest>,
    ) -> Result<Response<wire::NextReleaseResponse>, Status> {
        let (meta, ext, msg) = request.into_parts();
        let ctx = self
            .env
            .context(&meta, &ext, msg.repo.as_ref(), Access::Read, false)
            .await?;
        let drafts = self.env.has(&ext, &ctx, Access::Write).await;
        self.hub
            .release_next(&ctx, msg, drafts)
            .await
            .map(Response::new)
    }

    async fn create(
        &self,
        request: Request<wire::CreateReleaseRequest>,
    ) -> Result<Response<wire::CreateReleaseResponse>, Status> {
        let (meta, ext, msg) = request.into_parts();
        let ctx = self
            .env
            .context(&meta, &ext, msg.repo.as_ref(), Access::Write, true)
            .await?;
        self.hub.release_create(&ctx, msg).await.map(Response::new)
    }

    async fn publish(
        &self,
        request: Request<wire::PublishReleaseRequest>,
    ) -> Result<Response<wire::PublishReleaseResponse>, Status> {
        let (meta, ext, msg) = request.into_parts();
        let ctx = self
            .env
            .context(&meta, &ext, msg.repo.as_ref(), Access::Write, true)
            .await?;
        self.hub.release_publish(&ctx, msg).await.map(Response::new)
    }

    async fn list(
        &self,
        request: Request<wire::ListReleasesRequest>,
    ) -> Result<Response<wire::ListReleasesResponse>, Status> {
        let (meta, ext, msg) = request.into_parts();
        let ctx = self
            .env
            .context(&meta, &ext, msg.repo.as_ref(), Access::Read, false)
            .await?;
        let drafts = self.env.has(&ext, &ctx, Access::Write).await;
        self.hub
            .release_list(&ctx, msg, drafts)
            .await
            .map(Response::new)
    }

    async fn get(
        &self,
        request: Request<wire::GetReleaseRequest>,
    ) -> Result<Response<wire::GetReleaseResponse>, Status> {
        let (meta, ext, msg) = request.into_parts();
        let ctx = self
            .env
            .context(&meta, &ext, msg.repo.as_ref(), Access::Read, false)
            .await?;
        let drafts = self.env.has(&ext, &ctx, Access::Write).await;
        self.hub
            .release_get(&ctx, msg, drafts)
            .await
            .map(Response::new)
    }
}
