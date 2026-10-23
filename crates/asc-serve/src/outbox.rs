//! The outbox worker (SPEC §8.3, PLAN U43c).
//!
//! The step-8 transaction of `UpdateRefs` inserts one `outbox` row with the effects to run
//! after commit. This worker delivers them at least once: rows of one repository run in id
//! order (a failing row blocks the repository's later rows, never another repository's), each
//! effect is idempotent, and a failed row backs off from 1 s to 5 min, indefinitely. A worker
//! leases a row by moving `next_at` forward, so replicas do not deliver the same row at once and
//! a crashed worker's rows come back when the lease expires.

use std::collections::HashMap;
use std::panic::AssertUnwindSafe;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use futures::FutureExt;
use sqlx::{AnyPool, Row};
use tokio::sync::Notify;

use crate::http::Metrics;

/// Longest backoff between attempts.
pub const MAX_BACKOFF_SECS: i64 = 300;
/// Failures after which a row is logged as `outbox.dead` (it is still retried).
pub const DEAD_AFTER: i64 = 10;
/// Effects one row may list.
const MAX_EFFECTS: usize = 16;
/// Largest accepted `effects` document.
const MAX_DOC_BYTES: usize = 1024 * 1024;
/// Longest `last_error` stored.
const MAX_ERROR_CHARS: usize = 512;
/// Rows examined when measuring lag.
const LAG_SAMPLE: i64 = 32;

pub(crate) fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

/// Seconds to wait after the `failures`-th failure of a row (1, 2, 4, … up to 5 minutes).
pub fn backoff_secs(failures: i64) -> i64 {
    let exp = u32::try_from((failures - 1).clamp(0, 20)).unwrap_or(20);
    (1_i64 << exp).min(MAX_BACKOFF_SECS)
}

/// Total backoff a row with `failures` failures has waited: its age since it was due.
fn waited_secs(failures: i64) -> i64 {
    (1..=failures.min(12)).map(backoff_secs).sum::<i64>()
        + (failures - 12).max(0) * MAX_BACKOFF_SECS
}

/// One undelivered row.
#[derive(Clone, Debug)]
pub struct Job {
    pub id: i64,
    pub repo_id: i64,
    pub op_id: Vec<u8>,
    /// The whole `effects` document (`effects`, `pusher`, `refs`, … as the writer defined it).
    pub doc: serde_json::Value,
}

/// Why an effect failed; the row is retried.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct EffectError(pub String);

/// A side effect of a committed write. Implementations (Forgejo post-receive, bridge promotion,
/// cache eviction, code-intelligence enqueue) must be idempotent: a row is delivered at least once.
#[async_trait]
pub trait Effect: Send + Sync {
    /// The name rows list in `effects`.
    fn kind(&self) -> &'static str;
    async fn run(&self, job: &Job) -> Result<(), EffectError>;
}

/// Accepts the effect and does nothing: the placeholder until the owning unit registers the
/// real one.
pub struct NoopEffect(pub &'static str);

#[async_trait]
impl Effect for NoopEffect {
    fn kind(&self) -> &'static str {
        self.0
    }

    async fn run(&self, _job: &Job) -> Result<(), EffectError> {
        Ok(())
    }
}

/// The effects the worker can run, by name. Registering a kind again replaces it.
#[derive(Default)]
pub struct Effects {
    map: Mutex<HashMap<&'static str, Arc<dyn Effect>>>,
}

impl Effects {
    pub fn register(&self, e: Arc<dyn Effect>) {
        self.map
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(e.kind(), e);
    }

    fn get(&self, kind: &str) -> Option<Arc<dyn Effect>> {
        self.map
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(kind)
            .cloned()
    }
}

/// Worker settings.
#[derive(Clone, Debug)]
pub struct OutboxConfig {
    /// How long a claimed row is hidden from other workers.
    pub lease_secs: i64,
    /// Repositories delivered per pass.
    pub batch: i64,
    /// Sleep between passes when nothing woke the worker.
    pub poll: Duration,
}

impl Default for OutboxConfig {
    fn default() -> Self {
        Self {
            lease_secs: 60,
            batch: 16,
            poll: Duration::from_secs(1),
        }
    }
}

/// The outbox worker and its effect registry.
pub struct Outbox {
    pool: AnyPool,
    effects: Effects,
    metrics: Arc<Metrics>,
    cfg: OutboxConfig,
    wake: Notify,
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

impl Outbox {
    pub fn new(pool: AnyPool, metrics: Arc<Metrics>, cfg: OutboxConfig) -> Self {
        let outbox = Self {
            pool,
            effects: Effects::default(),
            metrics,
            cfg,
            wake: Notify::new(),
        };
        // Until U44 and the bridge arrive, `UpdateRefs` rows list only these.
        for kind in ["post_receive", "bridge_promote", crate::codeintel::queue::EFFECT] {
            outbox.effects.register(Arc::new(NoopEffect(kind)));
        }
        outbox
    }

    /// The effect registry: one registration line per unit.
    pub fn effects(&self) -> &Effects {
        &self.effects
    }

    /// Asks the worker to run a pass now (after a commit queued a row).
    pub fn wake(&self) {
        self.wake.notify_one();
    }

    /// Age in seconds of the oldest undelivered row, 0 when none.
    pub async fn lag_secs(&self) -> Result<i64, sqlx::Error> {
        let rows = sqlx::query(
            "SELECT CAST(next_at AS BIGINT) AS next_at, CAST(attempts AS BIGINT) AS attempts \
             FROM outbox WHERE done_at IS NULL ORDER BY id LIMIT $1",
        )
        .bind(LAG_SAMPLE)
        .fetch_all(&self.pool)
        .await?;
        let now = now();
        let mut lag = 0;
        for r in rows {
            let due_since = r.try_get::<i64, _>("next_at")? - waited_secs(r.try_get("attempts")?);
            lag = lag.max(now - due_since);
        }
        Ok(lag.max(0))
    }

    /// Runs passes until the task is dropped or aborted. Delivery is at least once, so abrupt
    /// cancellation is safe.
    pub async fn run(self: Arc<Self>) {
        loop {
            match self.deliver_due(now()).await {
                Ok(n) if n > 0 => continue,
                Ok(_) => {}
                Err(e) => tracing::error!(event = "outbox.pass", error = %e),
            }
            self.metrics
                .outbox_lag
                .set(self.lag_secs().await.unwrap_or(0));
            let _ = tokio::time::timeout(self.cfg.poll, self.wake.notified()).await;
        }
    }

    /// One pass at time `now`: delivers the head row of each repository whose row is due.
    /// Returns the rows examined.
    pub async fn deliver_due(&self, now: i64) -> Result<usize, sqlx::Error> {
        let rows = sqlx::query(
            "SELECT CAST(o.id AS BIGINT) AS id, CAST(o.repo_id AS BIGINT) AS repo_id, \
                    o.op_id AS op_id, o.effects AS effects, \
                    CAST(o.attempts AS BIGINT) AS attempts, CAST(o.next_at AS BIGINT) AS next_at \
             FROM outbox o \
             WHERE o.done_at IS NULL AND o.next_at <= $1 \
               AND o.id = (SELECT MIN(i.id) FROM outbox i \
                           WHERE i.repo_id = o.repo_id AND i.done_at IS NULL) \
             ORDER BY o.id LIMIT $2",
        )
        .bind(now)
        .bind(self.cfg.batch)
        .fetch_all(&self.pool)
        .await?;
        let mut work = Vec::new();
        for r in rows {
            let id: i64 = r.try_get("id")?;
            let attempts: i64 = r.try_get("attempts")?;
            let next_at: i64 = r.try_get("next_at")?;
            let claimed = sqlx::query(
                "UPDATE outbox SET next_at = $1 WHERE id = $2 AND next_at = $3 AND done_at IS NULL",
            )
            .bind(now + self.cfg.lease_secs)
            .bind(id)
            .bind(next_at)
            .execute(&self.pool)
            .await?
            .rows_affected();
            if claimed != 1 {
                continue; // another replica has it
            }
            let doc: Vec<u8> = r.try_get("effects")?;
            work.push((
                Job {
                    id,
                    repo_id: r.try_get("repo_id")?,
                    op_id: r.try_get("op_id")?,
                    doc: serde_json::Value::Null,
                },
                doc,
                attempts,
            ));
        }
        let n = work.len();
        let results = futures::future::join_all(
            work.into_iter()
                .map(|(job, doc, attempts)| self.deliver(job, doc, attempts, now)),
        )
        .await;
        for r in results {
            r?;
        }
        Ok(n)
    }

    async fn deliver(
        &self,
        mut job: Job,
        doc: Vec<u8>,
        attempts: i64,
        now: i64,
    ) -> Result<(), sqlx::Error> {
        let outcome = AssertUnwindSafe(self.run_effects(&mut job, &doc))
            .catch_unwind()
            .await
            .unwrap_or_else(|_| Err(EffectError("effect panicked".into())));
        match outcome {
            Ok(()) => {
                sqlx::query("UPDATE outbox SET done_at = $1, last_error = NULL WHERE id = $2")
                    .bind(now)
                    .bind(job.id)
                    .execute(&self.pool)
                    .await?;
            }
            Err(e) => {
                let failures = attempts + 1;
                let msg: String = e.0.chars().take(MAX_ERROR_CHARS).collect();
                let (repo_id, op_id) = (job.repo_id, hex(&job.op_id));
                if failures >= DEAD_AFTER {
                    tracing::error!(event = "outbox.dead", repo_id, %op_id, failures, error = %msg);
                } else {
                    tracing::warn!(event = "outbox.retry", repo_id, %op_id, failures, error = %msg);
                }
                self.metrics.outbox_failures.inc();
                sqlx::query(
                    "UPDATE outbox SET attempts = $1, next_at = $2, last_error = $3 WHERE id = $4",
                )
                .bind(i32::try_from(failures).unwrap_or(i32::MAX))
                .bind(now + backoff_secs(failures))
                .bind(msg)
                .bind(job.id)
                .execute(&self.pool)
                .await?;
            }
        }
        Ok(())
    }

    /// Parses the row's document and runs its effects in listed order.
    async fn run_effects(&self, job: &mut Job, doc: &[u8]) -> Result<(), EffectError> {
        if doc.len() > MAX_DOC_BYTES {
            return Err(EffectError("effects document too large".into()));
        }
        job.doc = serde_json::from_slice(doc)
            .map_err(|e| EffectError(format!("effects document: {e}")))?;
        let kinds: Vec<String> = match job.doc.get("effects") {
            None => Vec::new(),
            Some(v) => serde_json::from_value(v.clone())
                .map_err(|e| EffectError(format!("effects list: {e}")))?,
        };
        if kinds.len() > MAX_EFFECTS {
            return Err(EffectError(format!("more than {MAX_EFFECTS} effects")));
        }
        for kind in &kinds {
            let effect = self
                .effects
                .get(kind)
                .ok_or_else(|| EffectError(format!("no effect registered for {kind:?}")))?;
            effect.run(job).await?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;
    use crate::migrate;

    async fn setup(cfg: OutboxConfig) -> (Outbox, AnyPool) {
        let pool = migrate::connect("sqlite::memory:", 1).await.unwrap();
        migrate::migrate(&pool).await.unwrap();
        let outbox = Outbox::new(pool.clone(), Arc::new(Metrics::new()), cfg);
        (outbox, pool)
    }

    async fn push(pool: &AnyPool, repo: i64, op: u8, effects: &str, at: i64) {
        let doc = format!(r#"{{"effects":{effects},"pusher":"alice","refs":[]}}"#);
        sqlx::query(
            "INSERT INTO outbox (repo_id, op_id, effects, next_at) VALUES ($1, $2, $3, $4)",
        )
        .bind(repo)
        .bind(vec![op])
        .bind(doc.into_bytes())
        .bind(at)
        .execute(pool)
        .await
        .unwrap();
    }

    async fn pending(pool: &AnyPool) -> i64 {
        sqlx::query_scalar("SELECT COUNT(*) FROM outbox WHERE done_at IS NULL")
            .fetch_one(pool)
            .await
            .unwrap()
    }

    /// Records the op ids it ran for; fails or panics on demand.
    struct Probe {
        kind: &'static str,
        seen: Mutex<Vec<(i64, u8)>>,
        mode: Mutex<Mode>,
        calls: AtomicUsize,
    }

    #[derive(Clone, Copy)]
    enum Mode {
        Ok,
        Fail,
        Panic,
    }

    impl Probe {
        fn new(kind: &'static str, mode: Mode) -> Arc<Self> {
            Arc::new(Self {
                kind,
                seen: Mutex::default(),
                mode: Mutex::new(mode),
                calls: AtomicUsize::new(0),
            })
        }

        fn set(&self, m: Mode) {
            *self.mode.lock().unwrap() = m;
        }

        fn seen(&self) -> Vec<(i64, u8)> {
            self.seen.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl Effect for Probe {
        fn kind(&self) -> &'static str {
            self.kind
        }

        async fn run(&self, job: &Job) -> Result<(), EffectError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let mode = *self.mode.lock().unwrap();
            match mode {
                Mode::Ok => {
                    self.seen.lock().unwrap().push((job.repo_id, job.op_id[0]));
                    Ok(())
                }
                Mode::Fail => Err(EffectError("down".into())),
                Mode::Panic => panic!("crash after commit, before effects"),
            }
        }
    }

    #[test]
    fn backoff_doubles_to_five_minutes() {
        let got: Vec<_> = (1..=11).map(backoff_secs).collect();
        assert_eq!(got, [1, 2, 4, 8, 16, 32, 64, 128, 256, 300, 300]);
        assert_eq!(backoff_secs(i64::MAX), 300);
        assert_eq!(waited_secs(0), 0);
        assert_eq!(waited_secs(3), 7);
    }

    #[tokio::test]
    async fn delivers_in_listed_order_and_marks_done() {
        let (outbox, pool) = setup(OutboxConfig::default()).await;
        let a = Probe::new("a", Mode::Ok);
        let b = Probe::new("b", Mode::Ok);
        outbox.effects().register(a.clone());
        outbox.effects().register(b.clone());
        push(&pool, 1, 1, r#"["a","b","post_receive"]"#, 100).await;
        assert_eq!(outbox.deliver_due(100).await.unwrap(), 1);
        assert_eq!(pending(&pool).await, 0);
        assert_eq!(a.seen(), [(1, 1)]);
        assert_eq!(b.seen(), [(1, 1)]);
        assert_eq!(outbox.deliver_due(101).await.unwrap(), 0);
    }

    #[tokio::test]
    async fn a_failing_row_blocks_only_its_repository_and_backs_off() {
        let (outbox, pool) = setup(OutboxConfig::default()).await;
        let flaky = Probe::new("flaky", Mode::Fail);
        let fine = Probe::new("fine", Mode::Ok);
        outbox.effects().register(flaky.clone());
        outbox.effects().register(fine.clone());
        push(&pool, 1, 1, r#"["flaky"]"#, 100).await;
        push(&pool, 1, 2, r#"["fine"]"#, 100).await;
        push(&pool, 2, 3, r#"["fine"]"#, 100).await;

        outbox.deliver_due(100).await.unwrap();
        assert_eq!(fine.seen(), [(2, 3)], "repo 2 is not blocked by repo 1");
        assert_eq!(pending(&pool).await, 2);

        // Within the backoff nothing is retried; repo 1's second row waits behind the first.
        outbox.deliver_due(100).await.unwrap();
        assert_eq!(flaky.calls.load(Ordering::SeqCst), 1);

        flaky.set(Mode::Ok);
        outbox.deliver_due(101).await.unwrap();
        outbox.deliver_due(101).await.unwrap();
        assert_eq!(flaky.seen(), [(1, 1)]);
        assert_eq!(fine.seen(), [(2, 3), (1, 2)], "per-repo id order");
        assert_eq!(pending(&pool).await, 0);
    }

    #[tokio::test]
    async fn backoff_grows_with_each_failure() {
        let (outbox, pool) = setup(OutboxConfig::default()).await;
        outbox.effects().register(Probe::new("bad", Mode::Fail));
        push(&pool, 1, 1, r#"["bad"]"#, 0).await;
        let mut at = 0;
        for failures in 1..=4_i64 {
            assert_eq!(outbox.deliver_due(at).await.unwrap(), 1);
            let next: i64 = sqlx::query_scalar("SELECT next_at FROM outbox")
                .fetch_one(&pool)
                .await
                .unwrap();
            assert_eq!(next, at + backoff_secs(failures));
            at = next;
        }
        let (n, err): (i64, String) = sqlx::query_as("SELECT attempts, last_error FROM outbox")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!((n, err.as_str()), (4, "down"));
    }

    #[tokio::test]
    async fn unknown_effect_is_retried_not_dropped() {
        let (outbox, pool) = setup(OutboxConfig::default()).await;
        push(&pool, 1, 1, r#"["later_unit"]"#, 0).await;
        outbox.deliver_due(0).await.unwrap();
        assert_eq!(pending(&pool).await, 1);
        outbox
            .effects()
            .register(Probe::new("later_unit", Mode::Ok));
        outbox.deliver_due(1).await.unwrap();
        assert_eq!(pending(&pool).await, 0);
    }

    #[tokio::test]
    async fn bad_documents_do_not_wedge_the_worker() {
        let (outbox, pool) = setup(OutboxConfig::default()).await;
        sqlx::query(
            "INSERT INTO outbox (repo_id, op_id, effects, next_at) VALUES (1, x'01', x'ff', 0)",
        )
        .execute(&pool)
        .await
        .unwrap();
        push(&pool, 2, 2, r#"["post_receive"]"#, 0).await;
        assert_eq!(outbox.deliver_due(0).await.unwrap(), 2);
        assert_eq!(pending(&pool).await, 1);
    }

    /// Crash injection: the effect panics after the row committed. The panic never loses the
    /// row; a restarted worker (a fresh `Outbox` over the same database) delivers it.
    #[tokio::test]
    async fn panic_after_commit_then_restart_delivers() {
        let dir = tempfile::tempdir().unwrap();
        let db = format!(
            "sqlite:{}?mode=rwc",
            dir.path()
                .join("asc.db")
                .display()
                .to_string()
                .replace('\\', "/")
        );
        let pool = migrate::connect(&db, 2).await.unwrap();
        migrate::migrate(&pool).await.unwrap();
        push(&pool, 1, 7, r#"["notify"]"#, 100).await;

        let first = Outbox::new(
            pool.clone(),
            Arc::new(Metrics::new()),
            OutboxConfig::default(),
        );
        first.effects().register(Probe::new("notify", Mode::Panic));
        first.deliver_due(100).await.unwrap();
        assert_eq!(pending(&pool).await, 1, "the row survives the crash");
        drop(first);

        // A hard kill between claim and completion leaves only the lease behind.
        let leased = Outbox::new(
            pool.clone(),
            Arc::new(Metrics::new()),
            OutboxConfig::default(),
        );
        let at = 100 + backoff_secs(1);
        sqlx::query("UPDATE outbox SET next_at = $1")
            .bind(at)
            .execute(&pool)
            .await
            .unwrap();
        let notify = Probe::new("notify", Mode::Ok);
        leased.effects().register(notify.clone());
        leased.deliver_due(at).await.unwrap(); // claims and delivers
        assert_eq!(notify.seen(), [(1, 7)]);
        assert_eq!(pending(&pool).await, 0);
    }

    #[tokio::test]
    async fn a_claimed_row_is_hidden_until_its_lease_expires() {
        let (outbox, pool) = setup(OutboxConfig::default()).await;
        push(&pool, 1, 1, r#"["post_receive"]"#, 100).await;
        // Simulate a worker that claimed the row and died: next_at moved by the lease.
        sqlx::query("UPDATE outbox SET next_at = 160")
            .execute(&pool)
            .await
            .unwrap();
        assert_eq!(outbox.deliver_due(120).await.unwrap(), 0);
        assert_eq!(outbox.deliver_due(160).await.unwrap(), 1);
        assert_eq!(pending(&pool).await, 0);
    }

    #[tokio::test]
    async fn run_loop_delivers_when_woken() {
        let (outbox, pool) = setup(OutboxConfig {
            poll: Duration::from_millis(20),
            ..OutboxConfig::default()
        })
        .await;
        let outbox = Arc::new(outbox);
        let got = Probe::new("got", Mode::Ok);
        outbox.effects().register(got.clone());
        let task = tokio::spawn(outbox.clone().run());
        push(&pool, 1, 9, r#"["got"]"#, now()).await;
        outbox.wake();
        for _ in 0..100 {
            if !got.seen().is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        task.abort();
        assert_eq!(got.seen(), [(1, 9)]);
    }

    #[tokio::test]
    async fn lag_counts_from_when_the_row_was_due() {
        let (outbox, pool) = setup(OutboxConfig::default()).await;
        assert_eq!(outbox.lag_secs().await.unwrap(), 0);
        push(&pool, 1, 1, r#"["post_receive"]"#, now() - 90).await;
        let lag = outbox.lag_secs().await.unwrap();
        assert!((90..=92).contains(&lag), "{lag}");
    }
}
