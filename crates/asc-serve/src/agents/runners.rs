//! The agent runners: the server processes that execute agent runs.
//!
//! Every process with agents switched on is a runner. It writes its row in `agent_runners` (name,
//! slots, CPUs, version, start time) every [`BEAT_SECS`] and stamps the runs it takes with its
//! name (`agent_runs.runner`). The site administrator's runners page reads the rows and the runs
//! each runner is executing through `Maintenance.ListAgentRunners`. A runner whose heartbeat is
//! older than [`STALE_SECS`] is offline; one offline for [`KEEP_SECS`] is removed.

use std::collections::HashMap;

use sqlx::{AnyPool, Row};

use crate::ai::clock;

/// What `Version.features` says when the server answers `Maintenance.ListAgentRunners`.
pub const FEATURE: &str = "agent-runners.v1";

/// Seconds between two heartbeats of a runner.
pub const BEAT_SECS: u64 = 10;

/// A heartbeat older than this means the runner is offline. The run reaper uses the same bound.
pub const STALE_SECS: i64 = 90;

/// Seconds an offline runner stays listed (7 days).
pub const KEEP_SECS: i64 = 7 * 86_400;

/// Longest runner name.
pub const MAX_NAME: usize = 63;

/// Whether `name` can name a runner: 1 to [`MAX_NAME`] ASCII letters, digits, `.`, `-` or `_`.
pub fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_NAME
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_'))
}

/// The name of this process when `agents.runner_name` is not set: the host name, cut to the
/// characters a name may hold, or `asc-serve` when there is none.
pub fn default_name() -> String {
    let host = std::fs::read_to_string("/proc/sys/kernel/hostname")
        .ok()
        .or_else(|| std::env::var("HOSTNAME").ok())
        .or_else(|| std::env::var("COMPUTERNAME").ok())
        .unwrap_or_default();
    name_of(&host)
}

/// `raw` cut to a valid runner name; `asc-serve` when nothing is left.
pub fn name_of(raw: &str) -> String {
    let cut: String = raw
        .trim()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
        .take(MAX_NAME)
        .collect();
    if cut.is_empty() {
        "asc-serve".to_owned()
    } else {
        cut
    }
}

/// The CPUs this process may use.
pub fn vcpu() -> u32 {
    std::thread::available_parallelism().map_or(1, |n| u32::try_from(n.get()).unwrap_or(u32::MAX))
}

/// One run a runner is executing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ActiveRun {
    pub id: u64,
    pub repo: String,
    pub title: String,
    pub agent: String,
    /// Unix seconds.
    pub started: i64,
}

/// One runner as the runners page shows it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunnerView {
    pub name: String,
    pub slots: u32,
    pub vcpu: u32,
    pub version: String,
    /// Unix seconds.
    pub started: i64,
    /// Unix seconds of the last heartbeat.
    pub last_seen: i64,
    pub online: bool,
    /// The runs it is executing (running or waiting for input), oldest first.
    pub runs: Vec<ActiveRun>,
}

/// This process as a runner, and the table of all runners.
pub struct Runners {
    pool: AnyPool,
    name: String,
    slots: u32,
    vcpu: u32,
    version: String,
    started: i64,
}

impl Runners {
    /// This process, named `name`, with `slots` runs at once.
    pub fn new(pool: AnyPool, name: String, slots: u32) -> Self {
        Self {
            pool,
            name,
            slots,
            vcpu: vcpu(),
            version: env!("CARGO_PKG_VERSION").to_owned(),
            started: clock::now(),
        }
    }

    /// The name this process stamps on the runs it takes.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Writes this process's row with the heartbeat `now`, and removes the runners offline for
    /// longer than [`KEEP_SECS`].
    pub async fn beat(&self, now: i64) -> Result<(), sqlx::Error> {
        sqlx::query(
            "INSERT INTO agent_runners (name, slots, vcpu, version, started, heartbeat) \
             VALUES ($1, $2, $3, $4, $5, $6) \
             ON CONFLICT (name) DO UPDATE SET slots = $2, vcpu = $3, version = $4, started = $5, heartbeat = $6",
        )
        .bind(&self.name)
        .bind(i64::from(self.slots))
        .bind(i64::from(self.vcpu))
        .bind(&self.version)
        .bind(self.started)
        .bind(now)
        .execute(&self.pool)
        .await?;
        sqlx::query("DELETE FROM agent_runners WHERE heartbeat < $1")
            .bind(now - KEEP_SECS)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Every listed runner, online ones first, then by name, with the runs each is executing.
    pub async fn list(&self, now: i64) -> Result<Vec<RunnerView>, sqlx::Error> {
        let rows = sqlx::query(
            "SELECT name, slots, vcpu, version, started, heartbeat FROM agent_runners \
             WHERE heartbeat >= $1 ORDER BY name LIMIT 500",
        )
        .bind(now - KEEP_SECS)
        .fetch_all(&self.pool)
        .await?;
        let mut runs: HashMap<String, Vec<ActiveRun>> = HashMap::new();
        for r in sqlx::query(
            "SELECT id, repo, title, agent, started, runner FROM agent_runs \
             WHERE state IN (2, 3) AND runner <> '' ORDER BY id LIMIT 5000",
        )
        .fetch_all(&self.pool)
        .await?
        {
            let runner: String = r.try_get("runner")?;
            runs.entry(runner).or_default().push(ActiveRun {
                id: u64::try_from(r.try_get::<i64, _>("id")?).unwrap_or(0),
                repo: r.try_get("repo")?,
                title: r.try_get("title")?,
                agent: r.try_get("agent")?,
                started: r.try_get("started")?,
            });
        }
        let mut out = Vec::with_capacity(rows.len());
        for r in rows {
            let name: String = r.try_get("name")?;
            let last_seen: i64 = r.try_get("heartbeat")?;
            out.push(RunnerView {
                runs: runs.remove(&name).unwrap_or_default(),
                slots: u32::try_from(r.try_get::<i64, _>("slots")?).unwrap_or(0),
                vcpu: u32::try_from(r.try_get::<i64, _>("vcpu")?).unwrap_or(0),
                version: r.try_get("version")?,
                started: r.try_get("started")?,
                online: last_seen >= now - STALE_SECS,
                last_seen,
                name,
            });
        }
        out.sort_by(|a, b| b.online.cmp(&a.online).then_with(|| a.name.cmp(&b.name)));
        Ok(out)
    }
}
