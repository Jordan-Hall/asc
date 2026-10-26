//! Health checks of enabled targets.
//!
//! An enabled target is probed (write, read back, delete a probe object) every
//! `[storage] health_interval_secs`. Three failures in a row move it to `FAILED` with a sanitised
//! reason, which makes the router refuse it at once instead of every request waiting out a
//! timeout; the first success moves a target that was enabled back to `ENABLED`. Nothing here
//! consumes the owner's verification allowance, and nothing writes to the database while a
//! target stays healthy.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use super::audit::{Audit, AuditEvent};
use super::db::Db;
use super::live::{self, Builder, Router};
use super::model::{State, Target};

/// Consecutive failures before a target is marked failed.
pub const FAILURES_TO_FAIL: u32 = 3;
/// Targets probed at once.
const CONCURRENCY: usize = 4;
/// Most targets looked at per pass.
const PASS_LIMIT: i64 = 500;

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

/// The health checker.
pub struct Health {
    db: Db,
    builder: Builder,
    router: Arc<Router>,
    audit: Arc<dyn Audit>,
    failures: Mutex<HashMap<String, u32>>,
}

impl Health {
    pub fn new(db: Db, router: Arc<Router>, audit: Arc<dyn Audit>) -> Arc<Self> {
        Arc::new(Self {
            db,
            builder: router.builder.clone(),
            router,
            audit,
            failures: Mutex::default(),
        })
    }

    /// Runs a pass every `interval`.
    pub fn spawn(self: &Arc<Self>, interval: Duration) -> tokio::task::JoinHandle<()> {
        let this = self.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(interval);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tick.tick().await;
                this.pass().await;
            }
        })
    }

    /// One pass over the enabled targets and the failed ones that were enabled.
    pub async fn pass(&self) {
        use futures::StreamExt;
        let mut work: Vec<Target> = Vec::new();
        for state in [State::Enabled, State::Failed] {
            match self.db.targets_in(state, PASS_LIMIT).await {
                Ok(t) => work.extend(t.into_iter().filter(|t| t.state == State::Enabled || t.was_enabled)),
                Err(e) => tracing::warn!(error = %e, "storage health pass could not list targets"),
            }
        }
        futures::stream::iter(work)
            .for_each_concurrent(CONCURRENCY, |t| async move { self.check(&t).await })
            .await;
    }

    /// Probes one target and applies the outcome.
    pub async fn check(&self, t: &Target) {
        let Ok(Some(sealed)) = self.db.sealed(&t.id).await else {
            return;
        };
        let outcome = match live::probe(&self.builder, t, &sealed).await {
            Ok(report) if report.ok() => Ok(()),
            Ok(report) => Err(report.failure().unwrap_or_else(|| "the check failed".into())),
            // The vault being locked says nothing about the bucket.
            Err(live::RouteError::Vault(_)) => return,
            Err(e) => Err(e.to_string()),
        };
        match outcome {
            Ok(()) => {
                if let Ok(mut f) = self.failures.lock() {
                    f.remove(&t.id);
                }
                if t.state == State::Failed && t.was_enabled {
                    let _ = self
                        .db
                        .set_state(&t.id, State::Enabled, Some(true), "", Some(now()), now())
                        .await;
                    self.router.invalidate(&t.id);
                    self.audit.record(
                        AuditEvent::new("storage.target.verified")
                            .actor("system")
                            .owner(&t.owner.scope())
                            .target(&t.id)
                            .detail("result", "recovered"),
                    );
                } else if !t.last_error.is_empty() {
                    let _ = self.db.set_last_error(&t.id, "", now()).await;
                }
            }
            Err(why) => {
                let count = self.failures.lock().map_or(FAILURES_TO_FAIL, |mut f| {
                    let c = f.entry(t.id.clone()).or_insert(0);
                    *c += 1;
                    *c
                });
                if t.state == State::Enabled && count >= FAILURES_TO_FAIL {
                    let _ = self
                        .db
                        .set_state(&t.id, State::Failed, Some(true), &why, None, now())
                        .await;
                    self.router.invalidate(&t.id);
                    self.audit.record(
                        AuditEvent::new("storage.target.verified")
                            .actor("system")
                            .owner(&t.owner.scope())
                            .target(&t.id)
                            .outcome("error")
                            .detail("result", "failed"),
                    );
                    tracing::warn!(target_id = %t.id, "storage target failed its health checks");
                } else if t.last_error != why {
                    let _ = self.db.set_last_error(&t.id, &why, now()).await;
                }
            }
        }
    }
}
