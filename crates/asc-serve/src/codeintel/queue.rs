//! The code-intelligence job queue (`ci_jobs`, SPEC §23.5, PLAN I13).
//!
//! * `ci_jobs` has the primary key `(ns, kind, key, version)` and every insert is
//!   `ON CONFLICT DO NOTHING`, so an enqueue is idempotent and costs O(changed paths).
//! * A worker leases jobs by priority then age. Postgres selects with `FOR UPDATE SKIP LOCKED`;
//!   SQLite is one process, so one in-process mutex serialises the lease.
//! * A lease that is not extended expires and the job is handed out again. A failed job backs
//!   off and is retried; after [`MAX_ATTEMPTS`] it is parked as `dead` for an operator.
//! * The outbox effect `codeintel.enqueue` runs after the write commits, never on the write
//!   path. It asks a [`ChangeSource`] for the files the new changesets write, drops those
//!   `ci_docs` already knows, and enqueues a `file` job for the rest.

use std::collections::HashSet;
use std::sync::Arc;

use async_trait::async_trait;
use sqlx::{AnyPool, Row};
use tokio::sync::Mutex;

use crate::outbox::{Effect, EffectError, Job, now};

/// Name of the outbox effect.
pub const EFFECT: &str = "codeintel.enqueue";
/// Job kind for indexing one file.
pub const KIND_FILE: &str = "file";
/// Pipeline version stored in file jobs' `version`.
pub const FILE_JOB_VERSION: i64 = 1;
/// Failures after which a job is parked as `dead`.
pub const MAX_ATTEMPTS: i64 = 8;
/// Longest backoff after a failure.
pub const MAX_BACKOFF_SECS: i64 = 300;
/// Jobs one `enqueue` call accepts.
pub const MAX_ENQUEUE: usize = 100_000;
/// Jobs one `lease` call returns at most.
pub const MAX_LEASE: i64 = 1_000;
const MAX_KIND: usize = 32;
const MAX_KEY: usize = 256;
const MAX_ERROR_CHARS: usize = 512;
/// Rows per transaction while enqueuing.
const CHUNK: usize = 500;
/// Longest lease a caller can ask for.
const MAX_LEASE_SECS: i64 = 3600;
/// Refs one outbox document may list for this effect.
const MAX_REFS: usize = 1024;

/// Job priority; lower leases first.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
#[repr(i64)]
pub enum Priority {
    Interactive = 0,
    LandTip = 1,
    PrHead = 2,
    Backfill = 3,
}

impl Priority {
    fn from_i64(v: i64) -> Self {
        match v {
            0 => Self::Interactive,
            1 => Self::LandTip,
            2 => Self::PrHead,
            _ => Self::Backfill,
        }
    }

    /// The priority of work caused by an update of `ref_name`: pull requests, drafts and
    /// changes are heads; everything else (branches) is a land tip.
    pub fn for_ref(ref_name: &str) -> Self {
        let heads = ["refs/pull/", "refs/drafts/", "refs/changes/", "refs/for/"];
        if heads.iter().any(|p| ref_name.starts_with(p)) {
            Self::PrHead
        } else {
            Self::LandTip
        }
    }
}

/// Why a queue call failed.
#[derive(Debug, thiserror::Error)]
pub enum QueueError {
    #[error("database: {0}")]
    Sql(#[from] sqlx::Error),
    #[error("invalid job: {0}")]
    Invalid(&'static str),
}

/// A job to add.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NewJob {
    pub kind: String,
    pub key: Vec<u8>,
    pub version: i64,
    pub priority: Priority,
}

impl NewJob {
    /// A `file` job for `file` (the FileId bytes).
    pub fn file(file: &[u8], priority: Priority) -> Self {
        Self {
            kind: KIND_FILE.into(),
            key: file.to_vec(),
            version: FILE_JOB_VERSION,
            priority,
        }
    }
}

/// A leased job. `attempts` is the lease token: completing or failing with a stale lease
/// (the job was handed out again) changes nothing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Leased {
    pub ns: Vec<u8>,
    pub kind: String,
    pub key: Vec<u8>,
    pub version: i64,
    pub priority: Priority,
    pub attempts: i64,
    pub lease_until: i64,
}

/// Counts of jobs as `(kind, state, count)`.
pub type Depth = Vec<(String, String, i64)>;

/// Seconds to wait after the `attempts`-th failure (2, 4, … up to 5 minutes).
pub fn backoff_secs(attempts: i64) -> i64 {
    let exp = u32::try_from(attempts.clamp(1, 20)).unwrap_or(20);
    (1_i64 << exp).min(MAX_BACKOFF_SECS)
}

/// The job queue.
#[derive(Clone)]
pub struct Queue {
    pool: AnyPool,
    postgres: bool,
    /// SQLite has one writer process: one lease at a time keeps selection and claim atomic.
    local: Arc<Mutex<()>>,
}

impl Queue {
    pub fn new(pool: AnyPool) -> Self {
        let postgres = matches!(
            pool.connect_options().database_url.scheme(),
            "postgres" | "postgresql"
        );
        Self {
            pool,
            postgres,
            local: Arc::new(Mutex::new(())),
        }
    }

    /// Adds jobs; those whose `(ns, kind, key, version)` already exist are no-ops. Returns
    /// the number inserted.
    pub async fn enqueue(&self, ns: &[u8], jobs: &[NewJob], now: i64) -> Result<u64, QueueError> {
        if jobs.len() > MAX_ENQUEUE {
            return Err(QueueError::Invalid("too many jobs"));
        }
        for j in jobs {
            if j.kind.is_empty() || j.kind.len() > MAX_KIND || j.key.len() > MAX_KEY {
                return Err(QueueError::Invalid("kind or key size"));
            }
        }
        let mut inserted = 0;
        for chunk in jobs.chunks(CHUNK) {
            let mut tx = self.pool.begin().await?;
            for j in chunk {
                inserted += sqlx::query(
                    "INSERT INTO ci_jobs (ns, kind, key, version, priority, state, attempts, \
                                          available_at, created_at) \
                     VALUES ($1, $2, $3, $4, $5, 'queued', 0, $6, $6) \
                     ON CONFLICT (ns, kind, key, version) DO NOTHING",
                )
                .bind(ns.to_vec())
                .bind(j.kind.clone())
                .bind(j.key.clone())
                .bind(j.version)
                .bind(j.priority as i64)
                .bind(now)
                .execute(&mut *tx)
                .await?
                .rows_affected();
            }
            tx.commit().await?;
        }
        Ok(inserted)
    }

    /// Leases up to `limit` ready jobs of `ns` for `lease_secs`, most urgent first (priority,
    /// then age). Ready means queued and due, or leased with an expired lease.
    pub async fn lease(
        &self,
        ns: &[u8],
        limit: i64,
        lease_secs: i64,
        now: i64,
    ) -> Result<Vec<Leased>, QueueError> {
        let limit = limit.clamp(0, MAX_LEASE);
        let until = now.saturating_add(lease_secs.clamp(1, MAX_LEASE_SECS));
        let _guard = if self.postgres {
            None
        } else {
            Some(self.local.lock().await)
        };
        let mut tx = self.pool.begin().await?;
        let rows = sqlx::query(if self.postgres {
            SELECT_PG
        } else {
            SELECT_LOCAL
        })
        .bind(ns.to_vec())
        .bind(now)
        .bind(limit)
        .fetch_all(&mut *tx)
        .await?;
        let mut out = Vec::with_capacity(rows.len());
        for r in rows {
            let kind: String = r.try_get("kind")?;
            let key: Vec<u8> = r.try_get("key")?;
            let version: i64 = r.try_get("version")?;
            let attempts: i64 = r.try_get::<i64, _>("attempts")? + 1;
            sqlx::query(
                "UPDATE ci_jobs SET state = 'leased', lease_until = $1, attempts = $2 \
                 WHERE ns = $3 AND kind = $4 AND key = $5 AND version = $6",
            )
            .bind(until)
            .bind(attempts)
            .bind(ns.to_vec())
            .bind(kind.clone())
            .bind(key.clone())
            .bind(version)
            .execute(&mut *tx)
            .await?;
            out.push(Leased {
                ns: ns.to_vec(),
                kind,
                key,
                version,
                priority: Priority::from_i64(r.try_get("priority")?),
                attempts,
                lease_until: until,
            });
        }
        tx.commit().await?;
        Ok(out)
    }

    /// Namespaces that have a ready job (for a worker to pick from).
    pub async fn ready_namespaces(&self, now: i64, limit: i64) -> Result<Vec<Vec<u8>>, QueueError> {
        let rows = sqlx::query(
            "SELECT DISTINCT ns FROM ci_jobs \
             WHERE (state = 'queued' AND available_at <= $1) \
                OR (state = 'leased' AND lease_until <= $1) LIMIT $2",
        )
        .bind(now)
        .bind(limit.clamp(0, MAX_LEASE))
        .fetch_all(&self.pool)
        .await?;
        rows.iter()
            .map(|r| r.try_get("ns").map_err(QueueError::from))
            .collect()
    }

    /// Pushes a held lease out to `now + lease_secs`. False when the lease is no longer ours.
    pub async fn extend(
        &self,
        job: &Leased,
        lease_secs: i64,
        now: i64,
    ) -> Result<bool, QueueError> {
        let until = now.saturating_add(lease_secs.clamp(1, MAX_LEASE_SECS));
        let n = sqlx::query(
            "UPDATE ci_jobs SET lease_until = $1 \
             WHERE ns = $2 AND kind = $3 AND key = $4 AND version = $5 \
               AND state = 'leased' AND attempts = $6",
        )
        .bind(until)
        .bind(job.ns.clone())
        .bind(job.kind.clone())
        .bind(job.key.clone())
        .bind(job.version)
        .bind(job.attempts)
        .execute(&self.pool)
        .await?
        .rows_affected();
        Ok(n == 1)
    }

    /// Deletes a finished job. False when the lease is no longer ours.
    pub async fn complete(&self, job: &Leased) -> Result<bool, QueueError> {
        let n = sqlx::query(
            "DELETE FROM ci_jobs \
             WHERE ns = $1 AND kind = $2 AND key = $3 AND version = $4 \
               AND state = 'leased' AND attempts = $5",
        )
        .bind(job.ns.clone())
        .bind(job.kind.clone())
        .bind(job.key.clone())
        .bind(job.version)
        .bind(job.attempts)
        .execute(&self.pool)
        .await?
        .rows_affected();
        Ok(n == 1)
    }

    /// Records a failure: the job is retried after a backoff, or parked as `dead` after
    /// [`MAX_ATTEMPTS`]. Returns true when it was parked. A stale lease changes nothing.
    pub async fn fail(&self, job: &Leased, error: &str, now: i64) -> Result<bool, QueueError> {
        let dead = job.attempts >= MAX_ATTEMPTS;
        let msg: String = error.chars().take(MAX_ERROR_CHARS).collect();
        sqlx::query(
            "UPDATE ci_jobs SET state = $1, available_at = $2, lease_until = NULL, last_error = $3 \
             WHERE ns = $4 AND kind = $5 AND key = $6 AND version = $7 \
               AND state = 'leased' AND attempts = $8",
        )
        .bind(if dead { "dead" } else { "queued" })
        .bind(now.saturating_add(backoff_secs(job.attempts)))
        .bind(msg)
        .bind(job.ns.clone())
        .bind(job.kind.clone())
        .bind(job.key.clone())
        .bind(job.version)
        .bind(job.attempts)
        .execute(&self.pool)
        .await?;
        Ok(dead)
    }

    /// Job counts by kind and state (the `asc_ci_jobs{kind,state}` gauge, I29).
    pub async fn depth(&self) -> Result<Depth, QueueError> {
        let rows = sqlx::query(
            "SELECT kind, state, CAST(COUNT(*) AS BIGINT) AS n FROM ci_jobs GROUP BY kind, state",
        )
        .fetch_all(&self.pool)
        .await?;
        rows.iter()
            .map(|r| Ok((r.try_get("kind")?, r.try_get("state")?, r.try_get("n")?)))
            .collect()
    }
}

const SELECT_LOCAL: &str = "SELECT kind, key, CAST(version AS BIGINT) AS version,         CAST(priority AS BIGINT) AS priority, CAST(attempts AS BIGINT) AS attempts      FROM ci_jobs      WHERE ns = $1 AND ((state = 'queued' AND available_at <= $2)                      OR (state = 'leased' AND lease_until <= $2))      ORDER BY priority, created_at, kind, key LIMIT $3";
const SELECT_PG: &str = "SELECT kind, key, CAST(version AS BIGINT) AS version,         CAST(priority AS BIGINT) AS priority, CAST(attempts AS BIGINT) AS attempts      FROM ci_jobs      WHERE ns = $1 AND ((state = 'queued' AND available_at <= $2)                      OR (state = 'leased' AND lease_until <= $2))      ORDER BY priority, created_at, kind, key LIMIT $3 FOR UPDATE SKIP LOCKED";

// ------------------------------------------------------------------------- the outbox effect

/// The files newly written by the changesets an update made reachable.
#[derive(Clone, Debug, Default)]
pub struct Written {
    /// The repository's namespace id.
    pub ns: Vec<u8>,
    /// `FileId` bytes of every `Write` change of the new changesets, in any order.
    pub files: Vec<Vec<u8>>,
}

/// Resolves an update to the files it writes. Cost must be O(changed paths): it walks the new
/// changesets' `changes`, never the trees.
#[async_trait]
pub trait ChangeSource: Send + Sync {
    /// `targets` are the new ref targets of the update, in the wire form the outbox document
    /// stores (kind byte, then the id).
    async fn written(&self, repo_id: i64, targets: &[Vec<u8>]) -> Result<Written, EffectError>;
}

/// The `codeintel.enqueue` outbox effect. Delivery is at least once and enqueue is idempotent.
pub struct EnqueueEffect {
    pool: AnyPool,
    queue: Queue,
    source: Arc<dyn ChangeSource>,
}

impl EnqueueEffect {
    pub fn new(pool: AnyPool, source: Arc<dyn ChangeSource>) -> Self {
        Self {
            queue: Queue::new(pool.clone()),
            pool,
            source,
        }
    }

    /// Enqueues a `file` job for each of `files` that `ci_docs` does not know.
    pub async fn enqueue_unknown(
        &self,
        ns: &[u8],
        files: &[Vec<u8>],
        priority: Priority,
    ) -> Result<u64, QueueError> {
        let mut unknown = Vec::new();
        let mut seen = HashSet::new();
        for f in files {
            if f.len() > MAX_KEY || !seen.insert(f.clone()) {
                continue;
            }
            let known = sqlx::query("SELECT 1 AS x FROM ci_docs WHERE ns = $1 AND file = $2")
                .bind(ns.to_vec())
                .bind(f.clone())
                .fetch_optional(&self.pool)
                .await?
                .is_some();
            if !known {
                unknown.push(NewJob::file(f, priority));
            }
        }
        self.queue.enqueue(ns, &unknown, now()).await
    }
}

fn unhex(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) || !s.is_ascii() || s.len() > 2 * MAX_KEY {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok())
        .collect()
}

#[async_trait]
impl Effect for EnqueueEffect {
    fn kind(&self) -> &'static str {
        EFFECT
    }

    async fn run(&self, job: &Job) -> Result<(), EffectError> {
        let refs = job
            .doc
            .get("refs")
            .and_then(|v| v.as_array())
            .map_or(&[][..], Vec::as_slice);
        if refs.len() > MAX_REFS {
            return Err(EffectError("too many refs".into()));
        }
        let mut targets = Vec::new();
        let mut priority = Priority::Backfill;
        for r in refs {
            let Some(new) = r.get("new").and_then(|v| v.as_str()) else {
                continue; // a deletion makes nothing reachable
            };
            let bytes = unhex(new).ok_or_else(|| EffectError("malformed ref target".into()))?;
            targets.push(bytes);
            let name = r.get("name").and_then(|v| v.as_str()).unwrap_or("");
            priority = priority.min(Priority::for_ref(name));
        }
        if targets.is_empty() {
            return Ok(());
        }
        let written = self.source.written(job.repo_id, &targets).await?;
        self.enqueue_unknown(&written.ns, &written.files, priority)
            .await
            .map_err(|e| EffectError(e.to_string()))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex as StdMutex;

    use super::*;
    use crate::migrate;

    async fn pool() -> AnyPool {
        let p = migrate::connect("sqlite::memory:", 1).await.unwrap();
        migrate::migrate(&p).await.unwrap();
        p
    }

    fn jobs(n: u8, p: Priority) -> Vec<NewJob> {
        (0..n).map(|i| NewJob::file(&[i; 32], p)).collect()
    }

    #[tokio::test]
    async fn duplicate_enqueues_are_no_ops() {
        let q = Queue::new(pool().await);
        assert_eq!(
            q.enqueue(b"ns", &jobs(3, Priority::Backfill), 10)
                .await
                .unwrap(),
            3
        );
        assert_eq!(
            q.enqueue(b"ns", &jobs(5, Priority::Backfill), 11)
                .await
                .unwrap(),
            2
        );
        assert_eq!(
            q.enqueue(b"other", &jobs(1, Priority::Backfill), 11)
                .await
                .unwrap(),
            1
        );
        let depth = q.depth().await.unwrap();
        assert_eq!(depth, [("file".to_string(), "queued".to_string(), 6)]);
    }

    #[tokio::test]
    async fn rejects_oversized_keys() {
        let q = Queue::new(pool().await);
        let bad = NewJob::file(&vec![0; MAX_KEY + 1], Priority::Backfill);
        assert!(q.enqueue(b"ns", &[bad], 0).await.is_err());
    }

    #[tokio::test]
    async fn leases_by_priority_then_age_and_never_twice() {
        let q = Queue::new(pool().await);
        q.enqueue(b"ns", &[NewJob::file(&[1; 32], Priority::Backfill)], 1)
            .await
            .unwrap();
        q.enqueue(b"ns", &[NewJob::file(&[2; 32], Priority::PrHead)], 2)
            .await
            .unwrap();
        q.enqueue(b"ns", &[NewJob::file(&[3; 32], Priority::LandTip)], 3)
            .await
            .unwrap();
        q.enqueue(b"ns", &[NewJob::file(&[4; 32], Priority::Interactive)], 4)
            .await
            .unwrap();
        q.enqueue(b"ns", &[NewJob::file(&[5; 32], Priority::LandTip)], 0)
            .await
            .unwrap();
        let got = q.lease(b"ns", 10, 30, 100).await.unwrap();
        let order: Vec<u8> = got.iter().map(|j| j.key[0]).collect();
        assert_eq!(order, [4, 5, 3, 2, 1]);
        assert!(got.iter().all(|j| j.attempts == 1 && j.lease_until == 130));
        assert!(q.lease(b"ns", 10, 30, 101).await.unwrap().is_empty());
        assert!(q.lease(b"other", 10, 30, 101).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn concurrent_leases_do_not_overlap() {
        let q = Queue::new(pool().await);
        q.enqueue(b"ns", &jobs(20, Priority::Backfill), 1)
            .await
            .unwrap();
        let (a, b) = tokio::join!(q.lease(b"ns", 12, 30, 5), q.lease(b"ns", 12, 30, 5));
        let (a, b) = (a.unwrap(), b.unwrap());
        assert_eq!(a.len() + b.len(), 20);
        let mut keys: Vec<_> = a.iter().chain(&b).map(|j| j.key.clone()).collect();
        keys.sort();
        keys.dedup();
        assert_eq!(keys.len(), 20);
    }

    #[tokio::test]
    async fn an_expired_lease_is_handed_out_again_and_the_old_holder_is_stale() {
        let q = Queue::new(pool().await);
        q.enqueue(b"ns", &jobs(1, Priority::LandTip), 1)
            .await
            .unwrap();
        let first = q.lease(b"ns", 1, 10, 100).await.unwrap().remove(0);
        assert!(q.extend(&first, 10, 105).await.unwrap());
        assert!(
            q.lease(b"ns", 1, 10, 112).await.unwrap().is_empty(),
            "extended to 115"
        );
        let second = q.lease(b"ns", 1, 10, 115).await.unwrap().remove(0);
        assert_eq!(second.attempts, 2);
        assert!(!q.complete(&first).await.unwrap(), "stale holder");
        assert!(!q.extend(&first, 10, 116).await.unwrap());
        assert!(q.complete(&second).await.unwrap());
        assert!(q.depth().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn failures_back_off_then_park_as_dead() {
        let q = Queue::new(pool().await);
        q.enqueue(b"ns", &jobs(1, Priority::LandTip), 0)
            .await
            .unwrap();
        let mut now = 100;
        for attempt in 1..=MAX_ATTEMPTS {
            let j = q.lease(b"ns", 1, 10, now).await.unwrap().remove(0);
            assert_eq!(j.attempts, attempt);
            let dead = q.fail(&j, "boom", now).await.unwrap();
            assert_eq!(dead, attempt == MAX_ATTEMPTS);
            if !dead {
                assert!(
                    q.lease(b"ns", 1, 10, now).await.unwrap().is_empty(),
                    "backing off"
                );
            }
            now += backoff_secs(attempt);
        }
        assert!(
            q.lease(b"ns", 1, 10, now + 10_000)
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            q.depth().await.unwrap(),
            [("file".to_string(), "dead".to_string(), 1)]
        );
    }

    #[test]
    fn backoff_is_capped() {
        assert_eq!(backoff_secs(1), 2);
        assert_eq!(backoff_secs(3), 8);
        assert_eq!(backoff_secs(i64::MAX), MAX_BACKOFF_SECS);
    }

    #[test]
    fn ref_priorities() {
        assert_eq!(Priority::for_ref("refs/heads/master"), Priority::LandTip);
        assert_eq!(Priority::for_ref("refs/pull/4/head"), Priority::PrHead);
        assert!(Priority::Interactive < Priority::LandTip && Priority::PrHead < Priority::Backfill);
    }

    struct Fixed(StdMutex<Vec<Vec<u8>>>, Vec<u8>);

    #[async_trait]
    impl ChangeSource for Fixed {
        async fn written(&self, _repo: i64, targets: &[Vec<u8>]) -> Result<Written, EffectError> {
            assert!(targets.iter().all(|t| t.len() == 3));
            Ok(Written {
                ns: self.1.clone(),
                files: self.0.lock().unwrap().clone(),
            })
        }
    }

    fn outbox_job(doc: serde_json::Value) -> Job {
        Job {
            id: 1,
            repo_id: 1,
            op_id: vec![1],
            doc,
        }
    }

    #[tokio::test]
    async fn the_effect_enqueues_only_unknown_files_once() {
        let pool = pool().await;
        sqlx::query(
            "INSERT INTO ci_docs (ns, file, ord, size, versions) VALUES ($1, $2, 0, 1, $3)",
        )
        .bind(b"ns".to_vec())
        .bind(vec![1u8; 32])
        .bind(Vec::<u8>::new())
        .execute(&pool)
        .await
        .unwrap();
        let files = vec![vec![1u8; 32], vec![2; 32], vec![2; 32], vec![3; 32]];
        let src = Arc::new(Fixed(StdMutex::new(files), b"ns".to_vec()));
        let effect = EnqueueEffect::new(pool.clone(), src);
        let job = outbox_job(serde_json::json!({
            "effects": [EFFECT],
            "refs": [
                {"name": "refs/heads/master", "old": null, "new": "010203"},
                {"name": "refs/pull/1/head", "old": null, "new": "010204"},
                {"name": "refs/heads/gone", "old": "010203", "new": null},
            ],
        }));
        effect.run(&job).await.unwrap();
        effect.run(&job).await.unwrap(); // redelivery is a no-op
        let q = Queue::new(pool);
        let got = q.lease(b"ns", 10, 30, now() + 1).await.unwrap();
        let keys: Vec<u8> = got.iter().map(|j| j.key[0]).collect();
        assert_eq!(keys, [2, 3]);
        assert!(got.iter().all(|j| j.priority == Priority::LandTip));
    }

    #[tokio::test]
    async fn the_effect_rejects_malformed_targets() {
        let src = Arc::new(Fixed(StdMutex::new(vec![]), b"ns".to_vec()));
        let effect = EnqueueEffect::new(pool().await, src);
        let job = outbox_job(serde_json::json!({"refs": [{"name": "r", "new": "zz"}]}));
        assert!(effect.run(&job).await.is_err());
    }
}
