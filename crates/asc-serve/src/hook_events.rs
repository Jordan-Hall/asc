//! The events asc reports to the webhooks of the forge: an agent run started or finished, the
//! policy on trunk changed. Each is an outbox row with the [`EFFECT`] effect; with a forge the
//! row posts the event to `POST /api/internal/asc/hook-event`, and the forge queues it for the
//! repository's hooks that send it. The row's op id names the event: it is derived from what
//! happened, so the forge queues an event once even when it is reported twice.

use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use sqlx::AnyPool;
use uuid::Uuid;

/// The name rows list in `effects`.
pub const EFFECT: &str = "hook_event";

/// Longest policy summary the forge accepts.
pub const MAX_SUMMARY: usize = 200;

/// One event for the forge's webhooks.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HookEvent {
    RunStarted {
        run: u64,
    },
    RunFinished {
        run: u64,
        state: &'static str,
        changes: u64,
    },
    PolicyChanged {
        /// Hex of the trunk changesets before and after.
        before: String,
        after: String,
        summary: String,
        /// The login of who moved trunk, empty when unknown.
        sender: String,
    },
}

impl HookEvent {
    /// The event's name on the forge.
    pub fn name(&self) -> &'static str {
        match self {
            Self::RunStarted { .. } => "agent_run.started",
            Self::RunFinished { .. } => "agent_run.finished",
            Self::PolicyChanged { .. } => "policy.changed",
        }
    }

    /// What names the event among the repository's events: a run starts and ends once, a policy
    /// change is the change of one trunk tip.
    fn key(&self) -> String {
        match self {
            Self::RunStarted { run } => format!("run:{run}:started"),
            Self::RunFinished { run, .. } => format!("run:{run}:finished"),
            Self::PolicyChanged { after, .. } => format!("policy:{after}"),
        }
    }

    /// The body the forge reads, without `repo`.
    pub fn body(&self) -> Map<String, Value> {
        let mut m = Map::new();
        m.insert("event".into(), json!(self.name()));
        match self {
            Self::RunStarted { run } => {
                m.insert("run".into(), json!(run));
            }
            Self::RunFinished {
                run,
                state,
                changes,
            } => {
                m.insert("run".into(), json!(run));
                m.insert("state".into(), json!(state));
                m.insert("changes".into(), json!(changes));
            }
            Self::PolicyChanged {
                before,
                after,
                summary,
                sender,
            } => {
                m.insert("before".into(), json!(before));
                m.insert("after".into(), json!(after));
                m.insert("summary".into(), json!(clean_summary(summary)));
                if valid_login(sender) {
                    m.insert("sender".into(), json!(sender));
                }
            }
        }
        m
    }

    /// Reads back the event a row stored, `None` for a row that does not hold one.
    pub fn from_doc(doc: &Value) -> Option<Self> {
        let str_of = |k: &str| doc.get(k).and_then(Value::as_str);
        let run = || doc.get("run").and_then(Value::as_u64).filter(|r| *r > 0);
        Some(match str_of("event")? {
            "agent_run.started" => Self::RunStarted { run: run()? },
            "agent_run.finished" => Self::RunFinished {
                run: run()?,
                state: finished_state(str_of("state")?)?,
                changes: doc.get("changes").and_then(Value::as_u64).unwrap_or(0),
            },
            "policy.changed" => Self::PolicyChanged {
                before: str_of("before")?.to_owned(),
                after: str_of("after")?.to_owned(),
                summary: str_of("summary")?.to_owned(),
                sender: str_of("sender").unwrap_or_default().to_owned(),
            },
            _ => return None,
        })
    }
}

/// The name of the finished run state `s`, `None` when `s` names no such state.
fn finished_state(s: &str) -> Option<&'static str> {
    use crate::agents::runs::RunState;
    [
        RunState::Succeeded,
        RunState::Failed,
        RunState::Cancelled,
        RunState::BudgetExceeded,
        RunState::Expired,
    ]
    .into_iter()
    .map(RunState::name)
    .find(|n| *n == s)
}

/// The op id of `event` of the repository `repo_id`: the same event has the same id.
pub fn op_id(repo_id: i64, event: &HookEvent) -> Uuid {
    let digest = Sha256::digest(format!("asc-hook-event/{repo_id}/{}", event.key()));
    let mut bytes = [0u8; 16];
    bytes.copy_from_slice(&digest[..16]);
    uuid::Builder::from_custom_bytes(bytes).into_uuid()
}

/// A login the forge accepts as the sender.
fn valid_login(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 40
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-'))
}

/// `s` on one line, without markup characters, at most [`MAX_SUMMARY`] bytes.
fn clean_summary(s: &str) -> String {
    let mut out = String::new();
    for c in s.chars() {
        let c = match c {
            '\r' | '\n' | '\t' => ' ',
            '<' | '>' | '&' => continue,
            c => c,
        };
        if out.len() + c.len_utf8() > MAX_SUMMARY {
            break;
        }
        out.push(c);
    }
    out
}

/// Queues `event` of the repository `repo_id` (the `repos` row) for the forge's webhooks.
pub async fn enqueue(pool: &AnyPool, repo_id: i64, event: &HookEvent) -> Result<(), sqlx::Error> {
    let mut doc = event.body();
    doc.insert("effects".into(), json!([EFFECT]));
    let doc = serde_json::to_vec(&Value::Object(doc)).unwrap_or_default();
    sqlx::query("INSERT INTO outbox (repo_id, op_id, effects, next_at) VALUES ($1, $2, $3, $4)")
        .bind(repo_id)
        .bind(op_id(repo_id, event).as_bytes().to_vec())
        .bind(doc)
        .bind(crate::outbox::now())
        .execute(pool)
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn events_read_back_and_name_themselves() {
        let events = [
            HookEvent::RunStarted { run: 7 },
            HookEvent::RunFinished {
                run: 7,
                state: "failed",
                changes: 2,
            },
            HookEvent::PolicyChanged {
                before: "ab".into(),
                after: "cd".into(),
                summary: "A policy was added.".into(),
                sender: "priya".into(),
            },
        ];
        for e in &events {
            let doc = Value::Object(e.body());
            assert_eq!(HookEvent::from_doc(&doc).as_ref(), Some(e));
            assert_eq!(op_id(3, e), op_id(3, e));
            assert_ne!(op_id(3, e), op_id(4, e));
        }
        assert_ne!(op_id(3, &events[0]), op_id(3, &events[1]));
        assert_eq!(
            HookEvent::from_doc(&json!({"event": "agent_run.started"})),
            None
        );
        assert_eq!(
            HookEvent::from_doc(&json!({"event": "agent_run.finished", "run": 1, "state": "odd"})),
            None
        );
    }

    #[test]
    fn the_body_is_what_the_forge_accepts() {
        let e = HookEvent::PolicyChanged {
            before: "ab".into(),
            after: "cd".into(),
            summary: format!("<b>two\nlines</b> {}", "x".repeat(300)),
            sender: "not a login".into(),
        };
        let body = e.body();
        let summary = body["summary"].as_str().unwrap();
        assert!(summary.starts_with("btwo lines/b x"));
        assert_eq!(summary.len(), MAX_SUMMARY);
        assert!(!body.contains_key("sender"));
    }
}
