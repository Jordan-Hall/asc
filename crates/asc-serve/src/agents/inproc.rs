//! In-process calls to the server's own services, as the agent of a run.
//!
//! The runtime does not reach into the engine to write. It calls the same gRPC services a remote
//! client calls, through the router, with the run's own agent token, so authentication, the agent
//! gate (scope, budget, never trunk), the repository policy, secret scanning and the audit trail
//! bind an agent exactly as they bind anyone. A tool that the policy refuses fails the way it
//! would over the wire.

use asc_rpc::server::{MAX_MESSAGE, wire};
use asc_rpc::services::code_intel_client::CodeIntelClient;
use asc_rpc::services::repo_client::RepoClient;
use futures::StreamExt as _;
use tonic::metadata::{Ascii, MetadataValue};
use tonic::service::Routes;
use tonic::{Request, Status};
use uuid::Uuid;

/// Most search responses read from a stream.
const MAX_STREAM_ITEMS: usize = 64;

/// The router the services are served from.
#[derive(Clone)]
pub struct InProc {
    routes: Routes,
}

fn authed<T>(token: &str, msg: T, mutating: bool) -> Request<T> {
    let mut req = Request::new(msg);
    if let Ok(mut v) = format!("Bearer {token}").parse::<MetadataValue<Ascii>>() {
        v.set_sensitive(true);
        req.metadata_mut().insert("authorization", v);
    }
    if let Ok(v) =
        format!("asc/{} agent", env!("CARGO_PKG_VERSION")).parse::<MetadataValue<Ascii>>()
    {
        req.metadata_mut().insert("x-asc-client", v);
    }
    if mutating && let Ok(v) = Uuid::new_v4().to_string().parse::<MetadataValue<Ascii>>() {
        req.metadata_mut().insert("x-asc-op-id", v);
    }
    req
}

impl InProc {
    pub fn new(routes: Routes) -> Self {
        Self { routes }
    }

    fn repo(&self) -> RepoClient<Routes> {
        RepoClient::new(self.routes.clone())
            .max_decoding_message_size(MAX_MESSAGE)
            .max_encoding_message_size(MAX_MESSAGE)
    }

    fn intel(&self) -> CodeIntelClient<Routes> {
        CodeIntelClient::new(self.routes.clone())
            .max_decoding_message_size(MAX_MESSAGE)
            .max_encoding_message_size(MAX_MESSAGE)
    }

    /// `Repo.Commit`.
    pub async fn commit(
        &self,
        token: &str,
        req: wire::CommitRequest,
    ) -> Result<wire::CommitResponse, Status> {
        Ok(self
            .repo()
            .commit(authed(token, req, true))
            .await?
            .into_inner())
    }

    /// `Repo.Resolve`.
    pub async fn resolve(
        &self,
        token: &str,
        req: wire::ResolveRequest,
    ) -> Result<wire::ResolveResponse, Status> {
        Ok(self
            .repo()
            .resolve(authed(token, req, false))
            .await?
            .into_inner())
    }

    /// `Repo.UpdateRefs`.
    pub async fn update_refs(
        &self,
        token: &str,
        req: wire::UpdateRefsRequest,
    ) -> Result<wire::UpdateRefsResponse, Status> {
        Ok(self
            .repo()
            .update_refs(authed(token, req, true))
            .await?
            .into_inner())
    }

    /// `CodeIntel.Search`, the first pages of the stream.
    pub async fn search(
        &self,
        token: &str,
        req: wire::SearchRequest,
    ) -> Result<Vec<wire::SearchResponse>, Status> {
        let mut stream = self
            .intel()
            .search(authed(token, req, false))
            .await?
            .into_inner();
        let mut out = Vec::new();
        while let Some(item) = stream.next().await {
            out.push(item?);
            if out.len() >= MAX_STREAM_ITEMS {
                break;
            }
        }
        Ok(out)
    }

    /// `CodeIntel.Symbols`.
    pub async fn symbols(
        &self,
        token: &str,
        req: wire::SymbolsRequest,
    ) -> Result<wire::SymbolsResponse, Status> {
        Ok(self
            .intel()
            .symbols(authed(token, req, false))
            .await?
            .into_inner())
    }

    /// `CodeIntel.IndexStatus`: whether the index can answer for a scope.
    pub async fn index_status(
        &self,
        token: &str,
        req: wire::IndexStatusRequest,
    ) -> Result<wire::IndexStatusResponse, Status> {
        Ok(self
            .intel()
            .index_status(authed(token, req, false))
            .await?
            .into_inner())
    }
}
