//! The `MergeQueue` tonic service (`mergequeue.proto`). The rules are in `queue/mod.rs`; this file
//! decides who may call what, through the same `Env::context` as the `Review` service (so the hub's
//! forwarded claim applies, read/write/admin, and a claim for another repository is refused):
//!
//! | call | needs |
//! |---|---|
//! | `Enqueue` | repo write, as a person |
//! | `Dequeue` | the person who queued the entry, or repo admin |
//! | `List`, `Get`, `Preview`, `Watch` | repo read |
//! | `Revalidate` | repo write |
//! | `SetPaused`, `ReportEntry` | repo admin |

use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use asc_rpc::auth::{Access, AuthInterceptor, Authorizer};
use asc_rpc::server::{MAX_MESSAGE, ServerInfo, wire};
use asc_rpc::services::merge_queue_server::{MergeQueue, MergeQueueServer};
use futures::Stream;
use tonic::service::interceptor::InterceptedService;
use tonic::{Request, Response, Status};

use crate::review::{Env, Reviews, require_handle};

/// Longest a watch lives; the client resumes with the cursor of its last event.
const WATCH_LIFETIME: Duration = Duration::from_secs(10 * 60);
/// Open watches a server holds at once.
const MAX_WATCHERS: usize = 128;
const BATCH: i64 = 100;

type WatchStream = Pin<Box<dyn Stream<Item = Result<wire::QueueEvent, Status>> + Send + 'static>>;

/// The `asc.v1.MergeQueue` service.
#[derive(Clone)]
pub struct QueueService {
    hub: Arc<Reviews>,
    env: Env,
    slots: Arc<tokio::sync::Semaphore>,
}

impl QueueService {
    pub fn new(hub: Arc<Reviews>, authorizer: Arc<dyn Authorizer>, info: ServerInfo) -> Self {
        Self {
            hub,
            env: Env { authorizer, info },
            slots: Arc::new(tokio::sync::Semaphore::new(MAX_WATCHERS)),
        }
    }

    pub fn into_service(self) -> InterceptedService<MergeQueueServer<Self>, AuthInterceptor> {
        let server = MergeQueueServer::new(self)
            .max_decoding_message_size(MAX_MESSAGE)
            .max_encoding_message_size(MAX_MESSAGE)
            .accept_compressed(tonic::codec::CompressionEncoding::Zstd);
        InterceptedService::new(server, AuthInterceptor)
    }
}

#[tonic::async_trait]
impl MergeQueue for QueueService {
    type WatchStream = WatchStream;

    async fn enqueue(
        &self,
        request: Request<wire::EnqueueRequest>,
    ) -> Result<Response<wire::EnqueueResponse>, Status> {
        let (meta, ext, msg) = request.into_parts();
        let ctx = self
            .env
            .context(&meta, &ext, msg.repo.as_ref(), Access::Write, true)
            .await?;
        let by = require_handle(&ctx)?;
        self.hub.q_enqueue(&ctx, &by, msg).await.map(Response::new)
    }

    async fn dequeue(
        &self,
        request: Request<wire::DequeueRequest>,
    ) -> Result<Response<wire::DequeueResponse>, Status> {
        let (meta, ext, msg) = request.into_parts();
        let ctx = self
            .env
            .context(&meta, &ext, msg.repo.as_ref(), Access::Write, false)
            .await?;
        let by = require_handle(&ctx)?;
        let admin = self.env.has(&ext, &ctx, Access::Admin).await;
        self.hub
            .q_dequeue(&ctx, &by, admin, msg)
            .await
            .map(Response::new)
    }

    async fn list(
        &self,
        request: Request<wire::ListQueueRequest>,
    ) -> Result<Response<wire::ListQueueResponse>, Status> {
        let (meta, ext, msg) = request.into_parts();
        let ctx = self
            .env
            .context(&meta, &ext, msg.repo.as_ref(), Access::Read, false)
            .await?;
        self.hub.q_list(&ctx, msg).await.map(Response::new)
    }

    async fn get(
        &self,
        request: Request<wire::GetQueueEntryRequest>,
    ) -> Result<Response<wire::GetQueueEntryResponse>, Status> {
        let (meta, ext, msg) = request.into_parts();
        let ctx = self
            .env
            .context(&meta, &ext, msg.repo.as_ref(), Access::Read, false)
            .await?;
        self.hub.q_get_entry(&ctx, msg).await.map(Response::new)
    }

    async fn revalidate(
        &self,
        request: Request<wire::RevalidateRequest>,
    ) -> Result<Response<wire::RevalidateResponse>, Status> {
        let (meta, ext, msg) = request.into_parts();
        let ctx = self
            .env
            .context(&meta, &ext, msg.repo.as_ref(), Access::Write, false)
            .await?;
        self.hub.q_revalidate(&ctx, msg).await.map(Response::new)
    }

    async fn set_paused(
        &self,
        request: Request<wire::SetQueuePausedRequest>,
    ) -> Result<Response<wire::SetQueuePausedResponse>, Status> {
        let (meta, ext, msg) = request.into_parts();
        let ctx = self
            .env
            .context(&meta, &ext, msg.repo.as_ref(), Access::Admin, false)
            .await?;
        let by = require_handle(&ctx).unwrap_or_else(|_| ctx.principal.id.clone());
        self.hub
            .q_set_paused(&ctx, &by, msg)
            .await
            .map(Response::new)
    }

    async fn preview(
        &self,
        request: Request<wire::PreviewEnqueueRequest>,
    ) -> Result<Response<wire::PreviewEnqueueResponse>, Status> {
        let (meta, ext, msg) = request.into_parts();
        let ctx = self
            .env
            .context(&meta, &ext, msg.repo.as_ref(), Access::Read, false)
            .await?;
        self.hub.q_preview(&ctx, msg).await.map(Response::new)
    }

    async fn report_entry(
        &self,
        request: Request<wire::ReportQueueEntryRequest>,
    ) -> Result<Response<wire::ReportQueueEntryResponse>, Status> {
        let (meta, ext, msg) = request.into_parts();
        let ctx = self
            .env
            .context(&meta, &ext, msg.repo.as_ref(), Access::Admin, false)
            .await?;
        self.hub.q_report(&ctx, msg).await.map(Response::new)
    }

    async fn watch(
        &self,
        request: Request<wire::WatchQueueRequest>,
    ) -> Result<Response<Self::WatchStream>, Status> {
        let (meta, ext, msg) = request.into_parts();
        let ctx = self
            .env
            .context(&meta, &ext, msg.repo.as_ref(), Access::Read, false)
            .await?;
        let permit = Arc::clone(&self.slots)
            .try_acquire_owned()
            .map_err(|_| Status::resource_exhausted("too many open watches; retry shortly"))?;
        let entry = self.hub.open(&ctx).await?;
        let ns = entry.ns.as_bytes().to_vec();
        let target = if msg.target.is_empty() {
            entry.default_branch.clone()
        } else {
            msg.target.clone()
        };
        let mut buf = std::collections::VecDeque::new();
        let cursor = if msg.resume.is_empty() {
            // A new watch starts with the live entries as they are, then follows.
            let cursor = self.hub.q_newest_seq(&ns).await?;
            let list = self
                .hub
                .q_list(
                    &ctx,
                    wire::ListQueueRequest {
                        repo: msg.repo.clone(),
                        target: target.clone(),
                        limit: 200,
                        ..Default::default()
                    },
                )
                .await?;
            for e in list.entries {
                buf.push_back(wire::QueueEvent {
                    entry: Some(e),
                    paused: list.paused,
                    resume: bytes::Bytes::copy_from_slice(&cursor.to_be_bytes()),
                });
            }
            cursor
        } else {
            let raw: [u8; 8] = msg
                .resume
                .as_ref()
                .try_into()
                .map_err(|_| Status::invalid_argument("a resume cursor is 8 bytes"))?;
            i64::from_be_bytes(raw).max(0)
        };
        let started = std::time::Instant::now();
        let state = (self.hub.clone(), ns, target, cursor, buf, permit);
        let stream = futures::stream::unfold(
            state,
            move |(hub, ns, target, mut cursor, mut buf, permit)| async move {
                loop {
                    if let Some(ev) = buf.pop_front() {
                        return Some((Ok(ev), (hub, ns, target, cursor, buf, permit)));
                    }
                    if started.elapsed() > WATCH_LIFETIME {
                        return None;
                    }
                    match hub.q_events_after(&ns, &target, cursor, BATCH).await {
                        Ok((events, last)) if !events.is_empty() => {
                            cursor = last;
                            buf.extend(events);
                            continue;
                        }
                        Ok(_) => {}
                        Err(e) => {
                            let mut empty = std::collections::VecDeque::new();
                            empty.clear();
                            return Some((Err(e), (hub, ns, target, i64::MAX, empty, permit)));
                        }
                    }
                    tokio::time::sleep(Duration::from_millis(250)).await;
                }
            },
        );
        Ok(Response::new(Box::pin(stream)))
    }
}
