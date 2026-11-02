//! The `/mcp` audit record (SPEC 22.7.3; PLAN M29): one record per tool call, written by the
//! authenticator whatever the outcome. A record names the principal, the repository, the tool,
//! whether the call read or wrote, the outcome and the duration. It never holds a credential.

use std::sync::{Mutex, PoisonError};
use std::time::Duration;

/// What the call needed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Read,
    Write,
}

impl Mode {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Write => "write",
        }
    }
}

/// How the call ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// The authorizer allowed it and the endpoint answered with this HTTP status.
    Allowed(u16),
    /// No valid credential (401).
    Unauthenticated,
    /// A valid credential without the write access the tool needs (403).
    InsufficientScope,
    /// The repository is unreadable or missing (the two are not told apart).
    NotFound,
    /// The call was malformed before it could be authorised.
    BadRequest,
}

/// One audit record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuditRecord {
    /// The authorizer's principal id; `None` when no credential was accepted.
    pub principal: Option<String>,
    pub repo: Option<String>,
    pub tool: String,
    pub mode: Mode,
    pub outcome: Outcome,
    pub duration: Duration,
}

/// Where records go.
pub trait AuditSink: Send + Sync + 'static {
    fn record(&self, record: &AuditRecord);
}

/// The production sink: one `tracing` event per record on the `asc_serve::mcp::audit` target.
#[derive(Debug, Default, Clone, Copy)]
pub struct TracingAudit;

impl AuditSink for TracingAudit {
    fn record(&self, r: &AuditRecord) {
        tracing::info!(
            target: "asc_serve::mcp::audit",
            principal = r.principal.as_deref().unwrap_or("-"),
            repo = r.repo.as_deref().unwrap_or("-"),
            tool = %r.tool,
            mode = r.mode.as_str(),
            outcome = ?r.outcome,
            duration_ms = u64::try_from(r.duration.as_millis()).unwrap_or(u64::MAX),
            "mcp call"
        );
    }
}

/// Keeps the newest records in memory (tests, and a cheap ring for diagnostics).
#[derive(Debug, Default)]
pub struct MemoryAudit {
    records: Mutex<Vec<AuditRecord>>,
}

impl MemoryAudit {
    #[must_use]
    pub fn records(&self) -> Vec<AuditRecord> {
        self.records
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

impl AuditSink for MemoryAudit {
    fn record(&self, r: &AuditRecord) {
        let mut g = self.records.lock().unwrap_or_else(PoisonError::into_inner);
        if g.len() >= 10_000 {
            g.remove(0);
        }
        g.push(r.clone());
    }
}
