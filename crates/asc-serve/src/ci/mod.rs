//! The `Ci` service (`ci.proto`): CI and Actions runs, their jobs and job logs, recorded against a
//! change. Host-neutral: a runner or a forge adapter reports with a service credential (repo
//! admin); clients read with repo read.
//!
//! Bounds: [`MAX_RUNS`] runs per repository (older runs, with their jobs and logs, are deleted as
//! new ones arrive), [`MAX_JOBS`] jobs per run, [`MAX_LOG`] bytes of log per job (later bytes are
//! dropped and the append answers `dropped`), [`MAX_APPEND`] bytes per append.

pub mod rpc;

#[cfg(test)]
mod tests;

use asc_rpc::server::wire;

/// What `Version.features` adds for this service.
pub const FEATURE: &str = "ci.v1";
/// Runs kept per repository.
pub const MAX_RUNS: i64 = 50_000;
/// Jobs one run holds.
pub const MAX_JOBS: usize = 200;
/// Log bytes kept per job.
pub const MAX_LOG: i64 = 8 * 1024 * 1024;
/// Bytes in one `AppendJobLog`.
pub const MAX_APPEND: usize = 256 * 1024;
/// Runs in one `ListRuns` page when the request names no limit.
pub const DEFAULT_PAGE: usize = 30;
/// Runs in one `ListRuns` page, at most.
pub const MAX_PAGE: usize = 100;
/// Log bytes in one `JobLog` answer when the request names no size.
pub const DEFAULT_LOG: usize = 64 * 1024;
/// Log bytes in one `JobLog` answer, at most.
pub const MAX_LOG_READ: usize = 1024 * 1024;

/// Whether a status is final: a late report must not move a run out of it.
pub fn is_terminal(status: i64) -> bool {
    status == wire::CiStatus::Success as i64
        || status == wire::CiStatus::Failure as i64
        || status == wire::CiStatus::Cancelled as i64
        || status == wire::CiStatus::Skipped as i64
}
