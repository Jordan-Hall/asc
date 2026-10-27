//! `RestackChanges` (PLAN C22, SPEC §20.5 flow 4 and D11).
//!
//! After a parent lands, the forge asks the server to move the open children onto the new trunk
//! tip: `RestackChanges(onto, items bottom to top, atomic)`. Every item names a change; its head
//! is the newest published version in `change_versions`. The server rebases each head with the
//! engine's changeset rebase (metadata only when the paths are disjoint, a three-way file merge
//! otherwise) and stores a new changeset that keeps the author and message.
//!
//! * **Committer and time.** The rewritten changesets use one committer identity and one
//!   timestamp for the whole call. By default these are the authenticated actor and the server
//!   clock read once. The request may carry `committer_name`, `committer_email` and
//!   `committer_time`, so a retry yields the same ids; only the forge's internal principal may
//!   set them (`PERMISSION_DENIED` otherwise), because they are the identity written into history.
//! * **Base of an item.** The new head of the item's parent change when that change is also an
//!   item, else `onto`. An item whose head already sits on its base is left alone (it is not in
//!   `rewritten`), so a repeat of a finished call rewrites nothing.
//! * **Conflicts are an answer.** The conflicting item and every item above it in the stack stay
//!   as they are; the response lists the conflicts of the item that failed. With `atomic` any
//!   conflict means nothing is rewritten and no ref moves.
//! * **`patch_unchanged`** lists the changes whose own `changes` list is identical before and
//!   after, which is what keeps an approval alive (D11).
//! * **`dry_run`** stores the rewritten changesets, which nothing refers to, and writes no ref,
//!   `changes` row or version.
//! * **Refs.** `update_ref` is `refs/pull/<n>/head` or empty, on every item or on none. Any other
//!   ref, a branch above all, is refused: a restack never moves a branch. With refs, all moves go
//!   through one `UpdateRefs` (mode `FORCE`, hooks run, compare-and-set on `expect`, which must
//!   be the item's head when it names a changeset), so the same policy as a push binds. The call
//!   is idempotent by `x-asc-op-id`. Without refs the rewritten heads are ingested and indexed in
//!   one transaction.
//! * **Versions.** Each rewritten change gets its next version, with the old head as the lease
//!   predecessor, in the transaction that indexes it (no refs) or right after the ref move
//!   (refs; a retry with the same op id appends what a failed first attempt did not).
//!   `parent_change` is kept when the parent change is part of the call, else cleared: the item
//!   now sits on `onto`.
//!
//! Bounds: at most [`MAX_ITEMS`] items, [`MAX_CONFLICTS`] reported conflicts, and the merge text
//! budget is reserved before any file is read. The service has checked write access.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use asc_core::change::{ChangeId, RewriteOp};
use asc_core::id::{ChangesetId, RefTarget};
use asc_core::object::{Change, Changeset, Entry, Kind};
use asc_core::sig::Signature;
use asc_engine::commit::{NewChangeset, PathPolicy};
use asc_engine::merge::{self, Conflict, ConflictKind};
use asc_engine::rebase::Rebased;
use asc_rpc::errors::RpcError;
use asc_rpc::server::{CallCtx, MAX_STRING, wire};
use asc_rpc::{IdMode, WireId};
use bytes::Bytes;
use sqlx::{Any, Transaction};
use tonic::Status;
use tonic::metadata::MetadataMap;
use uuid::Uuid;

use super::rpc::MAX_STACK;
use super::{index, versions};
use crate::commit_graph_rpc::{commit_status, internal};
use crate::idmode::{Eng, IdCodec};
use crate::ingest::Ingestor;
use crate::mutations::MERGE_TEXT_ESTIMATE;
use crate::mutations::land_merge::agent_merge_options;
use crate::read::ReadHooks;
use crate::registry::{Extensions, Ns, Registry, RepoEntry, RepoMode};
use crate::update::{PipelineHooks, Rewrites, ServerRefs, info, ingest_status, rpc};

/// Most items one call may restack.
pub const MAX_ITEMS: usize = MAX_STACK;
/// Most conflicts one answer lists.
pub const MAX_CONFLICTS: usize = 1_000;
/// Longest committer name or email accepted.
const MAX_IDENT: usize = 512;

type Pairs = Vec<(ChangesetId, ChangesetId)>;

fn invalid(m: impl Into<String>) -> Status {
    Status::invalid_argument(m.into())
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|d| i64::try_from(d.as_secs()).ok())
        .unwrap_or(0)
}

/// Runs an engine future to completion on a blocking thread: the rebase and commit futures are
/// not provably `Send`.
async fn on_blocking_thread<F, Fut, T>(make: F) -> Result<T, Status>
where
    F: FnOnce() -> Fut + Send + 'static,
    Fut: std::future::Future<Output = T>,
    T: Send + 'static,
{
    let handle = tokio::runtime::Handle::current();
    tokio::task::spawn_blocking(move || handle.block_on(make()))
        .await
        .map_err(|e| internal("engine task", &e))
}

fn merge_status(e: merge::Error) -> Status {
    match e {
        merge::Error::Commit(e) => commit_status(e),
        e @ merge::Error::NotLinear(_) => Status::failed_precondition(e.to_string()),
        e => internal("merge", &e),
    }
}

/// The wire form of a merge conflict of the head `source`.
fn conflict_wire(source: ChangesetId, c: &Conflict) -> wire::Conflict {
    use wire::ConflictKind as W;
    let id = |e: &Option<Entry>| match e {
        Some(e) if matches!(e.kind, Kind::File | Kind::Exec | Kind::Symlink) => {
            Bytes::copy_from_slice(&e.id)
        }
        _ => Bytes::new(),
    };
    wire::Conflict {
        changeset: Bytes::copy_from_slice(source.as_bytes()),
        path: Bytes::copy_from_slice(c.path.as_bytes()),
        kind: match c.kind {
            ConflictKind::Content => W::Content,
            ConflictKind::AddAdd => W::AddAdd,
            ConflictKind::ModifyDelete => W::ModifyDelete,
            ConflictKind::FileDir => W::FileDir,
            ConflictKind::CaseFold => W::CaseFold,
            ConflictKind::Binary => W::Binary,
        } as i32,
        base_id: id(&c.stages.base),
        ours_id: id(&c.stages.ours),
        theirs_id: id(&c.stages.theirs),
        marked_id: c
            .marked_id
            .map_or_else(Bytes::new, |f| Bytes::copy_from_slice(f.as_bytes())),
    }
}

fn wire_pairs(pairs: &[(ChangesetId, ChangesetId)]) -> Vec<wire::Rewrite> {
    pairs
        .iter()
        .map(|(old, new)| wire::Rewrite {
            old: Bytes::copy_from_slice(old.as_bytes()),
            new: Bytes::copy_from_slice(new.as_bytes()),
        })
        .collect()
}

/// `refs/pull/<number>/head`, the only ref a restack moves.
fn is_pull_head(name: &str) -> bool {
    name.strip_prefix("refs/pull/")
        .and_then(|r| r.strip_suffix("/head"))
        .is_some_and(|n| n.len() <= 20 && !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
}

/// The `x-asc-op-id` header, when present.
pub(super) fn op_id_of(meta: &MetadataMap) -> Result<Option<Uuid>, Status> {
    meta.get("x-asc-op-id")
        .map(|v| {
            v.to_str()
                .ok()
                .and_then(|s| Uuid::parse_str(s).ok())
                .ok_or_else(|| invalid("malformed x-asc-op-id"))
        })
        .transpose()
}

/// Whether the patch of `a` and `b` is the same list of leaf changes.
fn same_patch(a: &Changeset, b: &Changeset) -> bool {
    a.changes == b.changes
}

/// `(destination, source)` for every copy annotation of `cs`.
fn copies_of(cs: &Changeset) -> Vec<(asc_core::path::RepoPath, asc_core::path::RepoPath)> {
    cs.changes
        .as_slice()
        .iter()
        .filter_map(|(path, change)| match change {
            Change::Write {
                copy_from: Some(src),
                ..
            } => Some((path.clone(), src.clone())),
            _ => None,
        })
        .collect()
}

// ------------------------------------------------------------------------------ the request

/// One validated item.
struct Item {
    change: ChangeId,
    update_ref: Option<String>,
    expect: wire::ref_update::Expect,
    /// The changeset `expect` names, when it names one.
    expected: Option<ChangesetId>,
}

/// A request that passed every check that needs no storage.
struct Parsed {
    items: Vec<Item>,
    ident: Option<(String, String)>,
    time: Option<i64>,
}

impl Parsed {
    fn with_refs(&self) -> bool {
        self.items.iter().any(|i| i.update_ref.is_some())
    }
}

/// What one item's head is.
struct Head {
    cs: ChangesetId,
    parent: Option<ChangeId>,
}

/// One rewritten item.
struct Step {
    item: usize,
    change: ChangeId,
    old: ChangesetId,
    new: ChangesetId,
    unchanged: bool,
}

// ------------------------------------------------------------------------------ the handler

/// The `RestackChanges` handler behind the `Changes` service.
pub struct Restacker {
    registry: Arc<Registry>,
    pipeline: Arc<PipelineHooks>,
    hooks: Arc<ReadHooks>,
    refs: ServerRefs,
}

/// The unit's one line in `register_all` (through [`super::register`]): hands the `Changes`
/// handler its `RestackChanges` implementation.
pub fn register(ext: &Extensions, registry: &Arc<Registry>, hooks: &Arc<ReadHooks>) {
    let handler = Arc::new(Restacker {
        registry: registry.clone(),
        pipeline: ext.pipeline.clone(),
        hooks: hooks.clone(),
        refs: ServerRefs::new(registry.clone(), ext.pipeline.clone()),
    });
    let changes = ext
        .changes
        .get()
        .expect("the Changes handler is registered before RestackChanges");
    assert!(
        changes.restack.set(handler).is_ok(),
        "RestackChanges is registered once"
    );
}

impl Restacker {
    fn parse(&self, ctx: &CallCtx, req: &wire::RestackChangesRequest) -> Result<Parsed, Status> {
        if req.items.is_empty() || req.items.len() > MAX_ITEMS {
            return Err(invalid(format!(
                "restack 1 to {MAX_ITEMS} changes per call"
            )));
        }
        if req.onto.len() > MAX_STRING {
            return Err(invalid("onto is too long"));
        }
        let mut changes = HashSet::new();
        let mut names = HashSet::new();
        let mut items = Vec::with_capacity(req.items.len());
        for it in &req.items {
            let change = <[u8; 16]>::try_from(it.change.as_ref())
                .map(ChangeId::from_bytes)
                .map_err(|_| invalid("a change id is 16 bytes"))?;
            if !changes.insert(change) {
                return Err(invalid(format!("change {change} is listed twice")));
            }
            let update_ref = (!it.update_ref.is_empty()).then(|| it.update_ref.clone());
            match &update_ref {
                Some(name) => {
                    if !is_pull_head(name) {
                        return Err(invalid("only refs/pull/<n>/head can be restacked"));
                    }
                    if !names.insert(name.clone()) {
                        return Err(invalid(format!("{name} is listed twice")));
                    }
                }
                None if it.expect.is_some() => {
                    return Err(invalid("expect needs an update_ref"));
                }
                None => {}
            }
            let (expect, expected) = match &it.expect {
                Some(t) => match RefTarget::try_from(t).map_err(|e| invalid(e.to_string()))? {
                    RefTarget::Changeset(c) => {
                        (wire::ref_update::Expect::Value(t.clone()), Some(c))
                    }
                    RefTarget::Tag(_) => return Err(invalid("expect must name a changeset")),
                },
                None => (wire::ref_update::Expect::Absent(true), None),
            };
            items.push(Item {
                change,
                update_ref,
                expect,
                expected,
            });
        }
        let with_refs = items.iter().filter(|i| i.update_ref.is_some()).count();
        if with_refs != 0 && with_refs != items.len() {
            return Err(invalid("update_ref is set on every item or on none"));
        }
        let overrides = !req.committer_name.is_empty()
            || !req.committer_email.is_empty()
            || req.committer_time != 0;
        if overrides && !self.pipeline.push_hooks().may_skip_pre(ctx) {
            return Err(Status::permission_denied(
                "only the forge may set the committer or the time of a restack",
            ));
        }
        if req.committer_time < 0 {
            return Err(invalid("committer_time is negative"));
        }
        if req.committer_name.is_empty() != req.committer_email.is_empty() {
            return Err(invalid("set the committer name and email together"));
        }
        if req.committer_name.len() > MAX_IDENT || req.committer_email.len() > MAX_IDENT {
            return Err(invalid("committer name or email is too long"));
        }
        Ok(Parsed {
            items,
            ident: (!req.committer_name.is_empty())
                .then(|| (req.committer_name.clone(), req.committer_email.clone())),
            time: (req.committer_time != 0).then_some(req.committer_time),
        })
    }

    /// The committer of every changeset of the call: the request's, else the actor; the time is
    /// read once.
    fn committer(ctx: &CallCtx, parsed: &Parsed) -> Result<Signature, Status> {
        let (name, email) = parsed.ident.clone().unwrap_or_else(|| {
            (
                ctx.principal.id.clone(),
                format!("{}@asc.invalid", ctx.principal.id),
            )
        });
        Signature::new(
            name.into_bytes(),
            email.into_bytes(),
            parsed.time.unwrap_or_else(now),
            0,
        )
        .map_err(|e| invalid(format!("committer: {e}")))
    }

    pub async fn restack_changes(
        &self,
        ctx: &CallCtx,
        req: wire::RestackChangesRequest,
    ) -> Result<wire::RestackChangesResponse, Status> {
        let parsed = self.parse(ctx, &req)?;
        if ctx.id_mode != IdMode::Asc {
            return Err(Status::unimplemented(
                "git id mode is not served by RestackChanges",
            ));
        }
        let writes = !req.dry_run;
        if writes && ctx.op_id.is_none() {
            return Err(invalid("mutating calls need x-asc-op-id"));
        }
        let entry = self.registry.open(&ctx.repo).await.map_err(|e| match e {
            crate::registry::RegistryError::NotFound(_) => Status::not_found("no such repository"),
            e => internal("registry", &e),
        })?;
        if writes && entry.mode == RepoMode::Mirror {
            return Err(rpc(RpcError::MirrorMode(info(vec![ctx.repo.to_string()]))));
        }
        self.authorize(&entry, ctx, &parsed.items).await?;
        let committer = Self::committer(ctx, &parsed)?;
        let engine = entry.backend.engine().clone();
        if writes
            && parsed.with_refs()
            && let Some(pairs) = self.replayed(&entry, ctx, &parsed).await?
        {
            return self
                .answer_replay(&entry, &engine, ctx, &pairs, committer.time())
                .await;
        }
        let ids = IdCodec::new(ctx.id_mode, &self.registry, &entry).for_viewer(&ctx.principal);
        let onto = self.onto(&ids, &entry, &req.onto).await?;
        let heads = self.heads(&entry, &parsed.items).await?;

        // Parents come first, and a caller that names a head other than the change's is stale.
        let pos: HashMap<ChangeId, usize> = parsed
            .items
            .iter()
            .enumerate()
            .map(|(i, it)| (it.change, i))
            .collect();
        for (i, (item, head)) in parsed.items.iter().zip(&heads).enumerate() {
            if let Some(p) = head.parent
                && pos.get(&p).is_some_and(|&at| at >= i)
            {
                return Err(invalid("items must run from the bottom of the stack up"));
            }
            if item.expected.is_some_and(|e| e != head.cs) {
                return Err(Status::failed_precondition(format!(
                    "the expected value of {} is not the head of change {}",
                    item.update_ref.as_deref().unwrap_or_default(),
                    item.change
                )));
            }
        }

        // Without refs `UpdateRefs` takes the fence itself; here it spans the ingest.
        let _gc = if writes && !parsed.with_refs() {
            Some(self.pipeline.gc().shared().await.map_err(|e| {
                tracing::warn!(error = %e, "gc fence unavailable");
                Status::unavailable("garbage-collection fence unavailable; retry")
            })?)
        } else {
            None
        };
        let _text = self
            .hooks
            .text_budget
            .acquire(MERGE_TEXT_ESTIMATE)
            .await
            .map_err(|_| {
                Status::resource_exhausted("text memory budget exhausted; retry shortly")
            })?;
        let windows_safe = self.refs.windows_safe(&entry).await?;

        let (steps, conflicts) = self
            .replay(
                &engine,
                &parsed,
                &heads,
                &pos,
                onto,
                &committer,
                windows_safe,
            )
            .await?;
        if req.atomic && !conflicts.is_empty() {
            return Ok(wire::RestackChangesResponse {
                conflicts,
                ..wire::RestackChangesResponse::default()
            });
        }
        let pairs: Pairs = steps.iter().map(|s| (s.old, s.new)).collect();
        let patch_unchanged = steps
            .iter()
            .filter(|s| s.unchanged)
            .map(|s| Bytes::copy_from_slice(s.change.as_bytes()))
            .collect();
        if !writes || pairs.is_empty() {
            return Ok(wire::RestackChangesResponse {
                rewritten: wire_pairs(&pairs),
                conflicts,
                patch_unchanged,
            });
        }
        let rewritten = if parsed.with_refs() {
            self.publish_refs(&entry, ctx, &parsed, &steps, &pairs, committer.time())
                .await?
        } else {
            self.publish_plain(&entry, ctx, &pairs, windows_safe, committer.time())
                .await?;
            wire_pairs(&pairs)
        };
        Ok(wire::RestackChangesResponse {
            rewritten,
            conflicts,
            patch_unchanged,
        })
    }

    /// The changeset `onto` names; empty names the default branch.
    async fn onto(
        &self,
        ids: &IdCodec,
        entry: &RepoEntry,
        spec: &str,
    ) -> Result<ChangesetId, Status> {
        let spec = if spec.is_empty() { "HEAD" } else { spec };
        // `resolve` refuses every name that reaches another user's ref and holds the engine's
        // resolver to the same rule; a ref the caller may not see is `not_found`.
        ids.resolve(spec, &entry.default_branch, |e| match e {
            asc_engine::graph::Error::Query(m) => invalid(m),
            e => internal("resolve", &e),
        })
        .await?
        .ok_or_else(|| Status::not_found(format!("unknown revision {spec:?}")))
    }

    /// Every item is a change the caller owns: it published the latest version or the first one
    /// (the change's creator). A restack appends versions to the changes it rewrites, so write
    /// access alone does not open another user's change to it. The forge's internal principal
    /// restacks on behalf of any author (SPEC §20.5 flow 4, D11); everyone else, the MCP
    /// `restack` tool included, only their own. A change that is not the caller's is refused
    /// for a dry run too, which would show its content.
    async fn authorize(
        &self,
        entry: &RepoEntry,
        ctx: &CallCtx,
        items: &[Item],
    ) -> Result<(), Status> {
        if self.pipeline.push_hooks().may_skip_pre(ctx) {
            return Ok(());
        }
        let mut conn = self
            .registry
            .pool()
            .acquire()
            .await
            .map_err(|e| internal("database", &e))?;
        for item in items {
            let latest = versions::latest(&mut conn, entry.ns, item.change)
                .await
                .map_err(|e| internal("versions", &e))?
                .ok_or_else(|| {
                    Status::not_found(format!("change {} has no published version", item.change))
                })?;
            if latest.actor == ctx.principal.id {
                continue;
            }
            let first = versions::list(&mut conn, entry.ns, item.change, None, 1)
                .await
                .map_err(|e| internal("versions", &e))?;
            if first.first().is_none_or(|v| v.actor != ctx.principal.id) {
                return Err(Status::permission_denied(format!(
                    "change {} is not yours to restack",
                    item.change
                )));
            }
        }
        Ok(())
    }

    /// The newest published version of every item.
    async fn heads(&self, entry: &RepoEntry, items: &[Item]) -> Result<Vec<Head>, Status> {
        let mut conn = self
            .registry
            .pool()
            .acquire()
            .await
            .map_err(|e| internal("database", &e))?;
        let mut out = Vec::with_capacity(items.len());
        for item in items {
            let latest = versions::latest(&mut conn, entry.ns, item.change)
                .await
                .map_err(|e| internal("versions", &e))?
                .ok_or_else(|| {
                    Status::not_found(format!("change {} has no published version", item.change))
                })?;
            out.push(Head {
                cs: latest.cs,
                parent: latest.parent_change,
            });
        }
        Ok(out)
    }

    /// Rebases every item bottom to top. A conflict stops that item and everything above it.
    #[allow(clippy::too_many_arguments)]
    async fn replay(
        &self,
        engine: &Arc<Eng>,
        parsed: &Parsed,
        heads: &[Head],
        pos: &HashMap<ChangeId, usize>,
        onto: ChangesetId,
        committer: &Signature,
        windows_safe: bool,
    ) -> Result<(Vec<Step>, Vec<wire::Conflict>), Status> {
        let mut steps = Vec::new();
        let mut conflicts = Vec::new();
        let mut new_head: HashMap<ChangeId, ChangesetId> = HashMap::new();
        let mut blocked: HashSet<ChangeId> = HashSet::new();
        for (i, (item, head)) in parsed.items.iter().zip(heads).enumerate() {
            let base = match head.parent.filter(|p| pos.contains_key(p)) {
                Some(p) if blocked.contains(&p) => {
                    blocked.insert(item.change);
                    continue;
                }
                Some(p) => *new_head
                    .get(&p)
                    .ok_or_else(|| internal("restack", &"a parent was not processed"))?,
                None => onto,
            };
            let old = engine.get_changeset(head.cs).await.map_err(commit_status)?;
            let [parent] = old.parents.as_slice() else {
                return Err(Status::failed_precondition(
                    "only a change with one parent can be restacked",
                ));
            };
            if *parent == base {
                new_head.insert(item.change, head.cs);
                continue;
            }
            let (engine2, from, opts) = (engine.clone(), head.cs, agent_merge_options());
            let rebased = on_blocking_thread(move || async move {
                engine2.rebase_changeset(from, base, &opts).await
            })
            .await?
            .map_err(merge_status)?;
            let stored = match rebased {
                Rebased::Done { id, .. } => id,
                Rebased::Conflicts(found) => {
                    blocked.insert(item.change);
                    let room = MAX_CONFLICTS.saturating_sub(conflicts.len());
                    conflicts.extend(found.iter().take(room).map(|c| conflict_wire(head.cs, c)));
                    continue;
                }
            };
            let new = self
                .with_committer(engine, stored, committer, windows_safe)
                .await?;
            let rewritten = engine.get_changeset(new).await.map_err(commit_status)?;
            steps.push(Step {
                item: i,
                change: item.change,
                old: head.cs,
                new,
                unchanged: same_patch(&old, &rewritten),
            });
            new_head.insert(item.change, new);
        }
        Ok((steps, conflicts))
    }

    /// `stored` again with the call's committer; the same changeset when it already has it.
    async fn with_committer(
        &self,
        engine: &Arc<Eng>,
        stored: ChangesetId,
        committer: &Signature,
        windows_safe: bool,
    ) -> Result<ChangesetId, Status> {
        let cs = engine.get_changeset(stored).await.map_err(commit_status)?;
        if cs.committer == *committer {
            return Ok(stored);
        }
        let new = NewChangeset {
            parents: cs.parents.clone(),
            author: cs.author.clone(),
            committer: committer.clone(),
            message: cs.message.clone(),
            extra: cs.extra.as_slice().to_vec(),
            root: cs.root,
            copies: copies_of(&cs),
        };
        let (engine, policy) = (engine.clone(), PathPolicy::Native { windows_safe });
        on_blocking_thread(move || async move { engine.create_changeset(new, policy).await })
            .await?
            .map_err(commit_status)
    }

    // ------------------------------------------------------------------ publishing

    /// Ingests the new heads and indexes the rewrites and versions in one transaction. The
    /// caller holds the GC fence.
    async fn publish_plain(
        &self,
        entry: &RepoEntry,
        ctx: &CallCtx,
        pairs: &[(ChangesetId, ChangesetId)],
        windows_safe: bool,
        at: i64,
    ) -> Result<(), Status> {
        let targets: Vec<RefTarget> = pairs.iter().map(|p| RefTarget::Changeset(p.1)).collect();
        // The policy of the `Ingest` RPC (SPEC §21.3.7), the only one a call with no ref has:
        // the caller is recorded as the ingester and pays for what it adds, and a caller with no
        // bytes left is refused before anything is recorded. The internal principal has no
        // owner and is not charged.
        let owner = crate::refpolicy::accounting_owner_of(&ctx.principal);
        let quota = self.pipeline.user_quota();
        let mut ingestor = Ingestor::new(
            self.registry.pool(),
            self.registry.refs(),
            entry,
            windows_safe,
        )
        .by(owner);
        if let Some(owner) = owner {
            let left = quota
                .remaining(self.registry.pool(), entry.ns, owner)
                .await?;
            ingestor = ingestor.with_budget(left);
        }
        ingestor
            .ingest(&targets, self.pipeline.observer())
            .await
            .map_err(ingest_status)?;
        let mut tx = self
            .registry
            .refs()
            .begin()
            .await
            .map_err(|e| internal("refs", &e))?;
        self.registry.guard(&mut tx, entry).await?;
        if let Some(owner) = owner {
            let news: Vec<ChangesetId> = pairs.iter().map(|p| p.1).collect();
            quota.claim(&mut tx, entry.ns, owner, &news).await?;
        }
        self.pipeline
            .observer()
            .on_rewritten(&mut tx, entry.key, pairs, RewriteOp::Restack)
            .await
            .map_err(|e| internal("on_rewritten", &e))?;
        self.append_versions(&mut tx, entry.ns, ctx, pairs, at)
            .await?;
        tx.commit().await.map_err(|e| internal("commit", &e))
    }

    /// Moves every `refs/pull/<n>/head` in one `UpdateRefs`, then appends the versions.
    async fn publish_refs(
        &self,
        entry: &RepoEntry,
        ctx: &CallCtx,
        parsed: &Parsed,
        steps: &[Step],
        pairs: &[(ChangesetId, ChangesetId)],
        at: i64,
    ) -> Result<Vec<wire::Rewrite>, Status> {
        let updates = steps
            .iter()
            .map(|s| {
                let item = &parsed.items[s.item];
                wire::RefUpdate {
                    name: item.update_ref.clone().unwrap_or_default(),
                    expect: Some(item.expect.clone()),
                    new: Some(wire::RefTarget::from(&RefTarget::Changeset(s.new))),
                    delete: false,
                }
            })
            .collect();
        let update = wire::UpdateRefsRequest {
            repo: Some(wire::RepoRef {
                name: ctx.repo.to_string(),
                id_mode: IdMode::Asc as i32,
            }),
            updates,
            // The new head is a rewrite of the old one, not a descendant; `expect` is the lease.
            mode: wire::UpdateMode::Force as i32,
            hooks: wire::HookMode::Run as i32,
            ..wire::UpdateRefsRequest::default()
        };
        let done = self
            .refs
            .update_rewriting(
                ctx,
                update,
                Some(Rewrites {
                    pairs,
                    op: RewriteOp::Restack,
                }),
            )
            .await?;
        self.versions_after_move(entry, ctx, pairs, at).await?;
        Ok(done.rewritten)
    }

    async fn versions_after_move(
        &self,
        entry: &RepoEntry,
        ctx: &CallCtx,
        pairs: &[(ChangesetId, ChangesetId)],
        at: i64,
    ) -> Result<(), Status> {
        let mut tx = self
            .registry
            .refs()
            .begin()
            .await
            .map_err(|e| internal("refs", &e))?;
        self.registry.guard(&mut tx, entry).await?;
        self.append_versions(&mut tx, entry.ns, ctx, pairs, at)
            .await?;
        tx.commit().await.map_err(|e| internal("commit", &e))
    }

    /// The next version of every rewritten change. A retry appends nothing twice.
    async fn append_versions(
        &self,
        tx: &mut Transaction<'_, Any>,
        ns: Ns,
        ctx: &CallCtx,
        pairs: &[(ChangesetId, ChangesetId)],
        at: i64,
    ) -> Result<(), Status> {
        let news: Vec<ChangesetId> = pairs.iter().map(|p| p.1).collect();
        let rows = index::rows_of(tx, ns, &news)
            .await
            .map_err(|e| internal("change index", &e))?;
        let change_of: HashMap<ChangesetId, ChangeId> =
            rows.iter().map(|r| (r.cs, r.change)).collect();
        let in_call: HashSet<ChangeId> = change_of.values().copied().collect();
        let op = ctx.op_id.map(|o| *o.as_bytes()).unwrap_or_default();
        for &(_, new) in pairs {
            let change = *change_of
                .get(&new)
                .ok_or_else(|| internal("restack", &"a rewritten changeset has no change"))?;
            let parent_change = versions::latest(tx, ns, change)
                .await?
                .and_then(|l| l.parent_change)
                .filter(|p| in_call.contains(p));
            versions::append(
                tx,
                ns,
                &versions::NewVersion {
                    change,
                    cs: new,
                    parent_change,
                    at,
                    actor: &ctx.principal.id,
                    op_id: &op,
                    force: false,
                },
            )
            .await?;
        }
        Ok(())
    }

    // ------------------------------------------------------------------ retries

    /// The rewrites a previous attempt of this `op_id` made, if it moved the refs of this call.
    async fn replayed(
        &self,
        entry: &RepoEntry,
        ctx: &CallCtx,
        parsed: &Parsed,
    ) -> Result<Option<Pairs>, Status> {
        let Some(op) = ctx.op_id else { return Ok(None) };
        let Some(done) = self.refs.stored(entry, op).await? else {
            return Ok(None);
        };
        let asked: HashSet<&str> = parsed
            .items
            .iter()
            .filter_map(|i| i.update_ref.as_deref())
            .collect();
        if done.results.is_empty()
            || done
                .results
                .iter()
                .any(|r| !asked.contains(r.name.as_str()))
        {
            return Err(invalid("op id was used for another operation"));
        }
        done.rewritten
            .iter()
            .map(|r| {
                Ok((
                    ChangesetId::from_slice(&r.old).map_err(|e| internal("op_results", &e))?,
                    ChangesetId::from_slice(&r.new).map_err(|e| internal("op_results", &e))?,
                ))
            })
            .collect::<Result<Pairs, Status>>()
            .map(Some)
    }

    /// The answer of a finished attempt; its versions are appended again in case the attempt
    /// stopped between the ref move and the version log.
    async fn answer_replay(
        &self,
        entry: &RepoEntry,
        engine: &Arc<Eng>,
        ctx: &CallCtx,
        pairs: &[(ChangesetId, ChangesetId)],
        at: i64,
    ) -> Result<wire::RestackChangesResponse, Status> {
        self.versions_after_move(entry, ctx, pairs, at).await?;
        let olds: Vec<ChangesetId> = pairs.iter().map(|p| p.0).collect();
        let rows = {
            let mut conn = self
                .registry
                .pool()
                .acquire()
                .await
                .map_err(|e| internal("database", &e))?;
            index::rows_of(&mut conn, entry.ns, &olds)
                .await
                .map_err(|e| internal("change index", &e))?
        };
        let change_of: HashMap<ChangesetId, ChangeId> =
            rows.iter().map(|r| (r.cs, r.change)).collect();
        let mut patch_unchanged = Vec::new();
        for &(old, new) in pairs {
            let (a, b) = (
                engine.get_changeset(old).await.map_err(commit_status)?,
                engine.get_changeset(new).await.map_err(commit_status)?,
            );
            if let Some(change) = change_of.get(&old)
                && same_patch(&a, &b)
            {
                patch_unchanged.push(Bytes::copy_from_slice(change.as_bytes()));
            }
        }
        Ok(wire::RestackChangesResponse {
            rewritten: wire_pairs(pairs),
            conflicts: Vec::new(),
            patch_unchanged,
        })
    }
}

#[cfg(test)]
mod tests {
    use asc_core::change::ChangeMeta;
    use asc_rpc::auth::{Access, BearerToken, Credential, StaticTokens};
    use asc_rpc::server::{RepoName, ServerInfo};
    use asc_rpc::services::changes_server::Changes;
    use asc_store::refs::Refs as _;
    use tonic::Request;

    use super::super::rpc::ChangesService;
    use super::*;
    use crate::ingest::testutil::{commit, registry, repo};
    use crate::update::PushHooks;

    const READER: &str = "reader-token";
    const WRITER: &str = "writer-token";
    const FORGE: &str = "forge-token";
    const MALLORY: &str = "mallory-token";
    const BOB: &str = "bob-token";

    struct Internal;
    impl PushHooks for Internal {
        fn may_skip_pre(&self, ctx: &CallCtx) -> bool {
            crate::authz::is_host_service(&ctx.principal.id)
        }
    }

    fn cid(n: u8) -> ChangeId {
        ChangeId::from_bytes([n; 16])
    }

    fn cbytes(n: u8) -> Bytes {
        Bytes::copy_from_slice(cid(n).as_bytes())
    }

    fn hex(b: &[u8]) -> String {
        b.iter().map(|b| format!("{b:02x}")).collect()
    }

    struct Fx {
        reg: Arc<Registry>,
        e: Arc<RepoEntry>,
        svc: ChangesService,
        /// The base commit.
        a: ChangesetId,
        /// Heads of changes 1, 2 and 3: 1 and 2 are a stack on `a`, 3 stands alone on `a`.
        c: [ChangesetId; 3],
        /// A trunk move that touches none of their paths.
        t: ChangesetId,
        /// A trunk move that adds `one` with other content: it conflicts with change 1.
        x: ChangesetId,
    }

    async fn publish(
        f: &(Arc<Registry>, Arc<RepoEntry>),
        n: u8,
        cs: ChangesetId,
        parent: Option<u8>,
    ) {
        publish_as(f, n, cs, parent, "writer").await;
    }

    async fn publish_as(
        f: &(Arc<Registry>, Arc<RepoEntry>),
        n: u8,
        cs: ChangesetId,
        parent: Option<u8>,
        actor: &str,
    ) {
        let idx = index::ChangeIndex::new(f.0.clone());
        let mut tx = f.0.refs().begin().await.unwrap();
        idx.write_meta(
            &mut tx,
            f.1.key,
            &ChangeMeta::new(cs, cid(n), vec![], RewriteOp::New),
        )
        .await
        .unwrap();
        versions::append(
            &mut tx,
            f.1.ns,
            &versions::NewVersion {
                change: cid(n),
                cs,
                parent_change: parent.map(cid),
                at: 5,
                actor,
                op_id: b"",
                force: false,
            },
        )
        .await
        .unwrap();
        tx.commit().await.unwrap();
    }

    async fn fx() -> Fx {
        let reg = registry().await;
        let e = repo(&reg).await;
        let a = commit(&e, &[], &[("a", b"a\n")]).await;
        let c1 = commit(&e, &[a], &[("one", b"1\n")]).await;
        let c2 = commit(&e, &[c1], &[("two", b"2\n")]).await;
        let c3 = commit(&e, &[a], &[("three", b"3\n")]).await;
        let t = commit(&e, &[a], &[("t", b"t\n")]).await;
        let x = commit(&e, &[a], &[("one", b"X\n")]).await;
        let ext = Extensions::new();
        ext.pipeline
            .set_push_hooks(Arc::new(Internal))
            .ok()
            .unwrap();
        let hooks = Arc::new(ReadHooks::default());
        super::super::register(&ext, &reg, &hooks);
        let auth = StaticTokens::new()
            .with_token(READER, "reader", Access::Read)
            .with_token(WRITER, "writer", Access::Write)
            .with_token(MALLORY, "mallory", Access::Write)
            .with_token(BOB, "7:bob", Access::Write)
            .with_token(FORGE, crate::authz::HOST_SERVICE_PRINCIPAL, Access::Admin);
        let svc = ChangesService::new(
            ext.changes.get().unwrap().clone(),
            Arc::new(auth),
            ServerInfo::default(),
        );
        let pair = (reg.clone(), e.clone());
        publish(&pair, 1, c1, None).await;
        publish(&pair, 2, c2, Some(1)).await;
        publish(&pair, 3, c3, None).await;
        Fx {
            reg,
            e,
            svc,
            a,
            c: [c1, c2, c3],
            t,
            x,
        }
    }

    fn item(n: u8) -> wire::RestackItem {
        wire::RestackItem {
            change: cbytes(n),
            update_ref: String::new(),
            expect: None,
        }
    }

    fn item_ref(n: u8, cs: ChangesetId) -> wire::RestackItem {
        wire::RestackItem {
            change: cbytes(n),
            update_ref: format!("refs/pull/{n}/head"),
            expect: Some(wire::RefTarget::from(&RefTarget::Changeset(cs))),
        }
    }

    fn msg(onto: ChangesetId, items: Vec<wire::RestackItem>) -> wire::RestackChangesRequest {
        wire::RestackChangesRequest {
            repo: Some(wire::RepoRef {
                name: "o/r".into(),
                id_mode: IdMode::Asc as i32,
            }),
            onto: onto.to_string(),
            items,
            ..wire::RestackChangesRequest::default()
        }
    }

    /// As the forge would send it: a committer and a time in the request.
    fn forge(mut m: wire::RestackChangesRequest) -> wire::RestackChangesRequest {
        m.committer_name = "Forge".into();
        m.committer_email = "forge@example.com".into();
        m.committer_time = 1_800_000_000;
        m
    }

    fn call(
        m: wire::RestackChangesRequest,
        token: &str,
        op: Option<Uuid>,
    ) -> Request<wire::RestackChangesRequest> {
        let mut r = Request::new(m);
        r.extensions_mut()
            .insert(Credential::bearer(BearerToken::new(token).unwrap()));
        if let Some(op) = op {
            r.metadata_mut()
                .insert("x-asc-op-id", op.to_string().parse().unwrap());
        }
        r
    }

    impl Fx {
        async fn run(
            &self,
            m: wire::RestackChangesRequest,
            token: &str,
        ) -> Result<wire::RestackChangesResponse, Status> {
            self.svc
                .restack_changes(call(m, token, Some(Uuid::new_v4())))
                .await
                .map(tonic::Response::into_inner)
        }

        async fn cs(&self, id: ChangesetId) -> Arc<Changeset> {
            self.e.backend.engine().get_changeset(id).await.unwrap()
        }

        async fn count(&self, sql: &str) -> i64 {
            sqlx::query_scalar::<_, i64>(sqlx::AssertSqlSafe(sql.to_owned()))
                .fetch_one(self.reg.pool())
                .await
                .unwrap()
        }

        async fn latest(&self, n: u8) -> versions::Version {
            let mut c = self.reg.pool().acquire().await.unwrap();
            versions::latest(&mut c, self.e.ns, cid(n))
                .await
                .unwrap()
                .unwrap()
        }

        async fn set_ref(&self, name: &str, cs: ChangesetId) {
            use asc_store::refs::{Actor, Expect, RefTxn, RefUpdate};
            let txn = RefTxn {
                updates: vec![RefUpdate {
                    name: asc_core::path::RefName::new(name.as_bytes()).unwrap(),
                    expect: Expect::Any,
                    new: Some(RefTarget::Changeset(cs)),
                    expires_at: None,
                }],
                actor: Actor {
                    id: "t".into(),
                    via: "test".into(),
                },
                reason: "test".into(),
                op_id: Uuid::new_v4(),
            };
            self.reg.refs().update(self.e.key, &txn).await.unwrap();
        }

        async fn ref_of(&self, name: &str) -> Option<RefTarget> {
            self.reg
                .refs()
                .get(
                    self.e.key,
                    &asc_core::path::RefName::new(name.as_bytes()).unwrap(),
                )
                .await
                .unwrap()
                .map(|e| e.target)
        }
    }

    fn news(r: &wire::RestackChangesResponse) -> Vec<ChangesetId> {
        r.rewritten
            .iter()
            .map(|w| ChangesetId::from_slice(&w.new).unwrap())
            .collect()
    }

    #[tokio::test]
    async fn a_stack_is_restacked_with_one_committer_and_time() {
        let f = fx().await;
        let done = f
            .run(forge(msg(f.t, vec![item(1), item(2)])), FORGE)
            .await
            .unwrap();
        let [n1, n2] = news(&done)[..] else {
            panic!("two rewrites")
        };
        assert_eq!(done.rewritten[0].old.as_ref(), f.c[0].as_bytes());
        assert_eq!(done.rewritten[1].old.as_ref(), f.c[1].as_bytes());
        assert_eq!(f.cs(n1).await.parents, vec![f.t]);
        assert_eq!(f.cs(n2).await.parents, vec![n1]);
        for (new, old) in [(n1, f.c[0]), (n2, f.c[1])] {
            let (new, old) = (f.cs(new).await, f.cs(old).await);
            assert_eq!(new.committer.name(), b"Forge");
            assert_eq!(new.committer.email(), b"forge@example.com");
            assert_eq!(new.committer.time(), 1_800_000_000);
            assert_eq!(new.author, old.author);
            assert_eq!(new.message, old.message);
        }
        // Both patches are untouched, so both approvals survive.
        assert_eq!(done.patch_unchanged, vec![cbytes(1), cbytes(2)]);
        assert!(done.conflicts.is_empty());
        // Each change has its next version; the child keeps its parent change, the bottom one
        // now sits on trunk.
        let (v1, v2) = (f.latest(1).await, f.latest(2).await);
        assert_eq!((v1.v, v1.cs, v1.parent_change), (2, n1, None));
        assert_eq!((v2.v, v2.cs, v2.parent_change), (2, n2, Some(cid(1))));
        assert_eq!(v1.actor, crate::authz::HOST_SERVICE_PRINCIPAL);
        // The index knows the new changesets as the same changes.
        for (n, new) in [(1u8, n1), (2, n2)] {
            let q = format!(
                "SELECT COUNT(*) FROM changes WHERE cs = X'{}' AND change = X'{}'",
                hex(new.as_bytes()),
                hex(cid(n).as_bytes()),
            );
            assert_eq!(f.count(&q).await, 1);
        }
    }

    #[tokio::test]
    async fn the_same_request_gives_the_same_ids() {
        let f = fx().await;
        let before = f.count("SELECT COUNT(*) FROM changes").await;
        let m = forge(msg(f.t, vec![item(1), item(2), item(3)]));
        let dry = wire::RestackChangesRequest {
            dry_run: true,
            ..m.clone()
        };
        let first = f.run(dry.clone(), FORGE).await.unwrap();
        let second = f.run(dry, FORGE).await.unwrap();
        assert_eq!(first, second);
        assert_eq!(first.rewritten.len(), 3);
        // A dry run writes no row and no version.
        assert_eq!(f.count("SELECT COUNT(*) FROM changes").await, before);
        assert_eq!(f.latest(1).await.v, 1);
        // The real call produces the very same ids.
        let real = f.run(m, FORGE).await.unwrap();
        assert_eq!(real.rewritten, first.rewritten);
        assert_eq!(f.latest(3).await.v, 2);
        assert_eq!(f.latest(3).await.cs, news(&real)[2]);
    }

    #[tokio::test]
    async fn a_finished_restack_has_nothing_left_to_rewrite() {
        let f = fx().await;
        let m = forge(msg(f.t, vec![item(1), item(2)]));
        assert_eq!(f.run(m.clone(), FORGE).await.unwrap().rewritten.len(), 2);
        let again = f.run(m, FORGE).await.unwrap();
        assert!(again.rewritten.is_empty() && again.conflicts.is_empty());
        assert_eq!(f.latest(1).await.v, 2);
    }

    #[tokio::test]
    async fn the_committer_defaults_to_the_actor_and_one_clock_reading() {
        let f = fx().await;
        let lo = now();
        let done = f
            .run(msg(f.t, vec![item(1), item(2)]), WRITER)
            .await
            .unwrap();
        let hi = now();
        let [n1, n2] = news(&done)[..] else {
            panic!("two rewrites")
        };
        let (c1, c2) = (
            f.cs(n1).await.committer.clone(),
            f.cs(n2).await.committer.clone(),
        );
        assert_eq!(c1, c2);
        assert_eq!(c1.name(), b"writer");
        assert!((lo..=hi).contains(&c1.time()));
    }

    #[tokio::test]
    async fn only_the_forge_sets_the_committer_or_the_time() {
        let f = fx().await;
        let base = msg(f.t, vec![item(1)]);
        for m in [
            wire::RestackChangesRequest {
                committer_name: "Mallory".into(),
                committer_email: "m@example.com".into(),
                ..base.clone()
            },
            wire::RestackChangesRequest {
                committer_time: 5,
                ..base.clone()
            },
        ] {
            let e = f.run(m, WRITER).await.unwrap_err();
            assert_eq!(e.code(), tonic::Code::PermissionDenied);
        }
        // Nothing was written, and the forge itself is checked for shape.
        assert_eq!(f.latest(1).await.v, 1);
        let e = f
            .run(
                wire::RestackChangesRequest {
                    committer_name: "only a name".into(),
                    ..base.clone()
                },
                FORGE,
            )
            .await
            .unwrap_err();
        assert_eq!(e.code(), tonic::Code::InvalidArgument);
        let e = f
            .run(
                wire::RestackChangesRequest {
                    committer_time: -1,
                    ..base
                },
                FORGE,
            )
            .await
            .unwrap_err();
        assert_eq!(e.code(), tonic::Code::InvalidArgument);
    }

    #[tokio::test]
    async fn restacking_needs_write_access() {
        let f = fx().await;
        let m = msg(f.t, vec![item(1)]);
        let e = f.run(m.clone(), READER).await.unwrap_err();
        assert_eq!(e.code(), tonic::Code::PermissionDenied);
        let e = f.svc.restack_changes(Request::new(m)).await.unwrap_err();
        assert_eq!(e.code(), tonic::Code::Unauthenticated);
        assert_eq!(f.latest(1).await.v, 1);
    }

    #[tokio::test]
    async fn only_pull_heads_are_moved() {
        let f = fx().await;
        f.set_ref("refs/heads/master", f.a).await;
        for bad in [
            "refs/heads/master",
            "refs/for/master/asc-x",
            "refs/tags/v1",
            "refs/pull/1/merge",
            "refs/pull/x/head",
            "refs/pull//head",
            "refs/pull/1/head/x",
            "refs/users/1/x",
        ] {
            let mut it = item_ref(1, f.c[0]);
            it.update_ref = bad.to_owned();
            let e = f.run(msg(f.t, vec![it]), WRITER).await.unwrap_err();
            assert_eq!(e.code(), tonic::Code::InvalidArgument, "{bad}");
        }
        // Refs on some items only, or the same ref twice, are refused as well.
        f.set_ref("refs/pull/1/head", f.c[0]).await;
        let e = f
            .run(msg(f.t, vec![item_ref(1, f.c[0]), item(3)]), WRITER)
            .await
            .unwrap_err();
        assert_eq!(e.code(), tonic::Code::InvalidArgument);
        let mut twice = item_ref(3, f.c[2]);
        twice.update_ref = "refs/pull/1/head".into();
        let e = f
            .run(msg(f.t, vec![item_ref(1, f.c[0]), twice]), WRITER)
            .await
            .unwrap_err();
        assert_eq!(e.code(), tonic::Code::InvalidArgument);
        assert_eq!(
            f.ref_of("refs/heads/master").await,
            Some(RefTarget::Changeset(f.a))
        );
        assert_eq!(f.latest(1).await.v, 1);
    }

    #[tokio::test]
    async fn a_write_needs_an_op_id_but_a_dry_run_does_not() {
        let f = fx().await;
        let m = msg(f.t, vec![item(1)]);
        let e = f
            .svc
            .restack_changes(call(m.clone(), WRITER, None))
            .await
            .unwrap_err();
        assert_eq!(e.code(), tonic::Code::InvalidArgument);
        assert_eq!(f.latest(1).await.v, 1);
        let dry = wire::RestackChangesRequest {
            dry_run: true,
            ..m.clone()
        };
        let done = f
            .svc
            .restack_changes(call(dry, WRITER, None))
            .await
            .unwrap();
        assert_eq!(done.get_ref().rewritten.len(), 1);
        let mut bad = call(m, WRITER, None);
        bad.metadata_mut()
            .insert("x-asc-op-id", "nope".parse().unwrap());
        let e = f.svc.restack_changes(bad).await.unwrap_err();
        assert_eq!(e.code(), tonic::Code::InvalidArgument);
    }

    #[tokio::test]
    async fn malformed_requests_are_refused_before_any_write() {
        let f = fx().await;
        let too_many = vec![item(1); MAX_ITEMS + 1];
        let mut short = item(1);
        short.change = Bytes::from_static(b"short");
        let mut orphan_expect = item(1);
        orphan_expect.expect = Some(wire::RefTarget::from(&RefTarget::Changeset(f.a)));
        let cases = [
            (vec![], tonic::Code::InvalidArgument),
            (too_many, tonic::Code::InvalidArgument),
            (vec![item(1), item(1)], tonic::Code::InvalidArgument),
            (vec![short], tonic::Code::InvalidArgument),
            (vec![orphan_expect], tonic::Code::InvalidArgument),
            // The child before its parent.
            (vec![item(2), item(1)], tonic::Code::InvalidArgument),
            // A change nobody published.
            (vec![item(9)], tonic::Code::NotFound),
        ];
        for (items, want) in cases {
            let e = f.run(msg(f.t, items), WRITER).await.unwrap_err();
            assert_eq!(e.code(), want);
        }
        let mut long = msg(f.t, vec![item(1)]);
        long.onto = "x".repeat(MAX_STRING + 1);
        assert_eq!(
            f.run(long, WRITER).await.unwrap_err().code(),
            tonic::Code::InvalidArgument
        );
        let mut missing = msg(f.t, vec![item(1)]);
        missing.onto = "0".repeat(64);
        assert!(f.run(missing, WRITER).await.is_err());
        assert_eq!(f.latest(1).await.v, 1);
        assert_eq!(f.count("SELECT COUNT(*) FROM change_versions").await, 3);
    }

    #[tokio::test]
    async fn refs_move_in_one_update_and_a_retry_replays() {
        let f = fx().await;
        f.set_ref("refs/pull/1/head", f.c[0]).await;
        f.set_ref("refs/pull/2/head", f.c[1]).await;
        let m = forge(wire::RestackChangesRequest {
            atomic: true,
            ..msg(f.t, vec![item_ref(1, f.c[0]), item_ref(2, f.c[1])])
        });
        let op = Uuid::new_v4();
        let done = f
            .svc
            .restack_changes(call(m.clone(), FORGE, Some(op)))
            .await
            .unwrap()
            .into_inner();
        let [n1, n2] = news(&done)[..] else {
            panic!("two rewrites")
        };
        assert_eq!(
            f.ref_of("refs/pull/1/head").await,
            Some(RefTarget::Changeset(n1))
        );
        assert_eq!(
            f.ref_of("refs/pull/2/head").await,
            Some(RefTarget::Changeset(n2))
        );
        assert_eq!(f.latest(1).await.cs, n1);
        assert_eq!(f.latest(2).await.v, 2);
        assert_eq!(done.patch_unchanged.len(), 2);
        // The same op id answers the same and writes nothing more.
        let versions_before = f.count("SELECT COUNT(*) FROM change_versions").await;
        let again = f
            .svc
            .restack_changes(call(m, FORGE, Some(op)))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(again, done);
        assert_eq!(
            f.count("SELECT COUNT(*) FROM change_versions").await,
            versions_before
        );
        // The op id belongs to those refs: another request under it is refused.
        let e = f
            .svc
            .restack_changes(call(
                forge(msg(f.t, vec![item_ref(3, f.c[2])])),
                FORGE,
                Some(op),
            ))
            .await
            .unwrap_err();
        assert_eq!(e.code(), tonic::Code::InvalidArgument);
    }

    #[tokio::test]
    async fn a_stale_expect_moves_nothing() {
        let f = fx().await;
        f.set_ref("refs/pull/1/head", f.c[0]).await;
        // The caller believes the head is `x`, which is not the change's head.
        let e = f
            .run(msg(f.t, vec![item_ref(1, f.x)]), WRITER)
            .await
            .unwrap_err();
        assert_eq!(e.code(), tonic::Code::FailedPrecondition);
        // The ref moved since the caller looked: the compare-and-set refuses.
        f.set_ref("refs/pull/1/head", f.x).await;
        assert!(
            f.run(msg(f.t, vec![item_ref(1, f.c[0])]), WRITER)
                .await
                .is_err()
        );
        assert_eq!(
            f.ref_of("refs/pull/1/head").await,
            Some(RefTarget::Changeset(f.x))
        );
        assert_eq!(f.latest(1).await.v, 1);
    }

    #[tokio::test]
    async fn a_conflict_is_an_answer_and_atomic_writes_nothing() {
        let f = fx().await;
        let m = forge(msg(f.x, vec![item(1), item(2), item(3)]));
        // Not atomic: change 3 restacks; change 1 conflicts and holds change 2 back.
        let dry = f
            .run(
                wire::RestackChangesRequest {
                    dry_run: true,
                    ..m.clone()
                },
                FORGE,
            )
            .await
            .unwrap();
        assert_eq!(dry.rewritten.len(), 1);
        assert_eq!(dry.conflicts.len(), 1);
        assert_eq!(dry.conflicts[0].path.as_ref(), b"one");
        assert_eq!(dry.conflicts[0].changeset.as_ref(), f.c[0].as_bytes());
        // Atomic: nothing is rewritten, nothing is written.
        let atomic = f
            .run(
                wire::RestackChangesRequest {
                    atomic: true,
                    ..m.clone()
                },
                FORGE,
            )
            .await
            .unwrap();
        assert!(atomic.rewritten.is_empty() && atomic.patch_unchanged.is_empty());
        assert_eq!(atomic.conflicts.len(), 1);
        assert_eq!(f.count("SELECT COUNT(*) FROM change_versions").await, 3);
        for n in 1..=3 {
            assert_eq!(f.latest(n).await.v, 1);
        }
        // Not atomic, for real: the clean change moves, the others stay.
        let part = f.run(m, FORGE).await.unwrap();
        assert_eq!(part.rewritten.len(), 1);
        assert_eq!(part.rewritten[0].old.as_ref(), f.c[2].as_bytes());
        assert_eq!(f.latest(3).await.v, 2);
        assert_eq!(f.latest(1).await.v, 1);
        assert_eq!(f.latest(2).await.v, 1);
    }

    #[tokio::test]
    async fn a_merged_rewrite_is_not_patch_unchanged() {
        let f = fx().await;
        let lines = |first: &str, last: &str| {
            let mut s = format!("{first}\n");
            for i in 2..9 {
                s.push_str(&format!("{i}\n"));
            }
            s.push_str(last);
            s.push('\n');
            s.into_bytes()
        };
        let base = commit(&f.e, &[], &[("f", &lines("1", "9"))]).await;
        let edit = commit(&f.e, &[base], &[("f", &lines("X", "9"))]).await;
        let trunk = commit(&f.e, &[base], &[("f", &lines("1", "Y"))]).await;
        publish(&(f.reg.clone(), f.e.clone()), 7, edit, None).await;
        let done = f.run(msg(trunk, vec![item(7)]), WRITER).await.unwrap();
        assert_eq!(done.rewritten.len(), 1);
        assert!(done.patch_unchanged.is_empty());
        let merged = f.cs(news(&done)[0]).await;
        assert_eq!(merged.parents, vec![trunk]);
    }

    #[tokio::test]
    async fn a_child_whose_parent_landed_restacks_onto_the_tip() {
        let f = fx().await;
        // Change 1 landed as `t`; change 2 alone goes onto it and leaves the stack.
        let done = f.run(msg(f.t, vec![item(2)]), WRITER).await.unwrap();
        let new = news(&done)[0];
        assert_eq!(f.cs(new).await.parents, vec![f.t]);
        let v = f.latest(2).await;
        assert_eq!((v.v, v.cs, v.parent_change), (2, new, None));
    }

    #[tokio::test]
    async fn a_mirror_takes_no_restack() {
        let f = fx().await;
        sqlx::query("UPDATE repos SET mode = 'mirror'")
            .execute(f.reg.pool())
            .await
            .unwrap();
        f.reg.forget(&RepoName::parse("o/r").unwrap());
        let e = f.run(msg(f.t, vec![item(1)]), WRITER).await.unwrap_err();
        assert_ne!(e.code(), tonic::Code::Ok);
        assert_eq!(f.latest(1).await.v, 1);
        // Reading the plan is still allowed.
        let dry = wire::RestackChangesRequest {
            dry_run: true,
            ..msg(f.t, vec![item(1)])
        };
        assert_eq!(f.run(dry, WRITER).await.unwrap().rewritten.len(), 1);
    }

    #[tokio::test]
    async fn another_user_cannot_restack_my_change() {
        let f = fx().await;
        f.set_ref("refs/pull/1/head", f.c[0]).await;
        let plain = msg(f.t, vec![item(1)]);
        let dry = wire::RestackChangesRequest {
            dry_run: true,
            ..plain.clone()
        };
        // Mallory has write access, and change 2 sits on change 1: neither is hers.
        for m in [
            plain,
            dry,
            msg(f.t, vec![item_ref(1, f.c[0])]),
            msg(f.t, vec![item(3), item(1)]),
            msg(f.t, vec![item(1), item(2)]),
        ] {
            let e = f.run(m, MALLORY).await.unwrap_err();
            assert_eq!(e.code(), tonic::Code::PermissionDenied);
        }
        assert_eq!(
            f.ref_of("refs/pull/1/head").await,
            Some(RefTarget::Changeset(f.c[0]))
        );
        assert_eq!(f.count("SELECT COUNT(*) FROM change_versions").await, 3);
        // The owner and the forge may.
        assert_eq!(
            f.run(msg(f.t, vec![item(1)]), WRITER)
                .await
                .unwrap()
                .rewritten
                .len(),
            1
        );
        assert_eq!(
            f.run(msg(f.t, vec![item(3)]), FORGE)
                .await
                .unwrap()
                .rewritten
                .len(),
            1
        );
        assert_eq!(f.latest(1).await.v, 2);
        assert_eq!(
            f.latest(3).await.actor,
            crate::authz::HOST_SERVICE_PRINCIPAL
        );
    }

    #[tokio::test]
    async fn a_restack_after_the_forge_stays_with_the_creator() {
        let f = fx().await;
        // The forge restacked change 3: its latest version is the forge's, its first is the
        // author's, who may restack it again.
        f.run(msg(f.t, vec![item(3)]), FORGE).await.unwrap();
        assert_eq!(
            f.latest(3).await.actor,
            crate::authz::HOST_SERVICE_PRINCIPAL
        );
        let e = f.run(msg(f.x, vec![item(3)]), MALLORY).await.unwrap_err();
        assert_eq!(e.code(), tonic::Code::PermissionDenied);
        f.run(msg(f.x, vec![item(3)]), WRITER).await.unwrap();
        assert_eq!(f.latest(3).await.v, 3);
    }

    #[tokio::test]
    async fn a_restack_without_refs_is_charged_to_the_caller_and_stops_at_the_quota() {
        let f = fx().await;
        let pair = (f.reg.clone(), f.e.clone());
        // Its own changeset: one shared with change 3 would belong to change 3.
        let c8 = commit(
            &f.e,
            &[f.a],
            &[(
                "eight", b"8
",
            )],
        )
        .await;
        publish_as(&pair, 8, c8, None, "7:bob").await;
        let ns = f.e.ns.as_bytes().to_vec();
        // No bytes left: refused before anything is recorded.
        sqlx::query("INSERT INTO cloud_usage (ns, owner, bytes, workspaces) VALUES ($1, 7, $2, 0)")
            .bind(ns.clone())
            .bind(i64::MAX / 2)
            .execute(f.reg.pool())
            .await
            .unwrap();
        let e = f.run(msg(f.t, vec![item(8)]), BOB).await.unwrap_err();
        assert!(
            asc_rpc::server::status_rpc_error(&e)
                .is_some_and(|r| format!("{r:?}").contains("QuotaExceeded")),
            "{e:?}"
        );
        assert_eq!(f.latest(8).await.v, 1);
        // The same call with the bytes back is charged to bob.
        sqlx::query("UPDATE cloud_usage SET bytes = 0 WHERE owner = 7")
            .execute(f.reg.pool())
            .await
            .unwrap();
        let done = f.run(msg(f.t, vec![item(8)]), BOB).await.unwrap();
        assert_eq!(done.rewritten.len(), 1);
        assert_eq!(f.latest(8).await.v, 2);
        // The claims cover the new changeset and the unshared history under it.
        let new = f.latest(8).await.cs;
        let claimed: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM cs_claim WHERE owner = 7 AND cs = $1")
                .bind(new.as_bytes().to_vec())
                .fetch_one(f.reg.pool())
                .await
                .unwrap();
        assert_eq!(claimed, 1);
        assert!(
            f.count("SELECT bytes FROM cloud_usage WHERE owner = 7")
                .await
                > 0
        );
    }

    #[tokio::test]
    async fn onto_naming_another_users_ref_is_not_found() {
        let f = fx().await;
        f.set_ref("refs/users/9/heads/feat", f.t).await;
        f.set_ref("refs/heads/main", f.t).await;
        for spec in [
            "refs/users/9/heads/feat",
            "users/9/heads/feat",
            "refs/users/9/heads/feat~0",
            "refs/users/9/heads/feat^{}",
            "refs/users/9/heads/feat@{0}",
        ] {
            // The owner is refused the foreign ref; a caller who does not own the change is
            // refused earlier, on ownership, which names nothing about `onto`.
            for (who, code) in [
                (WRITER, tonic::Code::NotFound),
                (MALLORY, tonic::Code::PermissionDenied),
            ] {
                let mut m = msg(f.t, vec![item(1)]);
                m.onto = spec.into();
                let e = f.run(m, who).await.unwrap_err();
                assert_eq!(e.code(), code, "{spec}");
            }
        }
        assert_eq!(f.latest(1).await.v, 1);
        // A shared ref still resolves.
        let mut m = msg(f.t, vec![item(1)]);
        m.onto = "refs/heads/main".into();
        assert_eq!(f.run(m, WRITER).await.unwrap().rewritten.len(), 1);
    }
}
