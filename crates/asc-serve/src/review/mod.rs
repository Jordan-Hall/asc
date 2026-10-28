//! Review threads, review requests and the shared plumbing of the `Review`, `Inbox` and `Ci`
//! services (host-neutral; `review.proto`, `inbox.proto`, `ci.proto`).
//!
//! The three services share one hub ([`Reviews`]) and one call-context routine ([`Env`]). Every
//! authorisation decision is made through the server's [`Authorizer`] before a handler runs, with
//! the access the RPC documents. A *user* is a stable handle taken from the verified principal
//! ([`handle_of`]), never from a request field, so the same person has the same inbox whether
//! they sign in through a forge or a personal token.
//!
//! Ids (threads, comments, runs, jobs) are per namespace and allocated as `max + 1` inside the
//! writing transaction; a lost race is a primary-key conflict and the writer retries
//! ([`retry_conflict`]).

pub mod acting;
pub mod brief;
pub mod gate;
pub mod hunk;
pub mod rpc;
pub mod store;

#[cfg(test)]
pub(crate) mod tests;
#[cfg(test)]
pub(crate) mod tests_acting;
#[cfg(test)]
mod tests_brief;
#[cfg(test)]
mod tests_gate;
#[cfg(test)]
mod tests_hunk;

use std::sync::{Arc, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use asc_rpc::IdMode;
use asc_rpc::RpcError;
use asc_rpc::auth::{Access, Authorizer, Credential};
use asc_rpc::errors::ErrorInfo;
use asc_rpc::server::{CallCtx, ClientVersion, RepoName, ServerInfo, rpc_status, wire};
use sqlx::AnyConnection;
use tonic::metadata::MetadataMap;
use tonic::{Extensions as TonicExt, Status};
use uuid::Uuid;

use crate::authz::UserIdentity;
use crate::registry::{Registry, RegistryError};

/// What `Version.features` adds for the `Review` service.
pub const FEATURE: &str = "review.v1";
/// Longest comment body, in bytes.
pub const MAX_BODY: usize = 64 * 1024;
/// Longest anchored path, in bytes.
pub const MAX_PATH: usize = 4096;
/// Longest user handle, in bytes.
pub const MAX_HANDLE: usize = 255;
/// Comments one thread holds.
pub const MAX_COMMENTS_PER_THREAD: i64 = 1_000;
/// Threads one change holds.
pub const MAX_THREADS_PER_CHANGE: i64 = 5_000;
/// Handles in one `add` or `remove` list.
pub const MAX_REVIEWERS: usize = 16;
/// Pending or finished review requests one change holds.
pub const MAX_REQUESTS_PER_CHANGE: i64 = 64;
/// Threads in one `ListThreads` page when the request names no limit.
pub const DEFAULT_PAGE: usize = 50;
/// Threads in one `ListThreads` page, at most.
pub const MAX_PAGE: usize = 200;
/// Attempts of a write that lost an id race.
const ATTEMPTS: usize = 5;

/// What `Version.features` adds for the three services.
pub fn features() -> Vec<String> {
    vec![
        FEATURE.to_owned(),
        crate::inbox::FEATURE.to_owned(),
        crate::ci::FEATURE.to_owned(),
        crate::queue::FEATURE.to_owned(),
    ]
}

pub(crate) fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|d| i64::try_from(d.as_secs()).ok())
        .unwrap_or(0)
}

pub(crate) fn internal(context: &str, e: &dyn std::fmt::Display) -> Status {
    crate::commit_graph_rpc::internal(context, e)
}

pub(crate) fn sql(e: sqlx::Error) -> Status {
    internal("database", &e)
}

pub(crate) fn registry_status(e: RegistryError) -> Status {
    crate::commit_graph_rpc::registry_status(e)
}

// ------------------------------------------------------------------------------ handles

/// Whether `s` can be a user handle: 1 to [`MAX_HANDLE`] bytes of `[A-Za-z0-9._@+-]`.
pub fn valid_handle(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= MAX_HANDLE
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-' | b'@' | b'+'))
}

/// The handle of the user a principal acts as: the login of a provider-verified user
/// (`<id>:<login>`) or the user of a personal token (`pat:<user>`). A deploy key, an Actions task,
/// a CI token, the anonymous caller and every other principal act as no user and have none.
pub fn handle_of(principal_id: &str) -> Option<String> {
    if let Some(who) = UserIdentity::parse(principal_id) {
        return who.is_user().then_some(who.handle);
    }
    let user = principal_id.strip_prefix("pat:")?;
    valid_handle(user).then(|| user.to_owned())
}

/// The caller's handle, or `PERMISSION_DENIED`.
pub(crate) fn require_handle(ctx: &CallCtx) -> Result<String, Status> {
    handle_of(&ctx.principal.id)
        .ok_or_else(|| Status::permission_denied("this call needs a user identity"))
}

// ------------------------------------------------------------------------------ hub

/// The state the `Review`, `Inbox` and `Ci` handlers share.
pub struct Reviews {
    pub(crate) registry: Arc<Registry>,
}

/// The place the server's single [`Reviews`] hub is registered, once (read by `build_routes`).
#[derive(Default)]
pub struct ReviewSlot(OnceLock<Arc<Reviews>>);

impl ReviewSlot {
    /// Registers `hub`; false when one is already registered.
    pub fn set(&self, hub: Arc<Reviews>) -> bool {
        self.0.set(hub).is_ok()
    }

    pub fn get(&self) -> Option<&Arc<Reviews>> {
        self.0.get()
    }
}

/// The unit's one line in `register_all`.
pub fn register(ext: &crate::registry::Extensions, registry: &Arc<Registry>) {
    let hub = Arc::new(Reviews {
        registry: registry.clone(),
    });
    assert!(ext.review.set(hub), "the review hub is registered once");
}

impl Reviews {
    pub fn new(registry: Arc<Registry>) -> Self {
        Self { registry }
    }

    /// Opens the repository of `ctx`.
    pub(crate) async fn open(
        &self,
        ctx: &CallCtx,
    ) -> Result<Arc<crate::registry::RepoEntry>, Status> {
        self.registry.open(&ctx.repo).await.map_err(registry_status)
    }
}

// ------------------------------------------------------------------------------ call context

/// The authorizer and server facts one service needs to build a [`CallCtx`].
#[derive(Clone)]
pub(crate) struct Env {
    pub(crate) authorizer: Arc<dyn Authorizer>,
    pub(crate) info: ServerInfo,
}

impl Env {
    /// Validates the repository, checks the client version and authorises `access`.
    pub(crate) async fn context(
        &self,
        meta: &MetadataMap,
        ext: &TonicExt,
        repo: Option<&wire::RepoRef>,
        access: Access,
        mutating: bool,
    ) -> Result<CallCtx, Status> {
        let repo = repo.ok_or_else(|| Status::invalid_argument("missing repo"))?;
        let name = RepoName::parse(&repo.name)?;
        let id_mode =
            IdMode::from_wire(repo.id_mode).map_err(|e| Status::invalid_argument(e.to_string()))?;
        let client = match meta.get("x-asc-client") {
            None => None,
            Some(v) => {
                let text = v
                    .to_str()
                    .map_err(|_| Status::invalid_argument("malformed x-asc-client"))?;
                ClientVersion::from_header(text)?
            }
        };
        if let (Some(min), Some(got)) = (self.info.min_client, client)
            && got < min
        {
            let mut info = ErrorInfo::default();
            info.paths.push(format!("minimum client version is {min}"));
            return Err(rpc_status(&RpcError::ClientTooOld(info)));
        }
        let principal = self
            .authorizer
            .authorize_credential(ext.get::<Credential>(), name.as_str(), access)
            .await?;
        let (principal, admin) = acting::act_as(&*self.authorizer, principal, meta, &name, access)?;
        let op_id = match meta.get("x-asc-op-id") {
            None => None,
            Some(v) => Some(
                v.to_str()
                    .ok()
                    .and_then(|s| Uuid::parse_str(s).ok())
                    .ok_or_else(|| Status::invalid_argument("malformed x-asc-op-id"))?,
            ),
        };
        if mutating && op_id.is_none() {
            return Err(Status::invalid_argument("mutating calls need x-asc-op-id"));
        }
        Ok(CallCtx {
            repo: name,
            id_mode,
            principal,
            op_id,
            client,
            admin,
        })
    }

    /// Whether the caller also holds `access` on the repository of `ctx` (a refusal means no).
    /// A call made on the hub's claim holds what the claim's role covers and nothing more.
    pub(crate) async fn has(&self, ext: &TonicExt, ctx: &CallCtx, access: Access) -> bool {
        if let Some(c) = ctx.admin.forwarded_claim() {
            return c.allows(acting::role_of(access));
        }
        self.authorizer
            .authorize_credential(ext.get::<Credential>(), ctx.repo.as_str(), access)
            .await
            .is_ok()
    }
}

// ------------------------------------------------------------------------------ writes

/// Why a write attempt failed: a database error (a unique violation is retried) or a final answer.
pub(crate) enum WriteError {
    Sql(sqlx::Error),
    Status(Status),
}

impl From<sqlx::Error> for WriteError {
    fn from(e: sqlx::Error) -> Self {
        Self::Sql(e)
    }
}

impl From<Status> for WriteError {
    fn from(s: Status) -> Self {
        Self::Status(s)
    }
}

impl WriteError {
    fn into_status(self) -> Status {
        match self {
            Self::Sql(e) => sql(e),
            Self::Status(s) => s,
        }
    }

    fn is_conflict(&self) -> bool {
        matches!(self, Self::Sql(sqlx::Error::Database(d)) if d.is_unique_violation())
    }
}

/// Runs `attempt` until it stops failing on a primary-key conflict (another writer took the id
/// first), at most [`ATTEMPTS`] times. Each attempt opens its own transaction.
pub(crate) async fn retry_conflict<T, F, Fut>(mut attempt: F) -> Result<T, Status>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T, WriteError>>,
{
    for _ in 0..ATTEMPTS {
        match attempt().await {
            Ok(v) => return Ok(v),
            Err(e) if e.is_conflict() => {}
            Err(e) => return Err(e.into_status()),
        }
    }
    Err(Status::aborted("concurrent writers; retry"))
}

/// The next id of `table` in `ns`: one more than the largest, or 1. `table` is a literal of this
/// crate.
pub(crate) async fn next_id(
    conn: &mut AnyConnection,
    table: &'static str,
    ns: &[u8],
) -> Result<i64, sqlx::Error> {
    let q = format!("SELECT CAST(COALESCE(MAX(id), 0) AS BIGINT) FROM {table} WHERE ns = $1");
    let max: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(q))
        .bind(ns.to_vec())
        .fetch_one(&mut *conn)
        .await?;
    Ok(max + 1)
}

/// `$start, $start+1, ...` for `n` bind slots.
pub(crate) fn placeholders(start: usize, n: usize) -> String {
    (start..start + n)
        .map(|i| format!("${i}"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// A 16-byte change id from the wire.
pub(crate) fn change_of(b: &[u8]) -> Result<asc_core::change::ChangeId, Status> {
    <[u8; 16]>::try_from(b)
        .map(asc_core::change::ChangeId::from_bytes)
        .map_err(|_| Status::invalid_argument("a change id is 16 bytes"))
}

/// The page size a request asks for.
pub(crate) fn page_limit(asked: u32, default: usize, max: usize) -> usize {
    match asked {
        0 => default,
        n => usize::try_from(n).unwrap_or(max).min(max),
    }
}

/// The cursor of an opaque page token (the last id of the previous page); 0 for none. `scope`
/// names the call and repository, `who` the caller: a token is good for those only.
pub(crate) fn parse_after(scope: &str, who: &str, token: &str) -> Result<i64, Status> {
    Ok(crate::pagetoken::open_id(scope, who, token)?.unwrap_or(0))
}

/// The opaque page token that continues after `id`.
pub(crate) fn after_token(scope: &str, who: &str, id: impl std::fmt::Display) -> String {
    crate::pagetoken::seal(scope, who, &id.to_string())
}
