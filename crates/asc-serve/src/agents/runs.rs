//! The SQL side of agent runs: the run rows, their event stream and the messages sent to them.
//!
//! The state of a run moves only through conditional updates (`WHERE state IN (...)`), so a
//! cancel racing a finish, or two replicas picking up the same queued run, can never both win.
//! The usage of a run is the usage of its token; there is one set of counters.

use asc_rpc::server::wire;
use sqlx::any::AnyRow;
use sqlx::{AnyPool, Row};

use super::tokens::Budget;
use crate::ai::clock;
use crate::ai::store::class;

/// Where a run is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RunState {
    Queued = 1,
    Running = 2,
    WaitingInput = 3,
    Succeeded = 4,
    Failed = 5,
    Cancelled = 6,
    BudgetExceeded = 7,
    Expired = 8,
}

impl RunState {
    pub fn from_i64(n: i64) -> Self {
        match n {
            1 => Self::Queued,
            2 => Self::Running,
            3 => Self::WaitingInput,
            4 => Self::Succeeded,
            6 => Self::Cancelled,
            7 => Self::BudgetExceeded,
            8 => Self::Expired,
            _ => Self::Failed,
        }
    }

    /// Not finished: the run can still change.
    pub fn is_active(self) -> bool {
        matches!(self, Self::Queued | Self::Running | Self::WaitingInput)
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Running => "running",
            Self::WaitingInput => "waiting_input",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
            Self::BudgetExceeded => "budget_exceeded",
            Self::Expired => "expired",
        }
    }

    pub fn to_wire(self) -> wire::AgentRunState {
        match self {
            Self::Queued => wire::AgentRunState::Queued,
            Self::Running => wire::AgentRunState::Running,
            Self::WaitingInput => wire::AgentRunState::WaitingInput,
            Self::Succeeded => wire::AgentRunState::Succeeded,
            Self::Failed => wire::AgentRunState::Failed,
            Self::Cancelled => wire::AgentRunState::Cancelled,
            Self::BudgetExceeded => wire::AgentRunState::BudgetExceeded,
            Self::Expired => wire::AgentRunState::Expired,
        }
    }

    pub fn from_wire(s: wire::AgentRunState) -> Option<Self> {
        Some(match s {
            wire::AgentRunState::Unspecified => return None,
            wire::AgentRunState::Queued => Self::Queued,
            wire::AgentRunState::Running => Self::Running,
            wire::AgentRunState::WaitingInput => Self::WaitingInput,
            wire::AgentRunState::Succeeded => Self::Succeeded,
            wire::AgentRunState::Failed => Self::Failed,
            wire::AgentRunState::Cancelled => Self::Cancelled,
            wire::AgentRunState::BudgetExceeded => Self::BudgetExceeded,
            wire::AgentRunState::Expired => Self::Expired,
        })
    }
}

/// A run row.
#[derive(Clone, Debug)]
pub struct RunRow {
    pub id: u64,
    pub repo: String,
    pub agent: String,
    pub title: String,
    pub state: RunState,
    pub target: String,
    /// The changeset the run started from (32 bytes), or empty when the repository was empty.
    pub base: Vec<u8>,
    pub task: String,
    pub paths: Vec<String>,
    pub ai_key_id: String,
    pub model: String,
    pub limits: Budget,
    /// Change ids (16 bytes each), bottom to top.
    pub changes: Vec<Vec<u8>>,
    pub ref_name: String,
    pub created_by: String,
    pub starter: String,
    pub owner_id: i64,
    pub created: i64,
    pub started: i64,
    pub finished: i64,
    pub failure_code: String,
    pub failure_message: String,
    pub token_id: String,
    pub workflow_run_id: u64,
    pub workflow_step: String,
    pub cancel_requested: bool,
    pub question: String,
}

/// A run to create.
#[derive(Clone, Debug)]
pub struct NewRun {
    pub repo: String,
    pub agent: String,
    pub title: String,
    pub target: String,
    pub base: Vec<u8>,
    pub task: String,
    pub paths: Vec<String>,
    pub ai_key_id: String,
    pub model: String,
    pub limits: Budget,
    pub created_by: String,
    pub starter: String,
    pub owner_id: i64,
    pub token_id: String,
    pub workflow_run_id: u64,
    pub workflow_step: String,
}

/// One event of a run, as stored.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EventRow {
    pub run_id: u64,
    pub seq: u64,
    pub kind: i64,
    pub time_ms: i64,
    pub state: i64,
    pub text: String,
    pub tool: String,
    pub change: Vec<u8>,
    pub usage_json: String,
}

/// What an event is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EventKind {
    State = 1,
    Message = 2,
    Input = 3,
    ToolCall = 4,
    ToolResult = 5,
    Change = 6,
    Usage = 7,
    Error = 8,
}

#[derive(Clone)]
pub struct RunStore {
    pool: AnyPool,
}

const COLS: &str = "id, repo, agent, title, state, target, base, task, paths_json, ai_key_id, model, limits_json, \
     changes_json, ref_name, created_by, starter, owner_id, created, started, finished, failure_code, failure_message, \
     token_id, workflow_run_id, workflow_step, cancel_requested, question";

/// The outbox effect that tells the forge a run ended, so it can open the run's changes for review.
pub const FINISHED_EFFECT: &str = "agent_run_finished";

/// Queues the "run finished" effect for the repository with the id `repo_id` (the `repos` row). The row is
/// delivered at least once; the receiver is idempotent (it finds the pull request of a change that has one).
pub async fn enqueue_finished(
    pool: &AnyPool,
    repo_id: i64,
    run: u64,
    state: RunState,
    changes: usize,
) -> Result<(), sqlx::Error> {
    let doc = serde_json::json!({
        "effects": [FINISHED_EFFECT],
        "run": run,
        "state": state.name(),
        "changes": changes,
    });
    let doc = serde_json::to_vec(&doc).unwrap_or_default();
    sqlx::query("INSERT INTO outbox (repo_id, op_id, effects, next_at) VALUES ($1, $2, $3, $4)")
        .bind(repo_id)
        .bind(uuid::Uuid::new_v4().as_bytes().to_vec())
        .bind(doc)
        .bind(crate::outbox::now())
        .execute(pool)
        .await?;
    Ok(())
}

fn u(v: i64) -> u64 {
    u64::try_from(v).unwrap_or(0)
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

fn limits_json(l: Budget) -> String {
    format!(
        "{{\"changes\":{},\"tokens\":{},\"cost\":{},\"paths\":{},\"runtime\":{}}}",
        l.changes, l.tokens, l.cost, l.paths, l.runtime
    )
}

pub(super) fn limits_of(s: &str) -> Budget {
    let v: serde_json::Value = serde_json::from_str(s).unwrap_or_default();
    let n = |k: &str| v.get(k).and_then(serde_json::Value::as_u64).unwrap_or(0);
    Budget {
        changes: n("changes"),
        tokens: n("tokens"),
        cost: n("cost"),
        paths: n("paths"),
        runtime: n("runtime"),
    }
}

fn row_of(r: &AnyRow) -> Result<RunRow, sqlx::Error> {
    let changes: Vec<String> =
        serde_json::from_str(&r.try_get::<String, _>("changes_json")?).unwrap_or_default();
    Ok(RunRow {
        id: u(r.try_get("id")?),
        repo: r.try_get("repo")?,
        agent: r.try_get("agent")?,
        title: r.try_get("title")?,
        state: RunState::from_i64(r.try_get("state")?),
        target: r.try_get("target")?,
        base: r.try_get("base")?,
        task: r.try_get("task")?,
        paths: serde_json::from_str(&r.try_get::<String, _>("paths_json")?).unwrap_or_default(),
        ai_key_id: r.try_get("ai_key_id")?,
        model: r.try_get("model")?,
        limits: limits_of(&r.try_get::<String, _>("limits_json")?),
        changes: changes.iter().filter_map(|c| unhex(c)).collect(),
        ref_name: r.try_get("ref_name")?,
        created_by: r.try_get("created_by")?,
        starter: r.try_get("starter")?,
        owner_id: r.try_get("owner_id")?,
        created: r.try_get("created")?,
        started: r.try_get("started")?,
        finished: r.try_get("finished")?,
        failure_code: r.try_get("failure_code")?,
        failure_message: r.try_get("failure_message")?,
        token_id: r.try_get("token_id")?,
        workflow_run_id: u(r.try_get("workflow_run_id")?),
        workflow_step: r.try_get("workflow_step")?,
        cancel_requested: r.try_get::<i64, _>("cancel_requested")? != 0,
        question: r.try_get("question")?,
    })
}

fn event_of(r: &AnyRow) -> Result<EventRow, sqlx::Error> {
    Ok(EventRow {
        run_id: u(r.try_get("run_id")?),
        seq: u(r.try_get("seq")?),
        kind: r.try_get("kind")?,
        time_ms: r.try_get("time_ms")?,
        state: r.try_get("state")?,
        text: r.try_get("text")?,
        tool: r.try_get("tool")?,
        change: r.try_get("change")?,
        usage_json: r.try_get("usage_json")?,
    })
}

impl RunStore {
    pub fn new(pool: AnyPool) -> Self {
        Self { pool }
    }

    pub fn pool(&self) -> &AnyPool {
        &self.pool
    }

    /// Creates a run in the queued state. The id is one more than the largest; a lost race is
    /// retried.
    pub async fn create(&self, n: &NewRun) -> Result<u64, sqlx::Error> {
        let now = clock::now();
        let mut last = None;
        for _ in 0..5 {
            let max: i64 =
                sqlx::query_scalar("SELECT CAST(COALESCE(MAX(id), 0) AS BIGINT) FROM agent_runs")
                    .fetch_one(&self.pool)
                    .await?;
            let id = max + 1;
            let r = sqlx::query(
                "INSERT INTO agent_runs (id, repo, agent, title, state, target, base, task, paths_json, ai_key_id, model, \
                 limits_json, created_by, starter, owner_id, created, token_id, workflow_run_id, workflow_step) \
                 VALUES ($1, $2, $3, $4, 1, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17, $18)",
            )
            .bind(id)
            .bind(n.repo.clone())
            .bind(n.agent.clone())
            .bind(n.title.clone())
            .bind(n.target.clone())
            .bind(n.base.clone())
            .bind(n.task.clone())
            .bind(serde_json::to_string(&n.paths).unwrap_or_else(|_| "[]".into()))
            .bind(n.ai_key_id.clone())
            .bind(n.model.clone())
            .bind(limits_json(n.limits))
            .bind(n.created_by.clone())
            .bind(n.starter.clone())
            .bind(n.owner_id)
            .bind(now)
            .bind(n.token_id.clone())
            .bind(i64::try_from(n.workflow_run_id).unwrap_or(0))
            .bind(n.workflow_step.clone())
            .execute(&self.pool)
            .await;
            match r {
                Ok(_) => return Ok(u(id)),
                Err(sqlx::Error::Database(d)) if d.is_unique_violation() => {
                    last = Some(sqlx::Error::Database(d))
                }
                Err(e) => return Err(e),
            }
        }
        Err(last.unwrap_or(sqlx::Error::PoolTimedOut))
    }

    pub async fn get(&self, id: u64) -> Result<Option<RunRow>, sqlx::Error> {
        let q = format!("SELECT {COLS} FROM agent_runs WHERE id = $1");
        sqlx::query(sqlx::AssertSqlSafe(q))
            .bind(i64::try_from(id).unwrap_or(0))
            .fetch_optional(&self.pool)
            .await?
            .as_ref()
            .map(row_of)
            .transpose()
    }

    /// Newest first. `before` is the id of the last row of the previous page.
    pub async fn list(
        &self,
        repo: &str,
        states: &[RunState],
        agent: &str,
        created_by: &str,
        before: Option<u64>,
        limit: usize,
    ) -> Result<Vec<RunRow>, sqlx::Error> {
        let mut conds = vec!["repo = $1".to_owned()];
        if !states.is_empty() {
            let list: Vec<String> = states.iter().map(|s| (*s as i64).to_string()).collect();
            conds.push(format!("state IN ({})", list.join(", ")));
        }
        let mut binds: Vec<String> = Vec::new();
        if !agent.is_empty() {
            binds.push(agent.to_owned());
            conds.push(format!("agent = ${}", binds.len() + 1));
        }
        if !created_by.is_empty() {
            binds.push(created_by.to_owned());
            conds.push(format!(
                "(created_by = ${n} OR starter = ${n})",
                n = binds.len() + 1
            ));
        }
        if let Some(b) = before {
            conds.push(format!("id < {}", i64::try_from(b).unwrap_or(i64::MAX)));
        }
        let q = format!(
            "SELECT {COLS} FROM agent_runs WHERE {} ORDER BY id DESC LIMIT {}",
            conds.join(" AND "),
            limit + 1
        );
        // Runs are stored under the lower-case name; a request may spell the repository otherwise.
        let mut query = sqlx::query(sqlx::AssertSqlSafe(q)).bind(repo.to_ascii_lowercase());
        for b in binds {
            query = query.bind(b);
        }
        query
            .fetch_all(&self.pool)
            .await?
            .iter()
            .map(row_of)
            .collect()
    }

    /// Moves a run from one of `from` to `to`. `Ok(false)`: it was in another state.
    pub async fn transition(
        &self,
        id: u64,
        from: &[RunState],
        to: RunState,
    ) -> Result<bool, sqlx::Error> {
        let list: Vec<String> = from.iter().map(|s| (*s as i64).to_string()).collect();
        let started = if to == RunState::Running {
            ", started = CASE WHEN started = 0 THEN $3 ELSE started END, heartbeat = $3"
        } else {
            ""
        };
        let finished = if to.is_active() {
            ""
        } else {
            ", finished = $3"
        };
        let q = format!(
            "UPDATE agent_runs SET state = $2{started}{finished} WHERE id = $1 AND state IN ({})",
            list.join(", ")
        );
        let r = sqlx::query(sqlx::AssertSqlSafe(q))
            .bind(i64::try_from(id).unwrap_or(0))
            .bind(to as i64)
            .bind(clock::now())
            .execute(&self.pool)
            .await?;
        Ok(r.rows_affected() == 1)
    }

    /// Ends a run in a terminal state with the reason.
    pub async fn finish(
        &self,
        id: u64,
        to: RunState,
        code: &str,
        message: &str,
    ) -> Result<bool, sqlx::Error> {
        let r = sqlx::query(
            "UPDATE agent_runs SET state = $2, finished = $3, failure_code = $4, failure_message = $5, question = '' \
             WHERE id = $1 AND state IN (1, 2, 3)",
        )
        .bind(i64::try_from(id).unwrap_or(0))
        .bind(to as i64)
        .bind(clock::now())
        .bind(code.to_owned())
        .bind(message.to_owned())
        .execute(&self.pool)
        .await?;
        Ok(r.rows_affected() == 1)
    }

    pub async fn set_ref(&self, id: u64, name: &str) -> Result<(), sqlx::Error> {
        sqlx::query("UPDATE agent_runs SET ref_name = $2 WHERE id = $1")
            .bind(i64::try_from(id).unwrap_or(0))
            .bind(name.to_owned())
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn set_question(&self, id: u64, text: &str) -> Result<(), sqlx::Error> {
        sqlx::query("UPDATE agent_runs SET question = $2 WHERE id = $1")
            .bind(i64::try_from(id).unwrap_or(0))
            .bind(text.to_owned())
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn set_limits(&self, id: u64, l: Budget) -> Result<(), sqlx::Error> {
        sqlx::query("UPDATE agent_runs SET limits_json = $2 WHERE id = $1")
            .bind(i64::try_from(id).unwrap_or(0))
            .bind(limits_json(l))
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Appends a change id (16 bytes) to the run's stack.
    pub async fn add_change(&self, id: u64, change: &[u8]) -> Result<(), sqlx::Error> {
        let Some(row) = self.get(id).await? else {
            return Ok(());
        };
        let mut all: Vec<String> = row.changes.iter().map(|c| hex(c)).collect();
        let h = hex(change);
        if !all.contains(&h) {
            all.push(h);
        }
        sqlx::query("UPDATE agent_runs SET changes_json = $2 WHERE id = $1")
            .bind(i64::try_from(id).unwrap_or(0))
            .bind(serde_json::to_string(&all).unwrap_or_else(|_| "[]".into()))
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Says the process that runs `id` is alive.
    pub async fn heartbeat(&self, id: u64) -> Result<(), sqlx::Error> {
        sqlx::query("UPDATE agent_runs SET heartbeat = $2 WHERE id = $1")
            .bind(i64::try_from(id).unwrap_or(0))
            .bind(clock::now())
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Running runs whose process has not been heard from since `before`.
    pub async fn stale(&self, before: i64) -> Result<Vec<RunRow>, sqlx::Error> {
        let q = format!(
            "SELECT {COLS} FROM agent_runs WHERE state IN (2, 3) AND heartbeat < $1 ORDER BY id LIMIT 200"
        );
        sqlx::query(sqlx::AssertSqlSafe(q))
            .bind(before)
            .fetch_all(&self.pool)
            .await?
            .iter()
            .map(row_of)
            .collect()
    }

    pub async fn request_cancel(&self, id: u64) -> Result<(), sqlx::Error> {
        sqlx::query("UPDATE agent_runs SET cancel_requested = 1 WHERE id = $1")
            .bind(i64::try_from(id).unwrap_or(0))
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn cancel_requested(&self, id: u64) -> Result<bool, sqlx::Error> {
        let v: Option<i64> =
            sqlx::query_scalar("SELECT cancel_requested FROM agent_runs WHERE id = $1")
                .bind(i64::try_from(id).unwrap_or(0))
                .fetch_optional(&self.pool)
                .await?;
        Ok(v.unwrap_or(1) != 0)
    }

    /// Runs in `states` (oldest first), for recovery and for the scheduler.
    pub async fn in_states(
        &self,
        states: &[RunState],
        limit: usize,
    ) -> Result<Vec<RunRow>, sqlx::Error> {
        let list: Vec<String> = states.iter().map(|s| (*s as i64).to_string()).collect();
        let q = format!(
            "SELECT {COLS} FROM agent_runs WHERE state IN ({}) ORDER BY id LIMIT {limit}",
            list.join(", ")
        );
        sqlx::query(sqlx::AssertSqlSafe(q))
            .fetch_all(&self.pool)
            .await?
            .iter()
            .map(row_of)
            .collect()
    }

    /// The queued runs a dispatcher may start now, oldest first: of each repository that has room
    /// (fewer than `per_repo` runs active), at most `per_repo` of its oldest queued runs. A
    /// repository at its cap contributes nothing, so its backlog cannot hide another's runs.
    pub async fn dispatchable(
        &self,
        per_repo: u32,
        limit: usize,
    ) -> Result<Vec<RunRow>, sqlx::Error> {
        let q = format!(
            "SELECT {COLS} FROM agent_runs WHERE id IN (               SELECT id FROM (                 SELECT id, ROW_NUMBER() OVER (PARTITION BY repo ORDER BY id) AS rn                  FROM agent_runs WHERE state = 1 AND repo NOT IN (                   SELECT repo FROM agent_runs WHERE state IN (2, 3)                    GROUP BY repo HAVING COUNT(*) >= $1)               ) q WHERE rn <= $1)              ORDER BY id LIMIT {limit}"
        );
        sqlx::query(sqlx::AssertSqlSafe(q))
            .bind(i64::from(per_repo))
            .fetch_all(&self.pool)
            .await?
            .iter()
            .map(row_of)
            .collect()
    }

    /// Runs of `repo` that are active.
    pub async fn active_in(&self, repo: &str) -> Result<i64, sqlx::Error> {
        sqlx::query_scalar(
            "SELECT CAST(COUNT(*) AS BIGINT) FROM agent_runs WHERE repo = $1 AND state IN (2, 3)",
        )
        .bind(repo.to_ascii_lowercase())
        .fetch_one(&self.pool)
        .await
    }

    /// Takes a queued run for execution (`Queued -> Running`). `Ok(false)`: another worker has it.
    pub async fn claim(&self, id: u64) -> Result<bool, sqlx::Error> {
        self.transition(id, &[RunState::Queued], RunState::Running)
            .await
    }

    // -------------------------------------------------------------------------- events

    /// Appends an event; returns its sequence number. The sequence is one more than the largest
    /// of the run; a lost race is retried.
    #[allow(clippy::too_many_arguments)] // one column per argument, called from one place
    pub async fn append(
        &self,
        run_id: u64,
        kind: EventKind,
        state: i64,
        text: &str,
        tool: &str,
        change: &[u8],
        usage_json: &str,
    ) -> Result<u64, sqlx::Error> {
        let rid = i64::try_from(run_id).unwrap_or(0);
        let now_ms = clock::now().saturating_mul(1000)
            + i64::from(
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_or(0, |d| d.subsec_millis()),
            );
        let mut last = None;
        for _ in 0..8 {
            let max: i64 = sqlx::query_scalar("SELECT CAST(COALESCE(MAX(seq), 0) AS BIGINT) FROM agent_run_events WHERE run_id = $1")
                .bind(rid)
                .fetch_one(&self.pool)
                .await?;
            let r = sqlx::query(
                "INSERT INTO agent_run_events (run_id, seq, kind, time_ms, state, text, tool, change, usage_json) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
            )
            .bind(rid)
            .bind(max + 1)
            .bind(kind as i64)
            .bind(now_ms)
            .bind(state)
            .bind(text.to_owned())
            .bind(tool.to_owned())
            .bind(change.to_vec())
            .bind(usage_json.to_owned())
            .execute(&self.pool)
            .await;
            match r {
                Ok(_) => return Ok(u(max + 1)),
                Err(sqlx::Error::Database(d)) if d.is_unique_violation() => {
                    last = Some(sqlx::Error::Database(d))
                }
                Err(e) => return Err(e),
            }
        }
        Err(last.unwrap_or(sqlx::Error::PoolTimedOut))
    }

    /// Events after `after`, oldest first.
    pub async fn events_after(
        &self,
        run_id: u64,
        after: u64,
        limit: usize,
    ) -> Result<Vec<EventRow>, sqlx::Error> {
        sqlx::query(
            "SELECT run_id, seq, kind, time_ms, state, text, tool, change, usage_json FROM agent_run_events \
             WHERE run_id = $1 AND seq > $2 ORDER BY seq LIMIT $3",
        )
        .bind(i64::try_from(run_id).unwrap_or(0))
        .bind(i64::try_from(after).unwrap_or(i64::MAX))
        .bind(i64::try_from(limit).unwrap_or(i64::MAX))
        .fetch_all(&self.pool)
        .await?
        .iter()
        .map(event_of)
        .collect()
    }

    /// Deletes the events of finished runs older than `before` (unix seconds).
    pub async fn prune_events(&self, before: i64) -> Result<u64, sqlx::Error> {
        let r = sqlx::query(
            "DELETE FROM agent_run_events WHERE run_id IN \
             (SELECT id FROM agent_runs WHERE state NOT IN (1, 2, 3) AND finished > 0 AND finished < $1)",
        )
        .bind(before)
        .execute(&self.pool)
        .await?;
        Ok(r.rows_affected())
    }

    // -------------------------------------------------------------------------- inputs

    pub async fn push_input(&self, run_id: u64, text: &str) -> Result<(), sqlx::Error> {
        let rid = i64::try_from(run_id).unwrap_or(0);
        for _ in 0..5 {
            let max: i64 = sqlx::query_scalar("SELECT CAST(COALESCE(MAX(seq), 0) AS BIGINT) FROM agent_run_inputs WHERE run_id = $1")
                .bind(rid)
                .fetch_one(&self.pool)
                .await?;
            let r = sqlx::query(
                "INSERT INTO agent_run_inputs (run_id, seq, text, at) VALUES ($1, $2, $3, $4)",
            )
            .bind(rid)
            .bind(max + 1)
            .bind(text.to_owned())
            .bind(clock::now())
            .execute(&self.pool)
            .await;
            match r {
                Ok(_) => return Ok(()),
                Err(sqlx::Error::Database(d)) if d.is_unique_violation() => {}
                Err(e) => return Err(e),
            }
        }
        Err(sqlx::Error::PoolTimedOut)
    }

    /// The messages not yet taken, oldest first, marked taken.
    pub async fn take_inputs(&self, run_id: u64) -> Result<Vec<String>, sqlx::Error> {
        let rid = i64::try_from(run_id).unwrap_or(0);
        let rows = sqlx::query("SELECT seq, text FROM agent_run_inputs WHERE run_id = $1 AND consumed = 0 ORDER BY seq")
            .bind(rid)
            .fetch_all(&self.pool)
            .await?;
        let mut out = Vec::new();
        for r in rows {
            let seq: i64 = r.try_get("seq")?;
            let done = sqlx::query("UPDATE agent_run_inputs SET consumed = 1 WHERE run_id = $1 AND seq = $2 AND consumed = 0")
                .bind(rid)
                .bind(seq)
                .execute(&self.pool)
                .await?;
            if done.rows_affected() == 1 {
                out.push(r.try_get("text")?);
            }
        }
        Ok(out)
    }

    pub async fn pending_inputs(&self, run_id: u64) -> Result<i64, sqlx::Error> {
        sqlx::query_scalar("SELECT CAST(COUNT(*) AS BIGINT) FROM agent_run_inputs WHERE run_id = $1 AND consumed = 0")
            .bind(i64::try_from(run_id).unwrap_or(0))
            .fetch_one(&self.pool)
            .await
    }
}

/// A database error in a run operation, reduced to its class for the logs.
pub fn db_class(e: &sqlx::Error) -> String {
    class(e)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn states_round_trip_and_know_when_they_are_over() {
        for s in [
            RunState::Queued,
            RunState::Running,
            RunState::WaitingInput,
            RunState::Succeeded,
            RunState::Failed,
            RunState::Cancelled,
            RunState::BudgetExceeded,
            RunState::Expired,
        ] {
            assert_eq!(RunState::from_i64(s as i64), s);
            assert_eq!(RunState::from_wire(s.to_wire()), Some(s));
            assert_eq!(
                s.is_active(),
                matches!(
                    s,
                    RunState::Queued | RunState::Running | RunState::WaitingInput
                )
            );
        }
        assert_eq!(RunState::from_wire(wire::AgentRunState::Unspecified), None);
        assert_eq!(RunState::from_i64(99), RunState::Failed);
    }

    #[test]
    fn hex_and_limits_round_trip() {
        assert_eq!(unhex(&hex(&[0, 255, 16])), Some(vec![0, 255, 16]));
        assert_eq!(unhex("abc"), None);
        assert_eq!(unhex("zz"), None);
        let l = Budget {
            changes: 1,
            tokens: 2,
            cost: 3,
            paths: 4,
            runtime: 5,
        };
        assert_eq!(limits_of(&limits_json(l)), l);
        assert_eq!(limits_of("garbage"), Budget::default());
    }
}
