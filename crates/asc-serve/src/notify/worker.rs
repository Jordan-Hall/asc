//! The dispatcher: posts `notify_events` rows to the hub and retries.
//!
//! A pass takes the due rows (`state = 0`, `next_at` reached), leases each by moving `next_at`
//! forward (so replicas do not post a row at once and a crashed worker's rows come back), posts
//! it through the [`Sink`] and records the outcome:
//!
//! * delivered: `state = 1`, with how many people the hub told;
//! * the hub refused the row (a `4xx` other than 408, 425 and 429): `state = 2`, never retried,
//!   because the same row is refused again;
//! * anything else (the hub is unreachable, `5xx`, 408, 425, 429): `attempts + 1`, `next_at` after
//!   the outbox's backoff (1 s doubling to 5 min). A row older than [`GIVE_UP_SECS`] is given up
//!   (`state = 2`).
//!
//! Delivery is at least once. Each post carries `x-asc-event-id` (`<ns hex>-<id>`), so a hub that
//! wants to drop a redelivery can.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use sqlx::{AnyPool, Row};
use tokio::sync::Notify;

use crate::outbox::{backoff_secs, now};

/// A row that the hub has not taken after this long is given up.
pub const GIVE_UP_SECS: i64 = 24 * 3600;
/// Delivered and rejected rows are kept this long (for `last_error` and diagnostics).
pub const KEEP_SECS: i64 = 7 * 24 * 3600;
/// Longest `last_error` stored.
const MAX_ERROR: usize = 300;
/// The longest one post may take.
const POST_TIMEOUT: Duration = Duration::from_secs(20);

/// What goes to the hub, as the hub's `ascEventRequest` names it.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct Outgoing {
    pub event: String,
    pub repo: String,
    pub users: Vec<String>,
    pub title: String,
    pub detail: String,
    pub path: String,
}

/// Why a post failed.
#[derive(Clone, Debug)]
pub struct Failure {
    /// The same row would fail again: it is not retried.
    pub permanent: bool,
    pub message: String,
}

/// Where the rows go. The hub's internal endpoint in production; a recorder in tests.
#[async_trait]
pub trait Sink: Send + Sync {
    /// Posts one event; `id` names the row. Returns how many people the hub told.
    async fn deliver(&self, id: &str, event: &Outgoing) -> Result<u64, Failure>;
}

/// The hub of `[forgejo]`: `POST /api/internal/asc/event` with the internal token.
#[cfg(feature = "forgejo")]
pub struct HubSink(pub Arc<crate::forgejo::ForgejoClient>);

#[cfg(feature = "forgejo")]
#[async_trait]
impl Sink for HubSink {
    async fn deliver(&self, id: &str, event: &Outgoing) -> Result<u64, Failure> {
        use crate::forgejo::ForgejoError;
        let body = serde_json::to_value(event).map_err(|e| Failure {
            permanent: true,
            message: e.to_string(),
        })?;
        self.0.asc_event(id, &body).await.map_err(|e| {
            let permanent = match &e {
                ForgejoError::Status { status, .. } => {
                    (400..500).contains(status) && !matches!(status, 408 | 425 | 429)
                }
                // A 200 the hub sent nonsense with was received: posting again would tell twice.
                ForgejoError::Invalid(_)
                | ForgejoError::NoInternalToken
                | ForgejoError::Decode(_) => true,
                _ => false,
            };
            Failure {
                permanent,
                message: e.to_string(),
            }
        })
    }
}

/// Dispatcher settings.
#[derive(Clone, Debug)]
pub struct DispatchConfig {
    /// How long a leased row is hidden from other workers.
    pub lease_secs: i64,
    /// Rows posted per pass.
    pub batch: i64,
    /// Sleep between passes when nothing woke the worker.
    pub poll: Duration,
}

impl Default for DispatchConfig {
    fn default() -> Self {
        Self {
            lease_secs: 60,
            batch: 16,
            poll: Duration::from_secs(2),
        }
    }
}

/// The worker.
pub struct Dispatcher {
    pool: AnyPool,
    sink: Option<Arc<dyn Sink>>,
    cfg: DispatchConfig,
    wake: Notify,
}

fn cut(s: &str) -> String {
    super::one_line(s, MAX_ERROR)
}

impl Dispatcher {
    /// Without a sink nothing is posted; the pass still gives up and prunes old rows.
    pub fn new(pool: AnyPool, sink: Option<Arc<dyn Sink>>, cfg: DispatchConfig) -> Self {
        Self {
            pool,
            sink,
            cfg,
            wake: Notify::new(),
        }
    }

    /// Runs passes until the task is aborted. Delivery is at least once, so aborting is safe.
    pub async fn run(self: Arc<Self>) {
        let mut last_prune = 0;
        loop {
            let t = now();
            match self.deliver_due(t).await {
                Ok(n) if n > 0 => continue,
                Ok(_) => {}
                Err(e) => tracing::warn!(event = "notify.pass", error = %e),
            }
            if t - last_prune >= 60 {
                last_prune = t;
                if let Err(e) = self.prune(t).await {
                    tracing::warn!(event = "notify.prune", error = %e);
                }
            }
            let _ = tokio::time::timeout(self.cfg.poll, self.wake.notified()).await;
        }
    }

    /// Gives up the rows the hub has not taken in [`GIVE_UP_SECS`] and deletes the finished rows
    /// older than [`KEEP_SECS`].
    pub async fn prune(&self, now: i64) -> Result<u64, sqlx::Error> {
        sqlx::query(
            "UPDATE notify_events SET state = 2, done_at = $1, last_error = 'given up' \
             WHERE state = 0 AND created_at < $2",
        )
        .bind(now)
        .bind(now - GIVE_UP_SECS)
        .execute(&self.pool)
        .await?;
        Ok(
            sqlx::query("DELETE FROM notify_events WHERE state <> 0 AND done_at < $1")
                .bind(now - KEEP_SECS)
                .execute(&self.pool)
                .await?
                .rows_affected(),
        )
    }

    /// One pass at time `now`: posts the due rows. Returns the rows examined.
    pub async fn deliver_due(&self, now: i64) -> Result<usize, sqlx::Error> {
        let Some(sink) = &self.sink else {
            return Ok(0);
        };
        let rows = sqlx::query(
            "SELECT n.ns AS ns, CAST(n.id AS BIGINT) AS id, r.name AS repo, n.event AS event, \
                    n.users_json AS users_json, n.title AS title, n.detail AS detail, n.path AS path, \
                    CAST(n.attempts AS BIGINT) AS attempts, CAST(n.next_at AS BIGINT) AS next_at, \
                    CAST(n.created_at AS BIGINT) AS created_at \
             FROM notify_events n JOIN repos r ON r.ns = n.ns \
             WHERE n.state = 0 AND n.next_at <= $1 \
             ORDER BY n.next_at, n.id LIMIT $2",
        )
        .bind(now)
        .bind(self.cfg.batch)
        .fetch_all(&self.pool)
        .await?;
        let seen = rows.len();
        for r in rows {
            let ns: Vec<u8> = r.try_get("ns")?;
            let id: i64 = r.try_get("id")?;
            let attempts: i64 = r.try_get("attempts")?;
            let next_at: i64 = r.try_get("next_at")?;
            let created_at: i64 = r.try_get("created_at")?;
            let claimed = sqlx::query(
                "UPDATE notify_events SET next_at = $1 \
                 WHERE ns = $2 AND id = $3 AND next_at = $4 AND state = 0",
            )
            .bind(now + self.cfg.lease_secs)
            .bind(ns.clone())
            .bind(id)
            .bind(next_at)
            .execute(&self.pool)
            .await?
            .rows_affected();
            if claimed != 1 {
                continue; // another replica has it
            }
            let users: Vec<String> =
                serde_json::from_str(&r.try_get::<String, _>("users_json")?).unwrap_or_default();
            let out = Outgoing {
                event: r.try_get("event")?,
                repo: r.try_get("repo")?,
                users,
                title: r.try_get("title")?,
                detail: r.try_get("detail")?,
                path: r.try_get("path")?,
            };
            let name = format!("{}-{id}", hex(&ns));
            let result = match tokio::time::timeout(POST_TIMEOUT, sink.deliver(&name, &out)).await {
                Ok(r) => r,
                Err(_) => Err(Failure {
                    permanent: false,
                    message: "the hub did not answer in time".into(),
                }),
            };
            match result {
                Ok(told) => {
                    sqlx::query(
                        "UPDATE notify_events SET state = 1, done_at = $1, notified = $2, last_error = '' \
                         WHERE ns = $3 AND id = $4",
                    )
                    .bind(now)
                    .bind(i64::try_from(told).unwrap_or(i64::MAX))
                    .bind(ns)
                    .bind(id)
                    .execute(&self.pool)
                    .await?;
                }
                Err(f) if f.permanent || now - created_at >= GIVE_UP_SECS => {
                    tracing::warn!(event = "notify.rejected", id = %name, error = %cut(&f.message));
                    sqlx::query(
                        "UPDATE notify_events SET state = 2, done_at = $1, attempts = $2, last_error = $3 \
                         WHERE ns = $4 AND id = $5",
                    )
                    .bind(now)
                    .bind(attempts + 1)
                    .bind(cut(&f.message))
                    .bind(ns)
                    .bind(id)
                    .execute(&self.pool)
                    .await?;
                }
                Err(f) => {
                    sqlx::query(
                        "UPDATE notify_events SET attempts = $1, next_at = $2, last_error = $3 \
                         WHERE ns = $4 AND id = $5",
                    )
                    .bind(attempts + 1)
                    .bind(now + backoff_secs(attempts + 1))
                    .bind(cut(&f.message))
                    .bind(ns)
                    .bind(id)
                    .execute(&self.pool)
                    .await?;
                }
            }
        }
        Ok(seen)
    }

    /// Wakes the worker after new rows were written (the poll finds them anyway).
    pub fn wake(&self) {
        self.wake.notify_one();
    }
}

fn hex(b: &[u8]) -> String {
    use std::fmt::Write;
    b.iter().take(8).fold(String::new(), |mut s, x| {
        let _ = write!(s, "{x:02x}");
        s
    })
}
