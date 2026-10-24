//! Land (pushrebase) inside the `UpdateRefs` pipeline (SPEC §8.5, PLAN U46).
//!
//! Only an explicit `UPDATE_MODE_LAND` request lands; a plain push stays a fast-forward. The
//! pushed tip is ingested first (pipeline step 4). Here the stack is found, the overlap policy
//! rewrites it onto the current tip, the rewritten changesets are ingested, the hooks see them,
//! and the step-8 transaction moves the ref, writes the `op_results` row, tells the
//! [`IngestObserver`] about the rewrites and appends to the mainline index (U65). A CAS miss
//! restarts from the stack search, at most [`MAX_ATTEMPTS`] times.

use std::collections::BTreeSet;
use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use asc_core::change::RewriteOp;
use asc_core::id::{ChangesetId, RefTarget};
use asc_engine::Engine;
use asc_engine::graph::Graph;
use asc_engine::land::NoMergeGlobs;
use asc_engine::merge::MergeOptions;
use asc_rpc::errors::RpcError;
use asc_rpc::server::{status_rpc_error, wire};
use asc_store::bucket::BucketObjects;
use asc_store::refs::{Actor, Expect, RefTxn, RefUpdate, Refs};
use sqlx::{Any, AnyPool, Row, Transaction};
use tonic::Status;
use uuid::Uuid;

use crate::graph_sql::SqlGraph;
use crate::ingest::{IngestObserver, Ingestor};
use crate::landing;
use crate::mainline::{self, MainlineIndex, MainlineStep, SqlMainline};
use crate::registry::RepoEntry;
use asc_core::path::RepoPath;
use asc_store::sqlrefs::SqlRefs;

use crate::update::{
    EffectRef, HookRequest, HookUpdate, OutboxEffects, Planned, ServerRefs, info, ingest_status,
    internal, rpc, target_hex,
};

/// Most CAS attempts of one land (SPEC §8.3 step 8).
pub const MAX_ATTEMPTS: usize = 20;
/// Longest stack a land rewrites; a longer one needs a client rebase.
pub const MAX_STACK: usize = 10_000;
/// Most first-parent steps indexed when a branch's mainline has to be built from its root.
pub const MAX_BACKFILL: usize = 200_000;
/// Most conflicting paths named in one `LAND_CONFLICT`.
pub(crate) const MAX_CONFLICT_PATHS: usize = 100;

type RepoEngine = Engine<BucketObjects, SqlRefs>;
type RepoGraph = Graph<SqlGraph<BucketObjects, SqlRefs>>;

/// Why a land could not produce a rewritten stack.
#[derive(Debug, thiserror::Error)]
pub enum LandError {
    /// The client has to rebase and push again (`LAND_NEEDS_CLIENT_REBASE`).
    #[error("land needs a client rebase: {0}")]
    NeedsClientRebase(&'static str),
    /// Paths the stack and the branch both changed (`LAND_CONFLICT`).
    #[error("land conflict on {} path(s)", .0.len())]
    Conflict(Vec<String>),
    /// The request set `no_content_merge` and these paths would need a content merge
    /// (`LAND_NEEDS_CONTENT_MERGE`, SPEC §20.5). No ref changes.
    #[error("land needs a content merge on {} path(s)", .0.len())]
    NeedsContentMerge(Vec<String>),
    /// As [`Self::Conflict`], with the `Conflict` records of the content merge (SPEC §8.5 step 3).
    #[error("land conflict on {} path(s)", .0.len())]
    Conflicts(Vec<wire::Conflict>),
    #[error("land: {0}")]
    Internal(String),
}

impl LandError {
    pub(crate) fn internal(e: impl std::fmt::Display) -> Self {
        Self::Internal(e.to_string())
    }
}

impl From<LandError> for Status {
    fn from(e: LandError) -> Self {
        match e {
            LandError::NeedsClientRebase(why) => {
                tracing::debug!(%why, "land refused");
                rpc(RpcError::LandNeedsClientRebase(info(Vec::new())))
            }
            LandError::Conflict(paths) => rpc(RpcError::LandConflict(info(paths))),
            LandError::NeedsContentMerge(paths) => {
                rpc(RpcError::LandNeedsContentMerge(info(paths)))
            }
            LandError::Conflicts(details) => {
                let mut detail = info(
                    details
                        .iter()
                        .map(|c| String::from_utf8_lossy(&c.path).into_owned())
                        .collect(),
                );
                match details
                    .iter()
                    .map(asc_rpc::convert::Conflict::try_from)
                    .collect::<Result<Vec<_>, _>>()
                {
                    Ok(records) => detail.conflicts = records,
                    Err(e) => return internal("land conflict", &e),
                }
                rpc(RpcError::LandConflict(detail))
            }
            LandError::Internal(m) => internal("land", &m),
        }
    }
}

// ------------------------------------------------------------------------------ overlap hook

/// Per-request knobs of the overlap policy.
#[derive(Clone, Copy, Debug, Default)]
pub struct LandOptions {
    /// `UpdateRefsRequest.no_content_merge`: fail instead of merging file contents (SPEC §20.5).
    pub no_content_merge: bool,
}

/// What a policy produced for a stack.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Rewrite {
    /// `(old, new)` for every stack changeset, oldest first; the new ones are stored.
    pub pairs: Vec<(ChangesetId, ChangesetId)>,
    /// Rewritten changesets that lost a `git.gpgsig`.
    pub signatures_dropped: usize,
}

/// How a land treats paths the stack shares with what the branch changed (SPEC §8.5 step 3).
/// The default refuses any overlap; U68 plugs the content merge in through this trait.
#[async_trait]
pub trait OverlapPolicy: Send + Sync {
    /// Rewrites `stack` (oldest first, each the child of the previous) onto `tip`, or names the
    /// conflicting paths with [`LandError::Conflict`].
    async fn rewrite(
        &self,
        engine: &Arc<RepoEngine>,
        stack: &[ChangesetId],
        tip: ChangesetId,
        opts: LandOptions,
    ) -> Result<Rewrite, LandError>;
}

/// Path-disjoint landing only (`land.contentMerge = false`, and the behaviour until U68).
pub struct DisjointOnly;

#[async_trait]
impl OverlapPolicy for DisjointOnly {
    async fn rewrite(
        &self,
        engine: &Arc<RepoEngine>,
        stack: &[ChangesetId],
        tip: ChangesetId,
        opts: LandOptions,
    ) -> Result<Rewrite, LandError> {
        // A glob that matches every path turns each overlap into a conflict without a merge.
        let all = NoMergeGlobs::new(&["**"]).map_err(LandError::internal)?;
        // The engine's rebase future is not provably `Send` (rustc cannot generalise a closure
        // inside `Engine::apply`), so it runs to completion on a blocking thread.
        let (engine, stack) = (engine.clone(), stack.to_vec());
        let handle = tokio::runtime::Handle::current();
        let merged = tokio::task::spawn_blocking(move || {
            handle.block_on(engine.content_merge_land(&stack, tip, &all, &MergeOptions::default()))
        })
        .await
        .map_err(LandError::internal)?
        .map_err(LandError::internal)?;
        if !merged.conflicts.is_empty() {
            let paths: BTreeSet<String> = merged
                .conflicts
                .iter()
                .take(MAX_CONFLICT_PATHS)
                .map(|c| c.path.to_string())
                .collect();
            let paths = paths.into_iter().collect();
            return Err(if opts.no_content_merge {
                LandError::NeedsContentMerge(paths)
            } else {
                LandError::Conflict(paths)
            });
        }
        Ok(Rewrite {
            pairs: merged.rewritten,
            signatures_dropped: merged.signatures_dropped,
        })
    }
}

// ------------------------------------------------------------------------------ planning

/// The outcome of one planning pass.
#[derive(Debug, PartialEq, Eq)]
pub struct Landing {
    /// Where the branch ends up.
    pub new_tip: ChangesetId,
    pub rewrite: Rewrite,
}

/// What a planning pass needs.
pub struct Plan<'a> {
    pub engine: &'a Arc<RepoEngine>,
    pub graph: &'a RepoGraph,
    pub policy: &'a dyn OverlapPolicy,
    pub pushed: ChangesetId,
    /// The branch tip now; `None` creates the branch.
    pub current: Option<ChangesetId>,
    pub require_signed: bool,
    pub opts: LandOptions,
}

/// Finds the stack from `pushed` back to the first changeset that is `tip` or one of its
/// ancestors (SPEC §8.5 step 1). Returns `(base, stack oldest first)`.
async fn find_stack(
    engine: &RepoEngine,
    graph: &RepoGraph,
    pushed: ChangesetId,
    tip: ChangesetId,
) -> Result<(ChangesetId, Vec<ChangesetId>), LandError> {
    let mut stack = Vec::new();
    let mut at = pushed;
    loop {
        if at == tip
            || graph
                .is_ancestor(at, tip)
                .await
                .map_err(LandError::internal)?
        {
            stack.reverse();
            return Ok((at, stack));
        }
        if stack.len() >= MAX_STACK {
            return Err(LandError::NeedsClientRebase("the stack is too long"));
        }
        let cs = engine
            .get_changeset(at)
            .await
            .map_err(LandError::internal)?;
        match cs.parents.as_slice() {
            [parent] => {
                stack.push(at);
                at = *parent;
            }
            _ => {
                return Err(LandError::NeedsClientRebase(
                    "the stack has a merge or no common history with the branch",
                ));
            }
        }
    }
}

/// SPEC §8.5 steps 1 to 5: what the branch should become for this request.
pub async fn plan(p: &Plan<'_>) -> Result<Landing, LandError> {
    let Some(tip) = p.current else {
        let fast = Landing {
            new_tip: p.pushed,
            rewrite: Rewrite::default(),
        };
        return Ok(fast);
    };
    let (base, stack) = find_stack(p.engine, p.graph, p.pushed, tip).await?;
    if stack.is_empty() {
        // Everything pushed is already part of the branch.
        return Ok(Landing {
            new_tip: tip,
            rewrite: Rewrite::default(),
        });
    }
    if base == tip {
        return Ok(Landing {
            new_tip: p.pushed,
            rewrite: Rewrite::default(),
        });
    }
    if p.require_signed {
        for &id in &stack {
            let cs = p
                .engine
                .get_changeset(id)
                .await
                .map_err(LandError::internal)?;
            if cs.extra.get(&b"git.gpgsig".to_vec()).is_some() {
                return Err(LandError::NeedsClientRebase(
                    "the branch requires signed commits and a server rebase would drop them",
                ));
            }
        }
    }
    let rewrite = p.policy.rewrite(p.engine, &stack, tip, p.opts).await?;
    let new_tip = rewrite
        .pairs
        .last()
        .map(|(_, new)| *new)
        .ok_or_else(|| LandError::internal("the policy rewrote nothing"))?;
    Ok(Landing { new_tip, rewrite })
}

// ------------------------------------------------------------------------------ request checks

/// A land moves exactly one branch to a changeset; returns that changeset.
pub(crate) fn check_request(planned: &[Planned], hooks: i32) -> Result<ChangesetId, Status> {
    let bad = |m: &str| Status::invalid_argument(m.to_owned());
    let [one] = planned else {
        return Err(bad("a land moves exactly one branch"));
    };
    if !one.text.starts_with("refs/heads/") {
        return Err(bad("only a branch can be landed on"));
    }
    if hooks == wire::HookMode::SkipPre as i32 {
        return Err(bad("a land cannot skip the pre-receive hook"));
    }
    match one.new {
        Some(RefTarget::Changeset(c)) => Ok(c),
        _ => Err(bad("a land needs a changeset target")),
    }
}

/// `repos.land_branches` lists the refs that accept LAND (short names or full refs). An empty
/// list means the default branch only.
pub(crate) async fn check_branch(
    pool: &AnyPool,
    entry: &RepoEntry,
    refname: &str,
) -> Result<(), Status> {
    let row = sqlx::query("SELECT land_branches FROM repos WHERE id = $1")
        .bind(entry.key.0)
        .fetch_one(pool)
        .await
        .map_err(|e| internal("repos", &e))?;
    let json: String = row
        .try_get("land_branches")
        .map_err(|e| internal("repos", &e))?;
    let listed: Vec<String> = serde_json::from_str(&json).unwrap_or_default();
    let short = refname.strip_prefix("refs/heads/").unwrap_or(refname);
    let ok = if listed.is_empty() {
        short == entry.default_branch
    } else {
        listed.iter().any(|b| b == refname || b == short)
    };
    if ok {
        Ok(())
    } else {
        Err(Status::failed_precondition(
            "this branch does not accept land requests",
        ))
    }
}

// ------------------------------------------------------------------------------ step 8 writes

/// The mainline rows a land appends (SPEC §5.1).
pub(crate) struct MainlineWrite {
    refname: String,
    base: Option<ChangesetId>,
    steps: Vec<MainlineStep>,
}

/// What the step-8 transaction does besides the ref CAS for a land.
pub(crate) struct TxWrites<'a> {
    observer: &'a dyn IngestObserver,
    pairs: &'a [(ChangesetId, ChangesetId)],
    op: RewriteOp,
    mainline: Option<MainlineWrite>,
}

impl<'a> TxWrites<'a> {
    /// Only the observer's `on_rewritten`, for a server-side `Merge` that rewrites changesets.
    pub(crate) fn rewrites(
        observer: &'a dyn IngestObserver,
        pairs: &'a [(ChangesetId, ChangesetId)],
        op: RewriteOp,
    ) -> Self {
        Self {
            observer,
            pairs,
            op,
            mainline: None,
        }
    }
}

impl TxWrites<'_> {
    pub(crate) async fn apply(
        &self,
        tx: &mut Transaction<'_, Any>,
        entry: &RepoEntry,
        txn: &RefTxn,
    ) -> Result<(), Status> {
        if !self.pairs.is_empty() {
            self.observer
                .on_rewritten(tx, entry.key, self.pairs, self.op)
                .await
                .map_err(|e| internal("on_rewritten", &e))?;
            // The rewrites belong to the single ref this update moves (SPEC §8.9).
            if let [one] = txn.updates.as_slice() {
                landing::record(tx, entry.ns, entry.key.0, &one.name, txn.op_id, self.pairs)
                    .await
                    .map_err(|e| internal("land_map", &e))?;
            }
        }
        if let Some(m) = &self.mainline {
            mainline::apply(tx, entry.ns, &m.refname, m.base, &m.steps)
                .await
                .map_err(|e| internal("mainline", &e))?;
            // The landing author's user refs whose stack has now landed get their marker
            // (SPEC §21.2.4a); other users' refs are never read.
            let steps: Vec<ChangesetId> = m.steps.iter().map(|s| s.cs).collect();
            let old_tip = match txn.updates.as_slice() {
                [
                    RefUpdate {
                        expect: Expect::Value(RefTarget::Changeset(c)),
                        ..
                    },
                ] => Some(*c),
                _ => None,
            };
            crate::reftl::mark_landed(
                tx,
                entry.ns,
                entry.key.0,
                &m.refname,
                &txn.actor.id,
                &steps,
                old_tip,
                self.pairs,
                crate::update::now(),
                crate::refauthz::DEFAULT_MAX_USER_REFS,
            )
            .await
            .map_err(|e| internal("landed marker", &e))?;
        }
        Ok(())
    }
}

/// The mainline steps that bring the index of `refname` from `old` to `new_tip`: the first-parent
/// changesets above `old` when it is indexed, else the whole first-parent history (bounded).
/// `None` skips the index (it is rebuilt by the next land that fits the bounds).
async fn mainline_write(
    entry: &RepoEntry,
    pool: &AnyPool,
    refname: &str,
    old: Option<ChangesetId>,
    new_tip: ChangesetId,
) -> Result<Option<MainlineWrite>, Status> {
    let engine = entry.backend.engine();
    let index = SqlMainline::new(pool.clone());
    let base = match old {
        Some(t) => index
            .position(entry.ns, refname, t)
            .await
            .map_err(|e| internal("mainline", &e))?
            .map(|_| t),
        None => None,
    };
    let cap = if base.is_some() {
        MAX_STACK + 1
    } else {
        MAX_BACKFILL
    };
    let mut steps = Vec::new();
    let mut at = new_tip;
    let reached = loop {
        if Some(at) == base {
            break true;
        }
        if steps.len() >= cap {
            tracing::warn!(%refname, "mainline index skipped: first-parent history too long");
            return Ok(None);
        }
        let cs = engine
            .get_changeset(at)
            .await
            .map_err(|e| internal("changeset", &e))?;
        let paths: Vec<RepoPath> = cs
            .changes
            .as_slice()
            .iter()
            .map(|(p, _)| p.clone())
            .collect();
        steps.push(MainlineStep { cs: at, paths });
        match cs.parents.first() {
            Some(&p) => at = p,
            None => break false,
        }
    };
    if base.is_some() && !reached {
        tracing::warn!(%refname, "mainline index skipped: new tip does not extend the indexed tip");
        return Ok(None);
    }
    steps.reverse();
    Ok(Some(MainlineWrite {
        refname: refname.to_owned(),
        base,
        steps,
    }))
}

// ------------------------------------------------------------------------------ the pipeline

/// One land request, after steps 1 to 4.
pub(crate) struct LandJob<'a> {
    pub ctx: &'an asc_rpc::server::CallCtx,
    pub req: &'a wire::UpdateRefsRequest,
    pub op_id: Uuid,
    pub entry: &'a RepoEntry,
    pub update: Planned,
    pub pushed: ChangesetId,
    pub graph: &'a RepoGraph,
    pub windows_safe: bool,
}

fn is_ref_conflict(s: &Status) -> bool {
    status_rpc_error(s).is_some_and(|e| e.code() == "REF_CONFLICT")
}

/// Pipeline steps 5 to 9 for a land.
pub(crate) async fn run(
    server: &ServerRefs,
    job: LandJob<'_>,
) -> Result<wire::UpdateRefsResponse, Status> {
    let hooks = server.hooks.push_hooks();
    let observer = server.hooks.observer();
    let engine = job.entry.backend.engine();
    let pool = server.registry.pool();
    let refname = job.update.text.clone();
    let require_signed = hooks.require_signed(job.ctx, &refname).await;
    let opts = LandOptions {
        no_content_merge: job.req.no_content_merge,
    };
    let mut last = Status::aborted("land did not complete");
    for _ in 0..MAX_ATTEMPTS {
        let current = server
            .registry
            .refs()
            .get(job.entry.key, &job.update.name)
            .await
            .map_err(|e| internal("refs", &e))?
            .map(|e| e.target);
        let current_cs = match current {
            None => None,
            Some(RefTarget::Changeset(c)) => Some(c),
            Some(RefTarget::Tag(_)) => {
                return Err(Status::failed_precondition(
                    "the ref does not name a changeset",
                ));
            }
        };
        let landing = plan(&Plan {
            engine,
            graph: job.graph,
            policy: server.hooks.overlap_policy(),
            pushed: job.pushed,
            current: current_cs,
            require_signed,
            opts,
        })
        .await?;

        // Step 5, second half: ingest what the rewrite produced.
        if !landing.rewrite.pairs.is_empty() {
            Ingestor::new(pool, server.registry.refs(), job.entry, job.windows_safe)
                .ingest(&[RefTarget::Changeset(landing.new_tip)], observer)
                .await
                .map_err(ingest_status)?;
        }

        let new = RefTarget::Changeset(landing.new_tip);
        let hook_updates = vec![HookUpdate {
            name: refname.clone(),
            old: current,
            new: Some(new),
        }];
        let hook_req = HookRequest {
            repo: &job.ctx.repo,
            op_id: job.op_id,
            principal: &job.ctx.principal,
            updates: &hook_updates,
            push_options: &job.req.push_options,
        };
        // Steps 6 and 7 see the rewritten changesets.
        hooks
            .bridge_quarantine(&hook_req)
            .await
            .map_err(|e| rpc(RpcError::HookRejected(info(vec![e.0]))))?;
        if let Err(e) = hooks.pre_receive(&hook_req).await {
            hooks.abort(&hook_req).await;
            return Err(rpc(RpcError::HookRejected(info(vec![e.0]))));
        }

        // Step 8: one transaction.
        let message = match landing.rewrite.signatures_dropped {
            0 => String::new(),
            n => format!(
                "signatures removed by server rebase: {n} (use 'git pull --rebase' and push again to keep them)"
            ),
        };
        let response = wire::UpdateRefsResponse {
            results: vec![wire::RefResult {
                name: refname.clone(),
                old: current.as_ref().map(wire::RefTarget::from),
                new: Some(wire::RefTarget::from(&new)),
                user: false,
            }],
            rewritten: landing
                .rewrite
                .pairs
                .iter()
                .map(|(old, new)| wire::Rewrite {
                    old: Bytes::copy_from_slice(old.as_bytes()),
                    new: Bytes::copy_from_slice(new.as_bytes()),
                })
                .collect(),
            message,
        };
        let txn = RefTxn {
            updates: vec![RefUpdate {
                name: job.update.name.clone(),
                expect: current.map_or(Expect::Absent, Expect::Value),
                new: Some(new),
                expires_at: None,
            }],
            actor: Actor {
                id: job.ctx.principal.id.clone(),
                via: "land".into(),
            },
            reason: "UpdateRefs".into(),
            op_id: job.op_id,
        };
        let outbox = serde_json::to_vec(&OutboxEffects {
            effects: hooks.outbox_effects(),
            pusher: &job.ctx.principal.id,
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
        let writes = TxWrites {
            observer,
            pairs: &landing.rewrite.pairs,
            op: RewriteOp::Land,
            mainline: mainline_write(job.entry, pool, &refname, current_cs, landing.new_tip)
                .await?,
        };
        match server
            .commit(job.entry, &txn, &response, &outbox, Some(&writes))
            .await
        {
            Ok(()) => return Ok(response),
            Err(e) => {
                hooks.abort(&hook_req).await;
                // A retry of the same op that committed in the meantime is not a conflict.
                if let Some(done) = server.stored(job.entry, job.op_id).await? {
                    return Ok(done);
                }
                if !is_ref_conflict(&e) {
                    return Err(e);
                }
                last = e;
            }
        }
    }
    Err(last)
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use asc_core::path::RefName;
    use asc_engine::commit::{NewChangeset, PathPolicy};
    use asc_rpc::IdMode;
    use asc_rpc::auth::Principal;
    use asc_rpc::server::{CallCtx, RefsHandler, RepoName};
    use futures::future::join_all;

    use super::*;
    use crate::ingest::testutil::{commit, registry, repo, sig, tree};
    use crate::registry::Registry;

    fn ctx(op: Uuid) -> CallCtx {
        CallCtx {
            repo: RepoName::parse("o/r").unwrap(),
            id_mode: IdMode::Asc,
            principal: Principal { id: "alice".into() },
            op_id: Some(op),
            client: None,
        }
    }

    fn req(
        name: &str,
        expect: wire::ref_update::Expect,
        to: ChangesetId,
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
                new: Some(wire::RefTarget::from(&RefTarget::Changeset(to))),
                delete: false,
            }],
            mode: mode as i32,
            ..wire::UpdateRefsRequest::default()
        }
    }

    fn land_req(to: ChangesetId) -> wire::UpdateRefsRequest {
        req(
            "refs/heads/master",
            wire::ref_update::Expect::Any(true),
            to,
            wire::UpdateMode::Land,
        )
    }

    fn code(s: &Status) -> String {
        status_rpc_error(s).map_or_else(|| s.code().to_string(), |e| e.code().to_owned())
    }

    fn master() -> RefName {
        RefName::new("refs/heads/master").unwrap()
    }

    async fn count(r: &Registry, sql: &str) -> i64 {
        sqlx::query(sqlx::AssertSqlSafe(sql.to_owned()))
            .fetch_one(r.pool())
            .await
            .unwrap()
            .get::<i64, _>(0)
    }

    async fn tip(r: &Registry, e: &RepoEntry) -> ChangesetId {
        match r
            .refs()
            .get(e.key, &master())
            .await
            .unwrap()
            .unwrap()
            .target
        {
            RefTarget::Changeset(c) => c,
            RefTarget::Tag(_) => panic!("tag"),
        }
    }

    /// A server with `master` at a commit that adds `base`.
    async fn setup() -> (Arc<Registry>, Arc<RepoEntry>, ServerRefs, ChangesetId) {
        let r = registry().await;
        let e = repo(&r).await;
        let s = ServerRefs::new(r.clone(), Arc::default());
        let a = commit(&e, &[], &[("base", b"0")]).await;
        let create = req(
            "refs/heads/master",
            wire::ref_update::Expect::Absent(true),
            a,
            wire::UpdateMode::FastForward,
        );
        s.update_refs(&ctx(Uuid::new_v4()), create).await.unwrap();
        (r, e, s, a)
    }

    async fn land(s: &ServerRefs, to: ChangesetId) -> Result<wire::UpdateRefsResponse, Status> {
        s.update_refs(&ctx(Uuid::new_v4()), land_req(to)).await
    }

    async fn first_parent_chain(e: &RepoEntry, mut at: ChangesetId) -> Vec<ChangesetId> {
        let mut out = vec![at];
        while let Some(&p) = e
            .backend
            .engine()
            .get_changeset(at)
            .await
            .unwrap()
            .parents
            .first()
        {
            out.push(p);
            at = p;
        }
        out.reverse();
        out
    }

    #[tokio::test]
    async fn twenty_concurrent_disjoint_lands_make_a_linear_history() {
        let (r, e, s, a) = setup().await;
        let names: Vec<String> = (0..20).map(|i| format!("f{i}")).collect();
        let mut tips = Vec::new();
        for n in &names {
            tips.push(commit(&e, &[a], &[(n.as_str(), b"x")]).await);
        }
        let results = join_all(tips.iter().map(|&t| land(&s, t))).await;
        for res in &results {
            res.as_ref().unwrap();
        }
        let head = tip(&r, &e).await;
        let chain = first_parent_chain(&e, head).await;
        assert_eq!(chain.len(), 21);
        assert_eq!(chain[0], a);
        // Every landed changeset is a single-parent child of the previous one.
        let root = e.backend.engine().get_changeset(head).await.unwrap().root;
        for n in &names {
            let got = e
                .backend
                .engine()
                .lookup(root, &RepoPath::new(n.as_str()).unwrap())
                .await
                .unwrap();
            assert!(got.is_some(), "{n}");
        }
        assert_eq!(count(&r, "SELECT COUNT(*) FROM mainline").await, 21);
        assert_eq!(
            count(&r, "SELECT COUNT(*) FROM ref_log WHERE via = 'land'").await,
            20
        );
        assert_eq!(count(&r, "SELECT COUNT(*) FROM op_results").await, 21);
    }

    #[tokio::test]
    async fn overlapping_changes_conflict_and_name_the_paths() {
        let (r, e, s, a) = setup().await;
        let mine = commit(&e, &[a], &[("x", b"1"), ("ok", b"1")]).await;
        let theirs = commit(&e, &[a], &[("x", b"2")]).await;
        land(&s, mine).await.unwrap();
        let before = tip(&r, &e).await;
        let err = land(&s, theirs).await.unwrap_err();
        assert_eq!(code(&err), "LAND_CONFLICT");
        assert_eq!(
            status_rpc_error(&err).unwrap().info().paths,
            vec!["x".to_string()]
        );
        assert_eq!(tip(&r, &e).await, before);
    }

    #[tokio::test]
    async fn file_directory_and_case_fold_overlaps_are_conflicts() {
        let (r, e, s, a) = setup().await;
        let file = commit(&e, &[a], &[("d", b"file"), ("A.ts", b"1")]).await;
        let dir = commit(&e, &[a], &[("d/inner", b"x")]).await;
        let fold = commit(&e, &[a], &[("a.ts", b"2")]).await;
        land(&s, file).await.unwrap();
        for t in [dir, fold] {
            let err = land(&s, t).await.unwrap_err();
            assert_eq!(code(&err), "LAND_CONFLICT");
        }
        assert_eq!(
            count(&r, "SELECT COUNT(*) FROM ref_log WHERE via = 'land'").await,
            1
        );
    }

    #[tokio::test]
    async fn a_retried_op_returns_the_original_mapping_without_rebasing_again() {
        let (r, e, s, a) = setup().await;
        let first = commit(&e, &[a], &[("one", b"1")]).await;
        let second = commit(&e, &[a], &[("two", b"2")]).await;
        land(&s, first).await.unwrap();
        let op = Uuid::new_v4();
        let done = s.update_refs(&ctx(op), land_req(second)).await.unwrap();
        assert_eq!(done.rewritten.len(), 1);
        let graph_rows = count(&r, "SELECT COUNT(*) FROM cs_graph").await;
        let again = s.update_refs(&ctx(op), land_req(second)).await.unwrap();
        assert_eq!(done, again);
        assert_eq!(count(&r, "SELECT COUNT(*) FROM cs_graph").await, graph_rows);
        assert_eq!(
            count(&r, "SELECT COUNT(*) FROM ref_log WHERE via = 'land'").await,
            2
        );
        let _ = e;
    }

    async fn with_extra(
        e: &RepoEntry,
        parent: ChangesetId,
        file: (&str, &[u8]),
        extra: Vec<(Vec<u8>, Vec<u8>)>,
    ) -> ChangesetId {
        let engine = e.backend.engine();
        let base = engine.get_changeset(parent).await.unwrap().root;
        let root = tree(e, base, &[file]).await;
        engine
            .create_changeset(
                NewChangeset {
                    parents: vec![parent],
                    author: sig(),
                    committer: sig(),
                    message: b"signed".to_vec(),
                    extra,
                    root,
                    copies: Vec::new(),
                },
                PathPolicy::Native { windows_safe: true },
            )
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn signed_stack_unprotected_drops_sig() {
        let (r, e, s, a) = setup().await;
        let signed = with_extra(
            &e,
            a,
            ("s", b"1"),
            vec![(
                b"git.gpgsig".to_vec(),
                b"-----BEGIN PGP SIGNATURE-----".to_vec(),
            )],
        )
        .await;
        let other = commit(&e, &[a], &[("o", b"1")]).await;
        land(&s, other).await.unwrap();
        let res = land(&s, signed).await.unwrap();
        assert!(
            res.message
                .starts_with("signatures removed by server rebase: 1")
        );
        let new = ChangesetId::from_bytes(res.rewritten[0].new.as_ref().try_into().unwrap());
        let cs = e.backend.engine().get_changeset(new).await.unwrap();
        assert!(cs.extra.get(&b"git.gpgsig".to_vec()).is_none());
        assert_eq!(tip(&r, &e).await, new);
    }

    struct Signed;

    #[async_trait]
    impl crate::update::PushHooks for Signed {
        async fn require_signed(&self, _: &CallCtx, _: &str) -> bool {
            true
        }
    }

    #[tokio::test]
    async fn a_signed_stack_on_a_protected_branch_needs_a_client_rebase() {
        let (r, e, s, a) = setup().await;
        s.hooks.set_push_hooks(Arc::new(Signed)).ok().unwrap();
        let signed = with_extra(
            &e,
            a,
            ("s", b"1"),
            vec![(b"git.gpgsig".to_vec(), b"sig".to_vec())],
        )
        .await;
        let other = commit(&e, &[a], &[("o", b"1")]).await;
        land(&s, other).await.unwrap();
        let err = land(&s, signed).await.unwrap_err();
        assert_eq!(code(&err), "LAND_NEEDS_CLIENT_REBASE");
        // A stack that is already on the tip is a fast-forward and needs no rewrite.
        let head = tip(&r, &e).await;
        let fast = with_extra(
            &e,
            head,
            ("t", b"1"),
            vec![(b"git.gpgsig".to_vec(), b"sig".to_vec())],
        )
        .await;
        let res = land(&s, fast).await.unwrap();
        assert!(res.rewritten.is_empty());
        assert_eq!(tip(&r, &e).await, fast);
    }

    #[tokio::test]
    async fn raw_git_commit_rebased_exports_valid() {
        let (r, e, s, a) = setup().await;
        // An imported changeset keeps its raw git body; it is already stored and known.
        let plain = commit(&e, &[a], &[("raw", b"1")]).await;
        let mut cs = (*e.backend.engine().get_changeset(plain).await.unwrap()).clone();
        cs.extra = asc_core::object::SortedMap::new(vec![(
            b"git.commit".to_vec(),
            b"tree 0000\n".to_vec(),
        )])
        .unwrap();
        let envelope = asc_core::codec::Enveloped::to_envelope(&cs);
        let raw = ChangesetId::of(&envelope);
        use asc_store::objects::Objects;
        e.objects
            .put(
                &asc_store::key::ObjKey::Changeset(raw),
                asc_store::codec::encode_stored(&envelope),
            )
            .await
            .unwrap();
        let mut tx = r.pool().begin().await.unwrap();
        crate::graph_sql::insert_nodes(
            &mut tx,
            e.ns,
            &[crate::graph_sql::NewNode {
                id: raw,
                generation: 2,
                parents: &[a],
            }],
        )
        .await
        .unwrap();
        tx.commit().await.unwrap();

        let other = commit(&e, &[a], &[("o", b"1")]).await;
        land(&s, other).await.unwrap();
        let res = land(&s, raw).await.unwrap();
        let new = ChangesetId::from_bytes(res.rewritten[0].new.as_ref().try_into().unwrap());
        let rewritten = e.backend.engine().get_changeset(new).await.unwrap();
        assert!(rewritten.extra.get(&b"git.commit".to_vec()).is_none());
        // It exports as a valid git commit whose parent is the previous tip.
        let git = e.backend.engine().git_commit_of(new).await.unwrap();
        let parent_git = e
            .backend
            .engine()
            .git_commit_of(rewritten.parents[0])
            .await
            .unwrap();
        let row = sqlx::query("SELECT COUNT(*) FROM git_map WHERE kind = 3 AND git IN ($1, $2)")
            .bind(git.as_bytes().to_vec())
            .bind(parent_git.as_bytes().to_vec())
            .fetch_one(r.pool())
            .await
            .unwrap();
        assert_eq!(row.get::<i64, _>(0), 2);
    }

    #[tokio::test]
    async fn fast_forward_land_and_unlisted_branches() {
        let (r, e, s, a) = setup().await;
        let next = commit(&e, &[a], &[("n", b"1")]).await;
        let res = land(&s, next).await.unwrap();
        assert!(res.rewritten.is_empty());
        assert_eq!(tip(&r, &e).await, next);
        let elsewhere = req(
            "refs/heads/topic",
            wire::ref_update::Expect::Any(true),
            next,
            wire::UpdateMode::Land,
        );
        let err = s
            .update_refs(&ctx(Uuid::new_v4()), elsewhere)
            .await
            .unwrap_err();
        assert_eq!(err.code(), tonic::Code::FailedPrecondition);
        sqlx::query("UPDATE repos SET land_branches = '[\"topic\"]'")
            .execute(r.pool())
            .await
            .unwrap();
        let elsewhere = req(
            "refs/heads/topic",
            wire::ref_update::Expect::Any(true),
            next,
            wire::UpdateMode::Land,
        );
        s.update_refs(&ctx(Uuid::new_v4()), elsewhere)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn the_observer_hears_rewrites_in_the_commit_transaction() {
        struct Rewrites(AtomicUsize);
        #[async_trait]
        impl IngestObserver for Rewrites {
            async fn on_rewritten(
                &self,
                _tx: &mut Transaction<'_, Any>,
                _: asc_store::refs::RepoKey,
                pairs: &[(ChangesetId, ChangesetId)],
                kind: RewriteOp,
            ) -> Result<(), crate::ingest::ObserverError> {
                assert_eq!(kind, RewriteOp::Land);
                self.0.fetch_add(pairs.len(), Ordering::SeqCst);
                Ok(())
            }
        }
        let (_r, e, s, a) = setup().await;
        let seen = Arc::new(Rewrites(AtomicUsize::new(0)));
        s.hooks.set_observer(seen.clone()).ok().unwrap();
        let one = commit(&e, &[a], &[("one", b"1")]).await;
        let two = commit(&e, &[one], &[("two", b"1")]).await;
        let other = commit(&e, &[a], &[("other", b"1")]).await;
        land(&s, other).await.unwrap();
        land(&s, two).await.unwrap();
        assert_eq!(seen.0.load(Ordering::SeqCst), 2);
    }
}
