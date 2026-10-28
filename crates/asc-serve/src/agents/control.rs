//! Control of a live run and the data of the run boards (docs/BUILDOUT.md section 18).
//!
//! * **Requests.** The runtime opens a *permission* request when a command matches
//!   `[agents.sandbox] ask_before` or the model calls `request_permission`, and a *plan* request
//!   when a run started with `plan_first` calls `propose_plan`. The run waits in `WAITING_INPUT`
//!   until a person decides (`DecideRequest`): a permission is allowed once or denied, a plan is
//!   approved or rejected. Decisions are stored with the decider and the reason (the approvals log).
//! * **Pause.** A paused run holds at its next tool call and sleeps until it is resumed or stopped.
//! * **Checkpoints.** Every commit of a run is a checkpoint; a paused run can be taken back to one.
//!   The ref of the run moves back, the later changes leave the run, the rollback is logged.
//! * **Spend.** Each model call adds to `agent_spend` (per repository, UTC day and agent), which
//!   feeds the monthly budgets and the spend board.

use std::sync::Arc;
use std::time::Duration;

use asc_rpc::server::wire;
use sqlx::Row;
use tonic::Status;

use super::runs::{EventKind, RunRow, RunState};
use super::runtime::Ended;
use super::tokens::Budget;
use crate::ai::model::{ToolCall, ToolSpec};
use super::tools::Committed;
use super::workspace::Workspace;
use super::{AgentsHub, Live};
use crate::ai::clock;

pub const KIND_PERMISSION: i64 = 1;
pub const KIND_PLAN: i64 = 2;
pub const PENDING: i64 = 1;
pub const ALLOWED: i64 = 2;
pub const DENIED: i64 = 3;
pub const APPROVED: i64 = 4;
pub const REJECTED: i64 = 5;
pub const EXPIRED: i64 = 6;

/// Longest reason a decider gives, in bytes.
pub const MAX_REASON: usize = 512;
/// Most steps in a plan, and the longest step.
pub const MAX_PLAN_STEPS: usize = 40;
pub const MAX_STEP: usize = 300;
/// Most bytes of a run log.
pub const MAX_LOG: usize = 4 << 20;

fn db(e: &sqlx::Error) -> Status {
    tracing::error!(class = %crate::ai::store::class(e), "agent control database error");
    Status::internal("internal error")
}

fn u(v: i64) -> u64 {
    u64::try_from(v).unwrap_or(0)
}

fn i(v: u64) -> i64 {
    i64::try_from(v).unwrap_or(i64::MAX)
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// `YYYY-MM-DD` (UTC) of a Unix time in seconds.
pub fn day_of(secs: i64) -> String {
    let z = secs.div_euclid(86_400) + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}")
}

/// `YYYY-MM` (UTC) of now.
pub fn this_month() -> String {
    day_of(clock::now())[..7].to_owned()
}

/// Whether `m` is `YYYY-MM`.
pub fn valid_month(m: &str) -> bool {
    let b = m.as_bytes();
    b.len() == 7
        && b[4] == b'-'
        && b[..4].iter().all(u8::is_ascii_digit)
        && b[5..].iter().all(u8::is_ascii_digit)
        && matches!(&m[5..], "01" | "02" | "03" | "04" | "05" | "06" | "07" | "08" | "09" | "10" | "11" | "12")
}

/// Control state of a run.
#[derive(Clone, Debug, Default)]
pub struct Ctl {
    pub paused: bool,
    pub paused_by: String,
    pub plan_first: bool,
    pub plan_ok: bool,
    pub steps_done: u64,
    pub for_change: Vec<u8>,
    pub continues: u64,
    pub rollback_to: u64,
}

/// A permission or plan request, as stored.
#[derive(Clone, Debug)]
pub struct ReqRow {
    pub id: u64,
    pub run_id: u64,
    pub repo: String,
    pub agent: String,
    pub kind: i64,
    pub summary: String,
    pub detail: String,
    pub state: i64,
    pub requested_at: i64,
    pub decided_by: String,
    pub decided_at: i64,
    pub reason: String,
}

/// A checkpoint, as stored.
#[derive(Clone, Debug)]
pub struct CheckpointRow {
    pub id: u64,
    pub run_id: u64,
    pub seq: u64,
    pub at: i64,
    pub label: String,
    pub changes: Vec<Vec<u8>>,
    pub tip: Vec<u8>,
    pub rolled_back_at: i64,
    pub rolled_back_by: String,
}

fn req_of(r: &sqlx::any::AnyRow) -> Result<ReqRow, sqlx::Error> {
    Ok(ReqRow {
        id: u(r.try_get("id")?),
        run_id: u(r.try_get("run_id")?),
        repo: r.try_get("repo")?,
        agent: r.try_get("agent")?,
        kind: r.try_get("kind")?,
        summary: r.try_get("summary")?,
        detail: r.try_get("detail")?,
        state: r.try_get("state")?,
        requested_at: r.try_get("requested_at")?,
        decided_by: r.try_get("decided_by")?,
        decided_at: r.try_get("decided_at")?,
        reason: r.try_get("reason")?,
    })
}

const REQ_COLS: &str =
    "id, run_id, repo, agent, kind, summary, detail, state, requested_at, decided_by, decided_at, reason";

fn cp_of(r: &sqlx::any::AnyRow) -> Result<CheckpointRow, sqlx::Error> {
    let changes: Vec<String> =
        serde_json::from_str(&r.try_get::<String, _>("changes_json")?).unwrap_or_default();
    Ok(CheckpointRow {
        id: u(r.try_get("id")?),
        run_id: u(r.try_get("run_id")?),
        seq: u(r.try_get("seq")?),
        at: r.try_get("at")?,
        label: r.try_get("label")?,
        changes: changes.iter().filter_map(|c| unhex(c)).collect(),
        tip: r.try_get("tip")?,
        rolled_back_at: r.try_get("rolled_back_at")?,
        rolled_back_by: r.try_get("rolled_back_by")?,
    })
}

fn unhex(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    (0..s.len() / 2)
        .map(|i| u8::from_str_radix(s.get(2 * i..2 * i + 2)?, 16).ok())
        .collect()
}

const CP_COLS: &str = "id, run_id, seq, at, label, changes_json, tip, rolled_back_at, rolled_back_by";

/// What the runtime does after looking at the pause and rollback state.
pub enum Hold {
    Go,
    Stop(Ended),
    Rollback(CheckpointRow),
}

/// What a control tool call came to.
pub enum Flow {
    /// Not a control call; run it as usual.
    Proceed,
    /// The text for the model.
    Result { content: String, error: bool },
    Stop(Ended),
}

/// Tools the control plane adds to the model's list.
pub fn extra_specs(plan_first: bool) -> Vec<ToolSpec> {
    let mut v = vec![ToolSpec {
        name: "request_permission".to_owned(),
        description: "Ask the person who started this run for permission to do something you are not allowed to do on your own. Say what you want to do and why. The answer is yes (once) or no with a reason. Do not do the action unless the answer is yes.".to_owned(),
        schema: serde_json::json!({"type": "object", "properties": {"action": {"type": "string"}, "reason": {"type": "string"}}, "required": ["action"]}),
    }];
    if plan_first {
        v.push(ToolSpec {
            name: "propose_plan".to_owned(),
            description: "Propose the plan for this task as an ordered list of short steps. The person approves or rejects it. You may use no other tool until a plan is approved. If it is rejected, read the reason, change the plan and propose it again.".to_owned(),
            schema: serde_json::json!({"type": "object", "properties": {"steps": {"type": "array", "items": {"type": "string"}}}, "required": ["steps"]}),
        });
        v.push(ToolSpec {
            name: "complete_step".to_owned(),
            description: "Mark a step of the approved plan as done (the number is from 1).".to_owned(),
            schema: serde_json::json!({"type": "object", "properties": {"step": {"type": "integer"}}, "required": ["step"]}),
        });
    }
    v
}

/// The part of the system prompt that belongs to the control plane.
pub fn prompt_tail(plan_first: bool) -> &'static str {
    if plan_first {
        "\n\nThis run needs an approved plan first: call propose_plan before any other tool, and mark steps done with complete_step."
    } else {
        ""
    }
}

impl AgentsHub {
    // ------------------------------------------------------------------------ ctl

    /// The control state of a run (all defaults when it has none).
    pub async fn ctl(&self, run_id: u64) -> Result<Ctl, Status> {
        let row = sqlx::query(
            "SELECT paused, paused_by, plan_first, plan_ok, steps_done, for_change, continues, rollback_to \
             FROM agent_run_ctl WHERE run_id = $1",
        )
        .bind(i(run_id))
        .fetch_optional(&self.core.pool)
        .await
        .map_err(|e| db(&e))?;
        let Some(r) = row else {
            return Ok(Ctl::default());
        };
        let g = |e: sqlx::Error| db(&e);
        Ok(Ctl {
            paused: r.try_get::<i64, _>("paused").map_err(g)? != 0,
            paused_by: r.try_get("paused_by").map_err(g)?,
            plan_first: r.try_get::<i64, _>("plan_first").map_err(g)? != 0,
            plan_ok: r.try_get::<i64, _>("plan_ok").map_err(g)? != 0,
            steps_done: u(r.try_get("steps_done").map_err(g)?),
            for_change: r.try_get("for_change").map_err(g)?,
            continues: u(r.try_get("continues").map_err(g)?),
            rollback_to: u(r.try_get("rollback_to").map_err(g)?),
        })
    }

    /// Writes the control row of a new run.
    pub async fn init_ctl(
        &self,
        run_id: u64,
        plan_first: bool,
        for_change: &[u8],
        continues: u64,
    ) -> Result<(), Status> {
        sqlx::query(
            "INSERT INTO agent_run_ctl (run_id, plan_first, for_change, continues) VALUES ($1, $2, $3, $4)",
        )
        .bind(i(run_id))
        .bind(i64::from(plan_first))
        .bind(for_change.to_vec())
        .bind(i(continues))
        .execute(&self.core.pool)
        .await
        .map_err(|e| db(&e))?;
        Ok(())
    }

    async fn set_ctl_i(&self, run_id: u64, column: &'static str, v: i64) -> Result<(), Status> {
        let q = format!("UPDATE agent_run_ctl SET {column} = $2 WHERE run_id = $1");
        let n = sqlx::query(sqlx::AssertSqlSafe(q))
            .bind(i(run_id))
            .bind(v)
            .execute(&self.core.pool)
            .await
            .map_err(|e| db(&e))?
            .rows_affected();
        if n == 0 {
            // A run created before the control tables existed.
            let _ = self.init_ctl(run_id, false, &[], 0).await;
            let q = format!("UPDATE agent_run_ctl SET {column} = $2 WHERE run_id = $1");
            sqlx::query(sqlx::AssertSqlSafe(q))
                .bind(i(run_id))
                .bind(v)
                .execute(&self.core.pool)
                .await
                .map_err(|e| db(&e))?;
        }
        Ok(())
    }

    /// Sets or clears the pause of a run.
    pub async fn set_paused(&self, run: &RunRow, paused: bool, by: &str) -> Result<(), Status> {
        if !run.state.is_active() {
            return Err(super::errors::run_not_active(run.state.name()));
        }
        self.set_ctl_i(run.id, "paused", i64::from(paused)).await?;
        sqlx::query("UPDATE agent_run_ctl SET paused_by = $2 WHERE run_id = $1")
            .bind(i(run.id))
            .bind(if paused { by.to_owned() } else { String::new() })
            .execute(&self.core.pool)
            .await
            .map_err(|e| db(&e))?;
        let fresh = self.core.runs.get(run.id).await.map_err(|e| db(&e))?.unwrap_or_else(|| run.clone());
        self.event(
            &fresh,
            EventKind::State,
            if paused { "paused" } else { "resumed" },
            "",
            &[],
            "",
        )
        .await;
        Ok(())
    }

    // ------------------------------------------------------------------------ requests

    async fn insert_with_id(&self, table: &'static str, build: impl Fn(i64) -> String + Send + Sync, bind: impl Fn(sqlx::query::Query<'_, sqlx::Any, sqlx::any::AnyArguments>) -> sqlx::query::Query<'_, sqlx::Any, sqlx::any::AnyArguments>) -> Result<i64, Status> {
        for _ in 0..8 {
            let q = format!("SELECT CAST(COALESCE(MAX(id), 0) AS BIGINT) FROM {table}");
            let max: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(q))
                .fetch_one(&self.core.pool)
                .await
                .map_err(|e| db(&e))?;
            let id = max + 1;
            let sql = build(id);
            let query = bind(sqlx::query(sqlx::AssertSqlSafe(sql)).bind(id));
            match query.execute(&self.core.pool).await {
                Ok(_) => return Ok(id),
                Err(sqlx::Error::Database(d)) if d.is_unique_violation() => {}
                Err(e) => return Err(db(&e)),
            }
        }
        Err(Status::aborted("concurrent writers; retry"))
    }

    /// Opens a request for `run` and returns its id.
    pub async fn open_request(
        &self,
        run: &RunRow,
        kind: i64,
        summary: &str,
        detail: &str,
    ) -> Result<u64, Status> {
        let (repo, agent, run_id, now) = (run.repo.clone(), run.agent.clone(), i(run.id), clock::now());
        let (summary, detail) = (summary.to_owned(), detail.to_owned());
        let id = self
            .insert_with_id(
                "agent_run_requests",
                |_| {
                    "INSERT INTO agent_run_requests (id, run_id, repo, agent, kind, summary, detail, state, requested_at) \
                     VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)"
                        .to_owned()
                },
                |q| {
                    q.bind(run_id)
                        .bind(repo.clone())
                        .bind(agent.clone())
                        .bind(kind)
                        .bind(summary.clone())
                        .bind(detail.clone())
                        .bind(PENDING)
                        .bind(now)
                },
            )
            .await?;
        self.audit(
            "agent.request.opened",
            &format!("agent:{}", run.agent),
            None,
            &run.repo,
            &id.to_string(),
            &[
                ("run", run.id.to_string()),
                ("kind", if kind == KIND_PLAN { "plan" } else { "permission" }.to_owned()),
                ("summary", summary_cut(&summary)),
            ],
        )
        .await;
        Ok(u(id))
    }

    pub async fn request(&self, id: u64) -> Result<Option<ReqRow>, Status> {
        let q = format!("SELECT {REQ_COLS} FROM agent_run_requests WHERE id = $1");
        let row = sqlx::query(sqlx::AssertSqlSafe(q))
            .bind(i(id))
            .fetch_optional(&self.core.pool)
            .await
            .map_err(|e| db(&e))?;
        row.map(|r| req_of(&r)).transpose().map_err(|e| db(&e))
    }

    /// Requests of a repository (or one run), newest first, `limit + 1` rows after `before`.
    pub async fn list_requests(
        &self,
        repo: &str,
        run_id: u64,
        states: &[i64],
        before: Option<u64>,
        limit: usize,
    ) -> Result<Vec<ReqRow>, Status> {
        let mut sql = format!("SELECT {REQ_COLS} FROM agent_run_requests WHERE repo = $1");
        let mut n = 1;
        if run_id != 0 {
            n += 1;
            sql.push_str(&format!(" AND run_id = ${n}"));
        }
        if let Some(_b) = before {
            n += 1;
            sql.push_str(&format!(" AND id < ${n}"));
        }
        if !states.is_empty() {
            let list: Vec<String> = states.iter().map(i64::to_string).collect();
            sql.push_str(&format!(" AND state IN ({})", list.join(", ")));
        }
        sql.push_str(&format!(" ORDER BY id DESC LIMIT {}", limit + 1));
        let mut q = sqlx::query(sqlx::AssertSqlSafe(sql)).bind(repo.to_ascii_lowercase());
        if run_id != 0 {
            q = q.bind(i(run_id));
        }
        if let Some(b) = before {
            q = q.bind(i(b));
        }
        let rows = q.fetch_all(&self.core.pool).await.map_err(|e| db(&e))?;
        rows.iter().map(|r| req_of(r).map_err(|e| db(&e))).collect()
    }

    pub async fn pending_requests(&self, run_id: u64) -> u32 {
        sqlx::query_scalar::<_, i64>(
            "SELECT CAST(COUNT(*) AS BIGINT) FROM agent_run_requests WHERE run_id = $1 AND state = 1",
        )
        .bind(i(run_id))
        .fetch_one(&self.core.pool)
        .await
        .map_or(0, |n| u32::try_from(n).unwrap_or(u32::MAX))
    }

    /// Decides a pending request. `Decision` must fit the kind. A request decided already answers
    /// with the stored decision, whatever `to` is (a retry changes nothing).
    pub async fn decide_request(
        &self,
        id: u64,
        to: i64,
        by: &str,
        reason: &str,
    ) -> Result<ReqRow, Status> {
        let Some(cur) = self.request(id).await? else {
            return Err(Status::not_found("no such request"));
        };
        let fits = match cur.kind {
            KIND_PERMISSION => matches!(to, ALLOWED | DENIED),
            _ => matches!(to, APPROVED | REJECTED),
        };
        if !fits {
            return Err(Status::invalid_argument(
                "a permission is allowed once or denied; a plan is approved or rejected",
            ));
        }
        if cur.state != PENDING {
            return Ok(cur);
        }
        let n = sqlx::query(
            "UPDATE agent_run_requests SET state = $2, decided_by = $3, decided_at = $4, reason = $5 \
             WHERE id = $1 AND state = 1",
        )
        .bind(i(id))
        .bind(to)
        .bind(by.to_owned())
        .bind(clock::now())
        .bind(reason.to_owned())
        .execute(&self.core.pool)
        .await
        .map_err(|e| db(&e))?
        .rows_affected();
        if n == 1 && to == APPROVED {
            self.set_ctl_i(cur.run_id, "plan_ok", 1).await?;
        }
        self.request(id).await?.ok_or_else(|| Status::internal("internal error"))
    }

    /// Marks the pending requests of a finished run expired.
    pub async fn expire_requests(&self, run_id: u64) {
        let _ = sqlx::query("UPDATE agent_run_requests SET state = $2 WHERE run_id = $1 AND state = 1")
            .bind(i(run_id))
            .bind(EXPIRED)
            .execute(&self.core.pool)
            .await;
    }

    /// Waits until the request is decided, the run is cancelled or `live` is stopped.
    async fn wait_decision(&self, id: u64, live: &Arc<Live>) -> Option<ReqRow> {
        loop {
            if live.cancel.is_cancelled() {
                return None;
            }
            match self.request(id).await {
                Ok(Some(r)) if r.state != PENDING => return Some(r),
                Ok(Some(_)) => {}
                _ => return None,
            }
            tokio::select! {
                () = tokio::time::sleep(Duration::from_millis(300)) => {}
                () = live.cancel.cancelled() => return None,
            }
        }
    }

    // ------------------------------------------------------------------------ plan

    pub async fn plan_of(&self, run_id: u64) -> Result<Vec<(u32, String, String)>, Status> {
        let rows = sqlx::query("SELECT idx, text, state FROM agent_run_plan WHERE run_id = $1 ORDER BY idx")
            .bind(i(run_id))
            .fetch_all(&self.core.pool)
            .await
            .map_err(|e| db(&e))?;
        rows.iter()
            .map(|r| {
                let idx: i64 = r.try_get("idx").map_err(|e| db(&e))?;
                let state: i64 = r.try_get("state").map_err(|e| db(&e))?;
                Ok((
                    u32::try_from(idx).unwrap_or(0),
                    r.try_get::<String, _>("text").map_err(|e| db(&e))?,
                    match state {
                        2 => "done",
                        1 => "doing",
                        _ => "todo",
                    }
                    .to_owned(),
                ))
            })
            .collect()
    }

    async fn set_plan(&self, run_id: u64, steps: &[String]) -> Result<(), Status> {
        let mut tx = self.core.pool.begin().await.map_err(|e| db(&e))?;
        sqlx::query("DELETE FROM agent_run_plan WHERE run_id = $1")
            .bind(i(run_id))
            .execute(&mut *tx)
            .await
            .map_err(|e| db(&e))?;
        for (n, s) in steps.iter().enumerate() {
            sqlx::query("INSERT INTO agent_run_plan (run_id, idx, text, state) VALUES ($1, $2, $3, 0)")
                .bind(i(run_id))
                .bind(i64::try_from(n).unwrap_or(0))
                .bind(s.clone())
                .execute(&mut *tx)
                .await
                .map_err(|e| db(&e))?;
        }
        tx.commit().await.map_err(|e| db(&e))
    }

    async fn mark_step(&self, run_id: u64, idx: usize, state: i64) -> Result<bool, Status> {
        Ok(sqlx::query("UPDATE agent_run_plan SET state = $3 WHERE run_id = $1 AND idx = $2")
            .bind(i(run_id))
            .bind(i64::try_from(idx).unwrap_or(0))
            .bind(state)
            .execute(&self.core.pool)
            .await
            .map_err(|e| db(&e))?
            .rows_affected()
            == 1)
    }

    /// `steps_done / steps_total / current` of a run.
    pub async fn progress(&self, run: &RunRow, ctl: &Ctl) -> wire::AgentRunProgress {
        let plan = self.plan_of(run.id).await.unwrap_or_default();
        if plan.is_empty() {
            return wire::AgentRunProgress {
                steps_done: u32::try_from(ctl.steps_done).unwrap_or(u32::MAX),
                steps_total: 0,
                current: String::new(),
                plan_first: ctl.plan_first,
            };
        }
        let done = plan.iter().filter(|p| p.2 == "done").count();
        let current = plan
            .iter()
            .find(|p| p.2 != "done")
            .map(|p| p.1.clone())
            .unwrap_or_default();
        wire::AgentRunProgress {
            steps_done: u32::try_from(done).unwrap_or(u32::MAX),
            steps_total: u32::try_from(plan.len()).unwrap_or(u32::MAX),
            current,
            plan_first: ctl.plan_first,
        }
    }

    // ------------------------------------------------------------------------ checkpoints

    pub async fn add_checkpoint(
        &self,
        run_id: u64,
        label: &str,
        changes: &[Vec<u8>],
        tip: &[u8],
    ) -> Result<(), Status> {
        let last: i64 = sqlx::query_scalar::<_, i64>(
            "SELECT CAST(COALESCE(MAX(seq), 0) AS BIGINT) FROM agent_checkpoints WHERE run_id = $1",
        )
        .bind(i(run_id))
        .fetch_one(&self.core.pool)
        .await
        .map_err(|e| db(&e))?;
        let seq = last + 1;
        let json = serde_json::to_string(&changes.iter().map(|c| hex(c)).collect::<Vec<_>>())
            .unwrap_or_else(|_| "[]".to_owned());
        let (label, tip) = (label.chars().take(200).collect::<String>(), tip.to_vec());
        let now = clock::now();
        self.insert_with_id(
            "agent_checkpoints",
            |_| {
                "INSERT INTO agent_checkpoints (id, run_id, seq, at, label, changes_json, tip) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7)"
                    .to_owned()
            },
            |q| {
                q.bind(i(run_id))
                    .bind(seq)
                    .bind(now)
                    .bind(label.clone())
                    .bind(json.clone())
                    .bind(tip.clone())
            },
        )
        .await
        .map(|_| ())
    }

    pub async fn checkpoints(&self, run_id: u64) -> Result<Vec<CheckpointRow>, Status> {
        let q = format!("SELECT {CP_COLS} FROM agent_checkpoints WHERE run_id = $1 ORDER BY seq");
        let rows = sqlx::query(sqlx::AssertSqlSafe(q))
            .bind(i(run_id))
            .fetch_all(&self.core.pool)
            .await
            .map_err(|e| db(&e))?;
        rows.iter().map(|r| cp_of(r).map_err(|e| db(&e))).collect()
    }

    pub async fn checkpoint(&self, id: u64) -> Result<Option<CheckpointRow>, Status> {
        let q = format!("SELECT {CP_COLS} FROM agent_checkpoints WHERE id = $1");
        let row = sqlx::query(sqlx::AssertSqlSafe(q))
            .bind(i(id))
            .fetch_optional(&self.core.pool)
            .await
            .map_err(|e| db(&e))?;
        row.map(|r| cp_of(&r)).transpose().map_err(|e| db(&e))
    }

    /// Asks the paused run to go back to checkpoint `cp` at its next tool call.
    pub async fn request_rollback(&self, run: &RunRow, cp: &CheckpointRow) -> Result<(), Status> {
        let ctl = self.ctl(run.id).await?;
        if !ctl.paused {
            return Err(Status::failed_precondition("pause the run before rolling it back"));
        }
        if cp.run_id != run.id {
            return Err(Status::not_found("no such checkpoint"));
        }
        if cp.rolled_back_at > 0 {
            return Err(Status::failed_precondition("the checkpoint was rolled back already"));
        }
        self.set_ctl_i(run.id, "rollback_to", i(cp.id)).await
    }

    async fn mark_rolled_back(&self, run_id: u64, cp: &CheckpointRow, by: &str, reason: &str) {
        let _ = sqlx::query(
            "UPDATE agent_checkpoints SET rolled_back_at = $3, rolled_back_by = $4, rollback_reason = $5 \
             WHERE run_id = $1 AND seq > $2 AND rolled_back_at = 0",
        )
        .bind(i(run_id))
        .bind(i(cp.seq))
        .bind(clock::now())
        .bind(by.to_owned())
        .bind(reason.to_owned())
        .execute(&self.core.pool)
        .await;
    }

    // ------------------------------------------------------------------------ spend

    /// Adds one model call to the spend of the day.
    pub async fn record_spend(&self, repo: &str, agent: &str, tokens: u64, cost: u64, requests: u64) {
        let day = day_of(clock::now());
        let repo = repo.to_ascii_lowercase();
        for _ in 0..3 {
            let n = sqlx::query(
                "UPDATE agent_spend SET tokens = tokens + $4, cost = cost + $5, requests = requests + $6 \
                 WHERE repo = $1 AND day = $2 AND agent = $3",
            )
            .bind(repo.clone())
            .bind(day.clone())
            .bind(agent.to_owned())
            .bind(i(tokens))
            .bind(i(cost))
            .bind(i(requests))
            .execute(&self.core.pool)
            .await
            .map(|r| r.rows_affected())
            .unwrap_or(0);
            if n > 0 {
                return;
            }
            let ins = sqlx::query(
                "INSERT INTO agent_spend (repo, day, agent, tokens, cost, requests) VALUES ($1, $2, $3, $4, $5, $6)",
            )
            .bind(repo.clone())
            .bind(day.clone())
            .bind(agent.to_owned())
            .bind(i(tokens))
            .bind(i(cost))
            .bind(i(requests))
            .execute(&self.core.pool)
            .await;
            if ins.is_ok() {
                return;
            }
        }
    }

    /// `(cost, tokens, requests)` of an agent (or all, with `agent` empty) in a month.
    pub async fn month_spend(&self, repo: &str, month: &str, agent: &str) -> Result<(u64, u64, u64), Status> {
        let like = format!("{month}-%");
        let mut sql = "SELECT CAST(COALESCE(SUM(cost), 0) AS BIGINT) AS c, CAST(COALESCE(SUM(tokens), 0) AS BIGINT) AS t, \
                       CAST(COALESCE(SUM(requests), 0) AS BIGINT) AS r FROM agent_spend WHERE repo = $1 AND day LIKE $2"
            .to_owned();
        if !agent.is_empty() {
            sql.push_str(" AND agent = $3");
        }
        let mut q = sqlx::query(sqlx::AssertSqlSafe(sql)).bind(repo.to_ascii_lowercase()).bind(like);
        if !agent.is_empty() {
            q = q.bind(agent.to_owned());
        }
        let r = q.fetch_one(&self.core.pool).await.map_err(|e| db(&e))?;
        let g = |k: &str| -> Result<u64, Status> {
            Ok(u(r.try_get::<i64, _>(k).map_err(|e| db(&e))?))
        };
        Ok((g("c")?, g("t")?, g("r")?))
    }

    /// The monthly cap of the repository (`agent` empty) or of one agent; 0 for none.
    pub async fn monthly_cap(&self, repo: &str, agent: &str) -> Result<u64, Status> {
        let v: Option<i64> =
            sqlx::query_scalar("SELECT cap FROM agent_monthly_budget WHERE repo = $1 AND agent = $2")
                .bind(repo.to_ascii_lowercase())
                .bind(agent.to_owned())
                .fetch_optional(&self.core.pool)
                .await
                .map_err(|e| db(&e))?;
        Ok(v.map_or(0, u))
    }

    pub async fn set_monthly_cap(&self, repo: &str, agent: &str, cap: u64, by: &str) -> Result<(), Status> {
        let repo = repo.to_ascii_lowercase();
        sqlx::query("DELETE FROM agent_monthly_budget WHERE repo = $1 AND agent = $2")
            .bind(repo.clone())
            .bind(agent.to_owned())
            .execute(&self.core.pool)
            .await
            .map_err(|e| db(&e))?;
        if cap > 0 {
            sqlx::query("INSERT INTO agent_monthly_budget (repo, agent, cap, set_by, set_at) VALUES ($1, $2, $3, $4, $5)")
                .bind(repo)
                .bind(agent.to_owned())
                .bind(i(cap))
                .bind(by.to_owned())
                .bind(clock::now())
                .execute(&self.core.pool)
                .await
                .map_err(|e| db(&e))?;
        }
        Ok(())
    }

    /// Whether the spend of this month reached the cap of the repository or of `agent`.
    pub async fn monthly_exceeded(&self, repo: &str, agent: &str) -> bool {
        let month = this_month();
        for who in ["", agent] {
            let cap = self.monthly_cap(repo, who).await.unwrap_or(0);
            if cap == 0 {
                continue;
            }
            if let Ok((cost, _, _)) = self.month_spend(repo, &month, who).await
                && cost >= cap
            {
                return true;
            }
        }
        false
    }

    // ------------------------------------------------------------------------ runtime hooks

    /// Holds the run while it is paused; reports a stop or a rollback to perform.
    pub(crate) async fn hold(&self, run: &RunRow, live: &Arc<Live>) -> Hold {
        loop {
            if let Some(e) = Self::stopped_for(live) {
                return Hold::Stop(e);
            }
            let Ok(ctl) = self.ctl(run.id).await else {
                return Hold::Go;
            };
            if ctl.rollback_to != 0 {
                let _ = self.set_ctl_i(run.id, "rollback_to", 0).await;
                if let Ok(Some(cp)) = self.checkpoint(ctl.rollback_to).await {
                    return Hold::Rollback(cp);
                }
            }
            if !ctl.paused {
                return Hold::Go;
            }
            tokio::select! {
                () = tokio::time::sleep(Duration::from_millis(300)) => {}
                () = live.cancel.cancelled() => {}
            }
        }
    }

    fn stopped_for(live: &Live) -> Option<Ended> {
        if live.cancel.is_cancelled() {
            Some(Ended::Cancelled("cancelled".into()))
        } else {
            None
        }
    }

    /// Takes the workspace and the run back to `cp`.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn do_rollback(
        &self,
        run: &RunRow,
        cp: &CheckpointRow,
        ws: &mut Workspace,
        secret: &str,
        ref_name: &str,
        tip: &mut Vec<u8>,
        ref_exists: &mut bool,
        committed: &mut Vec<Committed>,
    ) -> Result<(), String> {
        let by = {
            let c = self.ctl(run.id).await.map_err(|e| e.message().to_owned())?;
            c.paused_by
        };
        if *ref_exists && let Some(inproc) = self.inproc.get() {
            let update = if cp.tip.is_empty() {
                wire::RefUpdate {
                    name: ref_name.to_owned(),
                    expect: Some(wire::ref_update::Expect::Any(true)),
                    new: None,
                    delete: true,
                }
            } else {
                wire::RefUpdate {
                    name: ref_name.to_owned(),
                    expect: Some(wire::ref_update::Expect::Any(true)),
                    new: Some(wire::RefTarget {
                        t: Some(wire::ref_target::T::Changeset(bytes::Bytes::copy_from_slice(&cp.tip))),
                    }),
                    delete: false,
                }
            };
            let req = wire::UpdateRefsRequest {
                repo: Some(wire::RepoRef {
                    name: run.repo.clone(),
                    id_mode: asc_rpc::IdMode::Asc as i32,
                }),
                updates: vec![update],
                mode: wire::UpdateMode::Force as i32,
                ..Default::default()
            };
            inproc
                .update_refs(secret, req)
                .await
                .map_err(|s| format!("the ref could not be moved: {}", s.message()))?;
        }
        *tip = if cp.tip.is_empty() { run.base.clone() } else { cp.tip.clone() };
        *ref_exists = *ref_exists && !cp.tip.is_empty();
        committed.truncate(cp.changes.len());
        if let Some(root) = ws.root_of_commit(tip).await {
            ws.reset_to(root).await.map_err(|e| e.message().to_owned())?;
        }
        self.set_changes(run.id, &cp.changes).await.map_err(|e| e.message().to_owned())?;
        self.mark_rolled_back(run.id, cp, &by, "").await;
        let fresh = self.core.runs.get(run.id).await.ok().flatten().unwrap_or_else(|| run.clone());
        self.event(
            &fresh,
            EventKind::State,
            &format!("rolled back to checkpoint {}: {}", cp.seq, cp.label),
            "rollback",
            &[],
            "",
        )
        .await;
        self.audit(
            "agent.run.rolled_back",
            &by,
            None,
            &run.repo,
            &run.id.to_string(),
            &[("run", run.id.to_string()), ("checkpoint", cp.seq.to_string())],
        )
        .await;
        Ok(())
    }

    /// Replaces the change list of a run.
    pub async fn set_changes(&self, run_id: u64, changes: &[Vec<u8>]) -> Result<(), Status> {
        let json = serde_json::to_string(&changes.iter().map(|c| hex(c)).collect::<Vec<_>>())
            .unwrap_or_else(|_| "[]".to_owned());
        sqlx::query("UPDATE agent_runs SET changes_json = $2 WHERE id = $1")
            .bind(i(run_id))
            .bind(json)
            .execute(&self.core.pool)
            .await
            .map_err(|e| db(&e))?;
        Ok(())
    }

    /// Handles the tool calls of the control plane and the gates on the others (the plan must be
    /// approved first; a command that asks first waits for a yes).
    pub(crate) async fn control_call(
        &self,
        run: &RunRow,
        live: &Arc<Live>,
        call: &ToolCall,
    ) -> Flow {
        let ctl = match self.ctl(run.id).await {
            Ok(c) => c,
            Err(_) => return Flow::Proceed,
        };
        let fail = |m: &str| Flow::Result { content: m.to_owned(), error: true };
        match call.name.as_str() {
            "propose_plan" if ctl.plan_first => {
                let steps: Vec<String> = call
                    .input
                    .get("steps")
                    .and_then(|v| v.as_array())
                    .map(|a| {
                        a.iter()
                            .filter_map(|s| s.as_str())
                            .map(|s| s.trim().chars().take(MAX_STEP).collect::<String>())
                            .filter(|s| !s.is_empty())
                            .take(MAX_PLAN_STEPS)
                            .collect()
                    })
                    .unwrap_or_default();
                if steps.is_empty() {
                    return fail("a plan is 1 to 40 short steps");
                }
                if ctl.plan_ok {
                    return fail("the plan is approved already; carry it out");
                }
                if self.set_plan(run.id, &steps).await.is_err() {
                    return fail("the plan could not be saved");
                }
                let title = steps.first().cloned().unwrap_or_default();
                let Ok(id) = self
                    .open_request(run, KIND_PLAN, &format!("Plan: {title}"), &format!("{} steps", steps.len()))
                    .await
                else {
                    return fail("the plan could not be submitted");
                };
                self.wait_on(run, live, id, "plan").await
            }
            "complete_step" if ctl.plan_first => {
                let n = call.input.get("step").and_then(serde_json::Value::as_u64).unwrap_or(0);
                let ok = n >= 1 && self.mark_step(run.id, usize::try_from(n - 1).unwrap_or(0), 2).await.unwrap_or(false);
                if let (true, Ok(plan)) = (ok, self.plan_of(run.id).await)
                    && let Some(next) = plan.iter().find(|p| p.2 == "todo")
                {
                    let _ = self.mark_step(run.id, next.0 as usize, 1).await;
                }
                if ok {
                    Flow::Result { content: format!("step {n} is done"), error: false }
                } else {
                    fail("there is no such step")
                }
            }
            "request_permission" => {
                let action = call.input.get("action").and_then(|v| v.as_str()).unwrap_or("").trim().to_owned();
                if action.is_empty() {
                    return fail("say what you want to do");
                }
                let why = call.input.get("reason").and_then(|v| v.as_str()).unwrap_or("").to_owned();
                let Ok(id) = self
                    .open_request(run, KIND_PERMISSION, &action.chars().take(200).collect::<String>(), &why.chars().take(1000).collect::<String>())
                    .await
                else {
                    return fail("the request could not be submitted");
                };
                self.wait_on(run, live, id, "permission").await
            }
            "ask_user" => Flow::Proceed,
            _ if ctl.plan_first && !ctl.plan_ok => {
                fail("propose a plan with propose_plan and wait for it to be approved before using other tools")
            }
            "run_command" => {
                let argv: Vec<String> = call
                    .input
                    .get("argv")
                    .and_then(|v| v.as_array())
                    .map(|a| a.iter().filter_map(|s| s.as_str().map(str::to_owned)).collect())
                    .unwrap_or_default();
                let asks = self.core.cfg.sandbox.ask_before.iter().any(|p| {
                    !p.is_empty() && argv.len() >= p.len() && argv[..p.len()] == p[..]
                });
                if !asks {
                    return Flow::Proceed;
                }
                let line = argv.join(" ");
                let Ok(id) = self
                    .open_request(run, KIND_PERMISSION, &format!("run {}", line.chars().take(200).collect::<String>()), &line.chars().take(1000).collect::<String>())
                    .await
                else {
                    return fail("the command needs permission and the request could not be submitted");
                };
                match self.wait_on(run, live, id, "permission").await {
                    Flow::Result { error: false, .. } => Flow::Proceed,
                    other => other,
                }
            }
            _ => Flow::Proceed,
        }
    }

    /// Puts the run in `WAITING_INPUT`, waits for the decision of request `id` and turns it into
    /// the answer for the model.
    async fn wait_on(&self, run: &RunRow, live: &Arc<Live>, id: u64, what: &str) -> Flow {
        let core = &self.core;
        let moved = core
            .runs
            .transition(run.id, &[RunState::Running], RunState::WaitingInput)
            .await
            .unwrap_or(false);
        let fresh = core.runs.get(run.id).await.ok().flatten().unwrap_or_else(|| run.clone());
        if moved {
            self.event(&fresh, EventKind::State, &format!("waiting_input ({what} request {id})"), "", &[], "").await;
        }
        let decided = self.wait_decision(id, live).await;
        let _ = core
            .runs
            .transition(run.id, &[RunState::WaitingInput], RunState::Running)
            .await;
        let Some(r) = decided else {
            return Flow::Stop(Ended::Cancelled("cancelled".into()));
        };
        self.event(&fresh, EventKind::State, "running", "", &[], "").await;
        let why = if r.reason.is_empty() { String::new() } else { format!(": {}", r.reason) };
        match r.state {
            ALLOWED => Flow::Result { content: "permission granted once".to_owned(), error: false },
            APPROVED => Flow::Result { content: "the plan is approved; carry it out and mark steps done with complete_step".to_owned(), error: false },
            DENIED => Flow::Result { content: format!("permission denied{why}; do not do this"), error: true },
            REJECTED => Flow::Result { content: format!("the plan was rejected{why}; change it and propose it again"), error: true },
            _ => Flow::Result { content: "the request expired".to_owned(), error: true },
        }
    }

    /// Counts a model turn of a run without a plan.
    pub(crate) async fn count_step(&self, run_id: u64, steps: u64) {
        let _ = self.set_ctl_i(run_id, "steps_done", i(steps)).await;
    }

    // ------------------------------------------------------------------------ boards

    /// Median `(seconds, cost, tokens, samples)` of the finished runs of an agent in a repository.
    pub async fn estimate(&self, repo: &str, agent: &str, task_bytes: u32) -> Result<(u32, u64, u64, u32), Status> {
        let rows = sqlx::query(
            "SELECT id, started, finished, task, token_id FROM agent_runs \
             WHERE repo = $1 AND state = 4 AND finished > started AND started > 0 AND ($2 = '' OR agent = $2) \
             ORDER BY id DESC LIMIT 50",
        )
        .bind(repo.to_ascii_lowercase())
        .bind(agent.to_owned())
        .fetch_all(&self.core.pool)
        .await
        .map_err(|e| db(&e))?;
        let mut secs = Vec::new();
        let mut costs = Vec::new();
        let mut toks = Vec::new();
        for r in &rows {
            let (s, f): (i64, i64) = (
                r.try_get("started").map_err(|e| db(&e))?,
                r.try_get("finished").map_err(|e| db(&e))?,
            );
            let task: String = r.try_get("task").map_err(|e| db(&e))?;
            // Runs of a very different task size say little about this one.
            if task_bytes > 0 {
                let t = u32::try_from(task.len()).unwrap_or(u32::MAX);
                if t > task_bytes.saturating_mul(8) || t.saturating_mul(8) < task_bytes {
                    continue;
                }
            }
            secs.push(u64::try_from(f - s).unwrap_or(0));
            let token: String = r.try_get("token_id").map_err(|e| db(&e))?;
            let (c, t) = match self.core.tokens.get(&token).await {
                Ok(Some(t)) => (t.spent.cost, t.spent.tokens),
                _ => (0, 0),
            };
            costs.push(c);
            toks.push(t);
        }
        let median = |v: &mut Vec<u64>| -> u64 {
            v.sort_unstable();
            v.get(v.len() / 2).copied().unwrap_or(0)
        };
        let n = u32::try_from(secs.len()).unwrap_or(u32::MAX);
        Ok((
            u32::try_from(median(&mut secs)).unwrap_or(u32::MAX),
            median(&mut costs),
            median(&mut toks),
            n,
        ))
    }

    /// Distinct recent tasks, newest first. `by` empty returns everyone's.
    pub async fn recent_tasks(&self, repo: &str, by: &str, limit: usize) -> Result<Vec<RunRow>, Status> {
        let rows = self
            .core
            .runs
            .list(&repo.to_ascii_lowercase(), &[], "", by, None, 100)
            .await
            .map_err(|e| db(&e))?;
        let mut seen = std::collections::HashSet::new();
        let mut out = Vec::new();
        for r in rows {
            if seen.insert(r.task.clone()) {
                out.push(r);
                if out.len() >= limit {
                    break;
                }
            }
        }
        Ok(out)
    }

    /// The events, requests and checkpoints of a run as JSON lines, oldest first.
    pub async fn run_log(&self, run: &RunRow) -> Result<(Vec<u8>, bool), Status> {
        let mut lines: Vec<(i64, String)> = Vec::new();
        let mut after = 0u64;
        loop {
            let ev = self.core.runs.events_after(run.id, after, 500).await.map_err(|e| db(&e))?;
            if ev.is_empty() {
                break;
            }
            for e in &ev {
                after = e.seq;
                lines.push((
                    e.time_ms / 1000,
                    serde_json::json!({"type": "event", "seq": e.seq, "kind": e.kind, "time_ms": e.time_ms, "text": e.text, "tool": e.tool}).to_string(),
                ));
            }
            if lines.len() > 50_000 {
                break;
            }
        }
        for r in self.list_requests(&run.repo, run.id, &[], None, 200).await? {
            lines.push((
                r.requested_at,
                serde_json::json!({"type": "request", "id": r.id, "kind": if r.kind == KIND_PLAN { "plan" } else { "permission" },
                    "summary": r.summary, "state": r.state, "decided_by": r.decided_by, "decided_at": r.decided_at, "reason": r.reason})
                .to_string(),
            ));
        }
        for c in self.checkpoints(run.id).await? {
            lines.push((
                c.at,
                serde_json::json!({"type": "checkpoint", "id": c.id, "seq": c.seq, "label": c.label, "rolled_back_at": c.rolled_back_at, "rolled_back_by": c.rolled_back_by}).to_string(),
            ));
        }
        lines.sort_by_key(|l| l.0);
        let mut out = Vec::new();
        let mut truncated = false;
        for (_, l) in lines {
            if out.len() + l.len() + 1 > MAX_LOG {
                truncated = true;
                break;
            }
            out.extend_from_slice(l.as_bytes());
            out.push(b'\n');
        }
        Ok((out, truncated))
    }

    /// Lowers `asked` to the policy cap of the repository; the names of the limits it lowered.
    pub fn lowered(asked: Budget, got: Budget) -> Vec<String> {
        let mut v = Vec::new();
        for (name, a, g) in [
            ("changes", asked.changes, got.changes),
            ("tokens", asked.tokens, got.tokens),
            ("cost", asked.cost, got.cost),
            ("paths", asked.paths, got.paths),
            ("runtime", asked.runtime, got.runtime),
        ] {
            if a > 0 && g > 0 && g < a {
                v.push(name.to_owned());
            }
        }
        v
    }
}

fn summary_cut(s: &str) -> String {
    s.chars().take(120).collect()
}
