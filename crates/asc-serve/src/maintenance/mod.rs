//! Read-only maintenance of the server (SPEC §14.5).
//!
//! [`ReadOnly`] is the one flag every write path asks: the ref gate, the object upload and the
//! admin readiness. Two things raise it: a staged restore (until the restart, set by the backup
//! service) and the maintenance notice (one database row, switched by the `Maintenance` service
//! or `asc serve admin maintenance`). [`MaintenanceHub`] reads the row at start and every
//! [`POLL`], so every node of a cluster follows the same switch.

pub mod rpc;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use sqlx::{AnyPool, Row};
use tokio::task::JoinHandle;
use tonic::Status;

use crate::ai::audit::{Audit, Entry};
use crate::ai::ops::Ops;

/// What `Version.features` advertises.
pub const FEATURE: &str = "maintenance.v1";
/// How often a node reads the switch.
pub const POLL: Duration = Duration::from_secs(5);
/// Longest notice message, in characters.
pub const MAX_MESSAGE: usize = 500;
/// Longest version label.
pub const MAX_VERSION: usize = 64;
/// The principal the admin CLI records as the one who switched.
pub const OPERATOR: &str = "operator";

const RESTORE: &str =
    "the server is read-only: a restore is staged and applies when the server restarts";

/// The maintenance notice while it is on.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Notice {
    pub message: String,
    pub until: i64,
    pub version: String,
}

/// Whether writes are refused, and why.
#[derive(Debug, Default)]
pub struct ReadOnly {
    restore: AtomicBool,
    notice: RwLock<Option<Notice>>,
}

impl ReadOnly {
    /// A restore is staged: nothing is written until the restart applies it.
    pub fn stage_restore(&self) {
        self.restore.store(true, Ordering::Relaxed);
    }

    pub fn restore_staged(&self) -> bool {
        self.restore.load(Ordering::Relaxed)
    }

    /// The notice while maintenance is on.
    pub fn notice(&self) -> Option<Notice> {
        self.notice
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Switches the notice; `None` is off.
    pub fn set_notice(&self, notice: Option<Notice>) {
        *self
            .notice
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = notice;
    }

    /// Whether any write is refused now.
    pub fn is_read_only(&self) -> bool {
        self.restore_staged() || self.notice().is_some()
    }

    /// The answer to a write while the server is read-only, else `None`.
    pub fn refusal(&self) -> Option<Status> {
        if self.restore_staged() {
            return Some(Status::unavailable(RESTORE));
        }
        self.notice().map(|n| Status::unavailable(refusal_text(&n)))
    }
}

/// "the server is read-only for maintenance (upgrading to 2.4.0) until <RFC 3339 time>: <message>".
fn refusal_text(n: &Notice) -> String {
    let mut s = String::from("the server is read-only for maintenance");
    if !n.version.is_empty() {
        s.push_str(" (upgrading to ");
        s.push_str(&n.version);
        s.push(')');
    }
    if n.until > 0 {
        s.push_str(" until ");
        s.push_str(&crate::audit::export::rfc3339(n.until.saturating_mul(1000)));
    }
    if !n.message.is_empty() {
        s.push_str(": ");
        s.push_str(&n.message);
    }
    s
}

/// The stored switch, whether on or off.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Setting {
    pub on: bool,
    pub message: String,
    pub until: i64,
    pub version: String,
    pub updated: i64,
    pub updated_by: String,
}

impl Setting {
    fn notice(&self) -> Option<Notice> {
        self.on.then(|| Notice {
            message: self.message.clone(),
            until: self.until,
            version: self.version.clone(),
        })
    }
}

/// What a switch asks for.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Switch {
    pub on: bool,
    pub message: String,
    pub until: i64,
    pub version: String,
}

/// Why a switch was refused.
#[derive(Debug, thiserror::Error)]
pub enum MaintenanceError {
    #[error("{0}")]
    Invalid(&'static str),
    #[error("database: {0}")]
    Sql(#[from] sqlx::Error),
}

impl MaintenanceError {
    pub fn status(&self) -> Status {
        match self {
            Self::Invalid(m) => Status::invalid_argument(*m),
            Self::Sql(e) => {
                tracing::error!(error = %e, "maintenance database error");
                Status::internal("internal error")
            }
        }
    }
}

/// Checks a switch: the message is printable and bounded, the version a plain label, and the
/// expected end not in the past when switching on.
pub fn validate(s: &Switch, now: i64) -> Result<(), MaintenanceError> {
    if s.message.chars().count() > MAX_MESSAGE {
        return Err(MaintenanceError::Invalid(
            "the message is longer than 500 characters",
        ));
    }
    if s.message.chars().any(char::is_control) {
        return Err(MaintenanceError::Invalid(
            "the message holds a control character",
        ));
    }
    if s.version.len() > MAX_VERSION
        || !s
            .version
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'+' | b'-'))
    {
        return Err(MaintenanceError::Invalid(
            "a version is at most 64 characters of [A-Za-z0-9._+-]",
        ));
    }
    if s.until < 0 || (s.on && s.until > 0 && s.until < now) {
        return Err(MaintenanceError::Invalid("until is in the past"));
    }
    Ok(())
}

/// Reads the stored switch; a server that never set one is off.
pub async fn load(pool: &AnyPool) -> Result<Setting, sqlx::Error> {
    let row = sqlx::query(
        "SELECT enabled, message, until_at, version, updated, updated_by \
         FROM server_maintenance WHERE id = 1",
    )
    .fetch_optional(pool)
    .await?;
    let Some(r) = row else {
        return Ok(Setting::default());
    };
    Ok(Setting {
        on: r.try_get::<i64, _>("enabled")? != 0,
        message: r.try_get("message")?,
        until: r.try_get("until_at")?,
        version: r.try_get("version")?,
        updated: r.try_get("updated")?,
        updated_by: r.try_get("updated_by")?,
    })
}

/// Stores a validated switch made by `by` at `now`, and returns the previous setting.
pub async fn store(
    pool: &AnyPool,
    s: &Switch,
    by: &str,
    now: i64,
) -> Result<Setting, MaintenanceError> {
    validate(s, now)?;
    let mut tx = pool.begin().await?;
    let old = {
        let row = sqlx::query(
            "SELECT enabled, message, until_at, version, updated, updated_by \
             FROM server_maintenance WHERE id = 1",
        )
        .fetch_optional(&mut *tx)
        .await?;
        match row {
            Some(r) => Setting {
                on: r.try_get::<i64, _>("enabled")? != 0,
                message: r.try_get("message")?,
                until: r.try_get("until_at")?,
                version: r.try_get("version")?,
                updated: r.try_get("updated")?,
                updated_by: r.try_get("updated_by")?,
            },
            None => Setting::default(),
        }
    };
    sqlx::query(
        "INSERT INTO server_maintenance (id, enabled, message, until_at, version, updated, updated_by) \
         VALUES (1, $1, $2, $3, $4, $5, $6) \
         ON CONFLICT (id) DO UPDATE SET enabled = $1, message = $2, until_at = $3, version = $4, \
         updated = $5, updated_by = $6",
    )
    .bind(i64::from(s.on))
    .bind(s.message.clone())
    .bind(s.until)
    .bind(s.version.clone())
    .bind(now)
    .bind(by.to_owned())
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(old)
}

/// The details of a `server.maintenance.updated` audit event.
pub fn audit_details(old: &Setting, new: &Switch) -> Vec<(&'static str, String)> {
    vec![
        ("enabled", new.on.to_string()),
        ("old_enabled", old.on.to_string()),
        ("message", new.message.clone()),
        ("until", new.until.to_string()),
        ("version", new.version.clone()),
    ]
}

/// The server's maintenance switch: the stored row and the flag that follows it.
pub struct MaintenanceHub {
    pool: AnyPool,
    flag: Arc<ReadOnly>,
    audit: Arc<dyn Audit>,
    pub(crate) ops: Ops,
}

impl MaintenanceHub {
    pub fn new(pool: AnyPool, flag: Arc<ReadOnly>, audit: Arc<dyn Audit>) -> Arc<Self> {
        Arc::new(Self {
            ops: Ops::new(pool.clone()),
            pool,
            flag,
            audit,
        })
    }

    pub fn flag(&self) -> &Arc<ReadOnly> {
        &self.flag
    }

    /// The stored switch.
    pub async fn setting(&self) -> Result<Setting, MaintenanceError> {
        Ok(load(&self.pool).await?)
    }

    /// Reads the row into the flag. A failed read keeps what the flag says: a database that
    /// cannot be read neither starts nor ends maintenance.
    pub async fn refresh(&self) {
        match load(&self.pool).await {
            Ok(s) => self.flag.set_notice(s.notice()),
            Err(e) => tracing::warn!(error = %e, "maintenance: cannot read the switch"),
        }
    }

    /// Switches maintenance as `actor` (relayed by `relay`), applies it on this node at once,
    /// writes the audit event and returns the new setting.
    pub async fn switch(
        &self,
        s: &Switch,
        actor: &str,
        relay: Option<&str>,
        now: i64,
    ) -> Result<Setting, MaintenanceError> {
        let old = store(&self.pool, s, actor, now).await?;
        self.refresh().await;
        let details = audit_details(&old, s);
        self.audit
            .record(Entry {
                target: "maintenance",
                on_behalf: relay,
                details: &details,
                ..Entry::new("server.maintenance.updated", actor)
            })
            .await;
        self.setting().await
    }

    /// Reads the switch now and every [`POLL`].
    pub fn spawn_poller(self: &Arc<Self>) -> JoinHandle<()> {
        let hub = self.clone();
        tokio::spawn(async move {
            loop {
                hub.refresh().await;
                tokio::time::sleep(POLL).await;
            }
        })
    }
}

#[cfg(test)]
mod tests;
