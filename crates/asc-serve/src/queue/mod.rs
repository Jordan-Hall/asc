//! The merge queue (`mergequeue.proto`, docs/BUILDOUT.md section 7): entries, order, estimates, the
//! live event log and the fail-closed rules for moving an entry towards landing.
//!
//! * An entry is a stack prefix (bottom to top) of one target. Entries land in id order.
//! * **Fail closed.** `Revalidate` decides READY: every change of the entry has a current approval
//!   (one that applies to its newest version, carried ones included) and none has changes
//!   requested. A check this server cannot decide leaves the entry WAITING. `ReportEntry` moves
//!   an entry to LANDING only from READY; LANDED only from LANDING or READY, with the landed
//!   changeset. The host that lands is told so and cannot skip the check through this service.
//! * **Estimates.** `median_land_seconds` is the median time between LANDING and LANDED of the last
//!   20 landed entries of the target (180 without history); `starts_in = (position - 1) * median`,
//!   `eta = position * median`, and for the entry that is landing the median minus what has passed.
//! * **Events.** Every change of an entry appends a row to `queue_events`; `Watch` streams the
//!   entries those rows name, from a cursor, and ends after ten minutes (the client resumes).

pub mod rpc;

#[cfg(test)]
mod tests;

use asc_core::change::ChangeId;
use asc_rpc::server::{CallCtx, wire};
use bytes::Bytes;
use sqlx::Row;
use tonic::Status;

use crate::registry::RepoEntry;
use crate::review::acting;
use crate::review::{
    Reviews, WriteError, after_token, change_of, next_id, now, page_limit, parse_after,
    retry_conflict, sql, store,
};

/// What `Version.features` adds for the queue.
pub const FEATURE: &str = "mergequeue.v1";
/// Changes one entry holds.
pub const MAX_STACK: usize = 256;
/// Median landing time without history.
pub const DEFAULT_LAND_SECONDS: u32 = 180;
const MAX_DETAIL: usize = 200;
const MAX_REASON: usize = 256;

pub(crate) const WAITING: i64 = wire::QueueEntryState::Waiting as i64;
pub(crate) const REVALIDATING: i64 = wire::QueueEntryState::Revalidating as i64;
pub(crate) const READY: i64 = wire::QueueEntryState::Ready as i64;
pub(crate) const LANDING: i64 = wire::QueueEntryState::Landing as i64;
pub(crate) const LANDED: i64 = wire::QueueEntryState::Landed as i64;
pub(crate) const BLOCKED: i64 = wire::QueueEntryState::Blocked as i64;
pub(crate) const REMOVED: i64 = wire::QueueEntryState::Removed as i64;

/// Frees the changes an entry held, so they can be queued again.
async fn release_members(tx: &mut sqlx::AnyConnection, ns: &[u8], id: i64) -> Result<(), Status> {
    sqlx::query("DELETE FROM queue_members WHERE ns = $1 AND entry_id = $2")
        .bind(ns.to_vec())
        .bind(id)
        .execute(&mut *tx)
        .await
        .map_err(sql)?;
    Ok(())
}

fn live(state: i64) -> bool {
    matches!(state, WAITING | REVALIDATING | READY | LANDING)
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn unhex(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    (0..s.len() / 2)
        .map(|i| u8::from_str_radix(s.get(2 * i..2 * i + 2)?, 16).ok())
        .collect()
}

fn queue_error(code: &str, message: &str) -> String {
    format!("{code}: {message}")
}

/// A stored entry.
#[derive(Clone, Debug)]
pub(crate) struct Entry {
    pub id: i64,
    pub target: String,
    pub change: Vec<u8>,
    pub stack: Vec<Vec<u8>>,
    pub state: i64,
    pub style: i64,
    pub enqueued_by: String,
    pub queued_by_id: i64,
    pub enqueued_at: i64,
    pub reval: Option<(bool, Vec<i32>, i64)>,
    pub started_at: i64,
    pub landed_as: Vec<u8>,
    pub landed_at: i64,
    pub removed_reason: String,
    pub stage: String,
    pub progress: i64,
    pub detail: String,
}

const COLS: &str = "id, target, change, stack_json, state, style, enqueued_by, queued_by_id, enqueued_at, reval_json, reval_at, started_at, \
     landed_as, landed_at, removed_reason, stage, progress, detail";

fn row_of(r: &sqlx::any::AnyRow) -> Result<Entry, Status> {
    let stack: Vec<String> =
        serde_json::from_str(&r.try_get::<String, _>("stack_json").map_err(sql)?)
            .unwrap_or_default();
    let reval_json: String = r.try_get("reval_json").map_err(sql)?;
    let reval_at: i64 = r.try_get("reval_at").map_err(sql)?;
    let reval = serde_json::from_str::<(bool, Vec<i32>)>(&reval_json)
        .ok()
        .map(|(p, v)| (p, v, reval_at));
    Ok(Entry {
        id: r.try_get("id").map_err(sql)?,
        target: r.try_get("target").map_err(sql)?,
        change: r.try_get("change").map_err(sql)?,
        stack: stack.iter().filter_map(|s| unhex(s)).collect(),
        state: r.try_get("state").map_err(sql)?,
        style: r.try_get("style").map_err(sql)?,
        enqueued_by: r.try_get("enqueued_by").map_err(sql)?,
        queued_by_id: r.try_get("queued_by_id").map_err(sql)?,
        enqueued_at: r.try_get("enqueued_at").map_err(sql)?,
        reval,
        started_at: r.try_get("started_at").map_err(sql)?,
        landed_as: r.try_get("landed_as").map_err(sql)?,
        landed_at: r.try_get("landed_at").map_err(sql)?,
        removed_reason: r.try_get("removed_reason").map_err(sql)?,
        stage: r.try_get("stage").map_err(sql)?,
        progress: r.try_get("progress").map_err(sql)?,
        detail: r.try_get("detail").map_err(sql)?,
    })
}

impl Reviews {
    async fn q_target(&self, ctx: &CallCtx, target: &str) -> Result<String, Status> {
        if target.is_empty() {
            return Ok(self.open(ctx).await?.default_branch.clone());
        }
        if target.len() > 255
            || asc_core::path::RefName::new(format!("refs/heads/{target}").as_bytes()).is_err()
        {
            return Err(Status::invalid_argument("the target is not a branch name"));
        }
        Ok(target.to_owned())
    }

    /// The changes an enqueue of `change` adds, bottom to top: down its parent chain to the first
    /// change that is landed or has no parent.
    async fn q_stack(
        &self,
        entry: &crate::registry::RepoEntry,
        change: ChangeId,
    ) -> Result<Vec<ChangeId>, Status> {
        let mut conn = self.registry.pool().acquire().await.map_err(sql)?;
        let mut out = vec![change];
        let mut cur = change;
        while out.len() < MAX_STACK {
            let landed: Option<i64> = sqlx::query_scalar(
                "SELECT 1 FROM changes c JOIN mainline m ON m.ns = c.ns AND m.cs = c.cs WHERE c.ns = $1 AND c.change = $2 LIMIT 1",
            )
            .bind(entry.ns.as_bytes().to_vec())
            .bind(cur.as_bytes().to_vec())
            .fetch_optional(&mut *conn)
            .await
            .map_err(sql)?;
            if landed.is_some() {
                out.pop();
                break;
            }
            let v = crate::changes::versions::latest(&mut conn, entry.ns, cur)
                .await
                .map_err(|e| crate::review::internal("versions", &e))?;
            let Some(parent) = v.and_then(|v| v.parent_change) else {
                break;
            };
            out.push(parent);
            cur = parent;
        }
        out.reverse();
        Ok(out)
    }

    /// Why the stack is not ready: the reasons, and whether a change has changes requested.
    async fn q_reasons(
        &self,
        _ctx: &CallCtx,
        entry: &crate::registry::RepoEntry,
        stack: &[ChangeId],
    ) -> Result<(Vec<i32>, bool), Status> {
        let mut reasons: Vec<i32> = Vec::new();
        let mut blocked = false;
        for c in stack {
            let approvals = self.q_approvals(entry, *c).await?;
            let current = approvals
                .iter()
                .filter(|a| a.applies && a.state == wire::ApprovalState::Approved as i32)
                .count();
            let asked = approvals
                .iter()
                .any(|a| a.applies && a.state == wire::ApprovalState::ChangesRequested as i32);
            if current == 0
                && !reasons.contains(&(wire::RevalidationReason::ApprovalsMissing as i32))
            {
                reasons.push(wire::RevalidationReason::ApprovalsMissing as i32);
            }
            if asked {
                blocked = true;
                if !reasons.contains(&(wire::RevalidationReason::PolicyDenied as i32)) {
                    reasons.push(wire::RevalidationReason::PolicyDenied as i32);
                }
            }
        }
        Ok((reasons, blocked))
    }

    async fn q_approvals(
        &self,
        entry: &crate::registry::RepoEntry,
        change: ChangeId,
    ) -> Result<Vec<wire::ApprovalInfo>, Status> {
        self.approvals_for(entry, change).await
    }

    async fn q_positions(&self, ns: &[u8], target: &str) -> Result<Vec<i64>, Status> {
        sqlx::query_scalar::<_, i64>("SELECT id FROM queue_entries WHERE ns = $1 AND target = $2 AND state IN (1, 2, 3, 4) ORDER BY id")
            .bind(ns.to_vec())
            .bind(target.to_owned())
            .fetch_all(self.registry.pool())
            .await
            .map_err(sql)
    }

    /// The median of LANDING to LANDED, in seconds, of the last 20 landed entries of the target.
    async fn q_median(&self, ns: &[u8], target: &str) -> Result<u32, Status> {
        let rows = sqlx::query(
            "SELECT started_at, landed_at FROM queue_entries WHERE ns = $1 AND target = $2 AND state = 5 AND started_at > 0 \
             ORDER BY id DESC LIMIT 20",
        )
        .bind(ns.to_vec())
        .bind(target.to_owned())
        .fetch_all(self.registry.pool())
        .await
        .map_err(sql)?;
        let mut v: Vec<i64> = Vec::new();
        for r in &rows {
            let (s, l): (i64, i64) = (
                r.try_get("started_at").map_err(sql)?,
                r.try_get("landed_at").map_err(sql)?,
            );
            v.push((l - s).max(1));
        }
        v.sort_unstable();
        Ok(v.get(v.len() / 2).map_or(DEFAULT_LAND_SECONDS, |s| {
            u32::try_from(*s).unwrap_or(u32::MAX)
        }))
    }

    fn q_timing(row: &Entry, position: u32, median: u32) -> wire::QueueEntryTiming {
        let (eta, starts) = if !live(row.state) {
            (0, 0)
        } else if row.state == LANDING {
            let elapsed = u32::try_from((now() - row.started_at).max(0)).unwrap_or(u32::MAX);
            (median.saturating_sub(elapsed), 0)
        } else {
            (
                position.saturating_mul(median),
                position.saturating_sub(1).saturating_mul(median),
            )
        };
        let stage = if !row.stage.is_empty() {
            row.stage.clone()
        } else {
            match row.state {
                WAITING => "waiting",
                REVALIDATING | READY => "checking",
                LANDING => "landing",
                _ => "done",
            }
            .to_owned()
        };
        wire::QueueEntryTiming {
            eta_seconds: eta,
            starts_in_seconds: starts,
            stage,
            progress_percent: u32::try_from(row.progress).unwrap_or(0).min(100),
            detail: row.detail.clone(),
            median_land_seconds: median,
        }
    }

    pub(crate) fn q_wire(
        row: &Entry,
        position: u32,
        timing: wire::QueueEntryTiming,
    ) -> wire::QueueEntry {
        wire::QueueEntry {
            id: u64::try_from(row.id).unwrap_or(0),
            target: row.target.clone(),
            change: Bytes::copy_from_slice(&row.change),
            stack: row
                .stack
                .iter()
                .map(|c| Bytes::copy_from_slice(c))
                .collect(),
            state: i32::try_from(row.state).unwrap_or(0),
            style: i32::try_from(row.style).unwrap_or(0),
            position: if live(row.state) { position } else { 0 },
            enqueued_by: row.enqueued_by.clone(),
            queued_by_id: u64::try_from(row.queued_by_id).unwrap_or(0),
            enqueued_at: row.enqueued_at,
            revalidation: row
                .reval
                .as_ref()
                .map(|(passed, reasons, at)| wire::QueueRevalidation {
                    passed: *passed,
                    reasons: reasons.clone(),
                    checked_at: *at,
                    detail: row.detail.clone(),
                    ..Default::default()
                }),
            landed_as: Bytes::copy_from_slice(&row.landed_as),
            landed_at: row.landed_at,
            removed_reason: row.removed_reason.clone(),
            timing: Some(timing),
        }
    }

    async fn q_get(&self, ns: &[u8], id: i64) -> Result<Entry, Status> {
        let q = format!("SELECT {COLS} FROM queue_entries WHERE ns = $1 AND id = $2");
        let r = sqlx::query(sqlx::AssertSqlSafe(q))
            .bind(ns.to_vec())
            .bind(id)
            .fetch_optional(self.registry.pool())
            .await
            .map_err(sql)?
            .ok_or_else(|| Status::not_found("no such queue entry"))?;
        row_of(&r)
    }

    /// An entry as the wire shows it, with its position and estimate.
    pub(crate) async fn q_entry(&self, ns: &[u8], row: &Entry) -> Result<wire::QueueEntry, Status> {
        let pos = self.q_positions(ns, &row.target).await?;
        let position = pos
            .iter()
            .position(|i| *i == row.id)
            .map_or(0, |p| u32::try_from(p + 1).unwrap_or(u32::MAX));
        let median = self.q_median(ns, &row.target).await?;
        Ok(Self::q_wire(
            row,
            position,
            Self::q_timing(row, position, median),
        ))
    }

    async fn q_event(
        &self,
        tx: &mut sqlx::AnyConnection,
        ns: &[u8],
        id: i64,
    ) -> Result<(), Status> {
        let seq: i64 = sqlx::query_scalar::<_, i64>(
            "SELECT CAST(COALESCE(MAX(seq), 0) AS BIGINT) FROM queue_events WHERE ns = $1",
        )
        .bind(ns.to_vec())
        .fetch_one(&mut *tx)
        .await
        .map_err(sql)?;
        sqlx::query("INSERT INTO queue_events (ns, seq, entry_id, at) VALUES ($1, $2, $3, $4)")
            .bind(ns.to_vec())
            .bind(seq + 1)
            .bind(id)
            .bind(now())
            .execute(&mut *tx)
            .await
            .map_err(sql)?;
        Ok(())
    }

    async fn q_paused(&self, ns: &[u8], target: &str) -> Result<(bool, String), Status> {
        let r = sqlx::query("SELECT paused, reason FROM queue_pause WHERE ns = $1 AND target = $2")
            .bind(ns.to_vec())
            .bind(target.to_owned())
            .fetch_optional(self.registry.pool())
            .await
            .map_err(sql)?;
        Ok(match r {
            Some(r) => (
                r.try_get::<i64, _>("paused").map_err(sql)? != 0,
                r.try_get("reason").map_err(sql)?,
            ),
            None => (false, String::new()),
        })
    }

    /// `MergeQueue.Preview`.
    pub async fn q_preview(
        &self,
        ctx: &CallCtx,
        req: wire::PreviewEnqueueRequest,
    ) -> Result<wire::PreviewEnqueueResponse, Status> {
        let change = change_of(&req.change)?;
        let target = self.q_target(ctx, &req.target).await?;
        let entry = self.open(ctx).await?;
        self.q_known(&entry, change).await?;
        let ns = entry.ns.as_bytes().to_vec();
        let stack = self.q_stack(&entry, change).await?;
        let (reasons, _) = self.q_reasons(ctx, &entry, &stack).await?;
        let pos = self.q_positions(&ns, &target).await?;
        let queued: Vec<Bytes> = self.q_queued_among(&ns, &target, &stack).await?;
        let median = self.q_median(&ns, &target).await?;
        let position = u32::try_from(pos.len() + 1).unwrap_or(u32::MAX);
        let (paused, why) = self.q_paused(&ns, &target).await?;
        let fake = Entry {
            id: 0,
            target: target.clone(),
            change: change.as_bytes().to_vec(),
            stack: vec![],
            state: WAITING,
            style: 0,
            enqueued_by: String::new(),
            queued_by_id: 0,
            enqueued_at: 0,
            reval: None,
            started_at: 0,
            landed_as: vec![],
            landed_at: 0,
            removed_reason: String::new(),
            stage: String::new(),
            progress: 0,
            detail: String::new(),
        };
        Ok(wire::PreviewEnqueueResponse {
            preview: Some(wire::QueuePreview {
                stack: stack
                    .iter()
                    .filter(|c| !queued.iter().any(|q| q.as_ref() == c.as_bytes()))
                    .map(|c| Bytes::copy_from_slice(c.as_bytes()))
                    .collect(),
                already_queued: queued,
                position,
                timing: Some(Self::q_timing(&fake, position, median)),
                paused,
                paused_reason: why,
                would_wait_for: reasons,
            }),
        })
    }

    async fn q_known(
        &self,
        entry: &crate::registry::RepoEntry,
        change: ChangeId,
    ) -> Result<(), Status> {
        let mut conn = self.registry.pool().acquire().await.map_err(sql)?;
        if store::change_known(&mut conn, entry.ns, change).await? {
            Ok(())
        } else {
            Err(Status::not_found("unknown change"))
        }
    }

    /// The changes of `stack` that a live entry of `target` already holds.
    async fn q_queued_among(
        &self,
        ns: &[u8],
        target: &str,
        stack: &[ChangeId],
    ) -> Result<Vec<Bytes>, Status> {
        let rows = sqlx::query("SELECT stack_json FROM queue_entries WHERE ns = $1 AND target = $2 AND state IN (1, 2, 3, 4)")
            .bind(ns.to_vec())
            .bind(target.to_owned())
            .fetch_all(self.registry.pool())
            .await
            .map_err(sql)?;
        let mut held = std::collections::HashSet::new();
        for r in &rows {
            let s: Vec<String> =
                serde_json::from_str(&r.try_get::<String, _>("stack_json").map_err(sql)?)
                    .unwrap_or_default();
            held.extend(s);
        }
        Ok(stack
            .iter()
            .filter(|c| held.contains(&hex(c.as_bytes())))
            .map(|c| Bytes::copy_from_slice(c.as_bytes()))
            .collect())
    }

    /// `MergeQueue.Enqueue`. `by` is the verified person.
    pub async fn q_enqueue(
        &self,
        ctx: &CallCtx,
        by: &str,
        req: wire::EnqueueRequest,
    ) -> Result<wire::EnqueueResponse, Status> {
        let change = change_of(&req.change)?;
        let target = self.q_target(ctx, &req.target).await?;
        let entry = self.open(ctx).await?;
        self.q_known(&entry, change).await?;
        let ns = entry.ns.as_bytes().to_vec();
        let queued_by_id = crate::review::user_id_col(&ctx.principal.id);
        let (paused, why) = self.q_paused(&ns, &target).await?;
        if paused {
            return Err(Status::failed_precondition(queue_error(
                "QUEUE_PAUSED",
                &why,
            )));
        }
        let stack = self.q_stack(&entry, change).await?;
        if stack.is_empty() {
            return Err(Status::failed_precondition("the change has landed already"));
        }
        if !self.q_queued_among(&ns, &target, &stack).await?.is_empty() {
            return Err(Status::already_exists(queue_error(
                "QUEUE_ENTRY_EXISTS",
                "a change of this stack is queued already",
            )));
        }
        let (reasons, blocked) = self.q_reasons(ctx, &entry, &stack).await?;
        let state = if blocked {
            BLOCKED
        } else if reasons.is_empty() {
            READY
        } else {
            WAITING
        };
        let reval = serde_json::to_string(&(reasons.is_empty(), &reasons)).unwrap_or_default();
        let stack_json =
            serde_json::to_string(&stack.iter().map(|c| hex(c.as_bytes())).collect::<Vec<_>>())
                .unwrap_or_default();
        let id = retry_conflict(|| async {
            let mut tx = self.registry.pool().begin().await?;
            self.registry.guard(&mut tx, &entry).await?;
            let id = next_id(&mut tx, "queue_entries", &ns).await?;
            // The members first: the key refuses a change that another entry holds, even when two
            // enqueues race (the loser retries, finds the member and is told so).
            for c in &stack {
                let held: Option<i64> = sqlx::query_scalar(
                    "SELECT entry_id FROM queue_members WHERE ns = $1 AND target = $2 AND change = $3",
                )
                .bind(ns.clone())
                .bind(target.clone())
                .bind(c.as_bytes().to_vec())
                .fetch_optional(&mut *tx)
                .await?;
                if held.is_some() {
                    return Err(WriteError::Status(Status::already_exists(queue_error(
                        "QUEUE_ENTRY_EXISTS",
                        "a change of this stack is queued already",
                    ))));
                }
                sqlx::query("INSERT INTO queue_members (ns, target, change, entry_id) VALUES ($1, $2, $3, $4)")
                    .bind(ns.clone())
                    .bind(target.clone())
                    .bind(c.as_bytes().to_vec())
                    .bind(id)
                    .execute(&mut *tx)
                    .await?;
            }
            sqlx::query(
                "INSERT INTO queue_entries (ns, id, target, change, stack_json, state, style, enqueued_by, enqueued_at, reval_json, reval_at, landed_as, queued_by_id) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $9, $11, $12)",
            )
            .bind(ns.clone())
            .bind(id)
            .bind(target.clone())
            .bind(change.as_bytes().to_vec())
            .bind(stack_json.clone())
            .bind(state)
            .bind(i64::from(req.style))
            .bind(by.to_owned())
            .bind(now())
            .bind(reval.clone())
            .bind(Vec::<u8>::new())
            .bind(queued_by_id)
            .execute(&mut *tx)
            .await?;
            self.q_event(&mut tx, &ns, id).await.map_err(WriteError::Status)?;
            tx.commit().await?;
            Ok(id)
        })
        .await?;
        acting::record(
            self.registry.pool(),
            acting::event(ctx, "queue.enqueued", &id.to_string())
                .detail("change", hex(change.as_bytes()))
                .detail("target", target),
        )
        .await;
        let row = self.q_get(&ns, id).await?;
        Ok(wire::EnqueueResponse {
            entry: Some(self.q_entry(&ns, &row).await?),
        })
    }

    /// `MergeQueue.Dequeue`: the owner or an administrator (`admin`).
    pub async fn q_dequeue(
        &self,
        ctx: &CallCtx,
        by: &str,
        admin: bool,
        req: wire::DequeueRequest,
    ) -> Result<wire::DequeueResponse, Status> {
        if req.reason.len() > MAX_REASON {
            return Err(Status::invalid_argument("a reason is at most 256 bytes"));
        }
        let id = i64::try_from(req.id)
            .ok()
            .filter(|n| *n > 0)
            .ok_or_else(|| Status::invalid_argument("invalid id"))?;
        let entry = self.open(ctx).await?;
        let ns = entry.ns.as_bytes().to_vec();
        let row = self.q_get(&ns, id).await?;
        if !admin && row.enqueued_by != by {
            return Err(Status::permission_denied(
                "only the person who queued the entry or an administrator can remove it",
            ));
        }
        if live(row.state) {
            if row.state == LANDING {
                return Err(Status::failed_precondition("the entry is landing"));
            }
            let mut tx = self.registry.pool().begin().await.map_err(sql)?;
            let n = sqlx::query("UPDATE queue_entries SET state = $3, removed_reason = $4 WHERE ns = $1 AND id = $2 AND state IN (1, 2, 3)")
                .bind(ns.clone())
                .bind(id)
                .bind(REMOVED)
                .bind(req.reason.clone())
                .execute(&mut *tx)
                .await
                .map_err(sql)?
                .rows_affected();
            if n == 0 {
                return Err(Status::failed_precondition(
                    "the entry is landing or finished",
                ));
            }
            release_members(&mut tx, &ns, id).await?;
            self.q_event(&mut tx, &ns, id).await?;
            tx.commit().await.map_err(sql)?;
            acting::record(
                self.registry.pool(),
                acting::event(ctx, "queue.dequeued", &id.to_string())
                    .detail("reason", req.reason.clone()),
            )
            .await;
            if row.enqueued_by != by {
                let after = self.q_get(&ns, id).await?;
                self.tell_queue(
                    &entry,
                    &after,
                    "was dropped",
                    &req.reason,
                    &format!("{id}:7"),
                )
                .await;
            }
        }
        let row = self.q_get(&ns, id).await?;
        Ok(wire::DequeueResponse {
            entry: Some(self.q_entry(&ns, &row).await?),
        })
    }

    /// `MergeQueue.List`.
    pub async fn q_list(
        &self,
        ctx: &CallCtx,
        req: wire::ListQueueRequest,
    ) -> Result<wire::ListQueueResponse, Status> {
        let target = self.q_target(ctx, &req.target).await?;
        let entry = self.open(ctx).await?;
        let ns = entry.ns.as_bytes().to_vec();
        let limit = page_limit(req.limit, 50, 200);
        if req.states.len() > 8 {
            return Err(Status::invalid_argument("at most 8 states"));
        }
        let mut states: Vec<i64> = Vec::new();
        for s in &req.states {
            let st = wire::QueueEntryState::try_from(*s)
                .map_err(|_| Status::invalid_argument("unknown state"))?;
            if st != wire::QueueEntryState::Unspecified {
                states.push(st as i64);
            }
        }
        let newest_first = req.include_finished || states.iter().any(|s| !live(*s));
        let scope = format!("MergeQueue.List|{}|{target}|{newest_first}", ctx.repo);
        let cursor = parse_after(&scope, &ctx.principal.id, &req.page_token)?;
        let mut q =
            "SELECT ".to_owned() + COLS + " FROM queue_entries WHERE ns = $1 AND target = $2";
        if states.is_empty() {
            if !req.include_finished {
                q.push_str(" AND state IN (1, 2, 3, 4)");
            }
        } else {
            q.push_str(&format!(
                " AND state IN ({})",
                states
                    .iter()
                    .map(i64::to_string)
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        if cursor > 0 {
            q.push_str(if newest_first {
                " AND id < $3"
            } else {
                " AND id > $3"
            });
        }
        q.push_str(&format!(
            " ORDER BY id {} LIMIT {}",
            if newest_first { "DESC" } else { "ASC" },
            limit + 1
        ));
        let mut query = sqlx::query(sqlx::AssertSqlSafe(q))
            .bind(ns.clone())
            .bind(target.clone());
        if cursor > 0 {
            query = query.bind(cursor);
        }
        let rows = query.fetch_all(self.registry.pool()).await.map_err(sql)?;
        let mut rows = rows.iter().map(row_of).collect::<Result<Vec<_>, _>>()?;
        let next = if rows.len() > limit {
            rows.truncate(limit);
            rows.last()
                .map(|r| after_token(&scope, &ctx.principal.id, r.id))
                .unwrap_or_default()
        } else {
            String::new()
        };
        let pos = self.q_positions(&ns, &target).await?;
        let median = self.q_median(&ns, &target).await?;
        let entries = rows
            .iter()
            .map(|r| {
                let p = pos
                    .iter()
                    .position(|i| *i == r.id)
                    .map_or(0, |p| u32::try_from(p + 1).unwrap_or(u32::MAX));
                Self::q_wire(r, p, Self::q_timing(r, p, median))
            })
            .collect();
        let (paused, why) = self.q_paused(&ns, &target).await?;
        Ok(wire::ListQueueResponse {
            entries,
            next_page_token: next,
            paused,
            paused_reason: why,
            median_land_seconds: median,
        })
    }

    /// `MergeQueue.Get`.
    pub async fn q_get_entry(
        &self,
        ctx: &CallCtx,
        req: wire::GetQueueEntryRequest,
    ) -> Result<wire::GetQueueEntryResponse, Status> {
        let id = i64::try_from(req.id)
            .ok()
            .filter(|n| *n > 0)
            .ok_or_else(|| Status::invalid_argument("invalid id"))?;
        let entry = self.open(ctx).await?;
        let ns = entry.ns.as_bytes().to_vec();
        let row = self.q_get(&ns, id).await?;
        Ok(wire::GetQueueEntryResponse {
            entry: Some(self.q_entry(&ns, &row).await?),
        })
    }

    /// `MergeQueue.Revalidate`.
    pub async fn q_revalidate(
        &self,
        ctx: &CallCtx,
        req: wire::RevalidateRequest,
    ) -> Result<wire::RevalidateResponse, Status> {
        let id = i64::try_from(req.id)
            .ok()
            .filter(|n| *n > 0)
            .ok_or_else(|| Status::invalid_argument("invalid id"))?;
        let entry = self.open(ctx).await?;
        let ns = entry.ns.as_bytes().to_vec();
        let row = self.q_get(&ns, id).await?;
        if matches!(row.state, LANDING | LANDED | REMOVED) {
            return Ok(wire::RevalidateResponse {
                entry: Some(self.q_entry(&ns, &row).await?),
            });
        }
        let stack: Vec<ChangeId> = row.stack.iter().filter_map(|c| change_of(c).ok()).collect();
        let (reasons, blocked) = self.q_reasons(ctx, &entry, &stack).await?;
        let state = if blocked {
            BLOCKED
        } else if reasons.is_empty() {
            READY
        } else {
            WAITING
        };
        let reval = serde_json::to_string(&(reasons.is_empty(), &reasons)).unwrap_or_default();
        let mut tx = self.registry.pool().begin().await.map_err(sql)?;
        sqlx::query("UPDATE queue_entries SET state = $3, reval_json = $4, reval_at = $5 WHERE ns = $1 AND id = $2 AND state IN (1, 2, 3, 6)")
            .bind(ns.clone())
            .bind(id)
            .bind(state)
            .bind(reval)
            .bind(now())
            .execute(&mut *tx)
            .await
            .map_err(sql)?;
        if state != row.state {
            self.q_event(&mut tx, &ns, id).await?;
        }
        tx.commit().await.map_err(sql)?;
        let row = self.q_get(&ns, id).await?;
        Ok(wire::RevalidateResponse {
            entry: Some(self.q_entry(&ns, &row).await?),
        })
    }

    /// `MergeQueue.SetPaused`.
    pub async fn q_set_paused(
        &self,
        ctx: &CallCtx,
        by: &str,
        req: wire::SetQueuePausedRequest,
    ) -> Result<wire::SetQueuePausedResponse, Status> {
        if req.reason.len() > MAX_REASON {
            return Err(Status::invalid_argument("a reason is at most 256 bytes"));
        }
        let target = self.q_target(ctx, &req.target).await?;
        let entry = self.open(ctx).await?;
        let ns = entry.ns.as_bytes().to_vec();
        sqlx::query("DELETE FROM queue_pause WHERE ns = $1 AND target = $2")
            .bind(ns.clone())
            .bind(target.clone())
            .execute(self.registry.pool())
            .await
            .map_err(sql)?;
        sqlx::query("INSERT INTO queue_pause (ns, target, paused, reason, by, at) VALUES ($1, $2, $3, $4, $5, $6)")
            .bind(ns)
            .bind(target.clone())
            .bind(i64::from(req.paused))
            .bind(req.reason.clone())
            .bind(by.to_owned())
            .bind(now())
            .execute(self.registry.pool())
            .await
            .map_err(sql)?;
        acting::record(
            self.registry.pool(),
            acting::event(ctx, "queue.paused", &target)
                .detail("paused", req.paused.to_string())
                .detail("reason", req.reason),
        )
        .await;
        Ok(wire::SetQueuePausedResponse { paused: req.paused })
    }

    /// `MergeQueue.ReportEntry`: the host that lands reports. Fail closed: LANDING only from READY.
    pub async fn q_report(
        &self,
        ctx: &CallCtx,
        req: wire::ReportQueueEntryRequest,
    ) -> Result<wire::ReportQueueEntryResponse, Status> {
        let id = i64::try_from(req.id)
            .ok()
            .filter(|n| *n > 0)
            .ok_or_else(|| Status::invalid_argument("invalid id"))?;
        if req.detail.len() > MAX_DETAIL
            || req.stage.len() > 40
            || req.progress_percent > 100
            || req.detail.contains('\0')
        {
            return Err(Status::invalid_argument(
                "a detail is at most 200 bytes, a stage 40, progress 0 to 100",
            ));
        }
        let to = wire::QueueEntryState::try_from(req.state)
            .map_err(|_| Status::invalid_argument("unknown state"))? as i64;
        if !matches!(to, LANDING | LANDED | BLOCKED | REMOVED) {
            return Err(Status::invalid_argument(
                "the host reports LANDING, LANDED, BLOCKED or REMOVED",
            ));
        }
        let entry = self.open(ctx).await?;
        let ns = entry.ns.as_bytes().to_vec();
        let row = self.q_get(&ns, id).await?;
        let ok = match to {
            LANDING => matches!(row.state, READY | LANDING),
            LANDED => matches!(row.state, READY | LANDING) && req.landed_as.len() == 32,
            BLOCKED => live(row.state),
            _ => live(row.state),
        };
        if !ok {
            return Err(Status::failed_precondition(queue_error(
                "REVALIDATION_FAILED",
                "an entry lands only from READY, with the landed changeset for LANDED",
            )));
        }
        // Landing is decided again here, at the moment of landing: approvals may have changed since the
        // entry became READY. Fail closed: an entry that no longer passes goes back and is refused.
        if to == LANDING && row.state == READY {
            let stack: Vec<ChangeId> = row.stack.iter().filter_map(|c| change_of(c).ok()).collect();
            let (reasons, blocked) = self.q_reasons(ctx, &entry, &stack).await?;
            if !reasons.is_empty() || blocked {
                let back = if blocked { BLOCKED } else { WAITING };
                let reval = serde_json::to_string(&(false, &reasons)).unwrap_or_default();
                let mut tx = self.registry.pool().begin().await.map_err(sql)?;
                sqlx::query("UPDATE queue_entries SET state = $3, reval_json = $4, reval_at = $5 WHERE ns = $1 AND id = $2 AND state = 3")
                    .bind(ns.clone())
                    .bind(id)
                    .bind(back)
                    .bind(reval)
                    .bind(now())
                    .execute(&mut *tx)
                    .await
                    .map_err(sql)?;
                self.q_event(&mut tx, &ns, id).await?;
                tx.commit().await.map_err(sql)?;
                return Err(Status::failed_precondition(queue_error(
                    "REVALIDATION_FAILED",
                    "the entry no longer passes its checks; it went back",
                )));
            }
        }
        let at = now();
        let mut tx = self.registry.pool().begin().await.map_err(sql)?;
        // Only from the states the move is allowed from, decided by the database, not by the read above.
        let from = match to {
            LANDING | LANDED => "3, 4",
            _ => "1, 2, 3, 4, 6",
        };
        let q = format!(
            "UPDATE queue_entries SET state = $3, stage = $4, progress = $5, detail = $6, \
             started_at = CASE WHEN $3 = 4 AND started_at = 0 THEN $7 ELSE started_at END, \
             landed_at = CASE WHEN $3 = 5 THEN $7 ELSE landed_at END, landed_as = CASE WHEN $3 = 5 THEN $8 ELSE landed_as END, \
             removed_reason = CASE WHEN $3 IN (6, 7) THEN $6 ELSE removed_reason END \
             WHERE ns = $1 AND id = $2 AND state IN ({from})"
        );
        let done = sqlx::query(sqlx::AssertSqlSafe(q))
            .bind(ns.clone())
            .bind(id)
            .bind(to)
            .bind(req.stage.clone())
            .bind(i64::from(req.progress_percent))
            .bind(req.detail.clone())
            .bind(at)
            .bind(req.landed_as.to_vec())
            .execute(&mut *tx)
            .await
            .map_err(sql)?
            .rows_affected();
        if done == 0 {
            return Err(Status::failed_precondition(queue_error(
                "REVALIDATION_FAILED",
                "the entry changed while the report was made; read it again",
            )));
        }
        if matches!(to, LANDED | REMOVED) {
            release_members(&mut tx, &ns, id).await?;
        }
        self.q_event(&mut tx, &ns, id).await?;
        tx.commit().await.map_err(sql)?;
        acting::record(
            self.registry.pool(),
            acting::event(ctx, "queue.entry.reported", &id.to_string())
                .detail("state", to.to_string())
                .detail("detail", req.detail.clone()),
        )
        .await;
        let row = self.q_get(&ns, id).await?;
        let how = match to {
            LANDED => Some("landed"),
            BLOCKED => Some("is blocked"),
            REMOVED => Some("was dropped"),
            _ => None,
        };
        if let Some(how) = how {
            self.tell_queue(&entry, &row, how, &req.detail, &format!("{id}:{to}"))
                .await;
        }
        let e = self.q_entry(&ns, &row).await?;
        Ok(wire::ReportQueueEntryResponse {
            timing: e.timing.clone(),
            entry: Some(e),
        })
    }

    /// The hub's `merge_queue`: the person who queued the entry is told when it lands, is blocked
    /// or is dropped. `key` makes a repeated report tell once.
    async fn tell_queue(&self, entry: &RepoEntry, row: &Entry, how: &str, why: &str, key: &str) {
        let repo = entry.name.as_str();
        crate::notify::emit(
            self.registry.pool(),
            repo,
            crate::notify::Event::new(
                crate::notify::Kind::MergeQueue,
                format!("Your change in the merge queue for {} {how}", row.target),
            )
            .users([row.enqueued_by.as_str()])
            .detail(why)
            .path(crate::notify::paths::queue(repo))
            .dedup(format!("queue:{key}")),
        )
        .await;
    }

    /// The entries named by events after `cursor`, oldest first, and the new cursor.
    pub(crate) async fn q_events_after(
        &self,
        ns: &[u8],
        target: &str,
        cursor: i64,
        limit: i64,
    ) -> Result<(Vec<wire::QueueEvent>, i64), Status> {
        let rows = sqlx::query(
            "SELECT e.seq AS seq, e.entry_id AS entry_id FROM queue_events e JOIN queue_entries q ON q.ns = e.ns AND q.id = e.entry_id \
             WHERE e.ns = $1 AND e.seq > $2 AND q.target = $3 ORDER BY e.seq LIMIT $4",
        )
        .bind(ns.to_vec())
        .bind(cursor)
        .bind(target.to_owned())
        .bind(limit)
        .fetch_all(self.registry.pool())
        .await
        .map_err(sql)?;
        let (paused, _) = self.q_paused(ns, target).await?;
        let mut out = Vec::new();
        let mut last = cursor;
        for r in &rows {
            let (seq, id): (i64, i64) = (
                r.try_get("seq").map_err(sql)?,
                r.try_get("entry_id").map_err(sql)?,
            );
            last = seq;
            let row = self.q_get(ns, id).await?;
            out.push(wire::QueueEvent {
                entry: Some(self.q_entry(ns, &row).await?),
                paused,
                resume: Bytes::copy_from_slice(&seq.to_be_bytes()),
            });
        }
        Ok((out, last))
    }

    /// The newest event sequence of the queue.
    pub(crate) async fn q_newest_seq(&self, ns: &[u8]) -> Result<i64, Status> {
        sqlx::query_scalar::<_, i64>(
            "SELECT CAST(COALESCE(MAX(seq), 0) AS BIGINT) FROM queue_events WHERE ns = $1",
        )
        .bind(ns.to_vec())
        .fetch_one(self.registry.pool())
        .await
        .map_err(sql)
    }
}
