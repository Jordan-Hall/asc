//! Audit events of the storage and vault calls (docs/BUILDOUT.md section 3 names the actions).
//!
//! The audit log itself belongs to another unit. This module is the seam: the storage code
//! records an [`AuditEvent`] through an [`Audit`] sink, and the server wires the log in when it
//! exists. Until then [`TracingAudit`] writes each event as one structured log line.
//!
//! An event holds no secret by construction: the only free-text part is `details`, whose keys
//! the callers choose from a fixed list, whose values are truncated to 256 bytes and stripped
//! of control characters, and which never receive credentials, endpoints with a query, or
//! provider messages.

use std::sync::Mutex;

/// Longest value of a detail, in bytes.
pub const MAX_DETAIL: usize = 256;

/// One audit record.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AuditEvent {
    /// `storage.target.created`, `storage.migration.started`, `vault.rotated`, ...
    pub action: String,
    /// The principal that acted (`7:alice`, `host-service`).
    pub actor: String,
    /// `user:alice`, `org:acme`, `repo:acme/app`.
    pub owner: String,
    /// The id of the thing acted on.
    pub target: String,
    pub repo: String,
    /// `ok` or `denied` or `error`.
    pub outcome: String,
    pub details: Vec<(String, String)>,
}

fn clean(s: &str) -> String {
    let mut out: String = s.chars().filter(|c| !c.is_control()).collect();
    if out.len() > MAX_DETAIL {
        let mut cut = MAX_DETAIL;
        while !out.is_char_boundary(cut) {
            cut -= 1;
        }
        out.truncate(cut);
    }
    out
}

impl AuditEvent {
    pub fn new(action: &str) -> Self {
        Self {
            action: action.to_owned(),
            outcome: "ok".into(),
            ..Self::default()
        }
    }

    pub fn actor(mut self, actor: &str) -> Self {
        self.actor = clean(actor);
        self
    }

    pub fn owner(mut self, owner: &str) -> Self {
        self.owner = clean(owner);
        self
    }

    pub fn target(mut self, id: &str) -> Self {
        self.target = clean(id);
        self
    }

    pub fn repo(mut self, repo: &str) -> Self {
        self.repo = clean(repo);
        self
    }

    pub fn outcome(mut self, outcome: &str) -> Self {
        self.outcome = clean(outcome);
        self
    }

    pub fn detail(mut self, key: &str, value: &str) -> Self {
        self.details.push((clean(key), clean(value)));
        self
    }
}

/// Where events go.
pub trait Audit: Send + Sync {
    fn record(&self, event: AuditEvent);
}

/// Writes events to the log as `asc audit:` lines.
#[derive(Clone, Copy, Debug, Default)]
pub struct TracingAudit;

impl Audit for TracingAudit {
    fn record(&self, e: AuditEvent) {
        let details = e
            .details
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join(" ");
        tracing::info!(
            target: "asc::audit",
            action = %e.action,
            actor = %e.actor,
            owner = %e.owner,
            target_id = %e.target,
            repo = %e.repo,
            outcome = %e.outcome,
            details = %details,
            "asc audit"
        );
    }
}

/// Keeps events in memory, for tests.
#[derive(Default)]
pub struct MemoryAudit(Mutex<Vec<AuditEvent>>);

impl MemoryAudit {
    pub fn events(&self) -> Vec<AuditEvent> {
        self.0.lock().map(|e| e.clone()).unwrap_or_default()
    }

    pub fn actions(&self) -> Vec<String> {
        self.events().into_iter().map(|e| e.action).collect()
    }
}

impl Audit for MemoryAudit {
    fn record(&self, event: AuditEvent) {
        if let Ok(mut e) = self.0.lock() {
            e.push(event);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn details_are_bounded_and_clean() {
        let e = AuditEvent::new("storage.target.created")
            .detail("note", &format!("a\nb\u{7}{}", "é".repeat(400)));
        let v = &e.details[0].1;
        assert!(v.len() <= MAX_DETAIL && !v.contains('\n') && !v.contains('\u{7}'));
        assert!(v.is_char_boundary(v.len()));
        assert_eq!(e.outcome, "ok");
    }
}
