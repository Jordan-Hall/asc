//! `Stacks.RevertLanded` (stacks.proto): reverts a landed stack as a new stack under review.
//!
//! * The landed stack is `at` (a change, or the trunk changeset it landed as) and the landed
//!   changes below it, following each change's stack parent down to `bottom` or to the first
//!   parent that has not landed. A change that has not landed is `STACK_NOT_LANDED`.
//! * Each landed changeset is undone from the top down, each revert on the one before it and the
//!   first on `onto` (empty: the trunk tip). Changes that landed as one changeset share one
//!   revert. A merge changeset is undone against its first parent.
//! * The answer has one entry per revert, top first, naming the topmost change it undoes; with
//!   `squash` one entry, naming the top change, with every conflict.
//! * A dry run answers the reverts and their conflicts and stores nothing. An apply with a
//!   conflict is refused with `REVERT_CONFLICT` and the conflicts in its detail; a revert that
//!   would change nothing (already undone on the target) is refused too.
//! * An apply stores one new change per revert (or one for the whole stack with `squash`), as a
//!   stack whose bottom stands on the target, on the branch `revert/<landed>-<op>` of the caller
//!   (`refs/users/<caller>/heads/...` in a branchless repository), through the full `UpdateRefs`
//!   pipeline. Then each new change gets its meta and version 1. Trunk never moves.
//! * The change ids derive from the op id and the reverted change, so a retry of the same op
//!   answers the same stack: when `UpdateRefs` already stored the op, the call reads the branch
//!   it moved and finishes the index writes.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use asc_core::change::{ChangeId, ChangeMeta, RewriteOp};
use asc_core::id::{ChangesetId, RefTarget, TreeId};
use asc_core::sig::Signature;
use asc_engine::commit::{NewChangeset, PathPolicy};
use asc_rpc::IdMode;
use asc_rpc::errors::{ErrorInfo, RpcError};
use asc_rpc::server::{CallCtx, rpc_status, wire};
use bytes::Bytes;
use tonic::Status;
use uuid::Uuid;

use super::index::{self, ChangeIndex};
use super::restack::{
    MAX_CONFLICTS, Restacker, conflict_wire, merge_status, now, on_blocking_thread,
};
use super::rpc::{ChangesRpc, MAX_STACK, Reader, change_of};
use super::versions;
use crate::commit_graph_rpc::{commit_status, internal};
use crate::idmode::{Eng, IdCodec};
use crate::mutations::land_merge::agent_merge_options;
use crate::mutations::{MAX_MESSAGE_BYTES, revert_message};
use crate::registry::RepoEntry;

/// The subject of a reverted change, as `Revert "<subject>"` quotes it, at most this long.
const MAX_SUBJECT: usize = 200;

/// One landed change of the stack, top first.
struct Landed {
    change: ChangeId,
    cs: ChangesetId,
}

/// The revert of one landed changeset: its tree, or the conflicts it hit.
struct Undo {
    cs: ChangesetId,
    message: Vec<u8>,
    root: Option<TreeId>,
    conflicts: Vec<wire::Conflict>,
}

fn invalid(m: impl Into<String>) -> Status {
    Status::invalid_argument(m.into())
}

fn not_landed(change: ChangeId) -> Status {
    let id = Bytes::copy_from_slice(change.as_bytes());
    rpc_status(&RpcError::StackNotLanded(ErrorInfo {
        paths: vec![hex(&id)],
        ..ErrorInfo::default()
    }))
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// `REVERT_CONFLICT` with the conflicts in its detail.
fn conflict_error(conflicts: &[wire::Conflict]) -> Status {
    let mut detail = ErrorInfo {
        paths: conflicts
            .iter()
            .map(|c| String::from_utf8_lossy(&c.path).into_owned())
            .collect(),
        ..ErrorInfo::default()
    };
    match conflicts
        .iter()
        .map(asc_rpc::convert::Conflict::try_from)
        .collect::<Result<Vec<_>, _>>()
    {
        Ok(records) => detail.conflicts = records,
        Err(e) => return internal("revert conflict", &e),
    }
    rpc_status(&RpcError::RevertConflict(detail))
}

/// The change id of the revert of `original` made by the op `op`: the same on every retry.
fn revert_id(op: Uuid, original: ChangeId) -> ChangeId {
    let mut h = blake3::Hasher::new_derive_key("asc 2026 Stacks.RevertLanded change id");
    h.update(op.as_bytes());
    h.update(original.as_bytes());
    let mut out = [0u8; 16];
    out.copy_from_slice(&h.finalize().as_bytes()[..16]);
    ChangeId::from_bytes(out)
}

fn subject(message: &[u8]) -> &[u8] {
    let line = message.split(|&b| b == b'\n').next().unwrap_or_default();
    &line[..line.len().min(MAX_SUBJECT)]
}

/// The message of one revert for the whole stack.
fn squash_message(undos: &[Undo], top: &[u8]) -> Vec<u8> {
    let mut out = b"Revert the stack of \"".to_vec();
    out.extend_from_slice(subject(top));
    out.extend_from_slice(b"\"\n\nThis reverts commits:\n");
    for u in undos {
        out.extend_from_slice(format!("{}\n", u.cs).as_bytes());
    }
    out
}

/// The caller as author and committer of the reverts.
fn signature(ctx: &CallCtx) -> Result<Signature, Status> {
    let id = &ctx.principal.id;
    Signature::new(
        id.clone().into_bytes(),
        format!("{id}@asc.invalid").into_bytes(),
        now(),
        0,
    )
    .map_err(|e| invalid(format!("committer: {e}")))
}

/// The message of each new changeset, bottom (the revert of the top) first.
fn messages(req: &wire::RevertLandedRequest, undos: &[Undo]) -> Vec<Vec<u8>> {
    if !req.message.is_empty() {
        let n = if req.squash { 1 } else { undos.len() };
        return vec![req.message.to_vec(); n];
    }
    if !req.squash {
        return undos
            .iter()
            .map(|u| revert_message(&u.message, u.cs))
            .collect();
    }
    vec![match undos {
        [one] => revert_message(&one.message, one.cs),
        all => squash_message(all, &all[0].message),
    }]
}

/// The landed stack `req.at` names, top first, and the target the reverts stand on.
async fn plan(
    reader: &mut Reader<'_>,
    req: &wire::RevertLandedRequest,
) -> Result<Vec<Landed>, Status> {
    use wire::revert_landed_request::At;
    let bottom = if req.bottom.is_empty() {
        None
    } else {
        Some(change_of(&req.bottom)?)
    };
    let top = match &req.at {
        Some(At::Change(b)) => change_of(b)?,
        Some(At::LandedAs(b)) => {
            let cs = reader.ids.changeset_in(b).await?;
            let rows = index::rows_of(&mut *reader.conn, reader.ns, &[cs])
                .await
                .map_err(|e| internal("change index", &e))?;
            let Some(row) = rows.first() else {
                return Err(Status::not_found("no change landed as that changeset"));
            };
            if reader.landed(row.change).await? != Some(cs) {
                return Err(not_landed(row.change));
            }
            row.change
        }
        None => {
            return Err(invalid(
                "name the landed stack: its top change or the changeset it landed as",
            ));
        }
    };
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    let mut at = top;
    loop {
        let Some(head) = reader.head(at).await? else {
            return Err(Status::not_found("unknown change"));
        };
        let Some(cs) = reader.landed(at).await? else {
            if out.is_empty() {
                return Err(not_landed(at));
            }
            break;
        };
        out.push(Landed { change: at, cs });
        if Some(at) == bottom {
            return Ok(out);
        }
        if out.len() > MAX_STACK {
            return Err(Status::resource_exhausted(format!(
                "a stack has at most {MAX_STACK} changes"
            )));
        }
        let Some(parent) = head.parent else { break };
        if !seen.insert(at) || seen.contains(&parent) {
            return Err(Status::data_loss("the parent chain of a change loops"));
        }
        if reader.head(parent).await?.is_none() {
            break;
        }
        at = parent;
    }
    match bottom {
        Some(_) => Err(invalid(
            "bottom is not a landed change below the top of the stack",
        )),
        None => Ok(out),
    }
}

/// Undoes each distinct landed changeset from the top down, each on the tree before it.
async fn undo_all(
    engine: &Arc<Eng>,
    landed: &[Landed],
    onto_root: TreeId,
) -> Result<Vec<Undo>, Status> {
    let mut undos: Vec<Undo> = Vec::new();
    let mut base = onto_root;
    let mut reported = 0usize;
    for l in landed {
        if undos.iter().any(|u| u.cs == l.cs) {
            continue;
        }
        let undo = engine.get_changeset(l.cs).await.map_err(commit_status)?;
        let parent_root = match undo.parents.first() {
            Some(p) => engine.get_changeset(*p).await.map_err(commit_status)?.root,
            None => Eng::empty_tree_id(),
        };
        let (engine2, theirs, opts) = (engine.clone(), undo.root, agent_merge_options());
        let merged = on_blocking_thread(move || async move {
            engine2.merge_trees(theirs, base, parent_root, &opts).await
        })
        .await?
        .map_err(merge_status)?;
        if merged.conflicts.is_empty() {
            if merged.root == base {
                return Err(Status::failed_precondition(format!(
                    "reverting {} changes nothing on the target: it is undone already",
                    l.cs
                )));
            }
            base = merged.root;
            undos.push(Undo {
                cs: l.cs,
                message: undo.message.clone(),
                root: Some(merged.root),
                conflicts: Vec::new(),
            });
        } else {
            let room = MAX_CONFLICTS.saturating_sub(reported);
            let conflicts: Vec<wire::Conflict> = merged
                .conflicts
                .iter()
                .take(room)
                .map(|c| conflict_wire(l.cs, c))
                .collect();
            reported += conflicts.len();
            undos.push(Undo {
                cs: l.cs,
                message: undo.message.clone(),
                root: None,
                conflicts,
            });
        }
    }
    Ok(undos)
}

/// What a dry run or an apply answers, from the reverts made (`None` on a dry run).
async fn response(
    ids: &IdCodec,
    landed: &[Landed],
    undos: &[Undo],
    squash: bool,
    made: Option<&HashMap<ChangesetId, (ChangeId, ChangesetId)>>,
    ref_name: String,
) -> Result<wire::RevertLandedResponse, Status> {
    // One entry per revert: the top landed change of each, or one for the stack with `squash`.
    let entries: Vec<(&Landed, Vec<wire::Conflict>)> = if squash {
        let all = undos.iter().flat_map(|u| u.conflicts.iter().cloned());
        vec![(&landed[0], all.collect())]
    } else {
        undos
            .iter()
            .map(|u| {
                let top = landed
                    .iter()
                    .find(|l| l.cs == u.cs)
                    .expect("an undo comes from a landed change");
                (top, u.conflicts.clone())
            })
            .collect()
    };
    let mut reverts = Vec::with_capacity(entries.len());
    for (l, conflicts) in entries {
        let mut out = wire::RevertedChange {
            original: Bytes::copy_from_slice(l.change.as_bytes()),
            landed_as: ids.changeset_out(l.cs).await?,
            conflicts,
            ..wire::RevertedChange::default()
        };
        if let Some(&(change, cs)) = made.and_then(|m| m.get(&l.cs)) {
            out.revert_change = Bytes::copy_from_slice(change.as_bytes());
            out.revert_changeset = ids.changeset_out(cs).await?;
        }
        reverts.push(out);
    }
    let stack_top = made
        .and_then(|m| undos.last().and_then(|u| m.get(&u.cs)))
        .map_or_else(Bytes::new, |(c, _)| Bytes::copy_from_slice(c.as_bytes()));
    Ok(wire::RevertLandedResponse {
        reverts,
        applied: made.is_some(),
        stack_top,
        r#ref: if made.is_some() {
            ref_name
        } else {
            String::new()
        },
    })
}

/// `Stacks.RevertLanded`.
pub(super) async fn revert_landed(
    changes: &ChangesRpc,
    restacker: &Restacker,
    ctx: &CallCtx,
    req: wire::RevertLandedRequest,
) -> Result<wire::RevertLandedResponse, Status> {
    if ctx.id_mode != IdMode::Asc {
        return Err(Status::unimplemented(
            "git id mode is not served by Stacks.RevertLanded",
        ));
    }
    if req.message.len() > MAX_MESSAGE_BYTES {
        return Err(invalid("message is too long"));
    }
    let op = match (req.dry_run, ctx.op_id) {
        (true, _) => None,
        (false, Some(op)) => Some(op),
        (false, None) => return Err(invalid("mutating calls need x-asc-op-id")),
    };
    let (entry, ids, engine) = changes.open(ctx).await?;
    let (_, land_ref) = ChangesRpc::land_ref(&entry, "")?;
    let landed = {
        let mut conn = changes
            .registry
            .pool()
            .acquire()
            .await
            .map_err(|e| internal("database", &e))?;
        let mut reader = Reader {
            conn: &mut conn,
            ids: &ids,
            engine: &engine,
            ns: entry.ns,
            land_ref,
            heads: HashMap::new(),
        };
        plan(&mut reader, &req).await?
    };
    let top_cs = landed[0].cs;
    let branch = op.map(|op| branch_of(top_cs, op));

    // A retry of an apply whose branch was written: read it back and finish the index.
    if let (Some(op), Some(branch)) = (op, &branch)
        && let Some(done) = restacker.refs.stored(&entry, op).await?
    {
        let tip = match done.results.as_slice() {
            [r] if r.name == *branch => match r.new.as_ref().map(RefTarget::try_from) {
                Some(Ok(RefTarget::Changeset(c))) => c,
                _ => return Err(invalid("op id was used for another operation")),
            },
            _ => return Err(invalid("op id was used for another operation")),
        };
        return replay(changes, &entry, &ids, &engine, ctx, &req, &landed, op, tip).await;
    }

    let onto = restacker.onto(&ids, &entry, &req.onto).await?;
    let onto_root = engine
        .get_changeset(onto)
        .await
        .map_err(commit_status)?
        .root;
    let undos = undo_all(&engine, &landed, onto_root).await?;
    let conflicts: Vec<wire::Conflict> = undos
        .iter()
        .flat_map(|u| u.conflicts.iter().cloned())
        .collect();
    let (Some(op), Some(branch)) = (op, branch) else {
        return response(&ids, &landed, &undos, req.squash, None, String::new()).await;
    };
    if !conflicts.is_empty() {
        return Err(conflict_error(&conflicts));
    }

    // The new changesets, bottom (the revert of the top) first.
    let sig = signature(ctx)?;
    let policy = PathPolicy::Native {
        windows_safe: restacker.refs.windows_safe(&entry).await?,
    };
    let mut made: HashMap<ChangesetId, (ChangeId, ChangesetId)> = HashMap::new();
    let mut chain: Vec<(ChangeId, ChangesetId)> = Vec::new();
    let mut messages = messages(&req, &undos).into_iter();
    if req.squash {
        let message = messages.next().expect("one message");
        let root = undos
            .last()
            .and_then(|u| u.root)
            .expect("a clean revert has a tree");
        let cs = create(&engine, onto, root, message, &sig, policy).await?;
        let change = revert_id(op, landed[0].change);
        for u in &undos {
            made.insert(u.cs, (change, cs));
        }
        chain.push((change, cs));
    } else {
        let mut parent = onto;
        for (u, message) in undos.iter().zip(messages) {
            let root = u.root.expect("a clean revert has a tree");
            let cs = create(&engine, parent, root, message, &sig, policy).await?;
            let original = landed
                .iter()
                .find(|l| l.cs == u.cs)
                .expect("an undo comes from a landed change")
                .change;
            let change = revert_id(op, original);
            made.insert(u.cs, (change, cs));
            chain.push((change, cs));
            parent = cs;
        }
    }
    let tip = chain.last().expect("a stack has a change").1;
    let update = wire::UpdateRefsRequest {
        repo: Some(wire::RepoRef {
            name: ctx.repo.to_string(),
            id_mode: IdMode::Asc as i32,
        }),
        updates: vec![wire::RefUpdate {
            name: branch,
            expect: Some(wire::ref_update::Expect::Absent(true)),
            new: Some(wire::RefTarget::from(&RefTarget::Changeset(tip))),
            delete: false,
        }],
        mode: wire::UpdateMode::FastForward as i32,
        hooks: wire::HookMode::Run as i32,
        ..wire::UpdateRefsRequest::default()
    };
    restacker.refs.update_rewriting(ctx, update, None).await?;
    index_new(changes, &entry, ctx, op, &chain, sig.time()).await?;
    let stored = stored_name(changes, &entry, ctx, &branch_of(top_cs, op)).await?;
    response(&ids, &landed, &undos, req.squash, Some(&made), stored).await
}

/// A replay answers only for the reverts this call would have made: by the same caller, with
/// the same messages and trees, standing on the parent of the bottom one.
async fn verify_replay(
    engine: &Arc<Eng>,
    ctx: &CallCtx,
    req: &wire::RevertLandedRequest,
    landed: &[Landed],
    css: &[ChangesetId],
) -> Result<(), Status> {
    let other = || invalid("op id was used for another operation");
    let sig = signature(ctx)?;
    let mut made = Vec::with_capacity(css.len());
    for &cs in css {
        made.push(engine.get_changeset(cs).await.map_err(commit_status)?);
    }
    let onto = *made[0].parents.first().ok_or_else(other)?;
    let onto_root = engine
        .get_changeset(onto)
        .await
        .map_err(commit_status)?
        .root;
    // Reverts that stand on another stack undo nothing there: not this call's.
    let undos = undo_all(engine, landed, onto_root)
        .await
        .map_err(|e| match e.code() {
            tonic::Code::FailedPrecondition => other(),
            _ => e,
        })?;
    let roots: Vec<Option<TreeId>> = if req.squash {
        vec![undos.last().and_then(|u| u.root)]
    } else {
        undos.iter().map(|u| u.root).collect()
    };
    let messages = messages(req, &undos);
    if roots.len() != made.len() || undos.iter().any(|u| !u.conflicts.is_empty()) {
        return Err(other());
    }
    for ((c, root), message) in made.iter().zip(&roots).zip(&messages) {
        if Some(c.root) != *root
            || c.message != *message
            || c.committer.name() != sig.name()
            || c.committer.email() != sig.email()
            || c.author.name() != sig.name()
        {
            return Err(other());
        }
    }
    Ok(())
}

fn branch_of(top: ChangesetId, op: Uuid) -> String {
    let (top, op) = (top.to_string(), op.simple().to_string());
    format!("refs/heads/revert/{}-{}", &top[..12], &op[..8])
}

/// The name `UpdateRefs` stores `branch` under for the caller.
async fn stored_name(
    changes: &ChangesRpc,
    entry: &RepoEntry,
    ctx: &CallCtx,
    branch: &str,
) -> Result<String, Status> {
    let policy = crate::refpolicy::RepoPolicy::load(changes.registry.pool(), entry.key.0).await?;
    let class = policy.classify(
        crate::refpolicy::draft_owner_of(&ctx.principal, branch),
        branch,
    )?;
    Ok(class.stored(branch).to_owned())
}

async fn create(
    engine: &Arc<Eng>,
    parent: ChangesetId,
    root: TreeId,
    message: Vec<u8>,
    sig: &Signature,
    policy: PathPolicy,
) -> Result<ChangesetId, Status> {
    let new = NewChangeset {
        parents: vec![parent],
        author: sig.clone(),
        committer: sig.clone(),
        message,
        extra: Vec::new(),
        root,
        copies: Vec::new(),
    };
    let engine = engine.clone();
    on_blocking_thread(move || async move { engine.create_changeset(new, policy).await })
        .await?
        .map_err(commit_status)
}

/// The meta and version 1 of each new change, bottom first, in one transaction. A retry writes
/// nothing twice.
async fn index_new(
    changes: &ChangesRpc,
    entry: &RepoEntry,
    ctx: &CallCtx,
    op: Uuid,
    chain: &[(ChangeId, ChangesetId)],
    at: i64,
) -> Result<(), Status> {
    let registry = &changes.registry;
    let metas = ChangeIndex::new(registry.clone());
    let mut tx = registry
        .refs()
        .begin()
        .await
        .map_err(|e| internal("refs", &e))?;
    registry.guard(&mut tx, entry).await?;
    let mut below = None;
    for &(change, cs) in chain {
        metas
            .write_meta(
                &mut tx,
                entry.key,
                &ChangeMeta::new(cs, change, Vec::new(), RewriteOp::New),
            )
            .await
            .map_err(|e| internal("change index", &e))?;
        versions::append(
            &mut tx,
            entry.ns,
            &versions::NewVersion {
                change,
                cs,
                parent_change: below,
                at,
                actor: &ctx.principal.id,
                op_id: op.as_bytes(),
                force: false,
            },
        )
        .await
        .map_err(|e| internal("versions", &e))?;
        below = Some(change);
    }
    tx.commit().await.map_err(|e| internal("commit", &e))
}

/// The answer of an apply whose branch `UpdateRefs` already wrote, at `tip`: the reverts are the
/// changesets below it, one per distinct landed changeset (one in all with `squash`).
#[allow(clippy::too_many_arguments)] // the pieces of the call
async fn replay(
    changes: &ChangesRpc,
    entry: &RepoEntry,
    ids: &IdCodec,
    engine: &Arc<Eng>,
    ctx: &CallCtx,
    req: &wire::RevertLandedRequest,
    landed: &[Landed],
    op: Uuid,
    tip: ChangesetId,
) -> Result<wire::RevertLandedResponse, Status> {
    let mut distinct: Vec<&Landed> = Vec::new();
    for l in landed {
        if !distinct.iter().any(|d| d.cs == l.cs) {
            distinct.push(l);
        }
    }
    let n = if req.squash { 1 } else { distinct.len() };
    // Top down from the tip: the revert of the bottom landed change first.
    let mut css = Vec::with_capacity(n);
    let mut at = tip;
    for i in 0..n {
        css.push(at);
        if i + 1 < n {
            let c = engine.get_changeset(at).await.map_err(commit_status)?;
            at = *c
                .parents
                .first()
                .ok_or_else(|| Status::data_loss("a revert stack is shorter than its op"))?;
        }
    }
    css.reverse();
    verify_replay(engine, ctx, req, landed, &css).await?;
    let mut made = HashMap::new();
    let mut chain = Vec::with_capacity(n);
    if req.squash {
        let change = revert_id(op, landed[0].change);
        for d in &distinct {
            made.insert(d.cs, (change, css[0]));
        }
        chain.push((change, css[0]));
    } else {
        for (d, &cs) in distinct.iter().zip(&css) {
            let change = revert_id(op, d.change);
            made.insert(d.cs, (change, cs));
            chain.push((change, cs));
        }
    }
    let at = engine
        .get_changeset(tip)
        .await
        .map_err(commit_status)?
        .committer
        .time();
    index_new(changes, entry, ctx, op, &chain, at).await?;
    let undos: Vec<Undo> = distinct
        .iter()
        .map(|d| Undo {
            cs: d.cs,
            message: Vec::new(),
            root: None,
            conflicts: Vec::new(),
        })
        .collect();
    let stored = stored_name(changes, entry, ctx, &branch_of(landed[0].cs, op)).await?;
    response(ids, landed, &undos, req.squash, Some(&made), stored).await
}

#[cfg(test)]
mod tests {
    use asc_core::path::RepoPath;
    use asc_rpc::auth::Principal;
    use asc_rpc::server::RepoName;

    use super::super::restack::tests::{Fx, cbytes, fx};
    use super::*;
    use crate::ingest::testutil::commit;

    fn ctx(op: Option<Uuid>) -> CallCtx {
        CallCtx {
            repo: RepoName::parse("o/r").unwrap(),
            id_mode: IdMode::Asc,
            principal: Principal {
                id: "writer".into(),
            },
            op_id: op,
            client: None,
            admin: Default::default(),
        }
    }

    fn req(n: u8, dry_run: bool) -> wire::RevertLandedRequest {
        wire::RevertLandedRequest {
            at: Some(wire::revert_landed_request::At::Change(cbytes(n))),
            dry_run,
            ..wire::RevertLandedRequest::default()
        }
    }

    fn bytes(cs: ChangesetId) -> Bytes {
        Bytes::copy_from_slice(cs.as_bytes())
    }

    fn cs_of(b: &Bytes) -> ChangesetId {
        ChangesetId::from_bytes(b.as_ref().try_into().unwrap())
    }

    /// Changes 1 and 2 land as their heads; trunk is at change 2.
    async fn landed() -> Fx {
        let f = fx().await;
        let trunk = format!("refs/heads/{}", f.e.default_branch);
        for (pos, cs) in [(0i64, f.c[0]), (1, f.c[1])] {
            sqlx::query("INSERT INTO mainline (ns, ref, pos, cs) VALUES ($1, $2, $3, $4)")
                .bind(f.e.ns.as_bytes().to_vec())
                .bind(trunk.clone())
                .bind(pos)
                .bind(cs.as_bytes().to_vec())
                .execute(f.reg.pool())
                .await
                .unwrap();
        }
        f.set_ref(&trunk, f.c[1]).await;
        f
    }

    async fn run(
        f: &Fx,
        op: Option<Uuid>,
        r: wire::RevertLandedRequest,
    ) -> Result<wire::RevertLandedResponse, Status> {
        let restacker = f.rpc.restack.get().unwrap().clone();
        revert_landed(&f.rpc, &restacker, &ctx(op), r).await
    }

    async fn read(f: &Fx, cs: ChangesetId, path: &str) -> bool {
        let root = f.cs(cs).await.root;
        f.e.backend
            .engine()
            .lookup(root, &RepoPath::new(path).unwrap())
            .await
            .unwrap()
            .is_some()
    }

    async fn version(f: &Fx, change: ChangeId) -> versions::Version {
        let mut c = f.reg.pool().acquire().await.unwrap();
        versions::latest(&mut c, f.e.ns, change)
            .await
            .unwrap()
            .unwrap()
    }

    #[tokio::test]
    async fn a_dry_run_answers_the_landed_stack_top_first_and_stores_nothing() {
        let f = landed().await;
        let r = run(&f, None, req(2, true)).await.unwrap();
        let got: Vec<_> = r
            .reverts
            .iter()
            .map(|v| (v.original.clone(), v.landed_as.clone()))
            .collect();
        assert_eq!(
            got,
            vec![(cbytes(2), bytes(f.c[1])), (cbytes(1), bytes(f.c[0]))]
        );
        assert!(r.reverts.iter().all(|v| v.revert_change.is_empty()
            && v.revert_changeset.is_empty()
            && v.conflicts.is_empty()));
        assert!(!r.applied);
        assert!(r.stack_top.is_empty() && r.r#ref.is_empty());
    }

    #[tokio::test]
    async fn an_apply_stores_a_stack_of_reverts_on_a_branch_of_the_caller() {
        let f = landed().await;
        let op = Uuid::new_v4();
        let r = run(&f, Some(op), req(2, false)).await.unwrap();
        assert!(r.applied);
        let (top, below) = (&r.reverts[0], &r.reverts[1]);
        let (first, second) = (cs_of(&top.revert_changeset), cs_of(&below.revert_changeset));
        // The revert of the top stands on trunk; the revert of the change below it on that.
        assert_eq!(f.cs(first).await.parents, vec![f.c[1]]);
        assert_eq!(f.cs(second).await.parents, vec![first]);
        assert!(!read(&f, first, "two").await && read(&f, first, "one").await);
        assert!(!read(&f, second, "one").await && read(&f, second, "a").await);
        assert_eq!(
            f.cs(first).await.message,
            format!("Revert \"m\"\n\nThis reverts commit {}.\n", f.c[1]).into_bytes()
        );
        assert_eq!(r.stack_top, below.revert_change);
        assert!(r.r#ref.starts_with("refs/heads/revert/"), "{}", r.r#ref);
        assert_eq!(f.ref_of(&r.r#ref).await, Some(RefTarget::Changeset(second)));
        let (a, b) = (
            change_of(&top.revert_change).unwrap(),
            change_of(&below.revert_change).unwrap(),
        );
        let (va, vb) = (version(&f, a).await, version(&f, b).await);
        assert_eq!((va.v, va.cs, va.parent_change), (1, first, None));
        assert_eq!((vb.v, vb.cs, vb.parent_change), (1, second, Some(a)));

        // A retry of the op answers the same stack and writes nothing more.
        let again = run(&f, Some(op), req(2, false)).await.unwrap();
        assert_eq!(again, r);
        assert_eq!(version(&f, b).await.v, 1);
    }

    #[tokio::test]
    async fn a_replay_answers_only_the_caller_of_the_op() {
        let f = landed().await;
        let op = Uuid::new_v4();
        run(&f, Some(op), req(2, false)).await.unwrap();
        let restacker = f.rpc.restack.get().unwrap().clone();
        let mut other = ctx(Some(op));
        other.principal.id = "someone-else".into();
        let e = revert_landed(&f.rpc, &restacker, &other, req(2, false))
            .await
            .unwrap_err();
        assert_eq!(e.code(), tonic::Code::InvalidArgument, "{e:?}");
        let squash = wire::RevertLandedRequest {
            squash: true,
            ..req(2, false)
        };
        let e = run(&f, Some(op), squash).await.unwrap_err();
        assert_eq!(e.code(), tonic::Code::InvalidArgument, "{e:?}");
    }

    #[tokio::test]
    async fn a_squash_makes_one_revert_and_bottom_and_landed_as_pick_the_range() {
        let f = landed().await;
        let squash = wire::RevertLandedRequest {
            squash: true,
            ..req(2, false)
        };
        let r = run(&f, Some(Uuid::new_v4()), squash).await.unwrap();
        assert_eq!(r.reverts.len(), 1, "one revert for the stack");
        assert_eq!(r.reverts[0].original, cbytes(2));
        assert_eq!(r.reverts[0].revert_change, r.stack_top);
        let one = cs_of(&r.reverts[0].revert_changeset);
        assert_eq!(f.cs(one).await.parents, vec![f.c[1]]);
        assert_eq!(f.cs(one).await.root, f.cs(f.a).await.root);
        assert!(
            f.cs(one)
                .await
                .message
                .starts_with(b"Revert the stack of \"m\"")
        );

        let only_top = wire::RevertLandedRequest {
            at: Some(wire::revert_landed_request::At::LandedAs(bytes(f.c[1]))),
            bottom: cbytes(2),
            ..req(2, true)
        };
        let r = run(&f, None, only_top).await.unwrap();
        assert_eq!(r.reverts.len(), 1);
        assert_eq!(r.reverts[0].original, cbytes(2));

        let outside = wire::RevertLandedRequest {
            bottom: cbytes(3),
            ..req(2, true)
        };
        let e = run(&f, None, outside).await.unwrap_err();
        assert_eq!(e.code(), tonic::Code::InvalidArgument, "{e:?}");
    }

    #[tokio::test]
    async fn a_change_that_has_not_landed_is_refused() {
        let f = landed().await;
        let e = run(&f, None, req(3, true)).await.unwrap_err();
        assert_eq!(e.code(), tonic::Code::FailedPrecondition, "{e:?}");
        let e = run(&f, None, req(9, true)).await.unwrap_err();
        assert_eq!(e.code(), tonic::Code::NotFound, "{e:?}");
        let e = run(
            &f,
            None,
            wire::RevertLandedRequest {
                at: None,
                ..req(2, true)
            },
        )
        .await
        .unwrap_err();
        assert_eq!(e.code(), tonic::Code::InvalidArgument, "{e:?}");
        let e = run(&f, None, req(2, false)).await.unwrap_err();
        assert_eq!(
            e.code(),
            tonic::Code::InvalidArgument,
            "an apply needs an op id"
        );
    }

    #[tokio::test]
    async fn a_conflict_is_answered_by_a_dry_run_and_refuses_an_apply() {
        let f = landed().await;
        let later = commit(&f.e, &[f.c[1]], &[("two", b"changed\n")]).await;
        let onto = wire::RevertLandedRequest {
            onto: later.to_string(),
            ..req(2, true)
        };
        let r = run(&f, None, onto.clone()).await.unwrap();
        assert_eq!(r.reverts[0].conflicts.len(), 1);
        assert_eq!(r.reverts[0].conflicts[0].path.as_ref(), b"two");
        assert!(r.reverts[1].conflicts.is_empty());

        let e = run(
            &f,
            Some(Uuid::new_v4()),
            wire::RevertLandedRequest {
                dry_run: false,
                ..onto
            },
        )
        .await
        .unwrap_err();
        assert_eq!(e.code(), tonic::Code::Aborted, "{e:?}");
        assert!(e.message().starts_with("REVERT_CONFLICT"), "{e:?}");
    }

    #[tokio::test]
    async fn a_revert_that_changes_nothing_is_refused() {
        let f = landed().await;
        let e = run(
            &f,
            None,
            wire::RevertLandedRequest {
                onto: f.a.to_string(),
                ..req(2, true)
            },
        )
        .await
        .unwrap_err();
        assert_eq!(e.code(), tonic::Code::FailedPrecondition, "{e:?}");
        assert!(e.message().contains("undone already"), "{e:?}");
    }
}
