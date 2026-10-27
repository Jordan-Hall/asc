//! Making runs, dispatching them, and driving a run through its steps.
//!
//! A run is a row and a task. [`WorkflowsHub::start`] checks the caps and queues it with one
//! pending step per step of the definition. The dispatcher takes queued runs while there is room,
//! per server and per workflow (`max_parallel_runs`), and [`WorkflowsHub::drive`] then repeats:
//! skip the steps that can no longer run, start the steps whose `needs` succeeded, and wait until
//! one of them changes. Steps are independent tasks, so steps with no `needs` between them run
//! side by side (agent steps take turns, so a budget is spent exactly once).
//!
//! The server that drives a run writes a heartbeat; when it stops, another server takes the run
//! over (`reap_once`). A step that was running is picked up again where it can be (an agent step
//! attaches to its agent run, an approval keeps waiting, checks poll on) and failed where it
//! cannot (a command is not run twice).
//!
//! The limits are the server's: the budget of the definition, the timeouts of the steps and the
//! caps in `[workflows]` are checked here, never by anything the definition's text claims about
//! itself.

use std::collections::{BTreeMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use asc_core::workflow::{Step, StepKind, TriggerKind, Workflow};
use tokio::sync::OwnedSemaphorePermit;
use tokio::task::JoinHandle;
use tonic::Status;

use super::defs::Def;
use super::store::{NewRun, RunRow, RunState, StepRow, StepState};
use super::{LiveRun, SYSTEM_ACTOR, WorkflowsHub, clean, workflow_of};
use crate::agents::errors;
use crate::ai::audit::Entry;
use crate::ai::clock;

/// A heartbeat older than this means the server that drove the run is gone.
const STALE_SECS: i64 = 90;
/// Longest the driver sleeps between looks at its run.
const TICK: Duration = Duration::from_millis(1000);

/// Why a live run was told to stop.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StopWhy {
    Cancelled(String),
    Deadline,
    Shutdown,
}

/// A run to make.
#[derive(Clone, Debug)]
pub(crate) struct Start {
    pub repo: String,
    pub def: Arc<Def>,
    /// The trunk changeset the definition was read from.
    pub source: Vec<u8>,
    pub trigger: TriggerKind,
    pub started_by: String,
    pub requester: String,
    pub change: Vec<u8>,
    pub change_cs: Vec<u8>,
    pub event: BTreeMap<String, String>,
    pub inputs: BTreeMap<String, String>,
    pub depth: u32,
    pub origin: String,
}

/// Why a run was not made.
#[derive(Debug)]
pub(crate) enum StartError {
    /// The definition cannot run.
    Invalid(String),
    /// A cap of the server: `daily` or `queue`.
    Capped(&'static str),
    Sql(String),
}

impl StartError {
    pub(crate) fn status(&self) -> Status {
        match self {
            Self::Invalid(m) => {
                let e = asc_rpc::errors::ErrorInfo {
                    paths: vec![m.clone()],
                    ..Default::default()
                };
                asc_rpc::server::rpc_status(&asc_rpc::errors::RpcError::WorkflowInvalid(e))
            }
            Self::Capped("daily") => Status::resource_exhausted(
                "this repository reached the number of workflow runs it may start in a day",
            ),
            Self::Capped(_) => Status::resource_exhausted(
                "this workflow already has as many runs waiting as it may",
            ),
            Self::Sql(c) => crate::update::internal("workflows", c),
        }
    }
}

/// How a run ended, as the driver saw it.
enum Ended {
    Done(RunState, String),
    /// This server stopped driving it; the run stays as it is for another to take over.
    Interrupted,
}

impl WorkflowsHub {
    // -------------------------------------------------------------------------- making a run

    /// Checks the caps and queues a run of `s.def`.
    pub(crate) async fn start(&self, s: Start) -> Result<RunRow, StartError> {
        let wf = match s.def.workflow() {
            Some(w) => w,
            None => return Err(StartError::Invalid(s.def.error())),
        };
        let sql = |e: sqlx::Error| StartError::Sql(crate::ai::store::class(&e));
        let day_ago = clock::now() - 86_400;
        if self.store.runs_since(&s.repo, day_ago).await.map_err(sql)?
            >= u64::from(self.cfg.max_runs_per_repo_per_day)
        {
            return Err(StartError::Capped("daily"));
        }
        if self
            .store
            .queued_count(&s.repo, &s.def.name)
            .await
            .map_err(sql)?
            >= u64::from(self.cfg.max_queued_per_workflow)
        {
            return Err(StartError::Capped("queue"));
        }
        let id = self
            .store
            .create_run(&NewRun {
                repo: s.repo.clone(),
                workflow: s.def.name.clone(),
                source: s.source.clone(),
                definition: String::from_utf8_lossy(&s.def.text).into_owned(),
                trigger: i64::from(super::trigger_to_wire(s.trigger) as i32),
                started_by: s.started_by.clone(),
                requester: s.requester.clone(),
                change: s.change.clone(),
                change_cs: s.change_cs.clone(),
                event: s.event.clone(),
                inputs: s.inputs.clone(),
                depth: s.depth,
                origin: s.origin.clone(),
                steps: wf.steps.iter().map(|st| st.id.clone()).collect(),
            })
            .await
            .map_err(sql)?;
        let run = self
            .store
            .get_run(id)
            .await
            .map_err(sql)?
            .ok_or_else(|| StartError::Sql("missing".into()))?;
        let actor = if s.started_by.starts_with("system:") {
            SYSTEM_ACTOR
        } else {
            s.requester.as_str()
        };
        self.audit(Entry {
            action: "workflow.triggered",
            on_behalf: (actor == SYSTEM_ACTOR).then_some(s.requester.as_str()),
            repo: &s.repo,
            target: &id.to_string(),
            details: &[
                ("workflow", s.def.name.clone()),
                ("run", id.to_string()),
                ("trigger", s.trigger.as_str().to_owned()),
            ],
            ..Entry::new("workflow.triggered", actor)
        })
        .await;
        self.emit(id, "").await;
        self.wake.notify_one();
        Ok(run)
    }

    // -------------------------------------------------------------------------- the workers

    /// Starts the dispatcher, the event reader, the scheduler and the reaper.
    pub fn start_workers(self: &Arc<Self>) -> Vec<JoinHandle<()>> {
        let mut tasks = Vec::new();
        let hub = self.clone();
        tasks.push(tokio::spawn(async move { hub.dispatch_loop().await }));
        let hub = self.clone();
        tasks.push(tokio::spawn(async move { hub.poll_loop().await }));
        let hub = self.clone();
        tasks.push(tokio::spawn(async move { hub.reap_loop().await }));
        tasks
    }

    /// Tells live runs to stop (they stay as they are, for another server to take over) and
    /// stops the workers' loops.
    pub fn shutdown(&self) {
        self.shutdown.cancel();
        for live in self
            .live
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
        {
            *live
                .why
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(StopWhy::Shutdown);
            live.stop.cancel();
        }
    }

    async fn dispatch_loop(self: Arc<Self>) {
        loop {
            if self.shutdown.is_cancelled() {
                return;
            }
            if let Err(e) = self.dispatch_once().await {
                tracing::error!(class = %crate::ai::store::class(&e), "workflow dispatcher");
            }
            tokio::select! {
                () = self.wake.notified() => {}
                () = tokio::time::sleep(Duration::from_secs(2)) => {}
                () = self.shutdown.cancelled() => return,
            }
        }
    }

    async fn poll_loop(self: Arc<Self>) {
        let every = Duration::from_secs(u64::from(self.cfg.poll_interval_secs));
        loop {
            tokio::select! {
                () = tokio::time::sleep(every) => {}
                () = self.shutdown.cancelled() => return,
            }
            self.poll_all().await;
            self.tick_schedules(clock::now()).await;
        }
    }

    async fn reap_loop(self: Arc<Self>) {
        let mut n = 0u32;
        loop {
            tokio::select! {
                () = tokio::time::sleep(Duration::from_secs(30)) => {}
                () = self.shutdown.cancelled() => return,
            }
            if let Err(e) = self.reap_once().await {
                tracing::error!(class = %crate::ai::store::class(&e), "workflow reaper");
            }
            n += 1;
            if n.is_multiple_of(120) {
                let keep = i64::from(self.cfg.run_retention_days) * 86_400;
                let _ = self.store.prune_runs(clock::now() - keep).await;
                let _ = self.store.prune_slots(clock::now() - 7 * 86_400).await;
            }
        }
    }

    /// Takes queued runs while there is room; returns how many were started.
    pub async fn dispatch_once(self: &Arc<Self>) -> Result<usize, sqlx::Error> {
        let queued = self.store.queued(64).await?;
        let mut started = 0;
        for run in queued {
            let Ok(permit) = self.permits.clone().try_acquire_owned() else {
                break;
            };
            let wf = match workflow_of(&run) {
                Ok(w) => w,
                Err(why) => {
                    self.fail_unusable(&run, &why).await;
                    continue;
                }
            };
            let max = u64::from(wf.max_parallel_runs.max(1));
            if !self.store.claim(run.id, &self.instance, max).await? {
                continue;
            }
            started += 1;
            let hub = self.clone();
            tokio::spawn(async move { hub.drive(run.id, permit).await });
        }
        Ok(started)
    }

    async fn fail_unusable(&self, run: &RunRow, why: &str) {
        let _ = self.store.cancel_open_steps(run.id).await;
        let _ = self
            .store
            .finish(
                run.id,
                RunState::Failed,
                &clean(&format!("the definition cannot run: {why}"), 500),
            )
            .await;
        self.emit(run.id, "").await;
    }

    /// Takes over runs whose server stopped; returns how many.
    pub async fn reap_once(self: &Arc<Self>) -> Result<usize, sqlx::Error> {
        let before = clock::now() - STALE_SECS;
        let stale = self.store.stale(before).await?;
        let mut n = 0;
        for run in stale {
            if self.live(run.id).is_some() {
                continue;
            }
            let Ok(permit) = self.permits.clone().try_acquire_owned() else {
                break;
            };
            if !self.store.take_over(run.id, &self.instance, before).await? {
                continue;
            }
            n += 1;
            let hub = self.clone();
            tokio::spawn(async move { hub.drive(run.id, permit).await });
        }
        Ok(n)
    }

    // -------------------------------------------------------------------------- stopping

    /// Cancels a run. A queued run is cancelled at once; a running one is told to stop and the
    /// call waits a few seconds for it to.
    pub async fn cancel_run(
        self: &Arc<Self>,
        id: u64,
        reason: &str,
        by: &str,
    ) -> Result<RunRow, Status> {
        let sql =
            |e: sqlx::Error| crate::update::internal("workflows", &crate::ai::store::class(&e));
        let run = self
            .store
            .get_run(id)
            .await
            .map_err(sql)?
            .ok_or_else(|| Status::not_found("no such run"))?;
        if run.state == RunState::Cancelled {
            return Ok(run);
        }
        if !run.state.is_active() {
            return Err(errors::run_not_active(run.state.name()));
        }
        let reason = clean(reason, 256);
        if run.state == RunState::Queued {
            // Ending the run first makes the dispatcher's claim fail; then the steps follow.
            if self
                .store
                .finish(id, RunState::Cancelled, &format!("cancelled: {reason}"))
                .await
                .map_err(sql)?
            {
                let _ = self.store.cancel_open_steps(id).await;
                self.emit(id, "").await;
                return self
                    .store
                    .get_run(id)
                    .await
                    .map_err(sql)?
                    .ok_or_else(|| Status::not_found("no such run"));
            }
        }
        self.store.request_cancel(id).await.map_err(sql)?;
        if let Some(live) = self.live(id) {
            *live
                .why
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) =
                Some(StopWhy::Cancelled(reason.clone()));
            live.stop.cancel();
        }
        let _ = by;
        for _ in 0..40 {
            let now = self
                .store
                .get_run(id)
                .await
                .map_err(sql)?
                .ok_or_else(|| Status::not_found("no such run"))?;
            if !now.state.is_active() {
                return Ok(now);
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        self.store
            .get_run(id)
            .await
            .map_err(sql)?
            .ok_or_else(|| Status::not_found("no such run"))
    }

    // -------------------------------------------------------------------------- driving

    async fn drive(self: Arc<Self>, id: u64, _permit: OwnedSemaphorePermit) {
        let Ok(Some(run)) = self.store.get_run(id).await else {
            return;
        };
        let live = Arc::new(LiveRun {
            stop: self.shutdown.child_token(),
            why: std::sync::Mutex::new(None),
            changed: tokio::sync::Notify::new(),
            agent_turn: tokio::sync::Mutex::new(()),
        });
        self.live
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(id, live.clone());
        self.emit(id, "").await;
        let ended = self.run_loop(&run, &live).await;
        self.live
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&id);
        if let Ended::Done(state, error) = ended {
            let error = clean(&error, 500);
            if let Err(e) = self.store.finish(id, state, &error).await {
                tracing::error!(class = %crate::ai::store::class(&e), "workflow run not finished");
            }
            if state == RunState::Cancelled {
                let _ = self.store.cancel_open_steps(id).await;
            }
            self.emit(id, "").await;
        }
        self.wake.notify_one();
    }

    /// The deadline of the run, from its definition's `max_runtime_seconds`.
    fn deadline(wf: &Workflow, run: &RunRow) -> Option<i64> {
        let secs = i64::from(wf.limits.max_runtime_seconds);
        (secs > 0).then(|| run.started.max(1) + secs)
    }

    async fn run_loop(self: &Arc<Self>, run: &RunRow, live: &Arc<LiveRun>) -> Ended {
        let wf = match workflow_of(run) {
            Ok(w) => Arc::new(w),
            Err(why) => {
                return Ended::Done(
                    RunState::Failed,
                    format!("the definition cannot run: {why}"),
                );
            }
        };
        let id = run.id;
        let deadline = Self::deadline(&wf, run);
        let mut spawned: HashSet<String> = HashSet::new();
        let mut handles: Vec<(String, JoinHandle<()>)> = Vec::new();
        let sql_end = |e: &sqlx::Error| {
            Ended::Done(
                RunState::Failed,
                format!("database error: {}", crate::ai::store::class(e)),
            )
        };
        loop {
            if self.shutdown.is_cancelled() {
                for (_, h) in &handles {
                    h.abort();
                }
                return Ended::Interrupted;
            }
            match self.store.heartbeat(id, &self.instance).await {
                Ok(true) => {}
                // Another server took the run over; it drives it now.
                Ok(false) => {
                    for (_, h) in &handles {
                        h.abort();
                    }
                    return Ended::Interrupted;
                }
                Err(e) => return sql_end(&e),
            }
            if !live.stop.is_cancelled() {
                match self.store.cancel_requested(id).await {
                    Ok(true) => {
                        *live
                            .why
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner) =
                            Some(StopWhy::Cancelled("cancelled".into()));
                        live.stop.cancel();
                    }
                    Ok(false) => {}
                    Err(e) => return sql_end(&e),
                }
            }
            if !live.stop.is_cancelled() && deadline.is_some_and(|d| clock::now() >= d) {
                *live
                    .why
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(StopWhy::Deadline);
                live.stop.cancel();
            }
            if live.stop.is_cancelled() {
                let why = live
                    .why
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .clone();
                // Give the steps a moment to wind down (they stop their agent runs and commands).
                let wait = async {
                    for (_, h) in handles.iter_mut() {
                        let _ = h.await;
                    }
                };
                let _ = tokio::time::timeout(Duration::from_secs(20), wait).await;
                for (_, h) in &handles {
                    h.abort();
                }
                return match why {
                    Some(StopWhy::Shutdown) => Ended::Interrupted,
                    Some(StopWhy::Deadline) => {
                        let _ = self.store.cancel_open_steps(id).await;
                        Ended::Done(RunState::Failed, "the workflow ran past max_runtime_seconds of its budget (BUDGET_EXCEEDED runtime)".into())
                    }
                    Some(StopWhy::Cancelled(r)) => {
                        Ended::Done(RunState::Cancelled, format!("cancelled: {r}"))
                    }
                    None => Ended::Done(RunState::Cancelled, "cancelled".into()),
                };
            }

            // A step task that ended without recording its step (a panic) fails the step.
            for (step, h) in &mut handles {
                if h.is_finished()
                    && let Ok(Some(row)) = self.store.step(id, step).await
                    && !row.state.is_terminal()
                {
                    let _ = self
                        .store
                        .step_state(
                            id,
                            step,
                            &[StepState::Pending, StepState::Running, StepState::Waiting],
                            StepState::Failed,
                            "internal error",
                        )
                        .await;
                    self.emit(id, step).await;
                }
            }
            handles.retain(|(_, h)| !h.is_finished());

            let steps = match self.store.steps(id).await {
                Ok(s) => s,
                Err(e) => return sql_end(&e),
            };
            let state_of = |sid: &str| steps.iter().find(|s| s.step == sid).map(|s| s.state);
            // Steps that can no longer run, and steps that can start now.
            let mut ready: Vec<&Step> = Vec::new();
            for row in steps.iter().filter(|s| s.state == StepState::Pending) {
                let Some(def) = wf.step(&row.step) else {
                    continue;
                };
                let (mut blocked, mut dead) = (false, false);
                for need in &def.needs {
                    match state_of(need) {
                        Some(StepState::Succeeded) => {}
                        Some(StepState::Failed)
                            if wf.step(need).is_some_and(|n| n.continue_on_failure) => {}
                        Some(StepState::Pending | StepState::Running | StepState::Waiting) => {
                            blocked = true
                        }
                        _ => dead = true,
                    }
                }
                if dead {
                    if self
                        .store
                        .step_state(
                            id,
                            &row.step,
                            &[StepState::Pending],
                            StepState::Skipped,
                            "a step it needs did not succeed",
                        )
                        .await
                        .unwrap_or(false)
                    {
                        self.emit(id, &row.step).await;
                    }
                } else if !blocked {
                    ready.push(def);
                }
            }
            for def in ready {
                if !matches!(
                    self.store
                        .step_state(id, &def.id, &[StepState::Pending], StepState::Running, "")
                        .await,
                    Ok(true)
                ) {
                    continue;
                }
                self.emit(id, &def.id).await;
                spawned.insert(def.id.clone());
                handles.push((
                    def.id.clone(),
                    self.spawn_step(id, wf.clone(), def.id.clone(), live.clone(), false),
                ));
            }
            // Steps this server did not start (it took the run over): pick them up.
            for row in steps
                .iter()
                .filter(|s| matches!(s.state, StepState::Running | StepState::Waiting))
            {
                if spawned.contains(&row.step) {
                    continue;
                }
                spawned.insert(row.step.clone());
                let kind = wf.step(&row.step).map(|d| d.kind);
                if row.state == StepState::Running && kind == Some(StepKind::Command) {
                    let _ = self
                        .store
                        .step_state(
                            id,
                            &row.step,
                            &[StepState::Running],
                            StepState::Failed,
                            "interrupted by a restart of the server",
                        )
                        .await;
                    self.emit(id, &row.step).await;
                    continue;
                }
                handles.push((
                    row.step.clone(),
                    self.spawn_step(id, wf.clone(), row.step.clone(), live.clone(), true),
                ));
            }

            let steps = match self.store.steps(id).await {
                Ok(s) => s,
                Err(e) => return sql_end(&e),
            };
            if steps.iter().all(|s| s.state.is_terminal()) {
                return Self::verdict(&wf, &steps);
            }
            let waiting = steps.iter().any(|s| s.state == StepState::Waiting);
            let running = steps.iter().any(|s| s.state == StepState::Running);
            let (from, to) = if waiting && !running {
                (RunState::Running, RunState::WaitingApproval)
            } else {
                (RunState::WaitingApproval, RunState::Running)
            };
            if matches!(self.store.transition(id, &[from], to).await, Ok(true)) {
                self.emit(id, "").await;
            }
            tokio::select! {
                () = live.changed.notified() => {}
                () = tokio::time::sleep(TICK) => {}
                () = live.stop.cancelled() => {}
            }
        }
    }

    fn verdict(wf: &Workflow, steps: &[StepRow]) -> Ended {
        let bad: Vec<&StepRow> = steps
            .iter()
            .filter(|s| match s.state {
                StepState::Succeeded => false,
                StepState::Failed => !wf.step(&s.step).is_some_and(|d| d.continue_on_failure),
                _ => true,
            })
            .collect();
        match bad.iter().find(|s| s.state == StepState::Failed) {
            None if bad.is_empty() => Ended::Done(RunState::Succeeded, String::new()),
            Some(s) => Ended::Done(
                RunState::Failed,
                format!("step {} failed: {}", s.step, s.error),
            ),
            None => Ended::Done(RunState::Failed, "a step did not run".into()),
        }
    }

    fn spawn_step(
        self: &Arc<Self>,
        run_id: u64,
        wf: Arc<Workflow>,
        step: String,
        live: Arc<LiveRun>,
        resume: bool,
    ) -> JoinHandle<()> {
        let hub = self.clone();
        tokio::spawn(async move {
            hub.run_step(run_id, wf, step, live, resume).await;
        })
    }

    // -------------------------------------------------------------------------- approvals

    /// Records the decision of a person on an approval step.
    #[allow(clippy::too_many_arguments)] // the fields of one decision, called from one place
    pub async fn decide_approval(
        self: &Arc<Self>,
        run_id: u64,
        step: &str,
        by_principal: &str,
        by_handle: &str,
        approve: bool,
        comment: &str,
        is_in_role: impl Fn(&str) -> bool,
    ) -> Result<RunRow, Status> {
        let sql =
            |e: sqlx::Error| crate::update::internal("workflows", &crate::ai::store::class(&e));
        let run = self
            .store
            .get_run(run_id)
            .await
            .map_err(sql)?
            .ok_or_else(|| Status::not_found("no such run"))?;
        let wf = workflow_of(&run).map_err(|_| {
            Status::failed_precondition("the definition of this run cannot be read")
        })?;
        let def = wf
            .step(step)
            .ok_or_else(|| Status::not_found("no such step"))?;
        if def.kind != StepKind::Approval {
            return Err(Status::failed_precondition("this step is not an approval"));
        }
        if !run.state.is_active() {
            return Err(errors::run_not_active(run.state.name()));
        }
        let row = self
            .store
            .step(run_id, step)
            .await
            .map_err(sql)?
            .ok_or_else(|| Status::not_found("no such step"))?;
        if row.state != StepState::Waiting {
            return Err(Status::failed_precondition(
                "this approval is not waiting for a decision",
            ));
        }
        if comment.len() > 1024 || comment.contains('\0') {
            return Err(Status::invalid_argument(
                "a comment is at most 1,024 bytes of text",
            ));
        }
        let admitted = asc_core::workflow::approver_admits(&def.approvers, by_handle, is_in_role);
        // The person who started the run decides it only when the step names them: a role or a
        // pattern does not count.
        let starter = !run.started_by.is_empty() && run.started_by.eq_ignore_ascii_case(by_handle);
        let named = def.approvers.iter().any(|a| match a {
            asc_core::workflow::Approver::User(g) => {
                !g.contains('*') && g.eq_ignore_ascii_case(by_handle)
            }
            asc_core::workflow::Approver::Role(_) => false,
        });
        if !admitted || (starter && !named) {
            return Err(Status::permission_denied(
                "you are not one of the approvers of this step",
            ));
        }
        let comment = clean(comment, 1024);
        if !self
            .store
            .decide(run_id, step, by_handle, approve, &comment)
            .await
            .map_err(sql)?
        {
            return Err(Status::failed_precondition(
                "this approval was decided meanwhile",
            ));
        }
        self.audit(Entry {
            action: "workflow.approval.decided",
            repo: &run.repo,
            target: &run_id.to_string(),
            details: &[
                ("workflow", run.workflow.clone()),
                ("run", run_id.to_string()),
                (
                    "decision",
                    if approve { "approved" } else { "rejected" }.to_owned(),
                ),
            ],
            ..Entry::new("workflow.approval.decided", by_principal)
        })
        .await;
        self.emit(run_id, step).await;
        if let Some(live) = self.live(run_id) {
            live.changed.notify_one();
        }
        self.wake.notify_one();
        self.store
            .get_run(run_id)
            .await
            .map_err(sql)?
            .ok_or_else(|| Status::not_found("no such run"))
    }
}
