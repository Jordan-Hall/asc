//! Local GC of hidden, un-uploaded drafts (SPEC §21.2.4, local column; B06).
//!
//! `store/` holds the objects only this machine has, and is never evicted. The one removal is
//! here: objects reachable only from hidden draft heads whose `hidden_at` is older than
//! `asc.draft.hiddenDays` (default 90). The retention is independent of the op-log cap of §20.3.
//!
//! Safety rules:
//! * `store/` is shared by every repository of the machine, so the live roots are read from the
//!   whole of `state.db`: every ref, every workspace base, every visible or still-retained
//!   hidden head, every queued upload, every op-log snapshot, staged content and switch
//!   intents. An object any of them reaches is kept.
//! * Only objects present in `store/` are followed or deleted. An object that is only in `cache/`
//!   was uploaded and is none of this unit's business.
//! * Any unreadable or undecodable object, or a walk past [`MAX_OBJECTS`], aborts the run with
//!   nothing deleted.
//! * The live roots are read a second time just before the first delete and the walk is
//!   extended, which narrows (it cannot close) the window for a concurrent writer that
//!   re-references an old object. A writer that loses the race finds the object missing, never
//!   a wrong one.
//! * Deletion goes changesets first, so an interrupted run leaves no changeset whose trees are
//!   gone.

use std::collections::{HashMap, HashSet};

use asc_core::codec::decode_envelope;
use asc_core::git::EXTRA_GIT_COMMIT;
use asc_core::id::{ChangesetId, FileId, GitOid, RefTarget, TreeId};
use asc_core::object::{Change, Changeset, ChunkList, Kind, TagRecord, Tree};
use asc_state::state::StateDb;
use asc_store::cachedir::CacheDir;
use asc_store::key::ObjKey;
use asc_store::objects::Objects;
use futures::{StreamExt, stream};

use crate::cli::common::{CliError, Result, WsCtx};

/// Most objects one run walks; a larger store is left alone.
pub const MAX_OBJECTS: usize = 5_000_000;
/// Most rows read from one state table.
const MAX_ROWS: i64 = 2_000_000;
/// Most expired heads handled per run (the rest wait for the next run).
const MAX_EXPIRED: i64 = 10_000;
/// Concurrent object reads.
const READS: usize = 16;
/// Files above this size have a chunk list.
const CHUNK_THRESHOLD: u64 = 4 * 1024 * 1024;
const DAY: i64 = 86_400;

/// What a run found.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Report {
    /// Hidden heads past their retention.
    pub expired_heads: usize,
    /// `store/` objects reachable from them and from no live root.
    pub collectable: usize,
    /// Objects removed (zero on a dry run).
    pub deleted: usize,
    /// `draft_heads` rows dropped (zero on a dry run).
    pub rows_dropped: u64,
}

fn fatal(msg: impl std::fmt::Display) -> CliError {
    CliError::Fatal(format!("asc: draft gc: {msg}"))
}

fn db_err(e: sqlx::Error) -> CliError {
    CliError::from(asc_state::Error::from(e))
}

fn file_keys(out: &mut Vec<ObjKey>, id: FileId, size: u64) {
    out.push(ObjKey::File(id));
    if size > CHUNK_THRESHOLD {
        out.push(ObjKey::ChunkList(id));
    }
}

fn raw_commit_tree(raw: &[u8]) -> Option<GitOid> {
    let hex = raw.strip_prefix(b"tree ")?.get(..40)?;
    std::str::from_utf8(hex).ok()?.parse::<GitOid>().ok()
}

fn has_children(key: &ObjKey) -> bool {
    matches!(
        key,
        ObjKey::Changeset(_) | ObjKey::Tree(_) | ObjKey::Tag(_) | ObjKey::ChunkList(_)
    )
}

/// The objects a decoded object refers to.
fn children(key: &ObjKey, data: &[u8]) -> Result<Vec<ObjKey>> {
    let bad = |e: &dyn std::fmt::Display| fatal(format!("undecodable object {}: {e}", key.path()));
    let mut out = Vec::new();
    match key {
        ObjKey::Changeset(_) => {
            let cs: Changeset = decode_envelope(data).map_err(|e| bad(&e))?;
            out.extend(cs.parents.iter().map(|&p| ObjKey::Changeset(p)));
            out.push(ObjKey::Tree(cs.root));
            for (_, change) in cs.changes.as_slice() {
                if let Change::Write { file, size, .. } = change {
                    file_keys(&mut out, *file, *size);
                }
            }
            if let Some(raw) = cs.extra.get(&EXTRA_GIT_COMMIT.to_vec())
                && let Some(oid) = raw_commit_tree(raw)
            {
                out.push(ObjKey::GitTreeOverride(oid));
            }
        }
        ObjKey::Tree(_) => {
            let tree: Tree = decode_envelope(data).map_err(|e| bad(&e))?;
            for e in tree.entries() {
                match e.kind {
                    Kind::Dir => {
                        out.push(ObjKey::Tree(TreeId::from_bytes(e.id)));
                        out.push(ObjKey::GitTreeOverride(e.git));
                    }
                    Kind::File | Kind::Exec | Kind::Symlink => {
                        file_keys(&mut out, FileId::from_bytes(e.id), e.size);
                    }
                    Kind::Gitlink => {}
                }
            }
        }
        ObjKey::Tag(_) => {
            let tag: TagRecord = decode_envelope(data).map_err(|e| bad(&e))?;
            out.push(ObjKey::Changeset(tag.target));
        }
        ObjKey::ChunkList(_) => {
            let list: ChunkList = decode_envelope(data).map_err(|e| bad(&e))?;
            out.extend(list.chunks.iter().map(|&(c, _)| ObjKey::Chunk(c)));
        }
        _ => {}
    }
    Ok(out)
}

/// The `store/` objects reachable from some roots. `visited` is every key looked at, `present`
/// the ones `store/` holds. Walking again with the same sets continues where the first walk
/// stopped.
#[derive(Default)]
struct Walk {
    visited: HashSet<ObjKey>,
    present: HashSet<ObjKey>,
}

impl Walk {
    async fn extend(
        &mut self,
        store: &CacheDir,
        roots: impl IntoIterator<Item = ObjKey>,
    ) -> Result<()> {
        let mut frontier: Vec<ObjKey> = Vec::new();
        for r in roots {
            if self.visited.insert(r) {
                frontier.push(r);
            }
        }
        while !frontier.is_empty() {
            if self.visited.len() > MAX_OBJECTS {
                return Err(fatal("the store is too large to walk; nothing was removed"));
            }
            let take = frontier.split_off(frontier.len().saturating_sub(256));
            let fetched: Vec<_> = stream::iter(take)
                .map(|k| async move { (k, store.get(&k).await) })
                .buffer_unordered(READS)
                .collect()
                .await;
            for (k, got) in fetched {
                let Some(data) = got.map_err(CliError::Objects)? else {
                    continue;
                };
                self.present.insert(k);
                if !has_children(&k) {
                    continue;
                }
                for child in children(&k, &data)? {
                    if self.visited.insert(child) {
                        frontier.push(child);
                    }
                }
            }
        }
        Ok(())
    }
}

/// Fixed-width id column; a malformed value is skipped (it names nothing).
fn id32(b: Vec<u8>) -> Option<[u8; 32]> {
    b.try_into().ok()
}

async fn blobs(db: &StateDb, sql: &'static str, bind: Option<i64>) -> Result<Vec<Vec<u8>>> {
    let q = sqlx::query_scalar::<_, Vec<u8>>(sql);
    let q = match bind {
        Some(v) => q.bind(v),
        None => q,
    };
    let rows = q
        .bind(MAX_ROWS + 1)
        .fetch_all(db.pool())
        .await
        .map_err(db_err)?;
    if rows.len() as i64 > MAX_ROWS {
        return Err(fatal(
            "state.db has too many rows to read; nothing was removed",
        ));
    }
    Ok(rows)
}

/// Every root that keeps objects alive, across all repositories of this machine.
async fn live_roots(db: &StateDb, cutoff: i64) -> Result<Vec<ObjKey>> {
    let mut out = Vec::new();
    for b in blobs(db, "SELECT target FROM refs LIMIT ?", None).await? {
        match RefTarget::from_bytes(&b) {
            Ok(RefTarget::Changeset(c)) => out.push(ObjKey::Changeset(c)),
            Ok(RefTarget::Tag(t)) => out.push(ObjKey::Tag(t)),
            Err(_) => {
                return Err(fatal(
                    "a ref holds an unreadable target; nothing was removed",
                ));
            }
        }
    }
    let cs_sql = [
        "SELECT base_cs FROM workspaces WHERE base_cs IS NOT NULL LIMIT ?",
        "SELECT cs FROM cloud_queue LIMIT ?",
        "SELECT wc_snapshot FROM op_log WHERE wc_snapshot IS NOT NULL LIMIT ?",
        "SELECT cs FROM cloud_base LIMIT ?",
    ];
    for sql in cs_sql {
        for b in blobs(db, sql, None).await? {
            if let Some(id) = id32(b) {
                out.push(ObjKey::Changeset(ChangesetId::from_bytes(id)));
            }
        }
    }
    let retained = "SELECT cs FROM draft_heads WHERE hidden_at IS NULL OR hidden_at >= ? LIMIT ?";
    for b in blobs(db, retained, Some(cutoff)).await? {
        if let Some(id) = id32(b) {
            out.push(ObjKey::Changeset(ChangesetId::from_bytes(id)));
        }
    }
    for b in blobs(
        db,
        "SELECT file FROM stage WHERE file IS NOT NULL LIMIT ?",
        None,
    )
    .await?
    {
        if let Some(id) = id32(b) {
            out.push(ObjKey::File(FileId::from_bytes(id)));
            out.push(ObjKey::ChunkList(FileId::from_bytes(id)));
        }
    }
    for sql in [
        "SELECT from_root FROM switch_intent LIMIT ?",
        "SELECT to_root FROM switch_intent LIMIT ?",
    ] {
        for b in blobs(db, sql, None).await? {
            if let Some(id) = id32(b) {
                out.push(ObjKey::Tree(TreeId::from_bytes(id)));
            }
        }
    }
    Ok(out)
}

/// Hidden heads past the retention, oldest first: `(repo_id, cs)`.
async fn expired(db: &StateDb, cutoff: i64) -> Result<Vec<(i64, ChangesetId)>> {
    let rows: Vec<(i64, Vec<u8>)> = sqlx::query_as(
        "SELECT repo_id, cs FROM draft_heads WHERE hidden_at IS NOT NULL AND hidden_at < ? \
         ORDER BY hidden_at, cs LIMIT ?",
    )
    .bind(cutoff)
    .bind(MAX_EXPIRED)
    .fetch_all(db.pool())
    .await
    .map_err(db_err)?;
    Ok(rows
        .into_iter()
        .filter_map(|(r, b)| id32(b).map(|id| (r, ChangesetId::from_bytes(id))))
        .collect())
}

/// Delete order: a changeset goes before what it names.
fn rank(k: &ObjKey) -> u8 {
    match k {
        ObjKey::Changeset(_) => 0,
        ObjKey::Tag(_) => 1,
        ObjKey::Tree(_) => 2,
        ObjKey::ChunkList(_) => 3,
        ObjKey::File(_) => 4,
        _ => 5,
    }
}

/// Removes the hidden drafts past `hidden_days` (at `now`, Unix seconds). `dry_run` reports and
/// changes nothing.
pub async fn collect(ws: &WsCtx, hidden_days: u32, now: i64, dry_run: bool) -> Result<Report> {
    let cutoff = now.saturating_sub(i64::from(hidden_days).saturating_mul(DAY));
    let old = expired(&ws.db, cutoff).await?;
    let mut report = Report {
        expired_heads: old.len(),
        ..Report::default()
    };
    if old.is_empty() {
        return Ok(report);
    }
    let store = CacheDir::open(ws.home.store(), ws.home.tmp()).map_err(CliError::Objects)?;

    let mut dead = Walk::default();
    dead.extend(&store, old.iter().map(|(_, c)| ObjKey::Changeset(*c)))
        .await?;
    let mut live = Walk::default();
    live.extend(&store, live_roots(&ws.db, cutoff).await?)
        .await?;
    let mut doomed: Vec<ObjKey> = dead.present.difference(&live.present).copied().collect();
    report.collectable = doomed.len();
    if dry_run {
        return Ok(report);
    }

    // Second look at the roots, so a head or ref added during the walk keeps its objects.
    live.extend(&store, live_roots(&ws.db, cutoff).await?)
        .await?;
    doomed.retain(|k| !live.present.contains(k));
    doomed.sort_by_key(rank);
    for k in &doomed {
        store.delete(k).await.map_err(CliError::Objects)?;
        report.deleted += 1;
    }
    // The rows go last: a failed delete above leaves them, and the next run finishes the job.
    // The condition is re-checked, so a head unhidden meanwhile stays.
    let mut tx = ws.db.begin().await?;
    for (repo, cs) in &old {
        let done = sqlx::query(
            "DELETE FROM draft_heads WHERE repo_id = ? AND cs = ? AND hidden_at IS NOT NULL \
             AND hidden_at < ?",
        )
        .bind(*repo)
        .bind(cs.as_bytes().to_vec())
        .bind(cutoff)
        .execute(&mut *tx)
        .await
        .map_err(db_err)?;
        report.rows_dropped += done.rows_affected();
    }
    tx.commit().await.map_err(db_err)?;
    Ok(report)
}

/// The changesets of `ids` that no tier can supply (used by `asc undo` / `asc redo`: a head whose
/// objects expired cannot be restored).
pub async fn missing(objects: &dyn Objects, ids: &[ChangesetId]) -> Result<Vec<ChangesetId>> {
    let mut seen = HashMap::new();
    let mut keys = Vec::new();
    for id in ids {
        if seen.insert(*id, ()).is_none() {
            keys.push(ObjKey::Changeset(*id));
        }
    }
    let have = objects.has_many(&keys).await.map_err(CliError::Objects)?;
    Ok(keys
        .iter()
        .zip(have)
        .filter_map(|(k, ok)| match (k, ok) {
            (ObjKey::Changeset(c), false) => Some(*c),
            _ => None,
        })
        .collect())
}

/// Refuses (nothing changes) when a changeset an operation would restore has expired from
/// every tier: `asc undo` and `asc redo` call this with the heads, ref targets and snapshot of
/// the plan before they touch anything (SPEC §21.2.4).
pub async fn ensure_restorable(objects: &dyn Objects, ids: &[ChangesetId]) -> Result<()> {
    let gone = missing(objects, ids).await?;
    match gone.first() {
        None => Ok(()),
        Some(first) => Err(CliError::Fatal(format!(
            "objects expired: {first} was hidden for longer than asc.draft.hiddenDays and its              objects were removed ({} missing)",
            gone.len()
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn restore_of_an_expired_changeset_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let store = CacheDir::open(dir.path().join("store"), dir.path().join("tmp")).unwrap();
        let gone = ChangesetId::from_bytes([9; 32]);
        ensure_restorable(&store, &[]).await.unwrap();
        let err = ensure_restorable(&store, &[gone, gone]).await.unwrap_err();
        let CliError::Fatal(msg) = err else {
            panic!("wrong error kind")
        };
        assert!(msg.starts_with("objects expired:"), "{msg}");
        assert!(msg.contains("(1 missing)"), "{msg}");
    }
}
