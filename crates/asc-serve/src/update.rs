//! The `UpdateRefs` pipeline (SPEC §8.3, PLAN U43b).
//!
//! Steps 1 to 5, 8 and 9 live here. Steps 6 and 7 (bridge quarantine, pre-receive) are the
//! [`PushHooks`] extension point, a no-op until Forgejo arrives (U44). Step 4 runs under the
//! shared GC lock, which is held until the step-8 transaction commits (SPEC §3.6). LAND mode is
//! the [`crate::land`] unit.

use std::collections::{BTreeMap, HashSet};
use std::sync::{Arc, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use asc_core::change::RewriteOp;
use asc_core::id::{ChangesetId, RefTarget};
use asc_core::path::RefName;
use asc_engine::graph::Graph;
use asc_rpc::IdMode;
use asc_rpc::auth::Principal;
use asc_rpc::errors::{ErrorInfo, RpcError};
use asc_rpc::server::{CallCtx, MAX_REF_UPDATES, RefsHandler, RepoName, rpc_status, wire};
use asc_store::refs::{Actor, Expect, RefError, RefTxn, RefUpdate, Refs};
use async_trait::async_trait;
use bytes::Bytes;
use prost::Message;
use serde::Serialize;
use sqlx::{AnyPool, Row};
use tokio::sync::{OwnedRwLockReadGuard, OwnedRwLockWriteGuard, RwLock};
use tonic::Status;
use uuid::Uuid;

use crate::agents::gate::{AgentGate, RefChange};
use crate::cloud::quota::{self, UserQuota};
use crate::gc::{ClusterLock, PgClusterLock};
use crate::graph_sql::SqlGraph;
use crate::ingest::{IngestError, IngestObserver, Ingestor, NoObserver};
use crate::land::{self, DisjointOnly, OverlapPolicy};
use crate::policy::gate::{GateMode, GateRequest, GateUpdate, RefGate};
use crate::refpolicy::UserRefWriter;
use crate::registry::{Extensions, Registry, RegistryError, RepoEntry, RepoMode};
use crate::{refauthz, refpolicy};

/// What the ref log records as `via` for a push through `UpdateRefs`.
const DEFAULT_VIA: &str = "asc-push";
/// Push options per request, and the longest one.
const MAX_PUSH_OPTIONS: usize = 64;
const MAX_PUSH_OPTION_LEN: usize = 1024;
/// Advisory lock key of the GC fence (Postgres).
pub(crate) const GC_LOCK_KEY: i64 = 0x0063_675f_6763;
/// Ref namespaces that are local to a client and never stored on a server (SPEC §2.7).
const LOCAL_ONLY: &[&str] = &["refs/remotes/", "refs/stash", "refs/bisect/", "refs/for/"];

mod submit;

/// The error code of a status, for the tests of the child modules.
#[cfg(test)]
pub(crate) fn tests_code(s: &Status) -> String {
    asc_rpc::server::status_rpc_error(s)
        .map_or_else(|| s.code().to_string(), |e| e.code().to_owned())
}

// ------------------------------------------------------------------------------ hook points

/// One ref change as the hooks see it.
#[derive(Clone, Debug)]
pub struct HookUpdate {
    pub name: String,
    pub old: Option<RefTarget>,
    pub new: Option<RefTarget>,
}

/// What steps 6 and 7 are told about a push.
pub struct HookRequest<'a> {
    pub repo: &'a RepoName,
    pub op_id: Uuid,
    pub principal: &'a Principal,
    pub updates: &'a [HookUpdate],
    pub push_options: &'a [String],
}

/// A hook refused the push; the message is shown to the user (`HOOK_REJECTED`).
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct HookRejected(pub String);

/// Steps 6 and 7 of the pipeline and the Forgejo-only permissions. Every method has a no-op or
/// refusing default, so a server without Forgejo accepts pushes and never skips hooks.
#[async_trait]
pub trait PushHooks: Send + Sync {
    /// Whether this call may send `hooks = SKIP_PRE` (the Forgejo internal token only).
    fn may_skip_pre(&self, _ctx: &CallCtx) -> bool {
        false
    }

    /// Step 6: export the new git objects into a quarantine (only with `forgejo_reads = git`).
    async fn bridge_quarantine(&self, _req: &HookRequest<'_>) -> Result<(), HookRejected> {
        Ok(())
    }

    /// Step 7: Forgejo's pre-receive hook.
    async fn pre_receive(&self, _req: &HookRequest<'_>) -> Result<(), HookRejected> {
        Ok(())
    }

    /// AGit `refs/for/*` (a submit): Forgejo's proc-receive creates or updates the pull request
    /// of each update and answers per ref (SPEC section 13.5). The default has no forge to ask.
    async fn proc_receive(
        &self,
        _req: &HookRequest<'_>,
    ) -> Result<Vec<crate::forgejo::ProcRefResult>, HookRejected> {
        Ok(Vec::new())
    }

    /// The push did not commit after step 6; remove the quarantine.
    async fn abort(&self, _req: &HookRequest<'_>) {}

    /// Whether `branch` requires signed commits for this caller (`branch_rules.require_signed`
    /// from authz): a land that would rewrite a signed stack is then refused (SPEC §8.5 step 4).
    async fn require_signed(&self, _ctx: &CallCtx, _branch: &str) -> bool {
        false
    }

    /// Effects the step-8 transaction queues in the outbox, in order: the forge's `post_receive`,
    /// then `codeintel.enqueue`, which queues the update's new files for the indexing workers
    /// (a no-op unless `[codeintel]` is enabled, see `outbox::Outbox`), then `notify.policy`,
    /// which tells the contributors when the update of trunk changed the policy.
    fn outbox_effects(&self) -> &'static [&'static str] {
        &[
            "post_receive",
            crate::codeintel::queue::EFFECT,
            crate::notify::policy::EFFECT,
        ]
    }
}

struct NoHooks;

impl PushHooks for NoHooks {}

/// The GC fence: pushes hold it shared from step 4 to their commit; a sweep batch takes it
/// exclusively (O01). It has two halves, always taken in this order: an in-process `RwLock`, and
/// on Postgres a [`ClusterLock`] (a session-level advisory lock) that spans the same interval in
/// every server process. SQLite is a single-process database, so there it is the `RwLock` alone.
#[derive(Clone, Default)]
pub struct GcLock {
    local: Arc<RwLock<()>>,
    cluster: Arc<OnceLock<Arc<dyn ClusterLock>>>,
}

/// Both halves of the fence, released together. The cluster half goes first.
pub struct GcHold<G> {
    _cluster: Option<Box<dyn Send>>,
    _local: G,
}

impl GcLock {
    /// Installs the cross-process half; the first call wins.
    pub fn attach(&self, lock: Arc<dyn ClusterLock>) {
        let _ = self.cluster.set(lock);
    }

    /// Installs the Postgres advisory lock when `pool` is Postgres; SQLite needs none.
    pub fn attach_pool(&self, pool: &AnyPool) {
        if self.cluster.get().is_none() && is_postgres(pool) {
            self.attach(Arc::new(PgClusterLock::new(pool.clone())));
        }
    }

    /// Held by a push from before its ingest until its ref commit, and by a mirror sync.
    pub async fn shared(&self) -> Result<GcHold<OwnedRwLockReadGuard<()>>, sqlx::Error> {
        let local = self.local.clone().read_owned().await;
        let cluster = match self.cluster.get() {
            Some(c) => Some(c.shared().await?),
            None => None,
        };
        Ok(GcHold {
            _cluster: cluster,
            _local: local,
        })
    }

    /// Held by a sweep batch.
    pub async fn exclusive(&self) -> Result<GcHold<OwnedRwLockWriteGuard<()>>, sqlx::Error> {
        let local = self.local.clone().write_owned().await;
        let cluster = match self.cluster.get() {
            Some(c) => Some(c.exclusive().await?),
            None => None,
        };
        Ok(GcHold {
            _cluster: cluster,
            _local: local,
        })
    }
}

/// The ingest and `UpdateRefs` extension points; each is set at most once (U43a registry).
#[derive(Default)]
pub struct PipelineHooks {
    observer: OnceLock<Arc<dyn IngestObserver>>,
    push: OnceLock<Arc<dyn PushHooks>>,
    quota: OnceLock<UserQuota>,
    overlap: OnceLock<Arc<dyn OverlapPolicy>>,
    agent: OnceLock<Arc<dyn AgentGate>>,
    gate: OnceLock<Arc<dyn RefGate>>,
    gc: GcLock,
    gc_roots: crate::gc::GcRoots,
}

impl PipelineHooks {
    /// Installs the stack-metadata observer (C18); fails (returning it) when one is set.
    pub fn set_observer(&self, o: Arc<dyn IngestObserver>) -> Result<(), Arc<dyn IngestObserver>> {
        self.observer.set(o)
    }

    /// Installs the Forgejo hooks (U44); fails (returning them) when some are set.
    pub fn set_push_hooks(&self, h: Arc<dyn PushHooks>) -> Result<(), Arc<dyn PushHooks>> {
        self.push.set(h)
    }

    /// Installs the per-user quota (`[cloud] max_user_refs` and `max_user_bytes`, SPEC §21.3.7);
    /// fails (returning it) when one is set. Without one the SPEC defaults apply: user-ref pushes
    /// are never unmetered.
    pub fn set_user_quota(&self, q: UserQuota) -> Result<(), UserQuota> {
        self.quota.set(q)
    }

    /// The quota every user-ref push is checked against and charged to.
    pub fn user_quota(&self) -> &UserQuota {
        self.quota.get().unwrap_or_else(|| UserQuota::defaults())
    }

    /// Installs the land overlap policy (U68 content merge); fails (returning it) when one is set.
    pub fn set_overlap_policy(
        &self,
        p: Arc<dyn OverlapPolicy>,
    ) -> Result<(), Arc<dyn OverlapPolicy>> {
        self.overlap.set(p)
    }

    /// How a land treats paths the stack shares with the branch (path-disjoint only by default).
    pub fn overlap_policy(&self) -> &dyn OverlapPolicy {
        self.overlap.get().map_or(&DisjointOnly, |p| &**p)
    }

    /// Installs the gate every ref update passes (repository policy, secret scanning, signatures);
    /// fails (returning it) when one is set. Without one nothing but the forge's hooks binds an update.
    pub fn set_gate(&self, g: Arc<dyn RefGate>) -> Result<(), Arc<dyn RefGate>> {
        self.gate.set(g)
    }

    /// The installed gate, if any.
    pub fn gate(&self) -> Option<&dyn RefGate> {
        self.gate.get().map(|g| &**g)
    }

    /// Installs the gate that binds agent principals (scope, budget, own branches only); fails
    /// (returning it) when one is set. Without one an agent token is just a token.
    pub fn set_agent_gate(&self, g: Arc<dyn AgentGate>) -> Result<(), Arc<dyn AgentGate>> {
        self.agent.set(g)
    }

    /// The installed agent gate, if any.
    pub fn agent_gate(&self) -> Option<&dyn AgentGate> {
        self.agent.get().map(|g| &**g)
    }

    pub fn observer(&self) -> &dyn IngestObserver {
        self.observer.get().map_or(&NoObserver, |o| &**o)
    }

    pub fn push_hooks(&self) -> &dyn PushHooks {
        self.push.get().map_or(&NoHooks, |h| &**h)
    }

    pub fn gc(&self) -> &GcLock {
        &self.gc
    }

    /// Where units register their `GcRootSource` (O01).
    pub fn gc_roots(&self) -> &crate::gc::GcRoots {
        &self.gc_roots
    }
}

// ------------------------------------------------------------------------------ errors

pub(crate) fn rpc(e: RpcError) -> Status {
    rpc_status(&e)
}

pub(crate) fn info(paths: Vec<String>) -> ErrorInfo {
    ErrorInfo {
        paths,
        ..ErrorInfo::default()
    }
}

pub(crate) fn internal(context: &str, e: &dyn std::fmt::Display) -> Status {
    tracing::error!(%context, error = %e, "internal error");
    Status::internal("internal error")
}

pub(crate) fn ingest_status(e: IngestError) -> Status {
    match e {
        IngestError::Missing(keys) => rpc(RpcError::MissingObjects(info(
            keys.iter().map(|k| k.path()).collect(),
        ))),
        IngestError::Invalid { key, why } => {
            tracing::debug!(%key, %why, "push rejected");
            rpc(RpcError::InvalidObject(info(vec![key])))
        }
        IngestError::PathPolicy { path, why } => {
            tracing::debug!(%path, %why, "push rejected");
            rpc(RpcError::PathPolicy(info(vec![path])))
        }
        IngestError::TooLarge => Status::resource_exhausted(e.to_string()),
        IngestError::QuotaExceeded(what) => refauthz::quota_status(&refauthz::QuotaDenied(what)),
        IngestError::Stale { gone: true } => Status::not_found("no such repository"),
        IngestError::Stale { gone: false } => {
            Status::failed_precondition("the repository was renamed or replaced; retry")
        }
        e => internal("ingest", &e),
    }
}

fn registry_status(e: RegistryError) -> Status {
    match e {
        RegistryError::NotFound(_) => Status::not_found("no such repository"),
        e => internal("registry", &e),
    }
}

pub(crate) fn conflict(c: asc_store::refs::RefConflict) -> Status {
    rpc(RpcError::RefConflict(ErrorInfo {
        paths: vec![String::from_utf8_lossy(c.name.as_bytes()).into_owned()],
        actual: c
            .actual
            .map_or_else(Bytes::new, |t| Bytes::copy_from_slice(&t.to_bytes())),
        conflicts: Vec::new(),
    }))
}

/// The principal the forge's hooks are told about. A call the host service made on a person's verified
/// claim (their id and login, and a role that covers a write) is that person's write: the forge attributes
/// it to them. Without such a claim it is the caller's own principal, which for the host service is no
/// registered person, so the forge refuses the write as unattributed. Nothing but a claim the server has
/// verified for this repository and call ever names the person.
pub(crate) fn acting_principal(ctx: &CallCtx) -> Principal {
    if crate::authz::is_host_service(&ctx.principal.id)
        && let Some(claim) = ctx.admin.forwarded_claim()
        && claim.allows(asc_rpc::forwarded::Role::Write)
        && let Some(who) = crate::authz::UserIdentity::parse(&claim.user)
        && who.user_id > 0
        && who.deploy_key_id == 0
    {
        return Principal {
            id: who.principal_id(),
        };
    }
    ctx.principal.clone()
}

pub(crate) fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

fn is_postgres(pool: &AnyPool) -> bool {
    matches!(
        pool.connect_options().database_url.scheme(),
        "postgres" | "postgresql"
    )
}

pub(crate) fn target_hex(t: &RefTarget) -> String {
    t.to_bytes().iter().map(|b| format!("{b:02x}")).collect()
}

// ------------------------------------------------------------------------------ the request

/// One validated update.
pub(crate) struct Planned {
    pub(crate) text: String,
    pub(crate) name: RefName,
    pub(crate) expect: Expect,
    pub(crate) new: Option<RefTarget>,
    /// A user ref (SPEC §21.5.1): `name` is the stored name, `text` the pushed one. It skips
    /// pre-receive, the bridge and the outbox `post_receive`.
    pub(crate) user: bool,
}

fn plan(req: &wire::UpdateRefsRequest) -> Result<Vec<Planned>, Status> {
    use wire::ref_update::Expect as Wire;
    let bad = |m: &str| Status::invalid_argument(m.to_owned());
    if req.updates.is_empty() || req.updates.len() > MAX_REF_UPDATES {
        return Err(bad("bad number of updates"));
    }
    let mut seen = HashSet::new();
    let mut out = Vec::with_capacity(req.updates.len());
    for u in &req.updates {
        let name =
            RefName::new(u.name.as_bytes()).map_err(|e| Status::invalid_argument(e.to_string()))?;
        if LOCAL_ONLY.iter().any(|p| u.name.starts_with(p)) {
            return Err(bad("that ref namespace is local to a client"));
        }
        if !seen.insert(name.clone()) {
            return Err(bad("a ref is updated twice"));
        }
        let new = match (&u.new, u.delete) {
            (Some(t), false) => {
                Some(RefTarget::try_from(t).map_err(|e| Status::invalid_argument(e.to_string()))?)
            }
            (None, true) => None,
            _ => return Err(bad("an update either sets a target or deletes")),
        };
        if u.name.starts_with("refs/heads/") && matches!(new, Some(RefTarget::Tag(_))) {
            return Err(bad("a branch must point at a changeset"));
        }
        let expect = match &u.expect {
            Some(Wire::Any(true)) => Expect::Any,
            Some(Wire::Absent(true)) => Expect::Absent,
            Some(Wire::Value(t)) => Expect::Value(
                RefTarget::try_from(t).map_err(|e| Status::invalid_argument(e.to_string()))?,
            ),
            _ => return Err(bad("an update needs an expectation")),
        };
        out.push(Planned {
            text: u.name.clone(),
            name,
            expect,
            new,
            user: false,
        });
    }
    Ok(out)
}

#[derive(Serialize)]
pub(crate) struct EffectRef {
    pub(crate) name: String,
    pub(crate) old: Option<String>,
    pub(crate) new: Option<String>,
}

/// The `outbox.effects` document: the effects in order and what `post_receive` reports.
#[derive(Serialize)]
pub(crate) struct OutboxEffects<'a> {
    pub(crate) effects: &'a [&'static str],
    pub(crate) pusher: &'a str,
    /// Targets are hex of the 33-byte wire form (kind byte, then the id).
    pub(crate) refs: Vec<EffectRef>,
}

// ------------------------------------------------------------------------------ the handler

/// The `Refs` handler group of the full server: `UpdateRefs` through the pipeline; `ListRefs`
/// comes from the repository's engine until U47 replaces it.
pub struct ServerRefs {
    pub(crate) registry: Arc<Registry>,
    pub(crate) hooks: Arc<PipelineHooks>,
}

impl ServerRefs {
    pub fn new(registry: Arc<Registry>, hooks: Arc<PipelineHooks>) -> Self {
        hooks.gc().attach_pool(registry.pool());
        Self { registry, hooks }
    }

    pub(crate) async fn windows_safe(&self, entry: &RepoEntry) -> Result<bool, Status> {
        let row = sqlx::query(
            "SELECT CAST(CASE WHEN windows_safe THEN 1 ELSE 0 END AS BIGINT) AS ws FROM repos WHERE id = $1",
        )
        .bind(entry.key.0)
        .fetch_one(self.registry.pool())
        .await
        .map_err(|e| internal("repos", &e))?;
        Ok(row
            .try_get::<i64, _>("ws")
            .map_err(|e| internal("repos", &e))?
            != 0)
    }

    /// Step 2: the response stored for `op_id`, if any.
    pub(crate) async fn stored(
        &self,
        entry: &RepoEntry,
        op_id: Uuid,
    ) -> Result<Option<wire::UpdateRefsResponse>, Status> {
        let row = sqlx::query("SELECT repo_id, result FROM op_results WHERE op_id = $1")
            .bind(op_id.as_bytes().to_vec())
            .fetch_optional(self.registry.pool())
            .await
            .map_err(|e| internal("op_results", &e))?;
        let Some(row) = row else { return Ok(None) };
        let repo: i64 = row
            .try_get("repo_id")
            .map_err(|e| internal("op_results", &e))?;
        if repo != entry.key.0 {
            return Err(Status::invalid_argument(
                "op id was used for another repository",
            ));
        }
        let bytes: Vec<u8> = row
            .try_get("result")
            .map_err(|e| internal("op_results", &e))?;
        wire::UpdateRefsResponse::decode(bytes.as_slice())
            .map(Some)
            .map_err(|e| internal("op_results", &e))
    }

    async fn update(
        &self,
        ctx: &CallCtx,
        req: wire::UpdateRefsRequest,
    ) -> Result<wire::UpdateRefsResponse, Status> {
        self.update_rewriting(ctx, req, None).await
    }

    /// `update`, with the `via` the ref log records for the push (`git-push` for the git protocol
    /// layer, SPEC §25.2); every other caller records `asc-push`.
    pub(crate) async fn update_via(
        &self,
        ctx: &CallCtx,
        req: wire::UpdateRefsRequest,
        via: &'static str,
    ) -> Result<wire::UpdateRefsResponse, Status> {
        self.update_rewriting_via(ctx, req, None, via).await
    }

    /// `update`, for a server-side `Merge` that rewrote changesets: the step-8 transaction also
    /// tells the [`IngestObserver`] about `rewrites` and the response lists them. A LAND request
    /// finds its own rewrites, so it takes none.
    pub(crate) async fn update_rewriting(
        &self,
        ctx: &CallCtx,
        req: wire::UpdateRefsRequest,
        rewrites: Option<Rewrites<'_>>,
    ) -> Result<wire::UpdateRefsResponse, Status> {
        self.update_rewriting_via(ctx, req, rewrites, DEFAULT_VIA)
            .await
    }

    /// `update_rewriting_via_inner`, with a `push.refused` audit event when the push is
    /// refused for a reason of authorisation, policy or a hook.
    async fn update_rewriting_via(
        &self,
        ctx: &CallCtx,
        req: wire::UpdateRefsRequest,
        rewrites: Option<Rewrites<'_>>,
        via: &'static str,
    ) -> Result<wire::UpdateRefsResponse, Status> {
        let refs: Vec<String> = req.updates.iter().map(|u| u.name.clone()).collect();
        let result = self
            .update_rewriting_via_inner(ctx, req, rewrites, via)
            .await;
        if let Err(status) = &result
            && let Some(code) = crate::audit::hooks::refusal_code(status)
        {
            crate::audit::hooks::record_refused(self.registry.pool(), ctx, &refs, via, &code).await;
        }
        result
    }

    async fn update_rewriting_via_inner(
        &self,
        ctx: &CallCtx,
        req: wire::UpdateRefsRequest,
        rewrites: Option<Rewrites<'_>>,
        via: &'static str,
    ) -> Result<wire::UpdateRefsResponse, Status> {
        // Step 1: the caller has write access (the service checked); validate the request.
        let op_id = ctx
            .op_id
            .ok_or_else(|| Status::invalid_argument("mutating calls need x-asc-op-id"))?;
        if ctx.id_mode != IdMode::Asc {
            return Err(Status::unimplemented(
                "git id mode is not served by UpdateRefs",
            ));
        }
        // `refs/for/` updates publish versions of changes; they are never stored (see `submit`).
        if req.updates.iter().any(|u| u.name.starts_with("refs/for/")) {
            return self.submit(ctx, req).await;
        }
        let mut planned = plan(&req)?;
        let mode = wire::UpdateMode::try_from(req.mode)
            .map_err(|_| Status::invalid_argument("unknown update mode"))?;
        let pushed = if mode == wire::UpdateMode::Land {
            Some(land::check_request(&planned, req.hooks)?)
        } else {
            None
        };
        // An agent writes only its own draft branches and never lands (agents/gate.rs).
        if let Some(agent) = self.hooks.agent_gate() {
            let refs: Vec<(String, bool)> = planned
                .iter()
                .map(|p| (p.text.clone(), p.new.is_none()))
                .collect();
            agent.check_refs(ctx, pushed.is_some(), &refs).await?;
        }
        let hooks = self.hooks.push_hooks();
        if req.hooks == wire::HookMode::SkipPre as i32 && !hooks.may_skip_pre(ctx) {
            return Err(Status::permission_denied(
                "hooks cannot be skipped by this caller",
            ));
        }
        if req.push_options.len() > MAX_PUSH_OPTIONS
            || req
                .push_options
                .iter()
                .any(|o| o.len() > MAX_PUSH_OPTION_LEN)
        {
            return Err(Status::invalid_argument(
                "too many or too long push options",
            ));
        }
        if req.ttl_seconds < 0 {
            return Err(Status::invalid_argument("negative ttl"));
        }
        let expires_at = (req.ttl_seconds > 0).then(|| now().saturating_add(req.ttl_seconds));
        let entry = self
            .registry
            .open(&ctx.repo)
            .await
            .map_err(registry_status)?;

        // Step 1, classify (SPEC §21.5.1): a non-shared branch of a branchless repository is the
        // pusher's user ref; a tag outside `shared_refs` is `REF_NOT_SHARED`. A LAND names a
        // shared branch and is checked by `land::check_branch`.
        if pushed.is_none() {
            let policy = refpolicy::RepoPolicy::load(self.registry.pool(), entry.key.0).await?;
            for p in &mut planned {
                // An agent has an owner only inside its own token's namespace.
                let owner = refpolicy::draft_owner_of(&ctx.principal, &p.text);
                if let refpolicy::Class::User { stored, .. } = policy.classify(owner, &p.text)? {
                    p.name = RefName::new(stored.as_bytes())
                        .map_err(|e| Status::invalid_argument(e.to_string()))?;
                    p.user = true;
                }
            }
        }
        let user_owner = refpolicy::accounting_owner_of(&ctx.principal);
        let has_user = planned.iter().any(|p| p.user);
        if has_user {
            for p in planned.iter().filter(|p| p.user) {
                let stored = String::from_utf8_lossy(p.name.as_bytes());
                let owned =
                    refauthz::owns(refpolicy::draft_owner_of(&ctx.principal, &stored), &stored);
                if !owned {
                    return Err(Status::permission_denied(
                        "a user ref can only be changed by its owner",
                    ));
                }
            }
        }

        // Step 2: idempotency.
        if let Some(done) = self.stored(&entry, op_id).await? {
            return Ok(done);
        }
        // Step 3: a mirror takes no pushes.
        if entry.mode == RepoMode::Mirror {
            return Err(rpc(RpcError::MirrorMode(info(vec![ctx.repo.to_string()]))));
        }

        // The per-user ref cap (SPEC §21.3.2) is enforced once, inside the step-8 transaction
        // under the per-(repo, owner) lock (`refpolicy::check_user_ref_writes`).
        // Quota (SPEC §21.3.7), part 1: an owner with no bytes left is refused before ingest
        // writes anything, and a push made only of user refs may add no more than what is left.
        // A delete-only push is never refused here, so an owner can shrink. The charge itself is
        // taken in the step-8 transaction (`commit_as`), for what the user refs newly reach.
        let quota_owner = user_owner.filter(|_| planned.iter().any(|p| p.user && p.new.is_some()));
        let mut budget = None;
        if let Some(owner) = quota_owner {
            let left = self
                .hooks
                .user_quota()
                .remaining(self.registry.pool(), entry.ns, owner)
                .await?;
            if planned.iter().all(|p| p.user || p.new.is_none()) {
                budget = Some(left);
            }
        }

        if pushed.is_some() && rewrites.is_some() {
            return Err(Status::invalid_argument("a land finds its own rewrites"));
        }
        if pushed.is_some() {
            land::check_branch(self.registry.pool(), &entry, &planned[0].text).await?;
        }

        // Step 4: ingest the closure of every new target, under the shared GC lock. It is held
        // until this function returns, which is after the step-8 commit, in every server process.
        let _gc = self.hooks.gc().shared().await.map_err(|e| {
            tracing::warn!(error = %e, "gc fence unavailable");
            Status::unavailable("garbage-collection fence unavailable; retry")
        })?;
        let targets: Vec<RefTarget> = planned.iter().filter_map(|p| p.new).collect();
        let windows_safe = self.windows_safe(&entry).await?;
        // The ingesting principal is recorded for every changeset added, so a closure left
        // dangling by a refused push is charged to whoever later names it from a user ref.
        let mut ingestor = Ingestor::new(
            self.registry.pool(),
            self.registry.refs(),
            &entry,
            windows_safe,
        )
        .by(user_owner)
        .staged_for(&ctx.principal.id);
        if let Some(left) = budget {
            ingestor = ingestor.with_budget(left);
        }
        ingestor
            .ingest(&targets, self.hooks.observer())
            .await
            .map_err(ingest_status)?;

        let graph = Graph::new(SqlGraph::new(
            self.registry.pool().clone(),
            entry.ns,
            entry.backend.engine().clone(),
        ));
        if let Some(pushed) = pushed {
            let update = planned
                .into_iter()
                .next()
                .expect("check_request admits one update");
            let job = land::LandJob {
                ctx,
                req: &req,
                op_id,
                entry: &entry,
                update,
                pushed,
                graph: &graph,
                windows_safe,
            };
            return land::run(self, job).await;
        }

        // Step 5: policy. FAST_FORWARD needs the new value to descend from the current one.
        let mut updates = Vec::with_capacity(planned.len());
        let mut results = Vec::with_capacity(planned.len());
        let mut hook_updates = Vec::with_capacity(planned.len());
        let mut agent_updates = Vec::with_capacity(planned.len());
        let mut gate_updates = Vec::with_capacity(planned.len());
        for p in planned {
            let current = self
                .registry
                .refs()
                .get(entry.key, &p.name)
                .await
                .map_err(|e| internal("refs", &e))?
                .map(|e| e.target);
            let mut expect = p.expect;
            if mode == wire::UpdateMode::FastForward {
                if let (Some(old), Some(new)) = (current, p.new) {
                    let ff = match (old, new) {
                        (RefTarget::Changeset(o), RefTarget::Changeset(n)) => {
                            o == n
                                || graph
                                    .is_ancestor(o, n)
                                    .await
                                    .map_err(|e| internal("graph", &e))?
                        }
                        (o, n) => o == n,
                    };
                    if !ff {
                        return Err(rpc(RpcError::NotFastForward(info(vec![p.text]))));
                    }
                }
                // Pin the value the check saw, so a concurrent move cannot slip past it.
                if expect == Expect::Any {
                    expect = current.map_or(Expect::Absent, Expect::Value);
                }
            }
            results.push(wire::RefResult {
                name: p.text.clone(),
                old: current.as_ref().map(wire::RefTarget::from),
                new: p.new.as_ref().map(wire::RefTarget::from),
                user: p.user,
            });
            gate_updates.push(GateUpdate {
                name: p.text.clone(),
                old: current,
                new: p.new,
                user: p.user,
            });
            agent_updates.push(RefChange {
                name: p.text.clone(),
                old: current,
                new: p.new,
            });
            if !p.user {
                hook_updates.push(HookUpdate {
                    name: p.text,
                    old: current,
                    new: p.new,
                });
            }
            updates.push(RefUpdate {
                name: p.name,
                expect,
                new: p.new,
                expires_at: if p.new.is_some() { expires_at } else { None },
            });
        }

        // The repository policy, secret scanning and signatures, before any hook runs.
        if let Some(gate) = self.hooks.gate() {
            gate.check(&GateRequest {
                ctx,
                entry: &entry,
                updates: &gate_updates,
                mode: GateMode::Push {
                    force: mode == wire::UpdateMode::Force,
                },
            })
            .await?;
        }

        // Steps 6 and 7: hook points.
        let acting = acting_principal(ctx);
        let hook_req = HookRequest {
            repo: &ctx.repo,
            op_id,
            principal: &acting,
            updates: &hook_updates,
            push_options: &req.push_options,
        };
        // User refs run no hook: a push of only user refs has nothing to bridge or reject.
        let run_hooks = !hook_updates.is_empty();
        if run_hooks {
            hooks
                .bridge_quarantine(&hook_req)
                .await
                .map_err(|e| rpc(RpcError::HookRejected(info(vec![e.0]))))?;
            if req.hooks != wire::HookMode::SkipPre as i32
                && let Err(e) = hooks.pre_receive(&hook_req).await
            {
                hooks.abort(&hook_req).await;
                return Err(rpc(RpcError::HookRejected(info(vec![e.0]))));
            }
        }

        // Step 8: one transaction.
        let response = wire::UpdateRefsResponse {
            results,
            rewritten: rewrites.as_ref().map_or_else(Vec::new, |r| {
                r.pairs
                    .iter()
                    .map(|(old, new)| wire::Rewrite {
                        old: Bytes::copy_from_slice(old.as_bytes()),
                        new: Bytes::copy_from_slice(new.as_bytes()),
                    })
                    .collect()
            }),
            message: String::new(),
        };
        let writes = rewrites
            .as_ref()
            .filter(|r| !r.pairs.is_empty())
            .map(|r| land::TxWrites::rewrites(self.hooks.observer(), r.pairs, r.op));
        let txn = RefTxn {
            updates,
            actor: Actor {
                id: ctx.principal.id.clone(),
                via: via.into(),
            },
            reason: "UpdateRefs".into(),
            op_id,
        };
        let outbox = serde_json::to_vec(&OutboxEffects {
            effects: if run_hooks {
                hooks.outbox_effects()
            } else {
                &[]
            },
            pusher: &acting.id,
            refs: hook_updates
                .iter()
                .map(|u| EffectRef {
                    name: u.name.clone(),
                    old: u.old.as_ref().map(target_hex),
                    new: u.new.as_ref().map(target_hex),
                })
                .collect(),
        })
        .map_err(|e| internal("outbox", &e))?;
        // An agent's scope and budget (agents/gate.rs). The charge is given back if the commit fails.
        let agent_charge = match self.hooks.agent_gate() {
            Some(g) => match g.charge(ctx, &entry, &agent_updates).await {
                Ok(c) => c,
                Err(e) => {
                    if run_hooks {
                        hooks.abort(&hook_req).await;
                    }
                    return Err(e);
                }
            },
            None => None,
        };
        let committed = self
            .commit(&entry, &txn, &response, &outbox, writes.as_ref())
            .await;
        if let (Some(g), Some(c)) = (self.hooks.agent_gate(), agent_charge) {
            g.settle(c, committed.is_ok()).await;
        }
        match committed {
            Ok(()) => Ok(response),
            Err(e) => {
                if run_hooks {
                    hooks.abort(&hook_req).await;
                }
                // A retry of the same op that committed in the meantime is not a conflict.
                if let Some(done) = self.stored(&entry, op_id).await? {
                    return Ok(done);
                }
                Err(e)
            }
        }
    }

    /// The step-8 transaction: ref CAS and log, the op result, and the outbox row.
    pub(crate) async fn commit(
        &self,
        entry: &RepoEntry,
        txn: &RefTxn,
        response: &wire::UpdateRefsResponse,
        outbox: &[u8],
        land: Option<&land::TxWrites<'_>>,
    ) -> Result<(), Status> {
        self.commit_as(entry, txn, response, outbox, land, UserRefWriter::Client)
            .await
    }

    /// The expiry worker's deletion of expired user refs (SPEC §8.8, §21.5.1), the one writer
    /// that is not their owner. It is a method of the server, not a request: no RPC, push path or
    /// `UpdateRefs` field reaches it. It only deletes (each ref only if it still names its
    /// scanned target and is still expired at `at`), only user refs, in one transaction with the
    /// same repository guard, `ref_log` row, op result and (empty) outbox row as a client's
    /// delete. User refs queue no `post_receive`.
    pub(crate) async fn expire_user_refs(
        &self,
        repo: &RepoName,
        refs: &[(&str, RefTarget)],
        actor: &str,
        at: i64,
    ) -> Result<(), Status> {
        let entry = self.registry.open(repo).await.map_err(registry_status)?;
        if entry.mode == RepoMode::Mirror {
            return Err(rpc(RpcError::MirrorMode(info(vec![repo.to_string()]))));
        }
        let mut updates = Vec::with_capacity(refs.len());
        let mut results = Vec::with_capacity(refs.len());
        for (name, target) in refs {
            if refpolicy::parse_user_ref(name).is_none() {
                return Err(Status::invalid_argument("not a user ref"));
            }
            updates.push(RefUpdate {
                name: RefName::new(name.as_bytes())
                    .map_err(|e| Status::invalid_argument(e.to_string()))?,
                expect: Expect::Value(*target),
                new: None,
                expires_at: None,
            });
            results.push(wire::RefResult {
                name: (*name).to_owned(),
                old: Some(wire::RefTarget::from(target)),
                new: None,
                user: false,
            });
        }
        let response = wire::UpdateRefsResponse {
            results,
            rewritten: Vec::new(),
            message: String::new(),
        };
        let txn = RefTxn {
            updates,
            actor: Actor {
                id: actor.to_owned(),
                via: "asc-expiry".into(),
            },
            reason: "expiry".into(),
            op_id: Uuid::new_v4(),
        };
        let outbox = serde_json::to_vec(&OutboxEffects {
            effects: &[],
            pusher: actor,
            refs: Vec::new(),
        })
        .map_err(|e| internal("outbox", &e))?;
        self.commit_as(
            &entry,
            &txn,
            &response,
            &outbox,
            None,
            UserRefWriter::Expiry { at },
        )
        .await
    }

    /// Marks what shared refs reach as shared, then charges each user-ref owner for the changesets
    /// the owner's refs newly make reachable (`UserQuota::claim`). Reads no object but tags.
    async fn settle_quota(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Any>,
        entry: &RepoEntry,
        txn: &RefTxn,
        quota: &UserQuota,
    ) -> Result<(), Status> {
        let mut shared = Vec::new();
        let mut users: BTreeMap<i64, Vec<RefTarget>> = BTreeMap::new();
        for u in &txn.updates {
            let Some(new) = u.new else { continue };
            let name = std::str::from_utf8(u.name.as_bytes()).unwrap_or_default();
            match refpolicy::parse_user_ref(name) {
                Some((owner, _)) if name.starts_with(refpolicy::USER_NS) => {
                    users.entry(owner).or_default().push(new);
                }
                _ => shared.push(new),
            }
        }
        if !shared.is_empty() {
            let roots = quota::roots_of(&*entry.objects, &shared).await?;
            quota::mark_shared(&mut *tx, entry.ns, &roots).await?;
        }
        for (owner, targets) in users {
            let roots = quota::roots_of(&*entry.objects, &targets).await?;
            quota.claim(&mut *tx, entry.ns, owner, &roots).await?;
        }
        Ok(())
    }

    async fn commit_as(
        &self,
        entry: &RepoEntry,
        txn: &RefTxn,
        response: &wire::UpdateRefsResponse,
        outbox: &[u8],
        land: Option<&land::TxWrites<'_>>,
        writer: UserRefWriter,
    ) -> Result<(), Status> {
        let refs = self.registry.refs();
        let mut tx = refs.begin().await.map_err(|e| internal("refs", &e))?;
        self.registry.guard(&mut tx, entry).await?;
        if let UserRefWriter::Expiry { at } = writer {
            refpolicy::require_expired(&mut tx, entry.key, txn, at).await?;
        }
        match refs.update_in_tx(&mut tx, entry.key, txn).await {
            Ok(()) => {}
            Err(RefError::Conflict(c)) => return Err(conflict(c)),
            Err(e @ RefError::NonUtf8Name(_)) => {
                return Err(Status::invalid_argument(e.to_string()));
            }
            Err(e) => return Err(internal("refs", &e)),
        }
        // User refs have no pre-receive, so the owner and the cap are checked here, in the same
        // transaction as the write (SPEC §21.5.1).
        let quota = self.hooks.user_quota();
        refpolicy::check_user_ref_writes(&mut tx, entry.key, txn, writer, quota.max_user_refs())
            .await?;
        // Quota (SPEC §21.3.7), part 2, in this transaction: a shared ref settles its closure
        // (shared history costs a user nothing), then each owner is charged what their refs newly
        // make reachable. A refusal, or any later failure, rolls the charge back with the refs.
        self.settle_quota(&mut tx, entry, txn, quota).await?;
        if let Some(land) = land {
            land.apply(&mut tx, entry, txn).await?;
        }
        let at = now();
        sqlx::query("INSERT INTO op_results (op_id, repo_id, result, at) VALUES ($1, $2, $3, $4)")
            .bind(txn.op_id.as_bytes().to_vec())
            .bind(entry.key.0)
            .bind(response.encode_to_vec())
            .bind(at)
            .execute(&mut *tx)
            .await
            .map_err(|e| internal("op_results", &e))?;
        sqlx::query(
            "INSERT INTO outbox (repo_id, op_id, effects, next_at) VALUES ($1, $2, $3, $4)",
        )
        .bind(entry.key.0)
        .bind(txn.op_id.as_bytes().to_vec())
        .bind(outbox.to_vec())
        .bind(at)
        .execute(&mut *tx)
        .await
        .map_err(|e| internal("outbox", &e))?;
        // The audit events of this update, in the same transaction: a push that cannot be
        // recorded is not accepted.
        crate::audit::hooks::append_ref_updates(&mut tx, entry.key.0, entry.name.as_str(), txn)
            .await
            .map_err(|e| internal("audit", &e))?;
        tx.commit().await.map_err(|e| internal("commit", &e))
    }
}

/// Changesets a `Merge` rewrote, for the step-8 transaction of the `UpdateRefs` that moves its ref.
pub(crate) struct Rewrites<'a> {
    /// `(old, new)`.
    pub(crate) pairs: &'a [(ChangesetId, ChangesetId)],
    pub(crate) op: RewriteOp,
}

#[async_trait]
impl RefsHandler for ServerRefs {
    async fn list_refs(
        &self,
        ctx: &CallCtx,
        req: wire::ListRefsRequest,
    ) -> Result<wire::ListRefsResponse, Status> {
        let entry = self
            .registry
            .open(&ctx.repo)
            .await
            .map_err(registry_status)?;
        let policy = refpolicy::RepoPolicy::load(self.registry.pool(), entry.key.0).await?;
        if !policy.branchless() {
            let mut listed = entry.backend.list_refs(ctx, req).await?;
            listed
                .refs
                .retain(|r| !r.name.starts_with(refpolicy::USER_NS));
            // The engine does not know the repository's default branch; the policy does (the registry's trunk).
            if listed.head.is_empty() {
                listed.head = policy.head();
            }
            return Ok(listed);
        }
        if ctx.id_mode != IdMode::Asc {
            return Err(Status::unimplemented(
                "git id mode needs the git map index, which this backend does not keep",
            ));
        }
        refpolicy::advertise(
            &**self.registry.refs(),
            entry.key,
            &policy,
            refpolicy::owner_of(&ctx.principal),
            &req,
        )
        .await
    }

    async fn update_refs(
        &self,
        ctx: &CallCtx,
        req: wire::UpdateRefsRequest,
    ) -> Result<wire::UpdateRefsResponse, Status> {
        self.update(ctx, req).await
    }
}

/// The one registration line of this unit (called from `lib.rs`).
pub fn register(ext: &Extensions, registry: &Arc<Registry>) {
    let handler = Arc::new(ServerRefs::new(registry.clone(), ext.pipeline.clone()));
    ext.handlers
        .set_refs(handler)
        .expect("the refs handler group is registered once");
}

#[cfg(test)]
mod tests {
    use asc_core::codec::Enveloped;
    use asc_core::git::blob_oid;
    use asc_core::id::{ChangesetId, FileId};
    use asc_core::object::{Change, Changeset, Entry, Kind, SortedMap, Tree};
    use asc_core::path::{Name, RepoPath};
    use asc_store::codec::encode_stored;
    use asc_store::key::ObjKey;
    use asc_store::objects::Objects;

    use super::*;
    use crate::ingest::testutil::{commit, put_file, registry, repo, sig};

    fn ctx(op: Uuid) -> CallCtx {
        CallCtx {
            repo: RepoName::parse("o/r").unwrap(),
            id_mode: IdMode::Asc,
            principal: Principal { id: "alice".into() },
            op_id: Some(op),
            client: None,
            admin: Default::default(),
        }
    }

    fn request(
        name: &str,
        expect: wire::ref_update::Expect,
        to: Option<ChangesetId>,
        mode: wire::UpdateMode,
    ) -> wire::UpdateRefsRequest {
        wire::UpdateRefsRequest {
            repo: Some(wire::RepoRef {
                name: "o/r".into(),
                id_mode: 0,
            }),
            updates: vec![wire::RefUpdate {
                name: name.into(),
                expect: Some(expect),
                new: to.map(|c| wire::RefTarget::from(&RefTarget::Changeset(c))),
                delete: to.is_none(),
            }],
            mode: mode as i32,
            ..wire::UpdateRefsRequest::default()
        }
    }

    fn create(name: &str, to: ChangesetId) -> wire::UpdateRefsRequest {
        request(
            name,
            wire::ref_update::Expect::Absent(true),
            Some(to),
            wire::UpdateMode::FastForward,
        )
    }

    fn code(s: &Status) -> String {
        asc_rpc::server::status_rpc_error(s)
            .map_or_else(|| s.code().to_string(), |e| e.code().to_owned())
    }

    fn val(c: ChangesetId) -> wire::ref_update::Expect {
        wire::ref_update::Expect::Value(wire::RefTarget::from(&RefTarget::Changeset(c)))
    }

    async fn setup() -> (Arc<Registry>, Arc<RepoEntry>, ServerRefs) {
        let r = registry().await;
        let e = repo(&r).await;
        let server = ServerRefs::new(r.clone(), Arc::default());
        (r, e, server)
    }

    async fn run(
        s: &ServerRefs,
        req: wire::UpdateRefsRequest,
    ) -> Result<wire::UpdateRefsResponse, Status> {
        s.update_refs(&ctx(Uuid::new_v4()), req).await
    }

    async fn count(r: &Registry, sql: &str) -> i64 {
        sqlx::query(sqlx::AssertSqlSafe(sql.to_owned()))
            .fetch_one(r.pool())
            .await
            .unwrap()
            .get::<i64, _>(0)
    }

    fn master() -> RefName {
        RefName::new("refs/heads/master").unwrap()
    }

    #[tokio::test]
    async fn push_of_a_stack_ingests_closes_and_moves_the_ref() {
        let (r, e, s) = setup().await;
        let a = commit(&e, &[], &[("a", b"1")]).await;
        let b = commit(&e, &[a], &[("b", b"2")]).await;
        let c = commit(&e, &[b], &[("c", b"3")]).await;
        let res = run(&s, create("refs/heads/master", c)).await.unwrap();
        assert_eq!(res.results.len(), 1);
        assert!(res.results[0].old.is_none());
        let got = r.refs().get(e.key, &master()).await.unwrap().unwrap();
        assert_eq!(got.target, RefTarget::Changeset(c));
        assert_eq!(count(&r, "SELECT COUNT(*) FROM cs_graph").await, 3);
        assert_eq!(count(&r, "SELECT COUNT(*) FROM op_results").await, 1);
        assert_eq!(count(&r, "SELECT COUNT(*) FROM outbox").await, 1);
        let log = r.refs().log(e.key, &master(), 5).await.unwrap();
        assert_eq!((log.len(), log[0].actor.via.as_str()), (1, "asc-push"));

        // A fast-forward on top of it moves the ref and records the old value.
        let d = commit(&e, &[c], &[("d", b"4")]).await;
        let up = request(
            "refs/heads/master",
            val(c),
            Some(d),
            wire::UpdateMode::FastForward,
        );
        let res = run(&s, up).await.unwrap();
        assert_eq!(
            res.results[0].old,
            Some(wire::RefTarget::from(&RefTarget::Changeset(c)))
        );
        assert_eq!(count(&r, "SELECT COUNT(*) FROM cs_graph").await, 4);
    }

    #[tokio::test]
    async fn a_duplicate_op_id_returns_the_stored_result() {
        let (r, e, s) = setup().await;
        let a = commit(&e, &[], &[("a", b"1")]).await;
        let op = Uuid::new_v4();
        let first = s
            .update_refs(&ctx(op), create("refs/heads/master", a))
            .await
            .unwrap();
        // Replaying would fail (the ref exists, expectation `absent`); the stored result wins.
        let again = s
            .update_refs(&ctx(op), create("refs/heads/master", a))
            .await
            .unwrap();
        assert_eq!(first, again);
        assert_eq!(count(&r, "SELECT COUNT(*) FROM ref_log").await, 1);
        assert_eq!(count(&r, "SELECT COUNT(*) FROM outbox").await, 1);
    }

    #[tokio::test]
    async fn fast_forward_mode_refuses_a_rewrite_and_force_allows_it() {
        let (r, e, s) = setup().await;
        let a = commit(&e, &[], &[("a", b"1")]).await;
        let b = commit(&e, &[a], &[("b", b"2")]).await;
        let x = commit(&e, &[a], &[("x", b"9")]).await;
        run(&s, create("refs/heads/master", b)).await.unwrap();
        let rewrite = |mode| {
            request(
                "refs/heads/master",
                wire::ref_update::Expect::Any(true),
                Some(x),
                mode,
            )
        };
        let err = run(&s, rewrite(wire::UpdateMode::FastForward))
            .await
            .unwrap_err();
        assert_eq!(code(&err), "NOT_FAST_FORWARD");
        run(&s, rewrite(wire::UpdateMode::Force)).await.unwrap();
        let got = r.refs().get(e.key, &master()).await.unwrap().unwrap();
        assert_eq!(got.target, RefTarget::Changeset(x));
    }

    #[tokio::test]
    async fn a_wrong_expectation_is_a_ref_conflict() {
        let (_r, e, s) = setup().await;
        let a = commit(&e, &[], &[("a", b"1")]).await;
        let b = commit(&e, &[a], &[("b", b"2")]).await;
        run(&s, create("refs/heads/master", a)).await.unwrap();
        let stale = request(
            "refs/heads/master",
            val(b),
            Some(b),
            wire::UpdateMode::Force,
        );
        assert_eq!(code(&run(&s, stale).await.unwrap_err()), "REF_CONFLICT");
    }

    #[tokio::test]
    async fn missing_objects_are_named() {
        let (_r, _e, s) = setup().await;
        let ghost = ChangesetId::from_bytes([5; 32]);
        let err = run(&s, create("refs/heads/master", ghost))
            .await
            .unwrap_err();
        assert_eq!(code(&err), "MISSING_OBJECTS");
        let detail = asc_rpc::server::status_rpc_error(&err).unwrap();
        assert_eq!(detail.info().paths, vec![ObjKey::Changeset(ghost).path()]);
    }

    /// Stores `cs` without any check, as a malicious client could.
    async fn store(e: &RepoEntry, cs: &Changeset) -> ChangesetId {
        let envelope = cs.to_envelope();
        let id = ChangesetId::of(&envelope);
        e.objects
            .put(&ObjKey::Changeset(id), encode_stored(&envelope))
            .await
            .unwrap();
        id
    }

    async fn forged(e: &RepoEntry, size: u64, git: asc_core::id::GitOid) -> ChangesetId {
        let bytes = b"hello";
        let file = FileId::of(bytes);
        e.objects
            .put(&ObjKey::File(file), encode_stored(bytes))
            .await
            .unwrap();
        let entry = Entry {
            name: Name::new("f").unwrap(),
            kind: Kind::File,
            id: *file.as_bytes(),
            size,
            git,
        };
        let root = e
            .backend
            .engine()
            .put_tree(Tree::new(vec![entry], true).unwrap())
            .await
            .unwrap();
        let change = Change::write(Kind::File, file, size, git, None).unwrap();
        store(
            e,
            &Changeset {
                parents: vec![],
                root,
                author: sig(),
                committer: sig(),
                message: b"m".to_vec(),
                extra: SortedMap::default(),
                changes: SortedMap::new(vec![(RepoPath::new("f").unwrap(), change)]).unwrap(),
            },
        )
        .await
    }

    #[tokio::test]
    async fn a_client_lying_about_size_or_git_oid_is_invalid() {
        let (r, e, s) = setup().await;
        let good = blob_oid(b"hello");
        let honest = forged(&e, 5, good).await;
        run(&s, create("refs/heads/ok", honest)).await.unwrap();
        for (size, git) in [(6, good), (5, blob_oid(b"other"))] {
            let id = forged(&e, size, git).await;
            let err = run(&s, create("refs/heads/bad", id)).await.unwrap_err();
            assert_eq!(code(&err), "INVALID_OBJECT", "size {size}");
        }
        assert_eq!(count(&r, "SELECT COUNT(*) FROM refs").await, 1);
    }

    /// `master` holds an honest `a` (files `a` and `b`); returns `a` and an honest child `c`
    /// that rewrites both files, stored but not pushed.
    async fn master_with_a_child(
        e: &RepoEntry,
        s: &ServerRefs,
    ) -> (ChangesetId, ChangesetId, Changeset) {
        let a = commit(e, &[], &[("a", b"1"), ("b", b"2")]).await;
        run(s, create("refs/heads/master", a)).await.unwrap();
        let c = commit(e, &[a], &[("a", b"x"), ("b", b"y")]).await;
        let cs = (*e.backend.engine().get_changeset(c).await.unwrap()).clone();
        (a, c, cs)
    }

    /// Lands `to` on `master`, the only push that appends to the mainline path index.
    fn advance(_: ChangesetId, to: ChangesetId) -> wire::UpdateRefsRequest {
        request(
            "refs/heads/master",
            wire::ref_update::Expect::Any(true),
            Some(to),
            wire::UpdateMode::Land,
        )
    }

    /// Rows the mainline path index holds.
    async fn indexed_paths(r: &Registry) -> i64 {
        count(r, "SELECT COUNT(*) FROM path_changes").await
    }

    #[tokio::test]
    async fn an_honest_changes_list_is_accepted_and_indexed() {
        let (r, e, s) = setup().await;
        let (a, c, _) = master_with_a_child(&e, &s).await;
        let before = indexed_paths(&r).await;
        run(&s, advance(a, c)).await.unwrap();
        assert!(indexed_paths(&r).await > before);
    }

    #[tokio::test]
    async fn changes_that_omit_a_changed_path_are_invalid_and_never_indexed() {
        let (r, e, s) = setup().await;
        let (a, _, mut cs) = master_with_a_child(&e, &s).await;
        let kept: Vec<_> = cs
            .changes
            .as_slice()
            .iter()
            .filter(|(p, _)| p.as_bytes() != b"b")
            .cloned()
            .collect();
        assert_eq!(kept.len(), 1);
        cs.changes = SortedMap::new(kept).unwrap();
        let lie = store(&e, &cs).await;
        let before = indexed_paths(&r).await;
        let err = run(&s, advance(a, lie)).await.unwrap_err();
        assert_eq!(code(&err), "INVALID_OBJECT");
        assert_eq!(err.code(), tonic::Code::InvalidArgument);
        assert_eq!(indexed_paths(&r).await, before);
        assert_eq!(count(&r, "SELECT COUNT(*) FROM cs_graph").await, 1);
    }

    #[tokio::test]
    async fn changes_that_list_an_unchanged_path_are_invalid_and_never_indexed() {
        let (r, e, s) = setup().await;
        let a = commit(&e, &[], &[("a", b"1"), ("b", b"2")]).await;
        run(&s, create("refs/heads/master", a)).await.unwrap();
        // The child rewrites only `b`; its list also claims `a`, which is byte-identical.
        let c = commit(&e, &[a], &[("b", b"y")]).await;
        let mut cs = (*e.backend.engine().get_changeset(c).await.unwrap()).clone();
        let mut lied = cs.changes.as_slice().to_vec();
        lied.push((RepoPath::new("a").unwrap(), put_file(&e, b"1").await));
        lied.sort_by(|x, y| x.0.cmp(&y.0));
        cs.changes = SortedMap::new(lied).unwrap();
        let lie = store(&e, &cs).await;
        let before = indexed_paths(&r).await;
        let err = run(&s, advance(a, lie)).await.unwrap_err();
        assert_eq!(code(&err), "INVALID_OBJECT");
        assert_eq!(err.code(), tonic::Code::InvalidArgument);
        assert_eq!(indexed_paths(&r).await, before);
    }

    #[tokio::test]
    async fn a_forged_raw_git_extra_is_invalid() {
        let (_r, e, s) = setup().await;
        let a = commit(&e, &[], &[("a", b"1")]).await;
        let mut cs = (*e.backend.engine().get_changeset(a).await.unwrap()).clone();
        cs.extra = SortedMap::new(vec![(b"git.commit".to_vec(), b"tree x\n".to_vec())]).unwrap();
        let id = store(&e, &cs).await;
        let err = run(&s, create("refs/heads/master", id)).await.unwrap_err();
        assert_eq!(code(&err), "INVALID_OBJECT");
    }

    #[tokio::test]
    async fn mirror_mode_and_skip_pre_and_local_refs_are_refused() {
        let (r, e, s) = setup().await;
        let a = commit(&e, &[], &[("a", b"1")]).await;
        let mut skip = create("refs/heads/master", a);
        skip.hooks = wire::HookMode::SkipPre as i32;
        assert_eq!(
            run(&s, skip).await.unwrap_err().code(),
            tonic::Code::PermissionDenied
        );
        let local = create("refs/remotes/origin/master", a);
        assert_eq!(
            run(&s, local).await.unwrap_err().code(),
            tonic::Code::InvalidArgument
        );
        sqlx::query("UPDATE repos SET mode = 'mirror'")
            .execute(r.pool())
            .await
            .unwrap();
        r.forget(&RepoName::parse("o/r").unwrap());
        let err = run(&s, create("refs/heads/master", a)).await.unwrap_err();
        assert_eq!(code(&err), "MIRROR_MODE");
    }

    fn on_claim(principal: &str, user: &str, role: asc_rpc::forwarded::Role) -> CallCtx {
        CallCtx {
            principal: Principal {
                id: principal.into(),
            },
            admin: asc_rpc::auth::AdminProof::default().with_forwarded(Some(
                asc_rpc::forwarded::Claim {
                    user: user.into(),
                    role,
                },
            )),
            ..ctx(Uuid::new_v4())
        }
    }

    #[test]
    fn the_forge_is_told_the_person_of_a_verified_write_claim_and_nobody_else() {
        use asc_rpc::forwarded::Role;
        let host = crate::authz::HOST_SERVICE_PRINCIPAL;
        // The host service acting for a person who may write: the forge sees the person.
        assert_eq!(
            acting_principal(&on_claim(host, "44:carol", Role::Write)).id,
            "44:carol"
        );
        assert_eq!(
            acting_principal(&on_claim(host, "44:carol", Role::Admin)).id,
            "44:carol"
        );
        // A claim that only reads, a claim for no registered user, a deploy key and a malformed
        // claim leave the host service, whom the forge refuses as unattributed.
        for (user, role) in [
            ("44:carol", Role::Read),
            ("0:anonymous", Role::Write),
            ("44:carol:7", Role::Write),
            ("carol", Role::Write),
            ("x:carol", Role::Write),
            ("-2:task", Role::Write),
        ] {
            assert_eq!(
                acting_principal(&on_claim(host, user, role)).id,
                host,
                "{user} {role:?}"
            );
        }
        // No claim at all.
        let mut bare = ctx(Uuid::new_v4());
        bare.principal = Principal { id: host.into() };
        assert_eq!(acting_principal(&bare).id, host);
        // A claim does not rename any other caller: only the host service may act on one.
        assert_eq!(
            acting_principal(&on_claim("45:mallory", "44:carol", Role::Write)).id,
            "45:mallory"
        );
    }

    #[tokio::test]
    async fn a_write_made_on_a_claim_reaches_the_hook_as_the_person_and_one_without_as_the_host() {
        use std::sync::Mutex;
        struct Seen(Mutex<Vec<String>>);
        #[async_trait]
        impl PushHooks for Seen {
            async fn pre_receive(&self, req: &HookRequest<'_>) -> Result<(), HookRejected> {
                self.0.lock().unwrap().push(req.principal.id.clone());
                Ok(())
            }
        }
        let (_r, e, s) = setup().await;
        let seen = Arc::new(Seen(Mutex::new(Vec::new())));
        s.hooks.set_push_hooks(seen.clone()).ok().unwrap();
        let a = commit(&e, &[], &[("a", b"1")]).await;
        let b = commit(&e, &[a], &[("b", b"2")]).await;
        let host = crate::authz::HOST_SERVICE_PRINCIPAL;
        let mut with_claim = on_claim(host, "44:carol", asc_rpc::forwarded::Role::Write);
        with_claim.op_id = Some(Uuid::new_v4());
        s.update_refs(&with_claim, create("refs/heads/master", a))
            .await
            .unwrap();
        let mut without = ctx(Uuid::new_v4());
        without.principal = Principal { id: host.into() };
        s.update_refs(
            &without,
            request(
                "refs/heads/master",
                wire::ref_update::Expect::Value(wire::RefTarget::from(&RefTarget::Changeset(a))),
                Some(b),
                wire::UpdateMode::FastForward,
            ),
        )
        .await
        .unwrap();
        assert_eq!(*seen.0.lock().unwrap(), ["44:carol", host]);
    }

    #[tokio::test]
    async fn pre_receive_rejection_changes_nothing() {
        struct Reject;
        #[async_trait]
        impl PushHooks for Reject {
            async fn pre_receive(&self, _: &HookRequest<'_>) -> Result<(), HookRejected> {
                Err(HookRejected("no pushing on fridays".into()))
            }
        }
        let (r, e, s) = setup().await;
        s.hooks.set_push_hooks(Arc::new(Reject)).ok().unwrap();
        let a = commit(&e, &[], &[("a", b"1")]).await;
        let err = run(&s, create("refs/heads/master", a)).await.unwrap_err();
        assert_eq!(code(&err), "HOOK_REJECTED");
        assert_eq!(count(&r, "SELECT COUNT(*) FROM refs").await, 0);
        assert_eq!(count(&r, "SELECT COUNT(*) FROM op_results").await, 0);
    }

    #[tokio::test]
    async fn the_observer_runs_in_the_ingest_transaction() {
        struct Count(std::sync::atomic::AtomicUsize);
        #[async_trait]
        impl IngestObserver for Count {
            async fn on_ingested(
                &self,
                tx: &mut sqlx::Transaction<'_, sqlx::Any>,
                _: asc_store::refs::RepoKey,
                new: &[ChangesetId],
                closure: &[ObjKey],
            ) -> Result<(), crate::ingest::ObserverError> {
                // The graph rows are visible inside the same transaction.
                let n: i64 = sqlx::query("SELECT COUNT(*) FROM cs_graph")
                    .fetch_one(&mut **tx)
                    .await?
                    .get(0);
                assert_eq!(n as usize, new.len());
                assert_eq!(closure.len(), new.len());
                self.0
                    .fetch_add(new.len(), std::sync::atomic::Ordering::SeqCst);
                Ok(())
            }
        }
        let (_r, e, s) = setup().await;
        let seen = Arc::new(Count(Default::default()));
        s.hooks.set_observer(seen.clone()).ok().unwrap();
        let a = commit(&e, &[], &[("a", b"1")]).await;
        let b = commit(&e, &[a], &[("b", b"2")]).await;
        run(&s, create("refs/heads/master", b)).await.unwrap();
        assert_eq!(seen.0.load(std::sync::atomic::Ordering::SeqCst), 2);
    }

    /// U48c: a stale entry of a renamed or deleted repository cannot commit a ref update.
    #[tokio::test]
    async fn a_stale_entry_cannot_commit_a_ref_update() {
        let (r, e, s) = setup().await;
        let a = commit(&e, &[], &[("a", b"1")]).await;
        let txn = || RefTxn {
            updates: vec![RefUpdate {
                name: master(),
                expect: Expect::Absent,
                new: Some(RefTarget::Changeset(a)),
                expires_at: None,
            }],
            actor: Actor {
                id: "alice".into(),
                via: "test".into(),
            },
            reason: "UpdateRefs".into(),
            op_id: Uuid::new_v4(),
        };
        let done = wire::UpdateRefsResponse::default();
        // Renamed under the entry: the id still exists, the name is not the one authorised.
        sqlx::query("UPDATE repos SET name = 'o/moved' WHERE id = $1")
            .bind(e.key.0)
            .execute(r.pool())
            .await
            .unwrap();
        let err = s.commit(&e, &txn(), &done, b"{}", None).await.unwrap_err();
        assert_eq!(err.code(), tonic::Code::FailedPrecondition);
        // Deleted under the entry.
        sqlx::query("DELETE FROM repos WHERE id = $1")
            .bind(e.key.0)
            .execute(r.pool())
            .await
            .unwrap();
        let err = s.commit(&e, &txn(), &done, b"{}", None).await.unwrap_err();
        assert_eq!(err.code(), tonic::Code::NotFound);
        assert!(r.refs().get(e.key, &master()).await.unwrap().is_none());
        assert_eq!(count(&r, "SELECT COUNT(*) FROM op_results").await, 0);
    }
}
