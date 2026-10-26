//! The `Review` service: comment threads on a change and review requests (`review.proto`).
//!
//! Choices where the SPEC is silent:
//!
//! * Threads belong to a change, not to one version: `anchor.version` says which version the
//!   lines refer to (0 on a new thread means the newest), and a thread keeps showing on later
//!   versions so a client can map it forward.
//! * A comment on a resolved thread leaves it resolved; `ResolveThread(resolved = false)`
//!   reopens it.
//! * Resolving needs repo write, authorship of the change (the actor of its first version) or
//!   being the thread's opener.
//! * The first comment a requested reviewer makes on a change finishes their review request.
//! * A retried `x-asc-op-id` of `AddComment` returns the first answer (an `op_results` row).

use std::sync::Arc;

use asc_rpc::auth::{Access, AuthInterceptor, Authorizer};
use asc_rpc::server::{MAX_MESSAGE, ServerInfo, wire};
use asc_rpc::services::review_server::{Review, ReviewServer};
use bytes::Bytes;
use sqlx::Row;
use tonic::service::interceptor::InterceptedService;
use tonic::{Request, Response, Status};
use uuid::Uuid;

use super::store::{self, NewThread};
use super::{
    DEFAULT_PAGE, Env, MAX_BODY, MAX_COMMENTS_PER_THREAD, MAX_PAGE, MAX_PATH,
    MAX_REQUESTS_PER_CHANGE, MAX_REVIEWERS, MAX_THREADS_PER_CHANGE, Reviews, WriteError, change_of,
    handle_of, next_id, now, page_limit, parse_after, require_handle, retry_conflict, sql,
    valid_handle,
};
use crate::registry::RepoEntry;
use asc_rpc::server::CallCtx;

/// Marks an `op_results` row as the result of `AddComment`: the tag, the thread id, the comment id.
const OP_TAG: &[u8; 3] = b"rc1";
/// Furthest line a thread may name.
const MAX_LINE: u32 = 10_000_000;

fn id_of(n: u64) -> Result<i64, Status> {
    i64::try_from(n)
        .ok()
        .filter(|n| *n > 0)
        .ok_or_else(|| Status::invalid_argument("invalid id"))
}

/// A validated anchor of a new thread.
struct Anchor {
    path: Bytes,
    version: u32,
    side: i64,
    start_line: i64,
    end_line: i64,
}

fn anchor_of(a: Option<wire::ThreadAnchor>) -> Result<Anchor, Status> {
    let a = a.unwrap_or_default();
    if a.path.len() > MAX_PATH || a.path.contains(&0) {
        return Err(Status::invalid_argument("invalid anchor path"));
    }
    let side = wire::DiffSide::try_from(a.side)
        .map_err(|_| Status::invalid_argument("invalid anchor side"))?;
    if a.start_line > MAX_LINE || a.end_line > MAX_LINE {
        return Err(Status::invalid_argument("anchor line out of range"));
    }
    if a.end_line != 0 && a.end_line < a.start_line {
        return Err(Status::invalid_argument("anchor ends before it starts"));
    }
    if a.path.is_empty() {
        // A thread on the change as a whole has no side and no lines.
        if a.start_line != 0 || a.end_line != 0 || side != wire::DiffSide::Unspecified {
            return Err(Status::invalid_argument(
                "a thread on the whole change has no side or lines",
            ));
        }
        return Ok(Anchor {
            path: a.path,
            version: a.version,
            side: 0,
            start_line: 0,
            end_line: 0,
        });
    }
    let side = match side {
        wire::DiffSide::Unspecified => wire::DiffSide::New,
        s => s,
    };
    Ok(Anchor {
        path: a.path,
        version: a.version,
        side: side as i64,
        start_line: i64::from(a.start_line),
        end_line: i64::from(a.end_line.max(a.start_line)),
    })
}

fn body_of(body: &str) -> Result<&str, Status> {
    if body.trim().is_empty() || body.len() > MAX_BODY || body.contains('\0') {
        return Err(Status::invalid_argument(format!(
            "a comment is 1 to {MAX_BODY} bytes of text"
        )));
    }
    Ok(body)
}

impl Reviews {
    /// `Review.ListThreads`.
    pub async fn list_threads(
        &self,
        ctx: &CallCtx,
        req: wire::ListThreadsRequest,
    ) -> Result<wire::ListThreadsResponse, Status> {
        let change = change_of(&req.change)?;
        let status = wire::ThreadStatus::try_from(req.status)
            .map_err(|_| Status::invalid_argument("invalid status"))? as i64;
        let after = parse_after(&req.page_token)?;
        let limit = page_limit(req.limit, DEFAULT_PAGE, MAX_PAGE);
        let entry = self.open(ctx).await?;
        let mut conn = self.registry.pool().acquire().await.map_err(sql)?;
        if !store::change_known(&mut conn, entry.ns, change).await? {
            return Err(Status::not_found("unknown change"));
        }
        let mut threads = store::threads_page(
            &mut conn,
            entry.ns,
            change,
            status,
            i64::from(req.version),
            after,
            limit,
        )
        .await
        .map_err(sql)?;
        let next_page_token = if threads.len() > limit {
            threads.truncate(limit);
            threads.last().map(|t| t.id.to_string()).unwrap_or_default()
        } else {
            String::new()
        };
        let (_, open) = store::thread_counts(&mut conn, entry.ns, change)
            .await
            .map_err(sql)?;
        Ok(wire::ListThreadsResponse {
            threads,
            next_page_token,
            open_threads: u32::try_from(open).unwrap_or(u32::MAX),
        })
    }

    async fn stored(&self, entry: &RepoEntry, op_id: Uuid) -> Result<Option<(i64, i64)>, Status> {
        let row = sqlx::query("SELECT repo_id, result FROM op_results WHERE op_id = $1")
            .bind(op_id.as_bytes().to_vec())
            .fetch_optional(self.registry.pool())
            .await
            .map_err(sql)?;
        let Some(row) = row else { return Ok(None) };
        let repo: i64 = row.try_get("repo_id").map_err(sql)?;
        if repo != entry.key.0 {
            return Err(Status::invalid_argument(
                "op id was used for another repository",
            ));
        }
        let bytes: Vec<u8> = row.try_get("result").map_err(sql)?;
        match bytes.strip_prefix(OP_TAG.as_slice()) {
            Some(v) if v.len() == 16 => Ok(Some((
                i64::from_be_bytes(v[..8].try_into().expect("8")),
                i64::from_be_bytes(v[8..].try_into().expect("8")),
            ))),
            _ => Err(Status::invalid_argument(
                "op id was used for another operation",
            )),
        }
    }

    async fn thread_answer(
        &self,
        entry: &RepoEntry,
        thread: i64,
    ) -> Result<wire::ReviewThread, Status> {
        let mut conn = self.registry.pool().acquire().await.map_err(sql)?;
        store::thread(&mut conn, entry.ns, thread)
            .await
            .map_err(sql)?
            .ok_or_else(|| Status::not_found("unknown thread"))
    }

    /// `Review.AddComment`. `handle` is the verified commenter.
    pub async fn add_comment(
        &self,
        ctx: &CallCtx,
        handle: &str,
        req: wire::AddCommentRequest,
    ) -> Result<wire::AddCommentResponse, Status> {
        let change = change_of(&req.change)?;
        let body = body_of(&req.body)?;
        let op_id = ctx
            .op_id
            .ok_or_else(|| Status::invalid_argument("mutating calls need x-asc-op-id"))?;
        let existing = if req.thread_id == 0 {
            None
        } else {
            Some(id_of(req.thread_id)?)
        };
        let anchor = match existing {
            None => Some(anchor_of(req.anchor.clone())?),
            Some(_) => None,
        };
        let entry = self.open(ctx).await?;
        let (thread, comment) = retry_conflict(|| {
            self.try_add(
                &entry,
                change,
                handle,
                body,
                existing,
                anchor.as_ref(),
                op_id,
            )
        })
        .await?;
        Ok(wire::AddCommentResponse {
            thread: Some(self.thread_answer(&entry, thread).await?),
            comment_id: u64::try_from(comment).unwrap_or(0),
        })
    }

    #[allow(clippy::too_many_arguments)]
    async fn try_add(
        &self,
        entry: &RepoEntry,
        change: asc_core::change::ChangeId,
        handle: &str,
        body: &str,
        existing: Option<i64>,
        anchor: Option<&Anchor>,
        op_id: Uuid,
    ) -> Result<(i64, i64), WriteError> {
        // A retry of a call that already committed answers with the first result.
        if let Some(done) = self.stored(entry, op_id).await? {
            return Ok(done);
        }
        let ns = entry.ns;
        let at = now();
        let mut tx = self.registry.pool().begin().await?;
        // U48c: the namespace must still belong to this repository id and name.
        self.registry.guard(&mut tx, entry).await?;
        if !store::change_known(&mut tx, ns, change).await? {
            return Err(Status::not_found("unknown change").into());
        }
        let thread = match (existing, anchor) {
            (Some(id), _) => {
                let head = store::thread_head(&mut tx, ns, id).await?;
                match head {
                    Some((c, _, _)) if c == change.as_bytes() => {}
                    _ => return Err(Status::not_found("unknown thread").into()),
                }
                if store::comment_count(&mut tx, ns, id).await? >= MAX_COMMENTS_PER_THREAD {
                    return Err(Status::resource_exhausted("the thread is full").into());
                }
                id
            }
            (None, Some(a)) => {
                let latest = store::latest_version(&mut tx, ns, change).await?;
                let version = match a.version {
                    0 => latest,
                    v if v <= latest => v,
                    _ => return Err(Status::invalid_argument("unknown version").into()),
                };
                let (total, _) = store::thread_counts(&mut tx, ns, change).await?;
                if total >= MAX_THREADS_PER_CHANGE {
                    return Err(
                        Status::resource_exhausted("the change has too many threads").into(),
                    );
                }
                let id = next_id(&mut tx, "review_threads", ns.as_bytes()).await?;
                store::insert_thread(
                    &mut tx,
                    ns,
                    &NewThread {
                        id,
                        change,
                        path: &a.path,
                        version: i64::from(version),
                        side: a.side,
                        start_line: a.start_line,
                        end_line: a.end_line,
                        opened_by: handle,
                        at,
                    },
                )
                .await?;
                id
            }
            (None, None) => return Err(Status::invalid_argument("missing anchor").into()),
        };
        let comment = next_id(&mut tx, "review_comments", ns.as_bytes()).await?;
        store::insert_comment(&mut tx, ns, comment, thread, handle, body, at).await?;
        store::finish_request(&mut tx, ns, change, handle, at).await?;
        let mut result = OP_TAG.to_vec();
        result.extend_from_slice(&thread.to_be_bytes());
        result.extend_from_slice(&comment.to_be_bytes());
        sqlx::query("INSERT INTO op_results (op_id, repo_id, result, at) VALUES ($1, $2, $3, $4)")
            .bind(op_id.as_bytes().to_vec())
            .bind(entry.key.0)
            .bind(result)
            .bind(at)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok((thread, comment))
    }

    /// `Review.ResolveThread`. `can_write` is whether the caller holds repo write.
    pub async fn resolve_thread(
        &self,
        ctx: &CallCtx,
        handle: &str,
        can_write: bool,
        req: wire::ResolveThreadRequest,
    ) -> Result<wire::ResolveThreadResponse, Status> {
        let id = id_of(req.thread_id)?;
        let entry = self.open(ctx).await?;
        let ns = entry.ns;
        let mut tx = self.registry.pool().begin().await.map_err(sql)?;
        self.registry.guard(&mut tx, &entry).await?;
        let (change, opened_by, status) = store::thread_head(&mut tx, ns, id)
            .await
            .map_err(sql)?
            .ok_or_else(|| Status::not_found("unknown thread"))?;
        let allowed = if can_write || opened_by == handle {
            true
        } else {
            let change = change_of(&change)?;
            let author = store::first_actor(&mut tx, ns, change).await?;
            author.as_deref().and_then(handle_of).as_deref() == Some(handle)
        };
        if !allowed {
            return Err(Status::permission_denied(
                "only the author of the change, the opener of the thread or a writer may resolve it",
            ));
        }
        let want = if req.resolved {
            store::RESOLVED
        } else {
            store::OPEN
        };
        if status != want {
            store::set_status(&mut tx, ns, id, want, handle, now())
                .await
                .map_err(sql)?;
        }
        tx.commit().await.map_err(sql)?;
        Ok(wire::ResolveThreadResponse {
            thread: Some(self.thread_answer(&entry, id).await?),
        })
    }

    /// `Review.RequestReview`. `handle` is the requester.
    pub async fn request_review(
        &self,
        ctx: &CallCtx,
        handle: &str,
        req: wire::RequestReviewRequest,
    ) -> Result<wire::RequestReviewResponse, Status> {
        let change = change_of(&req.change)?;
        if req.add.len() > MAX_REVIEWERS || req.remove.len() > MAX_REVIEWERS {
            return Err(Status::invalid_argument(format!(
                "at most {MAX_REVIEWERS} reviewers per list"
            )));
        }
        if !req.add.iter().chain(&req.remove).all(|h| valid_handle(h)) {
            return Err(Status::invalid_argument("invalid reviewer handle"));
        }
        let entry = self.open(ctx).await?;
        let ns = entry.ns;
        let at = now();
        let mut tx = self.registry.pool().begin().await.map_err(sql)?;
        self.registry.guard(&mut tx, &entry).await?;
        if !store::change_known(&mut tx, ns, change).await? {
            return Err(Status::not_found("unknown change"));
        }
        for r in &req.remove {
            store::remove_request(&mut tx, ns, change, r)
                .await
                .map_err(sql)?;
        }
        for r in &req.add {
            store::request_review(&mut tx, ns, change, r, handle, at)
                .await
                .map_err(sql)?;
        }
        if store::request_count(&mut tx, ns, change)
            .await
            .map_err(sql)?
            > MAX_REQUESTS_PER_CHANGE
        {
            return Err(Status::resource_exhausted(
                "the change has too many review requests",
            ));
        }
        let requests = store::requests(&mut tx, ns, change).await.map_err(sql)?;
        tx.commit().await.map_err(sql)?;
        Ok(wire::RequestReviewResponse { requests })
    }
}

// ------------------------------------------------------------------------------ service

/// The `asc.v1.Review` tonic service.
pub struct ReviewService {
    hub: Arc<Reviews>,
    env: Env,
}

impl ReviewService {
    pub fn new(hub: Arc<Reviews>, authorizer: Arc<dyn Authorizer>, info: ServerInfo) -> Self {
        Self {
            hub,
            env: Env { authorizer, info },
        }
    }

    /// The tonic service with message limits, zstd request decoding and [`AuthInterceptor`].
    pub fn into_service(self) -> InterceptedService<ReviewServer<Self>, AuthInterceptor> {
        let server = ReviewServer::new(self)
            .max_decoding_message_size(MAX_MESSAGE)
            .max_encoding_message_size(MAX_MESSAGE)
            .accept_compressed(tonic::codec::CompressionEncoding::Zstd);
        InterceptedService::new(server, AuthInterceptor)
    }
}

#[tonic::async_trait]
impl Review for ReviewService {
    async fn list_threads(
        &self,
        request: Request<wire::ListThreadsRequest>,
    ) -> Result<Response<wire::ListThreadsResponse>, Status> {
        let (meta, ext, msg) = request.into_parts();
        let ctx = self
            .env
            .context(&meta, &ext, msg.repo.as_ref(), Access::Read, false)
            .await?;
        self.hub.list_threads(&ctx, msg).await.map(Response::new)
    }

    async fn add_comment(
        &self,
        request: Request<wire::AddCommentRequest>,
    ) -> Result<Response<wire::AddCommentResponse>, Status> {
        let (meta, ext, msg) = request.into_parts();
        let ctx = self
            .env
            .context(&meta, &ext, msg.repo.as_ref(), Access::Read, true)
            .await?;
        let handle = require_handle(&ctx)?;
        self.hub
            .add_comment(&ctx, &handle, msg)
            .await
            .map(Response::new)
    }

    async fn resolve_thread(
        &self,
        request: Request<wire::ResolveThreadRequest>,
    ) -> Result<Response<wire::ResolveThreadResponse>, Status> {
        let (meta, ext, msg) = request.into_parts();
        let ctx = self
            .env
            .context(&meta, &ext, msg.repo.as_ref(), Access::Read, false)
            .await?;
        let handle = require_handle(&ctx)?;
        let can_write = self.env.has(&ext, &ctx, Access::Write).await;
        self.hub
            .resolve_thread(&ctx, &handle, can_write, msg)
            .await
            .map(Response::new)
    }

    async fn request_review(
        &self,
        request: Request<wire::RequestReviewRequest>,
    ) -> Result<Response<wire::RequestReviewResponse>, Status> {
        let (meta, ext, msg) = request.into_parts();
        let ctx = self
            .env
            .context(&meta, &ext, msg.repo.as_ref(), Access::Write, false)
            .await?;
        let handle = require_handle(&ctx)?;
        self.hub
            .request_review(&ctx, &handle, msg)
            .await
            .map(Response::new)
    }
}
