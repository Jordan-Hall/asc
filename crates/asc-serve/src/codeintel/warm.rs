//! Scope warm (SPEC §23.5): derives `tdocs` for new land-branch tips, `refs/pull/*/head` and
//! draft change heads, following `ref_log` the way `projects_warm` does. Deriving a scope
//! assigns ords to the files it meets and enqueues their file jobs (the `OrdSource` of the
//! scope engine does that), so the work is queued in land-first order and memoised by the
//! engine. It never runs on the write path.
//!
//! The pass is bounded: at most [`MAX_ROWS`] log rows and [`MAX_TIPS`] tips. Tips beyond that
//! are left for the first query that needs them.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use asc_core::id::{ChangesetId, RefTarget};
use sqlx::{AnyPool, Row};
use tokio::task::JoinHandle;

use super::modelgen::Result;
use super::queue::Priority;
use super::sweep::{HeadKind, Scopes};

/// Log rows read per pass.
pub const MAX_ROWS: i64 = 2000;
/// Tips derived per pass.
pub const MAX_TIPS: usize = 256;
/// How often the log is polled when it was idle.
pub const POLL_INTERVAL: Duration = Duration::from_secs(1);

/// Finds the scope engine of a repository.
#[async_trait]
pub trait ScopeProvider: Send + Sync {
    /// `None` when the repository is gone.
    async fn scopes(&self, repo_id: i64) -> Option<Arc<dyn Scopes>>;
}

/// A tip to warm.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Tip {
    pub repo: i64,
    pub name: String,
    pub rev: ChangesetId,
    pub kind: HeadKind,
    pub seq: i64,
}

impl Tip {
    /// Land tips lease first, then PR and draft heads.
    pub fn priority(&self) -> Priority {
        match self.kind {
            HeadKind::Land => Priority::LandTip,
            _ => Priority::PrHead,
        }
    }
}

/// What the warmer has done.
#[derive(Debug, Default)]
pub struct Stats {
    pub derived: AtomicU64,
    pub failed: AtomicU64,
    pub dropped: AtomicU64,
}

/// The kind of head `name` is, if it is one worth warming: branches, `refs/pull/N/head`, and
/// draft or change heads.
pub fn head_kind(name: &str, land: &HashSet<String>) -> Option<HeadKind> {
    if land.contains(name) {
        return Some(HeadKind::Land);
    }
    if name.starts_with("refs/heads/") {
        return Some(HeadKind::Pr);
    }
    if name
        .strip_prefix("refs/pull/")
        .and_then(|r| r.strip_suffix("/head"))
        .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
    {
        return Some(HeadKind::Pr);
    }
    if ["refs/drafts/", "refs/changes/"]
        .iter()
        .any(|p| name.len() > p.len() && name.starts_with(p))
    {
        return Some(HeadKind::Draft);
    }
    None
}

/// Follows `ref_log` and warms scopes.
pub struct ScopeWarmer {
    pool: AnyPool,
    provider: Arc<dyn ScopeProvider>,
    stats: Arc<Stats>,
    cursor: AtomicI64,
}

impl ScopeWarmer {
    pub fn new(pool: AnyPool, provider: Arc<dyn ScopeProvider>) -> Self {
        Self {
            pool,
            provider,
            stats: Arc::new(Stats::default()),
            cursor: AtomicI64::new(0),
        }
    }

    pub fn stats(&self) -> &Arc<Stats> {
        &self.stats
    }

    pub fn cursor(&self) -> i64 {
        self.cursor.load(Ordering::SeqCst)
    }

    /// Starts after the newest log row: tips that existed before startup are warmed by the
    /// import backfill, not here.
    pub async fn start_at_head(&self) -> Result<()> {
        let row = sqlx::query("SELECT CAST(COALESCE(MAX(seq), 0) AS BIGINT) AS head FROM ref_log")
            .fetch_one(&self.pool)
            .await?;
        self.cursor.store(row.try_get("head")?, Ordering::SeqCst);
        Ok(())
    }

    async fn land_branches(&self, repo: i64) -> Result<HashSet<String>> {
        let row = sqlx::query("SELECT default_branch, land_branches FROM repos WHERE id = $1")
            .bind(repo)
            .fetch_optional(&self.pool)
            .await?;
        let Some(row) = row else {
            return Ok(HashSet::new());
        };
        let default: String = row.try_get("default_branch")?;
        let json: String = row.try_get("land_branches").unwrap_or_default();
        let listed: Vec<String> = serde_json::from_str(&json).unwrap_or_default();
        let full = |b: &str| {
            if b.starts_with("refs/") {
                b.to_owned()
            } else {
                format!("refs/heads/{b}")
            }
        };
        Ok(if listed.is_empty() {
            HashSet::from([full(&default)])
        } else {
            listed.iter().map(|b| full(b)).collect()
        })
    }

    /// Reads the next batch of log rows and warms their tips, land tips first. Returns the
    /// tips it derived, in order.
    pub async fn run_once(&self) -> Result<(usize, Vec<Tip>)> {
        let from = self.cursor.load(Ordering::SeqCst);
        let rows = sqlx::query(
            "SELECT CAST(seq AS BIGINT) AS seq, CAST(repo_id AS BIGINT) AS repo_id, name, new \
             FROM ref_log WHERE seq > $1 ORDER BY seq LIMIT $2",
        )
        .bind(from)
        .bind(MAX_ROWS)
        .fetch_all(&self.pool)
        .await?;
        let Some(last) = rows.last() else {
            return Ok((0, Vec::new()));
        };
        let last: i64 = last.try_get("seq")?;
        // Only the newest tip of each ref matters.
        let mut newest: HashMap<(i64, String), (ChangesetId, i64)> = HashMap::new();
        for row in &rows {
            let (seq, repo): (i64, i64) = (row.try_get("seq")?, row.try_get("repo_id")?);
            let name: String = row.try_get("name")?;
            let new: Option<Vec<u8>> = row.try_get("new")?;
            match new.as_deref().map(RefTarget::from_bytes) {
                Some(Ok(RefTarget::Changeset(cs))) => {
                    newest.insert((repo, name), (cs, seq));
                }
                _ => {
                    newest.remove(&(repo, name));
                }
            }
        }
        let mut lands: HashMap<i64, HashSet<String>> = HashMap::new();
        let mut tips = Vec::new();
        for ((repo, name), (rev, seq)) in newest {
            let land = match lands.entry(repo) {
                std::collections::hash_map::Entry::Occupied(e) => e.into_mut(),
                std::collections::hash_map::Entry::Vacant(e) => {
                    e.insert(self.land_branches(repo).await?)
                }
            };
            if let Some(kind) = head_kind(&name, land) {
                tips.push(Tip {
                    repo,
                    name,
                    rev,
                    kind,
                    seq,
                });
            }
        }
        tips.sort_by_key(|t| (t.priority(), t.seq));
        let dropped = tips.len().saturating_sub(MAX_TIPS);
        tips.truncate(MAX_TIPS);
        self.stats
            .dropped
            .fetch_add(dropped as u64, Ordering::Relaxed);
        let mut done = Vec::new();
        let mut seen = HashSet::new();
        for tip in tips {
            if !seen.insert((tip.repo, tip.rev)) {
                continue;
            }
            let Some(scopes) = self.provider.scopes(tip.repo).await else {
                self.stats.failed.fetch_add(1, Ordering::Relaxed);
                continue;
            };
            match scopes.ords(tip.rev).await {
                Ok(_) => {
                    self.stats.derived.fetch_add(1, Ordering::Relaxed);
                    done.push(tip);
                }
                Err(e) => {
                    self.stats.failed.fetch_add(1, Ordering::Relaxed);
                    tracing::debug!(repo = tip.repo, rev = %tip.rev, error = %e, "scope warm failed");
                }
            }
        }
        self.cursor.store(last, Ordering::SeqCst);
        Ok((rows.len(), done))
    }

    /// Runs passes until aborted: back to back while the log has more, else every
    /// [`POLL_INTERVAL`].
    pub fn spawn(self: Arc<Self>) -> JoinHandle<()> {
        tokio::spawn(async move {
            if let Err(e) = self.start_at_head().await {
                tracing::error!(error = %e, "scope warmer cannot start");
                return;
            }
            let mut tick = tokio::time::interval(POLL_INTERVAL);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tick.tick().await;
                loop {
                    match self.run_once().await {
                        Ok((n, _)) if n as i64 >= MAX_ROWS => {}
                        Ok(_) => break,
                        Err(e) => {
                            tracing::warn!(error = %e, "scope warmer pass failed");
                            break;
                        }
                    }
                }
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use roaring::RoaringBitmap;

    use super::*;
    use crate::codeintel::modelgen::{CiError, tests::pool};

    struct Recorder {
        seen: Mutex<Vec<ChangesetId>>,
    }

    #[async_trait]
    impl Scopes for Recorder {
        async fn ords(&self, rev: ChangesetId) -> Result<RoaringBitmap> {
            if rev == ChangesetId::from_bytes([99; 32]) {
                return Err(CiError::Invalid("cannot derive"));
            }
            self.seen.lock().unwrap().push(rev);
            Ok(RoaringBitmap::new())
        }
    }

    struct One(Arc<Recorder>);

    #[async_trait]
    impl ScopeProvider for One {
        async fn scopes(&self, repo_id: i64) -> Option<Arc<dyn Scopes>> {
            (repo_id == 1).then(|| self.0.clone() as Arc<dyn Scopes>)
        }
    }

    fn rev(n: u8) -> ChangesetId {
        ChangesetId::from_bytes([n; 32])
    }

    async fn log(pool: &AnyPool, repo: i64, name: &str, new: Option<ChangesetId>) {
        sqlx::query(
            "INSERT INTO ref_log (repo_id, name, old, new, actor, via, reason, op_id, at) \
             VALUES ($1, $2, NULL, $3, 'a', 'push', '', $4, 1)",
        )
        .bind(repo)
        .bind(name.to_owned())
        .bind(new.map(|c| RefTarget::Changeset(c).to_bytes().to_vec()))
        .bind(vec![0u8; 16])
        .execute(pool)
        .await
        .unwrap();
    }

    #[test]
    fn classifies_heads() {
        let land = HashSet::from(["refs/heads/master".to_owned()]);
        assert_eq!(head_kind("refs/heads/master", &land), Some(HeadKind::Land));
        assert_eq!(head_kind("refs/heads/feat", &land), Some(HeadKind::Pr));
        assert_eq!(head_kind("refs/pull/12/head", &land), Some(HeadKind::Pr));
        assert_eq!(head_kind("refs/pull/12/merge", &land), None);
        assert_eq!(head_kind("refs/pull/x/head", &land), None);
        assert_eq!(head_kind("refs/drafts/me/1", &land), Some(HeadKind::Draft));
        assert_eq!(head_kind("refs/drafts/", &land), None);
        assert_eq!(head_kind("refs/tags/v1", &land), None);
    }

    #[tokio::test]
    async fn warms_land_tips_first_and_skips_what_it_cannot_derive() {
        let pool = pool().await;
        sqlx::query("INSERT INTO repos (id, name, ns, created) VALUES (1, 'r', $1, 0)")
            .bind(b"ns".to_vec())
            .execute(&pool)
            .await
            .unwrap();
        let rec = Arc::new(Recorder {
            seen: Mutex::new(Vec::new()),
        });
        let w = ScopeWarmer::new(pool.clone(), Arc::new(One(rec.clone())));
        w.start_at_head().await.unwrap();

        log(&pool, 1, "refs/pull/3/head", Some(rev(1))).await;
        log(&pool, 1, "refs/heads/master", Some(rev(2))).await;
        log(&pool, 1, "refs/heads/master", Some(rev(3))).await; // supersedes rev(2)
        log(&pool, 1, "refs/drafts/me/1", Some(rev(4))).await;
        log(&pool, 1, "refs/tags/v1", Some(rev(5))).await;
        log(&pool, 1, "refs/heads/broken", Some(rev(99))).await;
        log(&pool, 2, "refs/heads/master", Some(rev(6))).await; // unknown repo
        log(&pool, 1, "refs/heads/gone", Some(rev(7))).await;
        log(&pool, 1, "refs/heads/gone", None).await; // deleted again

        let (rows, tips) = w.run_once().await.unwrap();
        assert_eq!(rows, 9);
        let seen = rec.seen.lock().unwrap().clone();
        assert_eq!(seen, [rev(3), rev(1), rev(4)]); // land tip, then PR head, then draft
        assert_eq!(tips[0].kind, HeadKind::Land);
        assert_eq!(w.stats().derived.load(Ordering::Relaxed), 3);
        assert_eq!(w.stats().failed.load(Ordering::Relaxed), 2); // broken scope, unknown repo
        // The cursor moved: the next pass has nothing to do.
        assert_eq!(w.run_once().await.unwrap().0, 0);
    }
}
