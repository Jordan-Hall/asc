//! Where AI keys, agents and workflows report what happened: the audit log
//! (docs/BUILDOUT.md section 3).
//!
//! The callers name an action of the catalogue and the allow-listed detail keys of that action
//! (`audit/catalogue.rs` drops any other key). No caller passes a secret: a key shows its last
//! four characters (`key_hint`), a token its id. [`PoolAudit`] writes to the log; a test or a
//! deployment without the log installs none and nothing is recorded.

use async_trait::async_trait;
use sqlx::AnyPool;

use crate::audit::actor::actor_of;
use crate::audit::store::record_best_effort;
use crate::audit::{NewEvent, Outcome};

/// One thing that happened.
#[derive(Clone, Copy, Debug)]
pub struct Entry<'a> {
    /// An action of the audit catalogue.
    pub action: &'a str,
    /// The principal id that acted.
    pub actor: &'a str,
    /// The person an agent acted for, when there is one.
    pub on_behalf: Option<&'a str>,
    /// `owner/name`, or empty.
    pub repo: &'a str,
    /// The owner scope when there is no repository (`user:alice` is written as `alice`).
    pub owner: &'a str,
    pub target: &'a str,
    pub denied: bool,
    pub details: &'a [(&'a str, String)],
}

impl<'a> Entry<'a> {
    pub fn new(action: &'a str, actor: &'a str) -> Self {
        Self {
            action,
            actor,
            on_behalf: None,
            repo: "",
            owner: "",
            target: "",
            denied: false,
            details: &[],
        }
    }
}

/// The sink the services record to.
#[async_trait]
pub trait Audit: Send + Sync {
    async fn record(&self, entry: Entry<'_>);
}

/// Appends to the audit log in the server's database. A failure is logged and does not fail the
/// action (the log is evidence, and a key or a run must not depend on it).
pub struct PoolAudit {
    pool: AnyPool,
}

impl PoolAudit {
    pub fn new(pool: AnyPool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl Audit for PoolAudit {
    async fn record(&self, e: Entry<'_>) {
        let mut ev = NewEvent::new(e.action, actor_of(e.actor)).target(e.target);
        if !e.repo.is_empty() {
            ev = ev.repo(e.repo);
        }
        if !e.owner.is_empty() {
            ev = ev.owner(e.owner);
        }
        if let Some(b) = e.on_behalf.filter(|b| !b.is_empty()) {
            ev = ev.on_behalf_of(actor_of(b));
        }
        if e.denied {
            ev = ev.outcome(Outcome::Denied);
        }
        for (k, v) in e.details {
            ev = ev.detail(k, v.clone());
        }
        record_best_effort(&self.pool, &ev).await;
    }
}
