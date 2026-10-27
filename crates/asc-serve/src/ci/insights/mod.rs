//! CI failure grouping and flaky tests (`ciinsights.proto`; docs/BUILDOUT.md section 8).
//!
//! A runner (or the forge adapter) reports the test cases of a job with `ReportTestCases`. The
//! server masks secrets out of each failure message, normalises it ([`asc_ci::insights`]), hashes
//! it with the job and the test name into a signature, and files the failure in the group of that
//! signature. From the results of one test it also keeps the verdict over its last runs: how often
//! the verdict flipped on one head, and whether that makes the test flaky under the repository
//! policy (`[ci] flaky_threshold`).
//!
//! * A group is *new* when none of its occurrences came from a run on the trunk.
//! * A result is *stale* when the change it was made for has a newer version
//!   ([`asc_ci::insights::is_stale`]); a stale failure still shows, marked.
//! * A group can be quarantined, which a landing check reads as "ignore this failure". It needs
//!   the policy to allow it (`[ci] allow_quarantine`) and a maintainer
//!   ([`asc_ci::insights::may_quarantine`]). [`store::quarantine`] is the list a check reads.
//! * Messages are masked with the built-in secret rules and the repository's own, then cut to
//!   8 KiB, before they are stored; `test_cases` older than `[ci_insights] retention_days` are
//!   deleted by the worker in [`retention`].

pub mod retention;
pub mod rpc;
pub mod store;

#[cfg(test)]
mod tests;

use std::sync::{Arc, OnceLock};

use crate::policy::PolicyStore;
use crate::registry::{Extensions, Registry};

/// What `Version.features` adds for the `CiInsights` service.
pub const FEATURE: &str = "ci-insights.v1";
/// Test cases in one `ReportTestCases`.
pub const MAX_CASES: usize = 5_000;
/// Bytes of one report (names and messages).
pub const MAX_REPORT_BYTES: usize = 16 * 1024 * 1024;
/// Longest test name, in bytes.
pub const MAX_NAME: usize = 512;
/// Longest stored message, in bytes.
pub const MAX_MESSAGE: usize = 8 * 1024;
/// Longest group note, in bytes.
pub const MAX_NOTE: usize = 1024;
/// Groups in one `ListFailureGroups` page when the request names no limit.
pub const DEFAULT_GROUPS: usize = 30;
/// Groups in one page, at most.
pub const MAX_GROUPS: usize = 100;
/// Occurrences in one `GetFailureGroup` page when the request names no limit.
pub const DEFAULT_OCCURRENCES: usize = 20;
/// Occurrences in one page, at most.
pub const MAX_OCCURRENCES: usize = 100;
/// Days of `FlakyHistory` when the request names none.
pub const DEFAULT_WINDOW_DAYS: u32 = 30;
/// Days of `FlakyHistory`, at most.
pub const MAX_WINDOW_DAYS: u32 = 180;
/// Tests in one `ListFlakyTests` page when the request names no limit.
pub const DEFAULT_FLAKY: usize = 30;
/// Tests in one page, at most.
pub const MAX_FLAKY: usize = 100;
/// Tests whose verdict one report recomputes, at most; the rest wait for their next report.
pub const MAX_RECOMPUTE: usize = 500;
/// Default of `[ci_insights] retention_days`.
pub const DEFAULT_RETENTION_DAYS: u32 = 90;
/// Default of `[ci_insights] flaky_window_runs`.
pub const DEFAULT_FLAKY_WINDOW_RUNS: u32 = 100;

/// `[ci_insights]`.
#[derive(Clone, Debug, PartialEq, Eq, serde::Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct InsightsConfig {
    /// Days test cases are kept; 0 keeps them all. History per day is kept for 180 days.
    pub retention_days: u32,
    /// Runs the flaky verdict of a test looks at.
    pub flaky_window_runs: u32,
}

impl Default for InsightsConfig {
    fn default() -> Self {
        Self {
            retention_days: DEFAULT_RETENTION_DAYS,
            flaky_window_runs: DEFAULT_FLAKY_WINDOW_RUNS,
        }
    }
}

impl InsightsConfig {
    /// Checks the bounds. The error names the key.
    pub fn validate(&self) -> Result<(), String> {
        if self.retention_days > 3650 {
            return Err("retention_days is above 3650; use 0 to keep everything".into());
        }
        if self.retention_days != 0 && self.retention_days < 7 {
            return Err("retention_days must be at least 7".into());
        }
        if self.flaky_window_runs < 10 || self.flaky_window_runs > 10_000 {
            return Err("flaky_window_runs must be 10 to 10000".into());
        }
        Ok(())
    }
}

/// The state the `CiInsights` handlers share.
pub struct Insights {
    pub(crate) registry: Arc<Registry>,
    pub(crate) policy: PolicyStore,
    pub(crate) cfg: InsightsConfig,
}

impl Insights {
    pub fn new(registry: Arc<Registry>, cfg: InsightsConfig) -> Self {
        Self {
            policy: PolicyStore::new(registry.clone()),
            registry,
            cfg,
        }
    }
}

/// The place the server's single [`Insights`] is registered, once.
#[derive(Default)]
pub struct InsightsSlot(OnceLock<Arc<Insights>>);

impl InsightsSlot {
    /// Registers `hub`; false when one is registered.
    pub fn set(&self, hub: Arc<Insights>) -> bool {
        self.0.set(hub).is_ok()
    }

    pub fn get(&self) -> Option<&Arc<Insights>> {
        self.0.get()
    }
}

/// The unit's one line in `serve`.
pub fn register(ext: &Extensions, registry: &Arc<Registry>, cfg: &InsightsConfig) {
    let hub = Arc::new(Insights::new(registry.clone(), cfg.clone()));
    assert!(ext.insights.set(hub), "the CI insights hub is registered once");
}
