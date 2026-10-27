//! Retention of CI insights data: test cases older than `[ci_insights] retention_days`, daily
//! totals older than 180 days, and failure groups with no occurrence left that nobody triaged.

use std::time::Duration;

use sqlx::AnyPool;

use super::InsightsConfig;

const DAY: i64 = 86_400;
/// Days of daily totals kept.
const DAILY_DAYS: i64 = 180;
/// Seconds between two runs of the worker.
const INTERVAL: Duration = Duration::from_secs(3600);

/// What one run deleted.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Deleted {
    pub cases: u64,
    pub days: u64,
    pub groups: u64,
}

/// Deletes what is older than the retention at `now` (unix seconds).
pub async fn run_once(
    pool: &AnyPool,
    cfg: &InsightsConfig,
    now: i64,
) -> Result<Deleted, sqlx::Error> {
    let mut out = Deleted::default();
    if cfg.retention_days > 0 {
        let cutoff = now - i64::from(cfg.retention_days) * DAY;
        out.cases = sqlx::query("DELETE FROM test_cases WHERE at < $1")
            .bind(cutoff)
            .execute(pool)
            .await?
            .rows_affected();
        // A group that nothing refers to any more and whose last sighting is past retention.
        out.groups = sqlx::query(
            "DELETE FROM failure_groups WHERE last_seen < $1 AND NOT EXISTS \
             (SELECT 1 FROM test_cases c WHERE c.ns = failure_groups.ns AND c.group_id = failure_groups.id)",
        )
        .bind(cutoff)
        .execute(pool)
        .await?
        .rows_affected();
    }
    out.days = sqlx::query("DELETE FROM flaky_daily WHERE day < $1")
        .bind(now - DAILY_DAYS * DAY)
        .execute(pool)
        .await?
        .rows_affected();
    Ok(out)
}

/// The worker: a run at start and then every hour.
pub fn spawn(pool: AnyPool, cfg: InsightsConfig) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(INTERVAL);
        loop {
            tick.tick().await;
            match run_once(&pool, &cfg, crate::review::now()).await {
                Ok(d) if d != Deleted::default() => {
                    tracing::info!(
                        cases = d.cases,
                        groups = d.groups,
                        days = d.days,
                        "ci insights retention"
                    );
                }
                Ok(_) => {}
                Err(e) => tracing::warn!(error = %e, "ci insights retention failed"),
            }
        }
    })
}
