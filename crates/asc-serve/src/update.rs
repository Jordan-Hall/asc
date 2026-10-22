//! The `UpdateRefs` pipeline (SPEC §8.3, PLAN U43b).
//!
//! Steps 1 to 5, 8 and 9 live here. Steps 6 and 7 (bridge quarantine, pre-receive) are the
//! [`PushHooks`] extension point, a no-op until Forgejo arrives (U44). Step 4 runs under the
//! shared GC lock, which is held until the step-8 transaction commits (SPEC §3.6). LAND mode is
//! added by the land unit.

use std::collections::HashSet;
use std::sync::{Arc, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use bytes::Bytes;
use asc_core::id::RefTarget;
use asc_core::path::RefName;
use asc_engine::graph::Graph;
use asc_rpc::IdMode;
use asc_rpc::auth::Principal;
use asc_rpc::errors::{ErrorInfo, RpcError};
use asc_rpc::server::{CallCtx, MAX_REF_UPDATES, RefsHandler, RepoName, rpc_status, wire};
use asc_store::refs::{Actor, Expect, RefError, RefTxn, RefUpdate, Refs};
use prost::Message;
use serde::Serialize;
use sqlx::{AnyPool, Row};
use tokio::sync::{OwnedRwLockReadGuard, OwnedRwLockWriteGuard, RwLock};
use tonic::Status;
use uuid::Uuid;

use crate::graph_sql::SqlGraph;
use crate::ingest::{IngestError, IngestObserver, Ingestor, NoObserver};
use crate::registry::{Extensions, Registry, RegistryError, RepoEntry, RepoMode};

/// Push options per request, and the longest one.
const MAX_PUSH_OPTIONS: usize = 64;
const MAX_PUSH_OPTION_LEN: usize = 1024;
/// Advisory lock key of the GC fence (Postgres).
const GC_LOCK_KEY: i64 = 0x0063_675f_6763;
/// Ref namespaces that are local to a client and never stored on a server (SPEC §2.7).
const LOCAL_ONLY: &[&str] = &["refs/remotes/", "refs/stash", "refs/bisect/", "refs/for/"];

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

    /// The push did not commit after step 6; remove the quarantine.
    async fn abort(&self, _req: &HookRequest<'_>) {}

    /// Effects the step-8 transaction queues in the outbox, in order.
    fn outbox_effects(&self) -> &'static [&'static str] {
        &["post_receive"]
    }
}

struct NoHooks;

impl PushHooks for NoHooks {}

/// The GC fence: pushes hold it shared from step 4 to their commit; a sweep batch takes it
/// exclusively (O01). On Postgres the step-8 transaction also takes the shared advisory lock
/// `pg_advisory_xact_lock_shared`, which a sweeper in another process takes exclusively.
#[derive(Clone, Default)]
pub struct GcLock(Arc<RwLock<()>>);

impl GcLock {
    pub async fn shared(&self) -> OwnedRwLockReadGuard<()> {
        self.0.clone().read_owned().await
    }

    pub async fn exclusive(&self) -> OwnedRwLockWriteGuard<()> {
        self.0.clone().write_owned().await
    }
}

/// The ingest and `UpdateRefs` extension points; each is set at most once (U43a registry).
#[derive(Default)]
pub struct PipelineHooks {
    observer: OnceLock<Arc<dyn IngestObserver>>,
    push: OnceLock<Arc<dyn PushHooks>>,
    gc: GcLock,
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

    pub fn observer(&self) -> &dyn IngestObserver {
        self.observer.get().map_or(&NoObserver, |o| &**o)
    }

    pub fn push_hooks(&self) -> &dyn PushHooks {
        self.push.get().map_or(&NoHooks, |h| &**h)
    }

    pub fn gc(&self) -> &GcLock {
        &self.gc
    }
}

// ------------------------------------------------------------------------------ errors

fn rpc(e: RpcError) -> Status {
    rpc_status(&e)
}

fn info(paths: Vec<String>) -> ErrorInfo {
    ErrorInfo {
        paths,
        ..ErrorInfo::default()
    }
}

fn internal(context: &str, e: &dyn std::fmt::Display) -> Status {
    tracing::error!(%context, error = %e, "internal error");
    Status::internal("internal error")
}

fn ingest_status(e: IngestError) -> Status {
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
        e => internal("ingest", &e),
    }
}

fn registry_status(e: RegistryError) -> Status {
    match e {
        RegistryError::NotFound(_) => Status::not_found("no such repository"),
        e => internal("registry", &e),
    }
}

fn conflict(c: asc_store::refs::RefConflict) -> Status {
    rpc(RpcError::RefConflict(ErrorInfo {
        paths: vec![String::from_utf8_lossy(c.name.as_bytes()).into_owned()],
        actual: c
            .actual
            .map_or_else(Bytes::new, |t| Bytes::copy_from_slice(&t.to_bytes())),
        conflicts: Vec::new(),
    }))
}

fn now() -> i64 {
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

fn target_hex(t: &RefTarget) -> String {
    t.to_bytes().iter().map(|b| format!("{b:02x}")).collect()
}

// ------------------------------------------------------------------------------ the request

/// One validated update.
struct Planned {
    text: String,
    name: RefName,
    expect: Expect,
    new: Option<RefTarget>,
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
        });
    }
    Ok(out)
}

#[derive(Serialize)]
struct EffectRef {
    name: String,
    old: Option<String>,
    new: Option<String>,
}

/// The `outbox.effects` document: the effects in order and what `post_receive` reports.
#[derive(Serialize)]
struct OutboxEffects<'a> {
    effects: &'a [&'static str],
    pusher: &'a str,
    /// Targets are hex of the 33-byte wire form (kind byte, then the id).
    refs: Vec<EffectRef>,
}

// ------------------------------------------------------------------------------ the handler

/// The `Refs` handler group of the full server: `UpdateRefs` through the pipeline; `ListRefs`
/// comes from the repository's engine until U47 replaces it.
pub struct ServerRefs {
    registry: Arc<Registry>,
    hooks: Arc<PipelineHooks>,
}

impl ServerRefs {
    pub fn new(registry: Arc<Registry>, hooks: Arc<PipelineHooks>) -> Self {
        Self { registry, hooks }
    }

    async fn windows_safe(&self, entry: &RepoEntry) -> Result<bool, Status> {
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
    async fn stored(
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
        // Step 1: the caller has write access (the service checked); validate the request.
        let op_id = ctx
            .op_id
            .ok_or_else(|| Status::invalid_argument("mutating calls need x-asc-op-id"))?;
        if ctx.id_mode != IdMode::Asc {
            return Err(Status::unimplemented(
                "git id mode is not served by UpdateRefs",
            ));
        }
        let planned = plan(&req)?;
        let mode = wire::UpdateMode::try_from(req.mode)
            .map_err(|_| Status::invalid_argument("unknown update mode"))?;
        if mode == wire::UpdateMode::Land {
            return Err(Status::unimplemented(
                "LAND is not available on this server yet",
            ));
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

        // Step 2: idempotency.
        if let Some(done) = self.stored(&entry, op_id).await? {
            return Ok(done);
        }
        // Step 3: a mirror takes no pushes.
        if entry.mode == RepoMode::Mirror {
            return Err(rpc(RpcError::MirrorMode(info(vec![ctx.repo.to_string()]))));
        }

        // Step 4: ingest the closure of every new target, under the shared GC lock.
        let _gc = self.hooks.gc().shared().await;
        let targets: Vec<RefTarget> = planned.iter().filter_map(|p| p.new).collect();
        let windows_safe = self.windows_safe(&entry).await?;
        Ingestor::new(
            self.registry.pool(),
            self.registry.refs(),
            &entry,
            windows_safe,
        )
        .ingest(&targets, self.hooks.observer())
        .await
        .map_err(ingest_status)?;

        // Step 5: policy. FAST_FORWARD needs the new value to descend from the current one.
        let graph = Graph::new(SqlGraph::new(
            self.registry.pool().clone(),
            entry.ns,
            entry.backend.engine().clone(),
        ));
        let mut updates = Vec::with_capacity(planned.len());
        let mut results = Vec::with_capacity(planned.len());
        let mut hook_updates = Vec::with_capacity(planned.len());
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
            });
            hook_updates.push(HookUpdate {
                name: p.text,
                old: current,
                new: p.new,
            });
            updates.push(RefUpdate {
                name: p.name,
                expect,
                new: p.new,
                expires_at: if p.new.is_some() { expires_at } else { None },
            });
        }

        // Steps 6 and 7: hook points.
        let hook_req = HookRequest {
            repo: &ctx.repo,
            op_id,
            principal: &ctx.principal,
            updates: &hook_updates,
            push_options: &req.push_options,
        };
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

        // Step 8: one transaction.
        let response = wire::UpdateRefsResponse {
            results,
            rewritten: Vec::new(),
            message: String::new(),
        };
        let txn = RefTxn {
            updates,
            actor: Actor {
                id: ctx.principal.id.clone(),
                via: "asc-push".into(),
            },
            reason: "UpdateRefs".into(),
            op_id,
        };
        let outbox = serde_json::to_vec(&OutboxEffects {
            effects: hooks.outbox_effects(),
            pusher: &ctx.principal.id,
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
        match self.commit(&entry, &txn, &response, &outbox).await {
            Ok(()) => Ok(response),
            Err(e) => {
                hooks.abort(&hook_req).await;
                // A retry of the same op that committed in the meantime is not a conflict.
                if let Some(done) = self.stored(&entry, op_id).await? {
                    return Ok(done);
                }
                Err(e)
            }
        }
    }

    /// The step-8 transaction: ref CAS and log, the op result, and the outbox row.
    async fn commit(
        &self,
        entry: &RepoEntry,
        txn: &RefTxn,
        response: &wire::UpdateRefsResponse,
        outbox: &[u8],
    ) -> Result<(), Status> {
        let refs = self.registry.refs();
        let mut tx = refs.begin().await.map_err(|e| internal("refs", &e))?;
        if is_postgres(self.registry.pool()) {
            sqlx::query("SELECT pg_advisory_xact_lock_shared($1)")
                .bind(GC_LOCK_KEY)
                .execute(&mut *tx)
                .await
                .map_err(|e| internal("gc lock", &e))?;
        }
        match refs.update_in_tx(&mut tx, entry.key, txn).await {
            Ok(()) => {}
            Err(RefError::Conflict(c)) => return Err(conflict(c)),
            Err(e @ RefError::NonUtf8Name(_)) => {
                return Err(Status::invalid_argument(e.to_string()));
            }
            Err(e) => return Err(internal("refs", &e)),
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
        tx.commit().await.map_err(|e| internal("commit", &e))
    }
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
        entry.backend.list_refs(ctx, req).await
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
    use crate::ingest::testutil::{commit, registry, repo, sig};

    fn ctx(op: Uuid) -> CallCtx {
        CallCtx {
            repo: RepoName::parse("o/r").unwrap(),
            id_mode: IdMode::Asc,
            principal: Principal { id: "alice".into() },
            op_id: Some(op),
            client: None,
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
}
