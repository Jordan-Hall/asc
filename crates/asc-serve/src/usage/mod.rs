//! Usage of an account or an organisation (`usage.proto`, docs/BUILDOUT.md section 25).
//!
//! Sources: AI spend from `ai_usage` and `agent_spend`, agent minutes from `agent_runs`, storage
//! and bandwidth from `usage_counters`. Which rows a caller sees is decided by the service from
//! the subject (see [`crate::settings::access`]); every query here is scoped to one owner and
//! takes the owner as a bound parameter, never as part of the SQL text.

pub mod rpc;
pub mod store;

#[cfg(test)]
mod tests;

use std::sync::OnceLock;

use serde::Deserialize;
use sqlx::AnyPool;
use tonic::Status;

use crate::ai::clock;
use crate::settings::access::Subject;
pub use store::{Quota, Store};

/// What `Version.features` adds for this service.
pub const FEATURE: &str = "usage.v1";
/// Longest range a spend report may span, in days.
pub const MAX_REPORT_DAYS: i64 = 400;

/// `[usage]`: how long the counters are kept.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct UsageConfig {
    /// Days of storage and bandwidth counters kept (7 to 3650).
    pub retention_days: u32,
}

impl Default for UsageConfig {
    fn default() -> Self {
        Self {
            retention_days: 400,
        }
    }
}

impl UsageConfig {
    pub fn validate(&self) -> Result<(), String> {
        if (7..=3650).contains(&self.retention_days) {
            Ok(())
        } else {
            Err("[usage] retention_days is 7 to 3650".into())
        }
    }
}

/// The four measures of one owner for one period, before quotas are applied.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Totals {
    pub storage_bytes: u64,
    pub agent_minutes: u64,
    pub ai_spend: u64,
    pub bandwidth_bytes: u64,
    pub byo_ai_spend: u64,
    pub ai_keys: u32,
    pub byo_storage: bool,
    pub repos: Vec<RepoRow>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RepoRow {
    pub repo: String,
    pub storage_bytes: u64,
    pub agent_minutes: u64,
    pub ai_spend: u64,
    pub bandwidth_bytes: u64,
}

/// A calendar month, UTC.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Period {
    pub start: i64,
    pub end: i64,
}

impl Period {
    /// `YYYY-MM`, or this month for the empty string. A month in the future, or one that
    /// is not a month, is `INVALID_ARGUMENT`.
    pub fn parse(s: &str, now: i64) -> Result<Self, Status> {
        let start = if s.is_empty() {
            clock::month_start(now)
        } else {
            let bad = || Status::invalid_argument("period is YYYY-MM");
            let (y, m) = s.split_once('-').ok_or_else(bad)?;
            if y.len() != 4 || m.len() != 2 {
                return Err(bad());
            }
            let (y, m): (i64, u32) = (y.parse().map_err(|_| bad())?, m.parse().map_err(|_| bad())?);
            if !(1970..=9999).contains(&y) || !(1..=12).contains(&m) {
                return Err(bad());
            }
            let t = clock::month_start_of(y, m);
            if t > now {
                return Err(Status::invalid_argument("period is in the future"));
            }
            t
        };
        Ok(Self {
            start,
            end: clock::next_month_start(start),
        })
    }

    pub fn label(&self) -> String {
        clock::month(self.start)
    }
}

/// The hub of the usage service.
pub struct UsageHub {
    pub(crate) store: Store,
    pub(crate) cfg: UsageConfig,
    pub(crate) ops: crate::ai::ops::Ops,
    pub(crate) audit: std::sync::Arc<dyn crate::ai::audit::Audit>,
}

/// The place the single [`UsageHub`] is registered.
#[derive(Default)]
pub struct UsageSlot(OnceLock<std::sync::Arc<UsageHub>>);

impl UsageHub {
    pub fn new(
        pool: AnyPool,
        cfg: UsageConfig,
        audit: std::sync::Arc<dyn crate::ai::audit::Audit>,
    ) -> std::sync::Arc<Self> {
        std::sync::Arc::new(Self {
            store: Store::new(pool.clone()),
            cfg,
            ops: crate::ai::ops::Ops::new(pool),
            audit,
        })
    }

    /// Adds served bytes to a repository's bandwidth for today. Called by the server as it
    /// serves objects; a failure is logged and never fails the request.
    pub async fn add_bandwidth(&self, repo: &str, bytes: u64) {
        if bytes == 0 {
            return;
        }
        if let Err(e) = self
            .store
            .report(repo, &clock::day(clock::now()), None, bytes, false)
            .await
        {
            tracing::warn!(class = %crate::ai::store::class(&e), "bandwidth counter");
        }
    }

    /// Deletes counters past the retention. Returns the rows removed.
    pub async fn sweep(&self) -> u64 {
        let cutoff = clock::day(clock::now() - i64::from(self.cfg.retention_days) * 86_400);
        self.store.sweep(&cutoff).await.unwrap_or(0)
    }

    /// The totals of `subject` in `period`.
    pub async fn totals(
        &self,
        subject: &Subject,
        p: Period,
        now: i64,
    ) -> Result<Totals, sqlx::Error> {
        self.store.totals(subject, p, now).await
    }
}

impl UsageSlot {
    pub fn set(&self, h: std::sync::Arc<UsageHub>) -> bool {
        self.0.set(h).is_ok()
    }
    pub fn get(&self) -> Option<&std::sync::Arc<UsageHub>> {
        self.0.get()
    }
}
