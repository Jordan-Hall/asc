//! The `Inbox` tonic service: `List`, `Count` and `SetState` (`inbox.proto`).
//!
//! Every call needs repo read and a user identity; the inbox is the caller's own and is keyed by
//! the verified handle, never by a request field.

use std::sync::Arc;

use asc_core::change::ChangeId;
use asc_rpc::auth::{Access, AuthInterceptor, Authorizer};
use asc_rpc::server::{CallCtx, MAX_MESSAGE, ServerInfo, wire};
use asc_rpc::services::inbox_server::{Inbox, InboxServer};
use sqlx::Row;
use tonic::service::interceptor::InterceptedService;
use tonic::{Request, Response, Status};

use super::{DEFAULT_PAGE, MAX_PAGE, MAX_SNOOZE, MAX_STATE_ROWS, etag, sections_of, visible};
use crate::review::{
    Env, Reviews, after_token, change_of, now, page_limit, parse_after, require_handle, sql, store,
};

impl Reviews {
    /// `Inbox.List`.
    pub async fn inbox_list(
        &self,
        ctx: &CallCtx,
        handle: &str,
        req: wire::InboxListRequest,
    ) -> Result<wire::InboxListResponse, Status> {
        let sections = sections_of(&req.sections)?;
        let limit = page_limit(req.limit, DEFAULT_PAGE, MAX_PAGE);
        let scope = format!("Inbox.List|{}", ctx.repo);
        let offset = usize::try_from(parse_after(&scope, &ctx.principal.id, &req.page_token)?)
            .unwrap_or(usize::MAX);
        let entry = self.open(ctx).await?;
        let at = now();
        let items: Vec<wire::InboxItem> = self
            .inbox_items(&entry, handle)
            .await?
            .into_iter()
            .filter(|i| sections.iter().any(|s| *s as i32 == i.section))
            .filter(|i| visible(i, at, req.include_snoozed))
            .collect();
        let total = u32::try_from(items.len()).unwrap_or(u32::MAX);
        let page: Vec<wire::InboxItem> = items.into_iter().skip(offset).take(limit + 1).collect();
        let next_page_token = if page.len() > limit {
            after_token(
                &scope,
                &ctx.principal.id,
                i64::try_from(offset + limit).unwrap_or(i64::MAX),
            )
        } else {
            String::new()
        };
        Ok(wire::InboxListResponse {
            items: page.into_iter().take(limit).collect(),
            total,
            next_page_token,
        })
    }

    /// `Inbox.Count`.
    pub async fn inbox_count(
        &self,
        ctx: &CallCtx,
        handle: &str,
        req: wire::InboxCountRequest,
    ) -> Result<wire::InboxCountResponse, Status> {
        let sections = sections_of(&req.sections)?;
        let entry = self.open(ctx).await?;
        let at = now();
        let items = self.inbox_items(&entry, handle).await?;
        let counts: Vec<wire::InboxSectionCount> = sections
            .iter()
            .map(|s| wire::InboxSectionCount {
                section: *s as i32,
                count: u32::try_from(
                    items
                        .iter()
                        .filter(|i| i.section == *s as i32 && visible(i, at, false))
                        .count(),
                )
                .unwrap_or(u32::MAX),
            })
            .collect();
        let tag = etag(&counts);
        if !req.if_none_match.is_empty() && req.if_none_match == tag {
            return Ok(wire::InboxCountResponse {
                etag: tag,
                not_modified: true,
                ..Default::default()
            });
        }
        Ok(wire::InboxCountResponse {
            total: counts.iter().map(|c| c.count).sum(),
            counts,
            etag: tag,
            not_modified: false,
        })
    }

    /// `Inbox.SetState`: pin and snooze one change for `handle`.
    pub async fn inbox_set_state(
        &self,
        ctx: &CallCtx,
        handle: &str,
        req: wire::InboxSetStateRequest,
    ) -> Result<wire::InboxSetStateResponse, Status> {
        let change = change_of(&req.change)?;
        let at = now();
        if let Some(t) = req.snoozed_until
            && (t < 0 || t > at + MAX_SNOOZE)
        {
            return Err(Status::invalid_argument("snoozed_until is out of range"));
        }
        let entry = self.open(ctx).await?;
        let ns = entry.ns.as_bytes().to_vec();
        let mut tx = self.registry.pool().begin().await.map_err(sql)?;
        self.registry.guard(&mut tx, &entry).await?;
        if !store::change_known(&mut tx, entry.ns, change).await? {
            return Err(Status::not_found("unknown change"));
        }
        let row = sqlx::query(
            "SELECT pinned, snoozed_until FROM inbox_state \
             WHERE ns = $1 AND handle = $2 AND change = $3",
        )
        .bind(ns.clone())
        .bind(handle.to_owned())
        .bind(change.as_bytes().to_vec())
        .fetch_optional(&mut *tx)
        .await
        .map_err(sql)?;
        let (was_pinned, was_snoozed): (i64, i64) = match &row {
            Some(r) => (
                r.try_get("pinned").map_err(sql)?,
                r.try_get("snoozed_until").map_err(sql)?,
            ),
            None => (0, 0),
        };
        let pinned = req.pinned.map_or(was_pinned != 0, |p| p);
        let snoozed = req.snoozed_until.map_or(was_snoozed, |t| t);
        // A snooze that has passed is no snooze.
        let snoozed = if snoozed <= at { 0 } else { snoozed };
        if !pinned && snoozed == 0 {
            sqlx::query("DELETE FROM inbox_state WHERE ns = $1 AND handle = $2 AND change = $3")
                .bind(ns)
                .bind(handle.to_owned())
                .bind(change.as_bytes().to_vec())
                .execute(&mut *tx)
                .await
                .map_err(sql)?;
        } else {
            if row.is_none() {
                let n: i64 = sqlx::query_scalar(
                    "SELECT CAST(COUNT(*) AS BIGINT) FROM inbox_state WHERE ns = $1 AND handle = $2",
                )
                .bind(ns.clone())
                .bind(handle.to_owned())
                .fetch_one(&mut *tx)
                .await
                .map_err(sql)?;
                if n >= MAX_STATE_ROWS {
                    return Err(Status::resource_exhausted(
                        "too many pinned or snoozed changes",
                    ));
                }
            }
            upsert_state(&mut tx, &ns, handle, change, pinned, snoozed, at).await?;
        }
        tx.commit().await.map_err(sql)?;
        Ok(wire::InboxSetStateResponse {
            pinned,
            snoozed_until: snoozed,
        })
    }
}

async fn upsert_state(
    conn: &mut sqlx::AnyConnection,
    ns: &[u8],
    handle: &str,
    change: ChangeId,
    pinned: bool,
    snoozed_until: i64,
    at: i64,
) -> Result<(), Status> {
    sqlx::query(
        "INSERT INTO inbox_state (ns, handle, change, pinned, snoozed_until, updated) \
         VALUES ($1, $2, $3, $4, $5, $6) \
         ON CONFLICT (ns, handle, change) DO UPDATE \
         SET pinned = $4, snoozed_until = $5, updated = $6",
    )
    .bind(ns.to_vec())
    .bind(handle.to_owned())
    .bind(change.as_bytes().to_vec())
    .bind(i64::from(pinned))
    .bind(snoozed_until)
    .bind(at)
    .execute(&mut *conn)
    .await
    .map_err(sql)?;
    Ok(())
}

/// The `asc.v1.Inbox` tonic service.
pub struct InboxService {
    hub: Arc<Reviews>,
    env: Env,
}

impl InboxService {
    pub fn new(hub: Arc<Reviews>, authorizer: Arc<dyn Authorizer>, info: ServerInfo) -> Self {
        Self {
            hub,
            env: Env { authorizer, info },
        }
    }

    /// The tonic service with message limits, zstd request decoding and [`AuthInterceptor`].
    pub fn into_service(self) -> InterceptedService<InboxServer<Self>, AuthInterceptor> {
        let server = InboxServer::new(self)
            .max_decoding_message_size(MAX_MESSAGE)
            .max_encoding_message_size(MAX_MESSAGE)
            .accept_compressed(tonic::codec::CompressionEncoding::Zstd);
        InterceptedService::new(server, AuthInterceptor)
    }
}

#[tonic::async_trait]
impl Inbox for InboxService {
    async fn list(
        &self,
        request: Request<wire::InboxListRequest>,
    ) -> Result<Response<wire::InboxListResponse>, Status> {
        let (meta, ext, msg) = request.into_parts();
        let ctx = self
            .env
            .context(&meta, &ext, msg.repo.as_ref(), Access::Read, false)
            .await?;
        let handle = require_handle(&ctx)?;
        self.hub
            .inbox_list(&ctx, &handle, msg)
            .await
            .map(Response::new)
    }

    async fn count(
        &self,
        request: Request<wire::InboxCountRequest>,
    ) -> Result<Response<wire::InboxCountResponse>, Status> {
        let (meta, ext, msg) = request.into_parts();
        let ctx = self
            .env
            .context(&meta, &ext, msg.repo.as_ref(), Access::Read, false)
            .await?;
        let handle = require_handle(&ctx)?;
        self.hub
            .inbox_count(&ctx, &handle, msg)
            .await
            .map(Response::new)
    }

    async fn set_state(
        &self,
        request: Request<wire::InboxSetStateRequest>,
    ) -> Result<Response<wire::InboxSetStateResponse>, Status> {
        let (meta, ext, msg) = request.into_parts();
        let ctx = self
            .env
            .context(&meta, &ext, msg.repo.as_ref(), Access::Read, false)
            .await?;
        let handle = require_handle(&ctx)?;
        self.hub
            .inbox_set_state(&ctx, &handle, msg)
            .await
            .map(Response::new)
    }
}
