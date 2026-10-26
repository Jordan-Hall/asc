//! The objects group of the primary server: `HasObjects`, `GetObjects` and `PutObjects` for
//! every repository of the registry (SPEC §3.3).
//!
//! Each call opens the repository named by the request (`Registry::open`), so an object is read
//! and written in that repository's namespace only, and a missing repository answers
//! `NOT_FOUND`. The service layer has already authorized the call (`HasObjects` and
//! `GetObjects` need read, `PutObjects` write) and admitted it by class (`limits::class_of`:
//! `GetObjects` and `PutObjects` are charged to the caller's `objects` byte bucket as they
//! stream). The repository's engine backend validates the uploaded objects (stored encoding,
//! content id, no git-only extras). An upload adds objects no ref points to; the user-ref quota
//! (`UserQuota::claim`) is charged by the `UpdateRefs` that makes them reachable.

use std::sync::Arc;

use async_trait::async_trait;
use asc_rpc::server::{CallCtx, ObjectsHandler, RpcStream, wire};
use tonic::Status;

use crate::registry::{Extensions, Registry, RegistryError, RepoEntry};

/// The objects handler over a registry.
pub struct ServerObjects {
    registry: Arc<Registry>,
}

impl ServerObjects {
    pub fn new(registry: Arc<Registry>) -> Self {
        Self { registry }
    }

    async fn entry(&self, ctx: &CallCtx) -> Result<Arc<RepoEntry>, Status> {
        self.registry.open(&ctx.repo).await.map_err(|e| match e {
            RegistryError::NotFound(_) => Status::not_found("no such repository"),
            e => {
                tracing::error!(error = %e, "objects: opening the repository failed");
                Status::internal("internal error")
            }
        })
    }
}

#[async_trait]
impl ObjectsHandler for ServerObjects {
    async fn has_objects(
        &self,
        ctx: &CallCtx,
        req: wire::HasObjectsRequest,
    ) -> Result<wire::HasObjectsResponse, Status> {
        self.entry(ctx).await?.backend.has_objects(ctx, req).await
    }

    async fn put_objects(
        &self,
        ctx: &CallCtx,
        req: wire::PutObjectsRequest,
    ) -> Result<wire::PutObjectsResponse, Status> {
        self.entry(ctx).await?.backend.put_objects(ctx, req).await
    }

    async fn get_objects(
        &self,
        ctx: &CallCtx,
        req: wire::GetObjectsRequest,
    ) -> Result<RpcStream<wire::GetObjectsResponse>, Status> {
        self.entry(ctx).await?.backend.get_objects(ctx, req).await
    }
}

/// Registers the objects group.
pub fn register(ext: &Extensions, registry: &Arc<Registry>) {
    ext.handlers
        .set_objects(Arc::new(ServerObjects::new(registry.clone())))
        .expect("the objects handler group is registered once");
}
