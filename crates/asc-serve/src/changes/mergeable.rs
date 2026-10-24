//! `Mergeability` and the `mergeable` table (PLAN C21, SPEC §20.4 and §20.5).
//!
//! `mergeable(ns, head, trunk_tip, state, conflicts, computed_at)` is a plain SQL table with one
//! overwritten row per head changeset. A row answers a request while its `trunk_tip` is the
//! target's current tip; otherwise the answer is recomputed and the row overwritten, and the rows
//! of the same change's older heads are deleted, so the table cannot grow with trunk history or
//! with versions. A closed PR deletes the row ([`super::overlaps::close`]).
//!
//! The check is lazy and cheap first (SPEC §4.5): when the head's changed paths and the paths
//! the trunk changed since their merge base are disjoint, nothing is read. Only intersecting
//! histories run the content merge. States:
//!
//! * `NEEDS_RESTACK`: the change sits on an older version of its parent change (or shares no
//!   history with the target). Content is not checked; this is decided on every call.
//! * `DISJOINT`: nothing to merge, or the path sets do not intersect.
//! * `CLEAN_MERGE`: the paths intersect and the three-way merge has no conflict.
//! * `CONFLICTS`: the merge has conflicts, listed (at most [`MAX_CONFLICTS`]).
//!
//! The merge stores conflict-marked blobs for text conflicts (the ids in the conflict list name
//! them); they are unreferenced objects the next GC removes.

use asc_core::change::ChangeId;
use asc_core::id::ChangesetId;
use asc_core::id::RefTarget;
use asc_core::path::RefName;
use asc_engine::history::HistoryApi;
use asc_engine::merge::MergeOptions;
use asc_rpc::server::wire;
use asc_store::refs::Refs;
use prost::Message;
use sqlx::{AnyConnection, Connection, Row};
use tonic::Status;

use super::fixed;
use super::rpc::Reader;
use crate::commit_graph_rpc::{commit_status, internal};
use crate::mutations::conflict_wire;
use crate::registry::{Ns, Registry, RepoEntry};

/// Most heads one call may ask about (each may run a content merge).
pub const MAX_HEADS: usize = 64;
/// Most conflicts kept and returned per head.
pub const MAX_CONFLICTS: usize = 1_000;
/// Largest stored `conflicts` blob read back; a larger or damaged one is recomputed.
const MAX_STORED: usize = 4 * 1024 * 1024;

use wire::mergeable::State;

fn state_code(s: State) -> i16 {
    s as i16
}

/// The changeset `land_ref` points at.
pub async fn trunk_tip(
    registry: &Registry,
    entry: &RepoEntry,
    land_ref: &str,
) -> Result<ChangesetId, Status> {
    let name = RefName::new(land_ref.as_bytes())
        .map_err(|_| Status::invalid_argument("invalid target branch"))?;
    match registry
        .refs()
        .get(entry.key, &name)
        .await
        .map_err(|e| internal("refs", &e))?
    {
        Some(e) => match e.target {
            RefTarget::Changeset(c) => Ok(c),
            RefTarget::Tag(_) => Err(Status::failed_precondition("the target is not a branch")),
        },
        None => Err(Status::failed_precondition(
            "the target branch does not exist",
        )),
    }
}

/// A memoised answer for `head` at `tip`.
async fn memo(
    db: &mut AnyConnection,
    ns: Ns,
    head: ChangesetId,
    tip: ChangesetId,
) -> Result<Option<wire::Mergeable>, Status> {
    let row = sqlx::query(
        "SELECT trunk_tip, CAST(state AS BIGINT) AS state, conflicts FROM mergeable \
         WHERE ns = $1 AND head = $2",
    )
    .bind(ns.as_bytes().to_vec())
    .bind(head.as_bytes().to_vec())
    .fetch_optional(&mut *db)
    .await
    .map_err(|e| internal("mergeable", &e))?;
    let Some(row) = row else { return Ok(None) };
    let stored: Option<[u8; 32]> = fixed(&row, "trunk_tip");
    if stored != Some(*tip.as_bytes()) {
        return Ok(None);
    }
    let blob: Vec<u8> = row
        .try_get("conflicts")
        .map_err(|e| internal("mergeable", &e))?;
    let state: i64 = row
        .try_get("state")
        .map_err(|e| internal("mergeable", &e))?;
    // A row this server cannot read is a miss: the answer is recomputed and overwritten.
    if blob.len() > MAX_STORED {
        return Ok(None);
    }
    let Ok(m) = wire::Mergeable::decode(blob.as_slice()) else {
        return Ok(None);
    };
    let Ok(code) = i32::try_from(state) else {
        return Ok(None);
    };
    if m.state != code || State::try_from(code).is_err() || m.conflicts.len() > MAX_CONFLICTS {
        return Ok(None);
    }
    Ok(Some(m))
}

/// Overwrites the row of `head` and drops the rows of the change's other changesets.
async fn store(
    db: &mut AnyConnection,
    ns: Ns,
    change: ChangeId,
    head: ChangesetId,
    tip: ChangesetId,
    answer: &wire::Mergeable,
    now: i64,
) -> Result<(), Status> {
    let mut tx = db.begin().await.map_err(|e| internal("mergeable", &e))?;
    sqlx::query(
        "DELETE FROM mergeable WHERE ns = $1 AND head <> $3 AND head IN \
         (SELECT cs FROM changes WHERE ns = $1 AND change = $2)",
    )
    .bind(ns.as_bytes().to_vec())
    .bind(change.as_bytes().to_vec())
    .bind(head.as_bytes().to_vec())
    .execute(&mut *tx)
    .await
    .map_err(|e| internal("mergeable", &e))?;
    sqlx::query(
        "INSERT INTO mergeable (ns, head, trunk_tip, state, conflicts, computed_at) \
         VALUES ($1, $2, $3, $4, $5, $6) \
         ON CONFLICT (ns, head) DO UPDATE SET trunk_tip = excluded.trunk_tip, \
         state = excluded.state, conflicts = excluded.conflicts, \
         computed_at = excluded.computed_at",
    )
    .bind(ns.as_bytes().to_vec())
    .bind(head.as_bytes().to_vec())
    .bind(tip.as_bytes().to_vec())
    .bind(state_code(
        State::try_from(answer.state).unwrap_or(State::Conflicts),
    ))
    .bind(answer.encode_to_vec())
    .bind(now)
    .execute(&mut *tx)
    .await
    .map_err(|e| internal("mergeable", &e))?;
    tx.commit().await.map_err(|e| internal("mergeable", &e))
}

/// Runs an engine future to completion on a blocking thread: the merges are not provably `Send`.
async fn on_blocking_thread<F, Fut, T>(make: F) -> Result<T, Status>
where
    F: FnOnce() -> Fut + Send + 'static,
    Fut: std::future::Future<Output = T>,
    T: Send + 'static,
{
    let handle = tokio::runtime::Handle::current();
    tokio::task::spawn_blocking(move || handle.block_on(make()))
        .await
        .map_err(|e| internal("merge task", &e))
}

/// The state of merging `head` into `tip`, with its conflicts (raw changeset ids).
async fn compute(
    engine: &std::sync::Arc<crate::idmode::Eng>,
    head: ChangesetId,
    tip: ChangesetId,
) -> Result<(State, Vec<wire::Conflict>), Status> {
    if engine
        .is_ancestor(head, tip)
        .await
        .map_err(|e| internal("ancestry", &e))?
    {
        return Ok((State::Disjoint, Vec::new()));
    }
    let base = engine
        .merge_base(tip, &[head], false)
        .await
        .map_err(|e| internal("merge base", &e))?
        .into_iter()
        .next();
    let Some(base) = base else {
        // No shared history: the change must be rebased onto the target first.
        return Ok((State::NeedsRestack, Vec::new()));
    };
    let ours = engine
        .paths_since(base, head)
        .await
        .map_err(|e| internal("paths", &e))?;
    let theirs = engine
        .paths_since(base, tip)
        .await
        .map_err(|e| internal("paths", &e))?;
    if ours.is_disjoint(&theirs) {
        return Ok((State::Disjoint, Vec::new()));
    }
    let root_of = |id| async move { engine.get_changeset(id).await.map(|c| c.root) };
    let (base_root, tip_root, head_root) = (
        root_of(base).await.map_err(commit_status)?,
        root_of(tip).await.map_err(commit_status)?,
        root_of(head).await.map_err(commit_status)?,
    );
    let eng = engine.clone();
    let merged = on_blocking_thread(move || async move {
        eng.merge_trees(base_root, tip_root, head_root, &MergeOptions::default())
            .await
    })
    .await?
    .map_err(|e| internal("merge", &e))?;
    if merged.conflicts.is_empty() {
        return Ok((State::CleanMerge, Vec::new()));
    }
    let conflicts = merged
        .conflicts
        .iter()
        .take(MAX_CONFLICTS)
        .map(|c| conflict_wire(Some(head), c))
        .collect();
    Ok((State::Conflicts, conflicts))
}

/// The mergeability of each of `changes` into the target whose tip is `tip`.
pub(crate) async fn mergeability(
    reader: &mut Reader<'_>,
    tip: ChangesetId,
    changes: &[ChangeId],
    now: i64,
) -> Result<Vec<wire::Mergeable>, Status> {
    let mut out = Vec::with_capacity(changes.len());
    let mut seen = std::collections::HashSet::new();
    for &change in changes {
        if !seen.insert(change) {
            continue;
        }
        let head = reader
            .head(change)
            .await?
            .ok_or_else(|| Status::not_found("unknown change"))?;
        let id = bytes::Bytes::copy_from_slice(change.as_bytes());
        let mut answer = if !reader.parent_current(&head).await? {
            wire::Mergeable {
                change: id.clone(),
                state: State::NeedsRestack as i32,
                conflicts: Vec::new(),
            }
        } else if let Some(m) = memo(&mut *reader.conn, reader.ns, head.cs, tip).await? {
            m
        } else {
            let (state, conflicts) = compute(reader.engine, head.cs, tip).await?;
            let m = wire::Mergeable {
                change: bytes::Bytes::new(),
                state: state as i32,
                conflicts,
            };
            if state != State::NeedsRestack {
                store(&mut *reader.conn, reader.ns, change, head.cs, tip, &m, now).await?;
            }
            m
        };
        answer.change = id;
        // Conflicts name the head in the caller's id mode.
        if !answer.conflicts.is_empty() {
            let named = reader.ids.changeset_out(head.cs).await?;
            for c in &mut answer.conflicts {
                c.changeset = named.clone();
            }
        }
        out.push(answer);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::changes::testkit::{Fx, cid, set_ref, stacked};

    async fn rows(f: &Fx) -> i64 {
        sqlx::query_scalar("SELECT COUNT(*) FROM mergeable")
            .fetch_one(f.reg.pool())
            .await
            .unwrap()
    }

    async fn ask(f: &Fx, change: u8, tip: ChangesetId) -> wire::Mergeable {
        let mut conn = f.reg.pool().acquire().await.unwrap();
        let mut r = f.reader(&mut conn);
        mergeability(&mut r, tip, &[cid(change)], 7)
            .await
            .unwrap()
            .remove(0)
    }

    #[tokio::test]
    async fn the_table_keeps_one_row_per_head_while_the_trunk_moves() {
        let f = stacked().await;
        // Change 1's head touches "a"; the trunk starts at the shared base and moves ten times.
        let mut tip = f.base;
        for i in 0..10 {
            tip = f.trunk_commit(tip, &[(&format!("t{i}"), b"x")]).await;
            let m = ask(&f, 1, tip).await;
            assert_eq!(m.state, State::Disjoint as i32);
            assert_eq!(rows(&f).await, 1, "after trunk move {i}");
        }
        // A second head adds exactly one row; asking again changes nothing.
        ask(&f, 2, tip).await;
        ask(&f, 2, tip).await;
        assert_eq!(rows(&f).await, 2);
    }

    #[tokio::test]
    async fn an_unchanged_tip_answers_from_the_row_and_a_moved_tip_recomputes() {
        let f = stacked().await;
        let tip = f.trunk_commit(f.base, &[("t", b"x")]).await;
        assert_eq!(ask(&f, 1, tip).await.state, State::Disjoint as i32);
        // Plant a different answer under the same head and tip: it is served ...
        let planted = wire::Mergeable {
            state: State::CleanMerge as i32,
            ..Default::default()
        };
        sqlx::query("UPDATE mergeable SET state = 1, conflicts = $1")
            .bind(planted.encode_to_vec())
            .execute(f.reg.pool())
            .await
            .unwrap();
        assert_eq!(ask(&f, 1, tip).await.state, State::CleanMerge as i32);
        // ... until the trunk moves, which recomputes and overwrites it.
        let again = f.trunk_commit(tip, &[("u", b"x")]).await;
        assert_eq!(ask(&f, 1, again).await.state, State::Disjoint as i32);
        let stored: i64 = sqlx::query_scalar("SELECT CAST(state AS BIGINT) FROM mergeable")
            .fetch_one(f.reg.pool())
            .await
            .unwrap();
        assert_eq!(stored, State::Disjoint as i64);
        // A row whose state disagrees with its blob is a miss.
        sqlx::query("UPDATE mergeable SET state = 2")
            .execute(f.reg.pool())
            .await
            .unwrap();
        assert_eq!(ask(&f, 1, again).await.state, State::Disjoint as i32);
    }

    #[tokio::test]
    async fn intersecting_paths_merge_clean_or_conflict() {
        let f = stacked().await;
        // The head of change 1 writes "a" = "a". A trunk that writes "a" = "other" conflicts.
        let conflicting = f.trunk_commit(f.base, &[("a", b"other")]).await;
        let m = ask(&f, 1, conflicting).await;
        assert_eq!(m.state, State::Conflicts as i32);
        assert_eq!(m.conflicts.len(), 1);
        assert_eq!(m.conflicts[0].path.as_ref(), b"a");
        assert_eq!(m.conflicts[0].changeset.len(), 32);
        // The same content on both sides merges clean although the paths intersect. (The commit
        // also writes "z": with "a" alone it would be change 1's own changeset, an ancestor.)
        let same = f.trunk_commit(f.base, &[("a", b"a"), ("z", b"z")]).await;
        assert_eq!(ask(&f, 1, same).await.state, State::CleanMerge as i32);
        assert_eq!(rows(&f).await, 1);
    }

    #[tokio::test]
    async fn a_head_already_in_the_target_has_nothing_to_merge() {
        let f = stacked().await;
        let m = ask(&f, 1, f.css[0]).await;
        assert_eq!(m.state, State::Disjoint as i32);
    }

    #[tokio::test]
    async fn a_stale_parent_needs_a_restack_and_is_not_stored() {
        let f = stacked().await;
        // Change 3 sits on change 2's head; publish a newer head of change 2.
        f.new_head_for(2).await;
        let m = ask(&f, 3, f.base).await;
        assert_eq!(m.state, State::NeedsRestack as i32);
        assert_eq!(rows(&f).await, 0);
    }

    #[tokio::test]
    async fn unknown_changes_and_missing_targets_are_refused() {
        let f = stacked().await;
        let mut conn = f.reg.pool().acquire().await.unwrap();
        let mut r = f.reader(&mut conn);
        let e = mergeability(&mut r, f.base, &[ChangeId::from_bytes([0xee; 16])], 0)
            .await
            .unwrap_err();
        assert_eq!(e.code(), tonic::Code::NotFound);
        drop(conn);
        let e = trunk_tip(&f.reg, &f.e, "refs/heads/nope")
            .await
            .unwrap_err();
        assert_eq!(e.code(), tonic::Code::FailedPrecondition);
        set_ref(&f, "refs/heads/master", f.base).await;
        assert_eq!(
            trunk_tip(&f.reg, &f.e, "refs/heads/master").await.unwrap(),
            f.base
        );
        let e = trunk_tip(&f.reg, &f.e, "refs/heads/bad..name")
            .await
            .unwrap_err();
        assert_eq!(e.code(), tonic::Code::InvalidArgument);
    }
}
