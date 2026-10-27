//! The SQL side of workflows: enablement, runs, steps, the event stream of a run, the slots that
//! make an event fire a workflow at most once, and the cursors of the event sources.
//!
//! State moves only through conditional updates (`WHERE state IN (...)`), so a cancel racing a
//! finish, or two servers taking the same queued run, cannot both win.

use std::collections::BTreeMap;

use asc_rpc::server::wire;
use sqlx::any::AnyRow;
use sqlx::{AnyPool, Row};

use crate::ai::clock;

/// Where a run is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RunState {
    Queued = 1,
    Running = 2,
    WaitingApproval = 3,
    Succeeded = 4,
    Failed = 5,
    Cancelled = 6,
}

impl RunState {
    pub fn from_i64(n: i64) -> Self {
        match n {
            1 => Self::Queued,
            2 => Self::Running,
            3 => Self::WaitingApproval,
            4 => Self::Succeeded,
            6 => Self::Cancelled,
            _ => Self::Failed,
        }
    }

    pub fn is_active(self) -> bool {
        matches!(self, Self::Queued | Self::Running | Self::WaitingApproval)
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Running => "running",
            Self::WaitingApproval => "waiting_approval",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }

    pub fn to_wire(self) -> wire::WorkflowRunState {
        match self {
            Self::Queued => wire::WorkflowRunState::Queued,
            Self::Running => wire::WorkflowRunState::Running,
            Self::WaitingApproval => wire::WorkflowRunState::WaitingApproval,
            Self::Succeeded => wire::WorkflowRunState::Succeeded,
            Self::Failed => wire::WorkflowRunState::Failed,
            Self::Cancelled => wire::WorkflowRunState::Cancelled,
        }
    }

    pub fn from_wire(s: wire::WorkflowRunState) -> Option<Self> {
        Some(match s {
            wire::WorkflowRunState::Unspecified => return None,
            wire::WorkflowRunState::Queued => Self::Queued,
            wire::WorkflowRunState::Running => Self::Running,
            wire::WorkflowRunState::WaitingApproval => Self::WaitingApproval,
            wire::WorkflowRunState::Succeeded => Self::Succeeded,
            wire::WorkflowRunState::Failed => Self::Failed,
            wire::WorkflowRunState::Cancelled => Self::Cancelled,
        })
    }
}

/// Where a step is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StepState {
    Pending = 1,
    Running = 2,
    Waiting = 3,
    Succeeded = 4,
    Failed = 5,
    Skipped = 6,
    Cancelled = 7,
}

impl StepState {
    pub fn from_i64(n: i64) -> Self {
        match n {
            1 => Self::Pending,
            2 => Self::Running,
            3 => Self::Waiting,
            4 => Self::Succeeded,
            6 => Self::Skipped,
            7 => Self::Cancelled,
            _ => Self::Failed,
        }
    }

    /// Over: the step will not change again.
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Succeeded | Self::Failed | Self::Skipped | Self::Cancelled
        )
    }

    pub fn to_wire(self) -> wire::WorkflowStepState {
        match self {
            Self::Pending => wire::WorkflowStepState::Pending,
            Self::Running => wire::WorkflowStepState::Running,
            Self::Waiting => wire::WorkflowStepState::Waiting,
            Self::Succeeded => wire::WorkflowStepState::Succeeded,
            Self::Failed => wire::WorkflowStepState::Failed,
            Self::Skipped => wire::WorkflowStepState::Skipped,
            Self::Cancelled => wire::WorkflowStepState::Cancelled,
        }
    }
}

/// A run row.
#[derive(Clone, Debug)]
pub struct RunRow {
    pub id: u64,
    pub repo: String,
    pub workflow: String,
    /// The trunk changeset the definition was read from (32 bytes), or empty.
    pub source: Vec<u8>,
    /// The text of the definition as it was when the run was made.
    pub definition: String,
    pub state: RunState,
    /// The wire `WorkflowTriggerKind`.
    pub trigger: i64,
    /// The person (handle) or system actor that started it.
    pub started_by: String,
    /// The principal the run's agent steps act for.
    pub requester: String,
    /// The change the event was about (16 bytes), or empty.
    pub change: Vec<u8>,
    /// The changeset of the version of that change the event was about (32 bytes), or empty.
    pub change_cs: Vec<u8>,
    pub event: BTreeMap<String, String>,
    pub inputs: BTreeMap<String, String>,
    pub depth: u32,
    /// The workflow whose agent made the change that started this run, or empty.
    pub origin: String,
    pub created: i64,
    pub started: i64,
    pub finished: i64,
    pub error: String,
    pub cancel_requested: bool,
    pub owner: String,
    pub heartbeat: i64,
}

/// A step row.
#[derive(Clone, Debug)]
pub struct StepRow {
    pub run_id: u64,
    pub step: String,
    pub ord: u32,
    pub state: StepState,
    pub agent_run_id: u64,
    pub started: i64,
    pub finished: i64,
    pub decided_by: String,
    pub comment: String,
    pub error: String,
    pub exit_code: i32,
    pub timed_out: bool,
    pub output: String,
    pub ci_run_id: u64,
    pub usage_json: String,
}

/// A run to create.
#[derive(Clone, Debug)]
pub struct NewRun {
    pub repo: String,
    pub workflow: String,
    pub source: Vec<u8>,
    pub definition: String,
    pub trigger: i64,
    pub started_by: String,
    pub requester: String,
    pub change: Vec<u8>,
    pub change_cs: Vec<u8>,
    pub event: BTreeMap<String, String>,
    pub inputs: BTreeMap<String, String>,
    pub depth: u32,
    pub origin: String,
    /// Step ids in file order.
    pub steps: Vec<String>,
}

/// What a step run found out, for [`Store::step_result`].
#[derive(Clone, Debug, Default)]
pub struct StepResult {
    pub exit_code: i32,
    pub timed_out: bool,
    pub output: String,
    pub ci_run_id: u64,
}

fn u(v: i64) -> u64 {
    u64::try_from(v).unwrap_or(0)
}

fn i(v: u64) -> i64 {
    i64::try_from(v).unwrap_or(i64::MAX)
}

fn map_of(s: &str) -> BTreeMap<String, String> {
    serde_json::from_str(s).unwrap_or_default()
}

fn json_of(m: &BTreeMap<String, String>) -> String {
    serde_json::to_string(m).unwrap_or_else(|_| "{}".into())
}

const RUN_COLS: &str = "id, repo, workflow, source, definition, state, trigger_kind, started_by, requester, change, change_cs, \
     event_json, inputs_json, depth, origin, created, started, finished, error, cancel_requested, owner, heartbeat";

fn run_of(r: &AnyRow) -> Result<RunRow, sqlx::Error> {
    Ok(RunRow {
        id: u(r.try_get("id")?),
        repo: r.try_get("repo")?,
        workflow: r.try_get("workflow")?,
        source: r.try_get("source")?,
        definition: r.try_get("definition")?,
        state: RunState::from_i64(r.try_get("state")?),
        trigger: r.try_get("trigger_kind")?,
        started_by: r.try_get("started_by")?,
        requester: r.try_get("requester")?,
        change: r.try_get("change")?,
        change_cs: r.try_get("change_cs")?,
        event: map_of(&r.try_get::<String, _>("event_json")?),
        inputs: map_of(&r.try_get::<String, _>("inputs_json")?),
        depth: u32::try_from(r.try_get::<i64, _>("depth")?).unwrap_or(0),
        origin: r.try_get("origin")?,
        created: r.try_get("created")?,
        started: r.try_get("started")?,
        finished: r.try_get("finished")?,
        error: r.try_get("error")?,
        cancel_requested: r.try_get::<i64, _>("cancel_requested")? != 0,
        owner: r.try_get("owner")?,
        heartbeat: r.try_get("heartbeat")?,
    })
}

const STEP_COLS: &str = "run_id, step, ord, state, agent_run_id, started, finished, decided_by, comment, error, exit_code, \
     timed_out, output, ci_run_id, usage_json";

fn step_of(r: &AnyRow) -> Result<StepRow, sqlx::Error> {
    Ok(StepRow {
        run_id: u(r.try_get("run_id")?),
        step: r.try_get("step")?,
        ord: u32::try_from(r.try_get::<i64, _>("ord")?).unwrap_or(0),
        state: StepState::from_i64(r.try_get("state")?),
        agent_run_id: u(r.try_get("agent_run_id")?),
        started: r.try_get("started")?,
        finished: r.try_get("finished")?,
        decided_by: r.try_get("decided_by")?,
        comment: r.try_get("comment")?,
        error: r.try_get("error")?,
        exit_code: i32::try_from(r.try_get::<i64, _>("exit_code")?).unwrap_or(0),
        timed_out: r.try_get::<i64, _>("timed_out")? != 0,
        output: r.try_get("output")?,
        ci_run_id: u(r.try_get("ci_run_id")?),
        usage_json: r.try_get("usage_json")?,
    })
}

/// Placeholders `$from .. $from+n-1`.
fn marks(from: usize, n: usize) -> String {
    (0..n)
        .map(|k| format!("${}", from + k))
        .collect::<Vec<_>>()
        .join(", ")
}

/// The store.
#[derive(Clone)]
pub struct Store {
    pool: AnyPool,
}

impl Store {
    pub fn new(pool: AnyPool) -> Self {
        Self { pool }
    }

    pub fn pool(&self) -> &AnyPool {
        &self.pool
    }

    // -------------------------------------------------------------------------- enablement

    /// Whether the workflow is switched on. Absent means off.
    pub async fn is_enabled(&self, repo: &str, name: &str) -> Result<bool, sqlx::Error> {
        let v: Option<i64> =
            sqlx::query_scalar("SELECT enabled FROM workflow_state WHERE repo = $1 AND name = $2")
                .bind(repo.to_owned())
                .bind(name.to_owned())
                .fetch_optional(&self.pool)
                .await?;
        Ok(v.is_some_and(|e| e != 0))
    }

    /// The names switched on in `repo`.
    pub async fn enabled_names(&self, repo: &str) -> Result<Vec<String>, sqlx::Error> {
        sqlx::query_scalar(
            "SELECT name FROM workflow_state WHERE repo = $1 AND enabled = 1 ORDER BY name",
        )
        .bind(repo.to_owned())
        .fetch_all(&self.pool)
        .await
    }

    /// The repositories with a workflow switched on.
    pub async fn enabled_repos(&self) -> Result<Vec<String>, sqlx::Error> {
        sqlx::query_scalar(
            "SELECT DISTINCT repo FROM workflow_state WHERE enabled = 1 ORDER BY repo",
        )
        .fetch_all(&self.pool)
        .await
    }

    /// Switches a workflow on or off. Returns whether the state changed.
    pub async fn set_enabled(
        &self,
        repo: &str,
        name: &str,
        enabled: bool,
        by: &str,
    ) -> Result<bool, sqlx::Error> {
        let now = clock::now();
        let want = i64::from(enabled);
        let had: Option<i64> =
            sqlx::query_scalar("SELECT enabled FROM workflow_state WHERE repo = $1 AND name = $2")
                .bind(repo.to_owned())
                .bind(name.to_owned())
                .fetch_optional(&self.pool)
                .await?;
        match had {
            None => {
                sqlx::query(
                    "INSERT INTO workflow_state (repo, name, enabled, updated_by, updated) VALUES ($1, $2, $3, $4, $5) \
                     ON CONFLICT (repo, name) DO NOTHING",
                )
                .bind(repo.to_owned())
                .bind(name.to_owned())
                .bind(want)
                .bind(by.to_owned())
                .bind(now)
                .execute(&self.pool)
                .await?;
                Ok(enabled)
            }
            Some(h) if h == want => Ok(false),
            Some(_) => {
                sqlx::query("UPDATE workflow_state SET enabled = $3, updated_by = $4, updated = $5 WHERE repo = $1 AND name = $2")
                    .bind(repo.to_owned())
                    .bind(name.to_owned())
                    .bind(want)
                    .bind(by.to_owned())
                    .bind(now)
                    .execute(&self.pool)
                    .await?;
                Ok(true)
            }
        }
    }

    /// Who last switched the workflow, for the principal its runs act for.
    pub async fn enabled_by(&self, repo: &str, name: &str) -> Result<Option<String>, sqlx::Error> {
        sqlx::query_scalar(
            "SELECT updated_by FROM workflow_state WHERE repo = $1 AND name = $2 AND enabled = 1",
        )
        .bind(repo.to_owned())
        .bind(name.to_owned())
        .fetch_optional(&self.pool)
        .await
    }

    // -------------------------------------------------------------------------- runs

    /// Inserts a queued run and its pending steps; returns its id.
    pub async fn create_run(&self, n: &NewRun) -> Result<u64, sqlx::Error> {
        let now = clock::now();
        let mut last = None;
        for _ in 0..5 {
            let mut tx = self.pool.begin().await?;
            let max: i64 = sqlx::query_scalar(
                "SELECT CAST(COALESCE(MAX(id), 0) AS BIGINT) FROM workflow_runs",
            )
            .fetch_one(&mut *tx)
            .await?;
            let id = max + 1;
            let ins = sqlx::query(sqlx::AssertSqlSafe(format!(
                "INSERT INTO workflow_runs ({RUN_COLS}) VALUES ($1, $2, $3, $4, $5, 1, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, 0, 0, '', 0, '', 0)"
            )))
            .bind(id)
            .bind(n.repo.clone())
            .bind(n.workflow.clone())
            .bind(n.source.clone())
            .bind(n.definition.clone())
            .bind(n.trigger)
            .bind(n.started_by.clone())
            .bind(n.requester.clone())
            .bind(n.change.clone())
            .bind(n.change_cs.clone())
            .bind(json_of(&n.event))
            .bind(json_of(&n.inputs))
            .bind(i64::from(n.depth))
            .bind(n.origin.clone())
            .bind(now)
            .execute(&mut *tx)
            .await;
            match ins {
                Ok(_) => {}
                Err(sqlx::Error::Database(d)) if d.is_unique_violation() => {
                    last = Some(sqlx::Error::Database(d));
                    continue;
                }
                Err(e) => return Err(e),
            }
            for (ord, step) in n.steps.iter().enumerate() {
                sqlx::query(
                    "INSERT INTO workflow_steps (run_id, step, ord, state) VALUES ($1, $2, $3, 1)",
                )
                .bind(id)
                .bind(step.clone())
                .bind(i64::try_from(ord).unwrap_or(0))
                .execute(&mut *tx)
                .await?;
            }
            tx.commit().await?;
            return Ok(u(id));
        }
        Err(last.unwrap_or(sqlx::Error::PoolTimedOut))
    }

    pub async fn get_run(&self, id: u64) -> Result<Option<RunRow>, sqlx::Error> {
        let q = format!("SELECT {RUN_COLS} FROM workflow_runs WHERE id = $1");
        match sqlx::query(sqlx::AssertSqlSafe(q))
            .bind(i(id))
            .fetch_optional(&self.pool)
            .await?
        {
            Some(r) => run_of(&r).map(Some),
            None => Ok(None),
        }
    }

    /// Runs of a repository, newest first; `limit + 1` rows so the caller can tell there are more.
    pub async fn list_runs(
        &self,
        repo: &str,
        workflow: &str,
        states: &[RunState],
        before: Option<u64>,
        limit: usize,
    ) -> Result<Vec<RunRow>, sqlx::Error> {
        let mut q = format!("SELECT {RUN_COLS} FROM workflow_runs WHERE repo = $1");
        let mut n = 1;
        if !workflow.is_empty() {
            n += 1;
            q.push_str(&format!(" AND workflow = ${n}"));
        }
        if let Some(_b) = before {
            n += 1;
            q.push_str(&format!(" AND id < ${n}"));
        }
        if !states.is_empty() {
            q.push_str(&format!(" AND state IN ({})", marks(n + 1, states.len())));
        }
        q.push_str(&format!(" ORDER BY id DESC LIMIT {}", limit + 1));
        let mut query = sqlx::query(sqlx::AssertSqlSafe(q)).bind(repo.to_owned());
        if !workflow.is_empty() {
            query = query.bind(workflow.to_owned());
        }
        if let Some(b) = before {
            query = query.bind(i(b));
        }
        for s in states {
            query = query.bind(*s as i64);
        }
        query
            .fetch_all(&self.pool)
            .await?
            .iter()
            .map(run_of)
            .collect()
    }

    /// Queued runs, oldest first.
    pub async fn queued(&self, limit: usize) -> Result<Vec<RunRow>, sqlx::Error> {
        let q = format!(
            "SELECT {RUN_COLS} FROM workflow_runs WHERE state = 1 ORDER BY id LIMIT {limit}"
        );
        sqlx::query(sqlx::AssertSqlSafe(q))
            .fetch_all(&self.pool)
            .await?
            .iter()
            .map(run_of)
            .collect()
    }

    /// Runs of one workflow that are being driven (running or waiting for an approval).
    pub async fn active_count(&self, repo: &str, workflow: &str) -> Result<u64, sqlx::Error> {
        let n: i64 = sqlx::query_scalar(
            "SELECT CAST(COUNT(*) AS BIGINT) FROM workflow_runs WHERE repo = $1 AND workflow = $2 AND state IN (2, 3)",
        )
        .bind(repo.to_owned())
        .bind(workflow.to_owned())
        .fetch_one(&self.pool)
        .await?;
        Ok(u(n))
    }

    pub async fn queued_count(&self, repo: &str, workflow: &str) -> Result<u64, sqlx::Error> {
        let n: i64 = sqlx::query_scalar(
            "SELECT CAST(COUNT(*) AS BIGINT) FROM workflow_runs WHERE repo = $1 AND workflow = $2 AND state = 1",
        )
        .bind(repo.to_owned())
        .bind(workflow.to_owned())
        .fetch_one(&self.pool)
        .await?;
        Ok(u(n))
    }

    /// Runs made for `repo` since `since`.
    pub async fn runs_since(&self, repo: &str, since: i64) -> Result<u64, sqlx::Error> {
        let n: i64 = sqlx::query_scalar(
            "SELECT CAST(COUNT(*) AS BIGINT) FROM workflow_runs WHERE repo = $1 AND created >= $2",
        )
        .bind(repo.to_owned())
        .bind(since)
        .fetch_one(&self.pool)
        .await?;
        Ok(u(n))
    }

    /// Takes a queued run for `owner` unless `max_parallel` runs of its workflow are being driven
    /// already. False when another server took it, it was cancelled, or the workflow is full.
    pub async fn claim(
        &self,
        id: u64,
        owner: &str,
        max_parallel: u64,
    ) -> Result<bool, sqlx::Error> {
        let now = clock::now();
        let r = sqlx::query(
            "UPDATE workflow_runs SET state = 2, owner = $2, started = $3, heartbeat = $3              WHERE id = $1 AND state = 1 AND cancel_requested = 0              AND (SELECT CAST(COUNT(*) AS BIGINT) FROM workflow_runs r2 WHERE r2.repo = workflow_runs.repo                   AND r2.workflow = workflow_runs.workflow AND r2.state IN (2, 3)) < $4",
        )
        .bind(i(id))
        .bind(owner.to_owned())
        .bind(now)
        .bind(i(max_parallel))
        .execute(&self.pool)
        .await?;
        Ok(r.rows_affected() == 1)
    }

    /// Ends every step of a run that is not over (a cancelled run).
    pub async fn cancel_open_steps(&self, run_id: u64) -> Result<u64, sqlx::Error> {
        let r = sqlx::query("UPDATE workflow_steps SET state = 7, finished = $2 WHERE run_id = $1 AND state IN (1, 2, 3)")
            .bind(i(run_id))
            .bind(clock::now())
            .execute(&self.pool)
            .await?;
        Ok(r.rows_affected())
    }

    /// Runs being driven whose server stopped writing its heartbeat.
    pub async fn stale(&self, before: i64) -> Result<Vec<RunRow>, sqlx::Error> {
        let q = format!(
            "SELECT {RUN_COLS} FROM workflow_runs WHERE state IN (2, 3) AND heartbeat < $1 ORDER BY id LIMIT 200"
        );
        sqlx::query(sqlx::AssertSqlSafe(q))
            .bind(before)
            .fetch_all(&self.pool)
            .await?
            .iter()
            .map(run_of)
            .collect()
    }

    /// Takes over a run whose heartbeat is older than `before`. False when it moved on.
    pub async fn take_over(&self, id: u64, owner: &str, before: i64) -> Result<bool, sqlx::Error> {
        let now = clock::now();
        let r = sqlx::query(
            "UPDATE workflow_runs SET owner = $2, heartbeat = $3 WHERE id = $1 AND state IN (2, 3) AND heartbeat < $4",
        )
        .bind(i(id))
        .bind(owner.to_owned())
        .bind(now)
        .bind(before)
        .execute(&self.pool)
        .await?;
        Ok(r.rows_affected() == 1)
    }

    pub async fn heartbeat(&self, id: u64, owner: &str) -> Result<bool, sqlx::Error> {
        let r = sqlx::query("UPDATE workflow_runs SET heartbeat = $3 WHERE id = $1 AND owner = $2 AND state IN (2, 3)")
            .bind(i(id))
            .bind(owner.to_owned())
            .bind(clock::now())
            .execute(&self.pool)
            .await?;
        Ok(r.rows_affected() == 1)
    }

    /// Moves a run between active states. False when it was not in one of `from`.
    pub async fn transition(
        &self,
        id: u64,
        from: &[RunState],
        to: RunState,
    ) -> Result<bool, sqlx::Error> {
        let q = format!(
            "UPDATE workflow_runs SET state = $1 WHERE id = $2 AND state IN ({})",
            marks(3, from.len())
        );
        let mut query = sqlx::query(sqlx::AssertSqlSafe(q))
            .bind(to as i64)
            .bind(i(id));
        for s in from {
            query = query.bind(*s as i64);
        }
        Ok(query.execute(&self.pool).await?.rows_affected() == 1)
    }

    /// Ends a run. False when it was already over.
    pub async fn finish(&self, id: u64, state: RunState, error: &str) -> Result<bool, sqlx::Error> {
        let r = sqlx::query(
            "UPDATE workflow_runs SET state = $2, error = $3, finished = $4 WHERE id = $1 AND state IN (1, 2, 3)",
        )
        .bind(i(id))
        .bind(state as i64)
        .bind(error.to_owned())
        .bind(clock::now())
        .execute(&self.pool)
        .await?;
        Ok(r.rows_affected() == 1)
    }

    pub async fn request_cancel(&self, id: u64) -> Result<(), sqlx::Error> {
        sqlx::query(
            "UPDATE workflow_runs SET cancel_requested = 1 WHERE id = $1 AND state IN (1, 2, 3)",
        )
        .bind(i(id))
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn cancel_requested(&self, id: u64) -> Result<bool, sqlx::Error> {
        let v: Option<i64> =
            sqlx::query_scalar("SELECT cancel_requested FROM workflow_runs WHERE id = $1")
                .bind(i(id))
                .fetch_optional(&self.pool)
                .await?;
        Ok(v.is_some_and(|v| v != 0))
    }

    /// Deletes finished runs older than `before`, with their steps and events.
    pub async fn prune_runs(&self, before: i64) -> Result<u64, sqlx::Error> {
        let ids: Vec<i64> = sqlx::query_scalar(
            "SELECT id FROM workflow_runs WHERE state IN (4, 5, 6) AND finished > 0 AND finished < $1 ORDER BY id LIMIT 500",
        )
        .bind(before)
        .fetch_all(&self.pool)
        .await?;
        for id in &ids {
            for t in ["workflow_events", "workflow_steps"] {
                sqlx::query(sqlx::AssertSqlSafe(format!(
                    "DELETE FROM {t} WHERE run_id = $1"
                )))
                .bind(*id)
                .execute(&self.pool)
                .await?;
            }
            sqlx::query("DELETE FROM workflow_runs WHERE id = $1")
                .bind(*id)
                .execute(&self.pool)
                .await?;
        }
        Ok(ids.len() as u64)
    }

    // -------------------------------------------------------------------------- steps

    pub async fn steps(&self, run_id: u64) -> Result<Vec<StepRow>, sqlx::Error> {
        let q = format!("SELECT {STEP_COLS} FROM workflow_steps WHERE run_id = $1 ORDER BY ord");
        sqlx::query(sqlx::AssertSqlSafe(q))
            .bind(i(run_id))
            .fetch_all(&self.pool)
            .await?
            .iter()
            .map(step_of)
            .collect()
    }

    pub async fn step(&self, run_id: u64, step: &str) -> Result<Option<StepRow>, sqlx::Error> {
        let q = format!("SELECT {STEP_COLS} FROM workflow_steps WHERE run_id = $1 AND step = $2");
        match sqlx::query(sqlx::AssertSqlSafe(q))
            .bind(i(run_id))
            .bind(step.to_owned())
            .fetch_optional(&self.pool)
            .await?
        {
            Some(r) => step_of(&r).map(Some),
            None => Ok(None),
        }
    }

    /// Moves a step between states. False when it was not in one of `from`. A step entering
    /// `Running` or `Waiting` for the first time records its start; entering a terminal state its end.
    pub async fn step_state(
        &self,
        run_id: u64,
        step: &str,
        from: &[StepState],
        to: StepState,
        error: &str,
    ) -> Result<bool, sqlx::Error> {
        let now = clock::now();
        let q = format!(
            "UPDATE workflow_steps SET state = $1, error = $2, \
             started = CASE WHEN started = 0 AND $1 IN (2, 3) THEN $3 ELSE started END, \
             finished = CASE WHEN $1 IN (4, 5, 6, 7) THEN $3 ELSE finished END \
             WHERE run_id = $4 AND step = $5 AND state IN ({})",
            marks(6, from.len())
        );
        let mut query = sqlx::query(sqlx::AssertSqlSafe(q))
            .bind(to as i64)
            .bind(error.to_owned())
            .bind(now)
            .bind(i(run_id))
            .bind(step.to_owned());
        for s in from {
            query = query.bind(*s as i64);
        }
        Ok(query.execute(&self.pool).await?.rows_affected() == 1)
    }

    pub async fn step_agent_run(
        &self,
        run_id: u64,
        step: &str,
        agent_run: u64,
    ) -> Result<(), sqlx::Error> {
        sqlx::query("UPDATE workflow_steps SET agent_run_id = $3 WHERE run_id = $1 AND step = $2")
            .bind(i(run_id))
            .bind(step.to_owned())
            .bind(i(agent_run))
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn step_usage(
        &self,
        run_id: u64,
        step: &str,
        usage_json: &str,
    ) -> Result<(), sqlx::Error> {
        sqlx::query("UPDATE workflow_steps SET usage_json = $3 WHERE run_id = $1 AND step = $2")
            .bind(i(run_id))
            .bind(step.to_owned())
            .bind(usage_json.to_owned())
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn step_result(
        &self,
        run_id: u64,
        step: &str,
        r: &StepResult,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            "UPDATE workflow_steps SET exit_code = $3, timed_out = $4, output = $5, ci_run_id = $6 \
             WHERE run_id = $1 AND step = $2",
        )
        .bind(i(run_id))
        .bind(step.to_owned())
        .bind(i64::from(r.exit_code))
        .bind(i64::from(r.timed_out))
        .bind(r.output.clone())
        .bind(i(r.ci_run_id))
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Records the decision of an approval step and ends it. False when the step was not waiting.
    pub async fn decide(
        &self,
        run_id: u64,
        step: &str,
        by: &str,
        approve: bool,
        comment: &str,
    ) -> Result<bool, sqlx::Error> {
        let to = if approve {
            StepState::Succeeded
        } else {
            StepState::Failed
        };
        let error = if approve { "" } else { "rejected" };
        let r = sqlx::query(
            "UPDATE workflow_steps SET state = $3, decided_by = $4, comment = $5, error = $6, finished = $7 \
             WHERE run_id = $1 AND step = $2 AND state = 3",
        )
        .bind(i(run_id))
        .bind(step.to_owned())
        .bind(to as i64)
        .bind(by.to_owned())
        .bind(comment.to_owned())
        .bind(error.to_owned())
        .bind(clock::now())
        .execute(&self.pool)
        .await?;
        Ok(r.rows_affected() == 1)
    }

    /// The step of a workflow run an agent run belongs to, if any.
    pub async fn step_of_agent_run(
        &self,
        agent_run: u64,
    ) -> Result<Option<(u64, String)>, sqlx::Error> {
        let row = sqlx::query("SELECT run_id, step FROM workflow_steps WHERE agent_run_id = $1")
            .bind(i(agent_run))
            .fetch_optional(&self.pool)
            .await?;
        match row {
            Some(r) => Ok(Some((u(r.try_get("run_id")?), r.try_get("step")?))),
            None => Ok(None),
        }
    }

    // -------------------------------------------------------------------------- events

    /// Appends a snapshot to the event stream of a run; returns its sequence number.
    pub async fn append_event(
        &self,
        run_id: u64,
        step: &str,
        snapshot: &[u8],
    ) -> Result<u64, sqlx::Error> {
        let now_ms = clock::now().saturating_mul(1000);
        let mut last = None;
        for _ in 0..5 {
            let max: i64 =
                sqlx::query_scalar("SELECT CAST(COALESCE(MAX(seq), 0) AS BIGINT) FROM workflow_events WHERE run_id = $1")
                    .bind(i(run_id))
                    .fetch_one(&self.pool)
                    .await?;
            let seq = max + 1;
            let r = sqlx::query(
                "INSERT INTO workflow_events (run_id, seq, time_ms, step, snapshot) VALUES ($1, $2, $3, $4, $5)",
            )
            .bind(i(run_id))
            .bind(seq)
            .bind(now_ms)
            .bind(step.to_owned())
            .bind(snapshot.to_vec())
            .execute(&self.pool)
            .await;
            match r {
                Ok(_) => return Ok(u(seq)),
                Err(sqlx::Error::Database(d)) if d.is_unique_violation() => {
                    last = Some(sqlx::Error::Database(d))
                }
                Err(e) => return Err(e),
            }
        }
        Err(last.unwrap_or(sqlx::Error::PoolTimedOut))
    }

    /// `(seq, time_ms, step, snapshot)` after `after`, oldest first.
    pub async fn events_after(
        &self,
        run_id: u64,
        after: u64,
        limit: usize,
    ) -> Result<Vec<(u64, i64, String, Vec<u8>)>, sqlx::Error> {
        let rows = sqlx::query(sqlx::AssertSqlSafe(format!(
            "SELECT seq, time_ms, step, snapshot FROM workflow_events WHERE run_id = $1 AND seq > $2 ORDER BY seq LIMIT {limit}"
        )))
        .bind(i(run_id))
        .bind(i(after))
        .fetch_all(&self.pool)
        .await?;
        rows.iter()
            .map(|r| {
                Ok((
                    u(r.try_get("seq")?),
                    r.try_get("time_ms")?,
                    r.try_get("step")?,
                    r.try_get("snapshot")?,
                ))
            })
            .collect()
    }

    // -------------------------------------------------------------------------- slots and cursors

    /// Claims `slot` for the workflow: true for the one caller that gets it.
    pub async fn claim_slot(
        &self,
        repo: &str,
        name: &str,
        slot: &str,
    ) -> Result<bool, sqlx::Error> {
        let r = sqlx::query(
            "INSERT INTO workflow_slots (repo, name, slot, at) VALUES ($1, $2, $3, $4) ON CONFLICT (repo, name, slot) DO NOTHING",
        )
        .bind(repo.to_owned())
        .bind(name.to_owned())
        .bind(slot.to_owned())
        .bind(clock::now())
        .execute(&self.pool)
        .await?;
        Ok(r.rows_affected() == 1)
    }

    /// Gives a slot back (the run it was claimed for could not be made).
    pub async fn release_slot(
        &self,
        repo: &str,
        name: &str,
        slot: &str,
    ) -> Result<(), sqlx::Error> {
        sqlx::query("DELETE FROM workflow_slots WHERE repo = $1 AND name = $2 AND slot = $3")
            .bind(repo.to_owned())
            .bind(name.to_owned())
            .bind(slot.to_owned())
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn prune_slots(&self, before: i64) -> Result<(), sqlx::Error> {
        sqlx::query("DELETE FROM workflow_slots WHERE at < $1")
            .bind(before)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn cursor(&self, repo: &str, source: &str) -> Result<Option<i64>, sqlx::Error> {
        sqlx::query_scalar("SELECT value FROM workflow_cursors WHERE repo = $1 AND source = $2")
            .bind(repo.to_owned())
            .bind(source.to_owned())
            .fetch_optional(&self.pool)
            .await
    }

    pub async fn set_cursor(
        &self,
        repo: &str,
        source: &str,
        value: i64,
    ) -> Result<(), sqlx::Error> {
        let done =
            sqlx::query("UPDATE workflow_cursors SET value = $3 WHERE repo = $1 AND source = $2")
                .bind(repo.to_owned())
                .bind(source.to_owned())
                .bind(value)
                .execute(&self.pool)
                .await?;
        if done.rows_affected() == 0 {
            sqlx::query(
                "INSERT INTO workflow_cursors (repo, source, value) VALUES ($1, $2, $3) ON CONFLICT (repo, source) DO NOTHING",
            )
            .bind(repo.to_owned())
            .bind(source.to_owned())
            .bind(value)
            .execute(&self.pool)
            .await?;
        }
        Ok(())
    }

    /// Forgets the cursors of a repository, so the next look starts from "now".
    pub async fn clear_cursors(&self, repo: &str) -> Result<(), sqlx::Error> {
        sqlx::query("DELETE FROM workflow_cursors WHERE repo = $1")
            .bind(repo.to_owned())
            .execute(&self.pool)
            .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn states_round_trip_and_know_when_they_are_over() {
        for s in [
            RunState::Queued,
            RunState::Running,
            RunState::WaitingApproval,
            RunState::Succeeded,
            RunState::Failed,
            RunState::Cancelled,
        ] {
            assert_eq!(RunState::from_i64(s as i64), s);
            assert_eq!(RunState::from_wire(s.to_wire()), Some(s));
        }
        assert!(RunState::Queued.is_active() && RunState::WaitingApproval.is_active());
        assert!(!RunState::Succeeded.is_active());
        assert_eq!(
            RunState::from_wire(wire::WorkflowRunState::Unspecified),
            None
        );
        for s in [StepState::Pending, StepState::Running, StepState::Waiting] {
            assert!(!s.is_terminal());
        }
        for s in [
            StepState::Succeeded,
            StepState::Failed,
            StepState::Skipped,
            StepState::Cancelled,
        ] {
            assert!(s.is_terminal());
            assert_eq!(StepState::from_i64(s as i64), s);
        }
        assert_eq!(marks(3, 3), "$3, $4, $5");
    }
}
