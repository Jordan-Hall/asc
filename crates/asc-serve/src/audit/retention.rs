//! Retention (docs/BUILDOUT.md section 3): events older than `[audit] retention_days` are dropped,
//! oldest first, per scope. Before the events go, the scope's segment record takes the hash of the
//! last dropped event, so the chain of the events that stay still verifies from there and the
//! drop itself cannot be mistaken for tampering. A scope whose oldest event does not follow its
//! segment (a gap) is left alone: retention never hides a broken chain.

use std::time::Duration;

use sqlx::{AnyPool, Row};

use super::store::{self, begin};
use super::{Actor, AuditConfig, AuditError, NewEvent, Scope, now_ms};

/// Most events one pass drops from one scope; the worker passes again until the scope is done.
pub const MAX_PER_PASS: u64 = 50_000;
/// Passes per scope per run, so one run cannot go on for ever.
const MAX_PASSES: usize = 20;

/// What a pass dropped.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pruned {
    pub scope: Scope,
    pub events: u64,
    /// Sequence numbers 1 to this are gone.
    pub until_seq: u64,
}

/// Drops the oldest events of `scope` that were written before `cutoff_ms`, at most `max` of them.
pub async fn prune_scope(
    pool: &AnyPool,
    scope: &Scope,
    cutoff_ms: i64,
    max: u64,
) -> Result<Option<Pruned>, AuditError> {
    let (kind, name) = (scope.kind.as_str(), scope.name.as_str());
    let mut tx = begin(pool).await?;
    let first: Option<i64> = sqlx::query_scalar(
        "SELECT MIN(seq) FROM audit_events WHERE scope_kind = $1 AND scope_name = $2",
    )
    .bind(kind)
    .bind(name)
    .fetch_one(&mut *tx)
    .await?;
    let Some(first) = first else {
        return Ok(None);
    };
    let segment = sqlx::query(
        "SELECT to_seq, events FROM audit_segments WHERE scope_kind = $1 AND scope_name = $2",
    )
    .bind(kind)
    .bind(name)
    .fetch_optional(&mut *tx)
    .await?;
    let (seg_to, seg_events) = match &segment {
        Some(r) => (r.try_get::<i64, _>("to_seq")?, r.try_get::<i64, _>("events")?),
        None => (0, 0),
    };
    if first != seg_to + 1 {
        tracing::warn!(scope = %scope, "audit retention skipped: the oldest event does not follow the retention record");
        return Ok(None);
    }
    // The first event that stays: the oldest one that is not older than the cutoff.
    let stays: Option<i64> = sqlx::query_scalar(
        "SELECT MIN(seq) FROM audit_events WHERE scope_kind = $1 AND scope_name = $2 AND time_ms >= $3",
    )
    .bind(kind)
    .bind(name)
    .bind(cutoff_ms)
    .fetch_one(&mut *tx)
    .await?;
    let newest: i64 = sqlx::query_scalar(
        "SELECT MAX(seq) FROM audit_events WHERE scope_kind = $1 AND scope_name = $2",
    )
    .bind(kind)
    .bind(name)
    .fetch_one(&mut *tx)
    .await?;
    let upto = stays.map_or(newest, |s| s - 1);
    let cap = first.saturating_add(i64::try_from(max).unwrap_or(i64::MAX) - 1);
    let upto = upto.min(cap);
    if upto < first {
        return Ok(None);
    }
    let row = sqlx::query(
        "SELECT hash FROM audit_events WHERE scope_kind = $1 AND scope_name = $2 AND seq = $3",
    )
    .bind(kind)
    .bind(name)
    .bind(upto)
    .fetch_optional(&mut *tx)
    .await?;
    let Some(row) = row else {
        tracing::warn!(scope = %scope, seq = upto, "audit retention skipped: an event is missing");
        return Ok(None);
    };
    let head_hash: Vec<u8> = row.try_get("hash")?;
    let dropped = upto - first + 1;
    sqlx::query("DELETE FROM audit_segments WHERE scope_kind = $1 AND scope_name = $2")
        .bind(kind)
        .bind(name)
        .execute(&mut *tx)
        .await?;
    sqlx::query(
        "INSERT INTO audit_segments (scope_kind, scope_name, to_seq, head_hash, events, dropped_ms) \
         VALUES ($1, $2, $3, $4, $5, $6)",
    )
    .bind(kind)
    .bind(name)
    .bind(upto)
    .bind(head_hash)
    .bind(seg_events + dropped)
    .bind(now_ms())
    .execute(&mut *tx)
    .await?;
    let deleted = sqlx::query(
        "DELETE FROM audit_events WHERE scope_kind = $1 AND scope_name = $2 AND seq <= $3",
    )
    .bind(kind)
    .bind(name)
    .bind(upto)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if i64::try_from(deleted).ok() != Some(dropped) {
        // A gap inside the range: nothing is dropped, the chain check will name it.
        tx.rollback().await?;
        tracing::warn!(scope = %scope, "audit retention skipped: the range has a gap");
        return Ok(None);
    }
    tx.commit().await?;
    Ok(Some(Pruned {
        scope: scope.clone(),
        events: deleted,
        until_seq: u64::try_from(upto).unwrap_or(0),
    }))
}

/// One run: drops what is older than `cfg.retention_days` before `now_ms` in every scope, and
/// records each drop in the instance log. `retention_days = 0` keeps everything.
pub async fn run_once(
    pool: &AnyPool,
    cfg: &AuditConfig,
    now_ms: i64,
) -> Result<Vec<Pruned>, AuditError> {
    if cfg.retention_days == 0 {
        return Ok(Vec::new());
    }
    let cutoff = now_ms.saturating_sub(i64::from(cfg.retention_days) * 86_400_000);
    let mut out = Vec::new();
    for scope in store::scopes(pool).await? {
        let mut total = 0u64;
        let mut until = 0u64;
        for _ in 0..MAX_PASSES {
            match prune_scope(pool, &scope, cutoff, MAX_PER_PASS).await? {
                Some(p) => {
                    total += p.events;
                    until = p.until_seq;
                }
                None => break,
            }
        }
        if total > 0 {
            let event = NewEvent::new("audit.retention.pruned", Actor::system("audit-retention"))
                .detail("scope", scope.to_string())
                .detail("events", total.to_string())
                .detail("until_seq", until.to_string());
            store::record(pool, &event).await?;
            out.push(Pruned {
                scope,
                events: total,
                until_seq: until,
            });
        }
    }
    Ok(out)
}

/// The retention worker: a run at start and then every `[audit] prune_interval_secs`.
pub fn spawn(pool: AnyPool, cfg: AuditConfig) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(cfg.prune_interval_secs));
        loop {
            tick.tick().await;
            match run_once(&pool, &cfg, now_ms()).await {
                Ok(p) if !p.is_empty() => {
                    let events: u64 = p.iter().map(|p| p.events).sum();
                    tracing::info!(scopes = p.len(), events, "audit retention dropped events");
                }
                Ok(_) => {}
                Err(e) => tracing::warn!(error = %e, "audit retention failed"),
            }
        }
    })
}
