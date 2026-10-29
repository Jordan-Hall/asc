//! The `DataExport` tonic service. Every call is for the caller's own account.

use std::sync::Arc;

use asc_rpc::auth::{AuthInterceptor, Authorizer};
use asc_rpc::forwarded::Role;
use asc_rpc::server::{MAX_MESSAGE, ServerInfo, wire};
use asc_rpc::services::data_export_server::{DataExport, DataExportServer};
use futures::stream::{self, BoxStream};
use tonic::service::interceptor::InterceptedService;
use tonic::{Request, Response, Status};

use super::{ExportHub, Job};
use crate::ai::audit::Entry;
use crate::ai::caller::{once, op_id};
use crate::settings::access::{Actor, Gate, Subject};

/// The `asc.v1.DataExport` service.
pub struct ExportService {
    hub: Arc<ExportHub>,
    gate: Gate,
}

fn fail(e: super::ExportError) -> Status {
    e.status()
}

fn wire_job(j: &Job) -> wire::ExportJob {
    wire::ExportJob {
        id: j.id.clone(),
        state: match j.state {
            super::PENDING => wire::ExportState::Pending,
            super::RUNNING => wire::ExportState::Running,
            super::READY => wire::ExportState::Ready,
            super::FAILED => wire::ExportState::Failed,
            super::EXPIRED => wire::ExportState::Expired,
            _ => wire::ExportState::Unspecified,
        } as i32,
        requested: j.requested,
        finished: j.finished,
        expires: j.expires,
        size_bytes: u64::try_from(j.size).unwrap_or(0),
        rows: u64::try_from(j.rows).unwrap_or(0),
        truncated: j.truncated,
        error: j.error.clone(),
    }
}

impl ExportService {
    pub fn new(hub: Arc<ExportHub>, authorizer: Arc<dyn Authorizer>, _info: ServerInfo) -> Self {
        Self {
            hub,
            gate: Gate::new(authorizer),
        }
    }

    pub fn into_service(self) -> InterceptedService<DataExportServer<Self>, AuthInterceptor> {
        let server = DataExportServer::new(self)
            .max_decoding_message_size(MAX_MESSAGE)
            .max_encoding_message_size(MAX_MESSAGE)
            .accept_compressed(tonic::codec::CompressionEncoding::Zstd);
        InterceptedService::new(server, AuthInterceptor)
    }

    /// The person and the actor: the account of the caller, never another.
    async fn me(
        &self,
        ext: &tonic::Extensions,
        meta: &tonic::metadata::MetadataMap,
        needed: Role,
    ) -> Result<(String, Actor), Status> {
        let caller = self.gate.caller(ext).await?;
        let (subject, actor) = self.gate.person(&caller, meta, needed)?;
        match subject {
            Subject::User(login) => Ok((login, actor)),
            _ => Err(Gate::denied()),
        }
    }

    async fn audit(&self, action: &str, actor: &Actor, login: &str, id: &str) {
        self.hub
            .audit
            .record(Entry {
                owner: login,
                target: id,
                on_behalf: actor.relay.as_deref(),
                ..Entry::new(action, &actor.id)
            })
            .await;
    }
}

#[tonic::async_trait]
impl DataExport for ExportService {
    type DownloadStream = BoxStream<'static, Result<wire::ExportChunk, Status>>;

    async fn request_export(
        &self,
        request: Request<wire::RequestExportRequest>,
    ) -> Result<Response<wire::RequestExportResponse>, Status> {
        let (meta, ext, _) = request.into_parts();
        let (login, actor) = self.me(&ext, &meta, Role::Write).await?;
        let op = op_id(&meta)?;
        let hub = self.hub.clone();
        once(
            &hub.ops,
            op,
            &actor.replay_key,
            "DataExport/RequestExport",
            true,
            || async {
                let job = hub.request(&login).await.map_err(fail)?;
                self.audit("export.requested", &actor, &login, &job.id)
                    .await;
                Ok(wire::RequestExportResponse {
                    job: Some(wire_job(&job)),
                })
            },
        )
        .await
        .map(Response::new)
    }

    async fn list_exports(
        &self,
        request: Request<wire::ListExportsRequest>,
    ) -> Result<Response<wire::ListExportsResponse>, Status> {
        let (meta, ext, _) = request.into_parts();
        let (login, _) = self.me(&ext, &meta, Role::Read).await?;
        let jobs = self.hub.list(&login).await.map_err(fail)?;
        Ok(Response::new(wire::ListExportsResponse {
            jobs: jobs.iter().map(wire_job).collect(),
        }))
    }

    async fn get_export(
        &self,
        request: Request<wire::GetExportRequest>,
    ) -> Result<Response<wire::GetExportResponse>, Status> {
        let (meta, ext, msg) = request.into_parts();
        let (login, _) = self.me(&ext, &meta, Role::Read).await?;
        let j = self.hub.get(&login, &msg.id).await.map_err(fail)?;
        Ok(Response::new(wire::GetExportResponse {
            job: Some(wire_job(&j)),
        }))
    }

    async fn download(
        &self,
        request: Request<wire::DownloadExportRequest>,
    ) -> Result<Response<Self::DownloadStream>, Status> {
        let (meta, ext, msg) = request.into_parts();
        let (login, actor) = self.me(&ext, &meta, Role::Write).await?;
        let (job, path) = self.hub.ready(&login, &msg.id).await.map_err(fail)?;
        self.audit("export.downloaded", &actor, &login, &job.id)
            .await;
        let size = u64::try_from(job.size).unwrap_or(0);
        let s = stream::unfold(Some(0u64), move |state| {
            let path = path.clone();
            async move {
                let offset = state?;
                let chunk = tokio::task::spawn_blocking(move || super::read_chunk(&path, offset))
                    .await
                    .map_err(|_| Status::internal("internal error"))
                    .and_then(|r| r.map_err(|_| Status::not_found("the export is gone")));
                match chunk {
                    Err(e) => Some((Err(e), None)),
                    Ok(data) => {
                        let end = offset + data.len() as u64;
                        let last = data.is_empty() || end >= size;
                        let msg = wire::ExportChunk {
                            offset,
                            data: data.into(),
                            last,
                        };
                        Some((Ok(msg), (!last).then_some(end)))
                    }
                }
            }
        });
        Ok(Response::new(Box::pin(s)))
    }

    async fn delete_export(
        &self,
        request: Request<wire::DeleteExportRequest>,
    ) -> Result<Response<wire::DeleteExportResponse>, Status> {
        let (meta, ext, msg) = request.into_parts();
        let (login, actor) = self.me(&ext, &meta, Role::Write).await?;
        let op = op_id(&meta)?;
        let hub = self.hub.clone();
        once(
            &hub.ops,
            op,
            &actor.replay_key,
            "DataExport/DeleteExport",
            true,
            || async {
                hub.delete(&login, &msg.id).await.map_err(fail)?;
                self.audit("export.deleted", &actor, &login, &msg.id).await;
                Ok(wire::DeleteExportResponse {})
            },
        )
        .await
        .map(Response::new)
    }
}
