//! The `Stacks` tonic service (`stacks.proto`): the ref log and ref undo ([`super::undo`]), and the
//! two whole-stack operations (`Restack`, `RevertLanded`), which a [`StackOps`] implementation
//! provides. Without one registered those two answer `UNIMPLEMENTED` naming the method, the way a
//! server that lacks the feature does.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock, PoisonError};

use asc_rpc::auth::{Access, AuthInterceptor, Authorizer};
use asc_rpc::errors::{ErrorInfo, RpcError};
use asc_rpc::unavailable::not_available;
use asc_rpc::server::{CallCtx, MAX_MESSAGE, ServerInfo, rpc_status, wire};
use asc_rpc::services::stacks_server::{Stacks, StacksServer};
use async_trait::async_trait;
use tonic::service::interceptor::InterceptedService;
use tonic::{Request, Response, Status};

use super::undo::{Caller, RefLog};
use crate::refpolicy::owner_of;
use crate::review::Env;

/// The whole-stack operations of `Stacks`. The unit that rewrites and reverts stacks registers one
/// with [`StackOpsSlot::set`].
#[async_trait]
pub trait StackOps: Send + Sync {
    async fn restack(
        &self,
        ctx: &CallCtx,
        req: wire::RestackStackRequest,
    ) -> Result<wire::RestackStackResponse, Status>;

    async fn revert_landed(
        &self,
        ctx: &CallCtx,
        req: wire::RevertLandedRequest,
    ) -> Result<wire::RevertLandedResponse, Status>;
}

/// Where the [`StackOps`] of the server are registered, once.
#[derive(Default)]
pub struct StackOpsSlot(OnceLock<Arc<dyn StackOps>>);

impl StackOpsSlot {
    /// Registers `ops`; false when some are registered.
    pub fn set(&self, ops: Arc<dyn StackOps>) -> bool {
        self.0.set(ops).is_ok()
    }

    pub fn get(&self) -> Option<&Arc<dyn StackOps>> {
        self.0.get()
    }
}

/// Restacks that apply, one at a time per repository. A restack reads the stack, plans and moves
/// its refs; two at once would plan from the same heads and the second would rewrite what the
/// first just rewrote. The lock covers the pin check and the apply together, so the heads a
/// caller pinned are the heads the apply starts from. It is per repository, which keeps every
/// stack of a repository in order and so every stack on its own. Across server processes the
/// atomic ref update of the apply is the guard.
#[derive(Default)]
pub struct RestackLocks(Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>);

impl RestackLocks {
    async fn lock(&self, repo: &str) -> tokio::sync::OwnedMutexGuard<()> {
        let one = {
            let mut map = self.0.lock().unwrap_or_else(PoisonError::into_inner);
            // An entry only this map holds has no holder and no waiter.
            map.retain(|_, m| Arc::strong_count(m) > 1);
            map.entry(repo.to_owned()).or_default().clone()
        };
        one.lock_owned().await
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Checks the plan an apply is about to carry out against the heads the caller pinned. Refused
/// with `REF_CONFLICT` (ABORTED), naming the changes at fault, when a pinned change has another
/// head now or is no longer in the stack, or when the call would rewrite a change that is not
/// pinned.
fn check_pins(
    pins: &[wire::ExpectedHead],
    plan: &wire::RestackStackResponse,
) -> Result<(), Status> {
    let mut pinned: HashMap<&[u8], &[u8]> = HashMap::with_capacity(pins.len());
    for p in pins {
        if p.change.is_empty() {
            return Err(Status::invalid_argument(
                "expected_heads: a change id is required",
            ));
        }
        if pinned
            .insert(p.change.as_ref(), p.old_head.as_ref())
            .is_some_and(|prev| prev != p.old_head.as_ref())
        {
            return Err(Status::invalid_argument(
                "expected_heads: a change is pinned to two heads",
            ));
        }
    }
    let mut stale: Vec<String> = Vec::new();
    let mut seen: Vec<&[u8]> = Vec::with_capacity(plan.items.len());
    for item in &plan.items {
        seen.push(item.change.as_ref());
        let rewrites = item.state == wire::RestackItemState::Rewritten as i32;
        match pinned.get(item.change.as_ref()) {
            Some(head) if *head == item.old_head.as_ref() => {}
            Some(_) => stale.push(hex(&item.change)),
            None if rewrites => stale.push(hex(&item.change)),
            None => {}
        }
    }
    for p in pins {
        if !seen.contains(&p.change.as_ref()) {
            stale.push(hex(&p.change));
        }
    }
    if stale.is_empty() {
        return Ok(());
    }
    stale.dedup();
    Err(rpc_status(&RpcError::RefConflict(ErrorInfo {
        paths: stale,
        ..ErrorInfo::default()
    })))
}

/// `Stacks.Restack` around the registered [`StackOps`]: serialised per repository, and, when the
/// caller pinned `expected_heads`, applied only if a fresh plan under the lock still matches them.
async fn restack_guarded(
    locks: &RestackLocks,
    ops: &dyn StackOps,
    ctx: &CallCtx,
    req: wire::RestackStackRequest,
) -> Result<wire::RestackStackResponse, Status> {
    // A dry run stores nothing and ignores the pins.
    if req.dry_run {
        return ops.restack(ctx, req).await;
    }
    let _held = locks.lock(ctx.repo.as_str()).await;
    if !req.expected_heads.is_empty() {
        let plan_req = wire::RestackStackRequest {
            dry_run: true,
            expected_heads: Vec::new(),
            ..req.clone()
        };
        let plan = ops.restack(ctx, plan_req).await?;
        check_pins(&req.expected_heads, &plan)?;
    }
    ops.restack(ctx, req).await
}

/// The `Stacks` service.
pub struct StacksService {
    log: Arc<RefLog>,
    ops: Arc<super::Audit>,
    env: Env,
    locks: RestackLocks,
}

impl StacksService {
    pub fn new(
        audit: Arc<super::Audit>,
        authorizer: Arc<dyn Authorizer>,
        info: ServerInfo,
    ) -> Self {
        Self {
            log: audit.ref_log.clone(),
            ops: audit,
            env: Env { authorizer, info },
            locks: RestackLocks::default(),
        }
    }

    /// The tonic service with message limits, zstd request decoding and [`AuthInterceptor`].
    pub fn into_service(self) -> InterceptedService<StacksServer<Self>, AuthInterceptor> {
        let server = StacksServer::new(self)
            .max_decoding_message_size(MAX_MESSAGE)
            .max_encoding_message_size(MAX_MESSAGE)
            .accept_compressed(tonic::codec::CompressionEncoding::Zstd);
        InterceptedService::new(server, AuthInterceptor)
    }

    async fn caller(&self, ext: &tonic::Extensions, ctx: &CallCtx) -> Caller {
        Caller {
            id: ctx.principal.id.clone(),
            owner: owner_of(&ctx.principal),
            maintainer: self.env.has(ext, ctx, Access::Admin).await,
        }
    }
}

#[tonic::async_trait]
impl Stacks for StacksService {
    async fn restack(
        &self,
        request: Request<wire::RestackStackRequest>,
    ) -> Result<Response<wire::RestackStackResponse>, Status> {
        let (meta, ext, msg) = request.into_parts();
        let ctx = self
            .env
            .context(&meta, &ext, msg.repo.as_ref(), Access::Write, true)
            .await?;
        match self.ops.stack_ops.get() {
            Some(ops) => restack_guarded(&self.locks, ops.as_ref(), &ctx, msg)
                .await
                .map(Response::new),
            None => Err(not_available("asc.v1.Stacks/Restack")),
        }
    }

    async fn revert_landed(
        &self,
        request: Request<wire::RevertLandedRequest>,
    ) -> Result<Response<wire::RevertLandedResponse>, Status> {
        let (meta, ext, msg) = request.into_parts();
        let ctx = self
            .env
            .context(&meta, &ext, msg.repo.as_ref(), Access::Write, true)
            .await?;
        match self.ops.stack_ops.get() {
            Some(ops) => ops.revert_landed(&ctx, msg).await.map(Response::new),
            None => Err(not_available("asc.v1.Stacks/RevertLanded")),
        }
    }

    async fn list_ref_log(
        &self,
        request: Request<wire::ListRefLogRequest>,
    ) -> Result<Response<wire::ListRefLogResponse>, Status> {
        let (meta, ext, msg) = request.into_parts();
        let ctx = self
            .env
            .context(&meta, &ext, msg.repo.as_ref(), Access::Read, false)
            .await?;
        let caller = self.caller(&ext, &ctx).await;
        self.log.list(&ctx, &caller, msg).await.map(Response::new)
    }

    async fn undo_ref(
        &self,
        request: Request<wire::UndoRefRequest>,
    ) -> Result<Response<wire::UndoRefResponse>, Status> {
        let (meta, ext, msg) = request.into_parts();
        let ctx = self
            .env
            .context(&meta, &ext, msg.repo.as_ref(), Access::Write, true)
            .await?;
        let caller = self.caller(&ext, &ctx).await;
        self.log.undo(&ctx, &caller, msg).await.map(Response::new)
    }
}

#[cfg(test)]
#[path = "restack_guard_tests.rs"]
mod restack_guard_tests;
