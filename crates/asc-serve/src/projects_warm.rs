//! The project graph warmer (PLAN G09, SPEC §5 and §12A.5).
//!
//! A background task follows `ref_log` and derives `manifests.v1` and `pgraph.v1` for every new
//! tip of `refs/heads/*` and `refs/pull/*/head`, land-branch tips first. It never runs on the
//! `UpdateRefs` path: a push commits, and the graph is derived a moment later. A request that
//! arrives before the warmer has caught up derives the graph itself, bounded by one link per
//! distinct manifest set.
//!
//! The work per pass is bounded: at most [`MAX_ROWS`] log rows are read and at most
//! [`MAX_TIPS`] tips are derived. Tips beyond that are left for a cold request.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::time::Duration;

use asc_core::id::{ChangesetId, RefTarget};
use asc_rpc::server::RepoName;
use sqlx::Row;
use tokio::task::JoinHandle;

use crate::registry::Registry;

/// Log rows read per pass.
pub const MAX_ROWS: i64 = 2000;
/// Tips derived per pass.
pub const MAX_TIPS: usize = 256;
/// Tips remembered as warm (per server, across repositories).
const WARM_ITEMS: usize = 16 * 1024;
/// How often the log is polled when it was idle.
pub const POLL_INTERVAL: Duration = Duration::from_secs(1);

/// The changesets whose graph is known to be derived.
pub struct WarmSet {
    seen: quick_cache::sync::Cache<(i64, ChangesetId), ()>,
}

impl WarmSet {
    pub fn new() -> Self {
        Self {
            seen: quick_cache::sync::Cache::new(WARM_ITEMS),
        }
    }

    pub fn insert(&self, repo: i64, cs: ChangesetId) {
        self.seen.insert((repo, cs), ());
    }

    pub fn contains(&self, repo: i64, cs: ChangesetId) -> bool {
        self.seen.get(&(repo, cs)).is_some()
    }
}

impl Default for WarmSet {
    fn default() -> Self {
        Self::new()
    }
}

/// What the warmer and the service have done, for tests and metrics.
#[derive(Default)]
pub struct Stats {
    /// Tips whose graph the warmer derived.
    pub derived: AtomicU64,
    /// Tips the warmer could not derive (an invalid config, a damaged object, a deleted repo).
    pub failed: AtomicU64,
    /// Tips left to a cold request because a pass hit its bound.
    pub dropped: AtomicU64,
    /// Requests whose changeset was already warm.
    pub warm_requests: AtomicU64,
    /// Requests that had to derive the graph themselves.
    pub cold_requests: AtomicU64,
}

impl Stats {
    pub fn derived(&self) -> u64 {
        self.derived.load(Ordering::Relaxed)
    }

    pub fn warm_requests(&self) -> u64 {
        self.warm_requests.load(Ordering::Relaxed)
    }

    pub fn cold_requests(&self) -> u64 {
        self.cold_requests.load(Ordering::Relaxed)
    }
}

/// Why a pass failed as a whole.
#[derive(Debug, thiserror::Error)]
pub enum WarmError {
    #[error("projects warmer: database: {0}")]
    Sql(#[from] sqlx::Error),
}

/// A tip to derive.
struct Tip {
    repo: i64,
    cs: ChangesetId,
    land: bool,
    seq: i64,
}

/// Follows `ref_log` and derives graphs.
pub struct Warmer {
    registry: Arc<Registry>,
    warm: Arc<WarmSet>,
    stats: Arc<Stats>,
    cursor: AtomicI64,
}

/// Whether `name` is a ref whose tip is worth warming.
fn warmable(name: &str) -> bool {
    if name.starts_with("refs/heads/") {
        return true;
    }
    name.strip_prefix("refs/pull/")
        .and_then(|r| r.strip_suffix("/head"))
        .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
}

impl Warmer {
    pub fn new(registry: Arc<Registry>, warm: Arc<WarmSet>, stats: Arc<Stats>) -> Self {
        Self {
            registry,
            warm,
            stats,
            cursor: AtomicI64::new(0),
        }
    }

    pub fn stats(&self) -> &Arc<Stats> {
        &self.stats
    }

    /// Starts after the newest log row: tips that existed before startup are not back-filled.
    pub async fn start_at_head(&self) -> Result<(), WarmError> {
        let row = sqlx::query("SELECT COALESCE(MAX(seq), 0) AS head FROM ref_log")
            .fetch_one(self.registry.pool())
            .await?;
        self.cursor.store(row.try_get("head")?, Ordering::SeqCst);
        Ok(())
    }

    /// The land branches of repository `repo`, as full ref names.
    async fn land_branches(&self, repo: i64) -> Result<HashSet<String>, WarmError> {
        let row = sqlx::query("SELECT default_branch, land_branches FROM repos WHERE id = $1")
            .bind(repo)
            .fetch_optional(self.registry.pool())
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

    /// Reads the next batch of log rows and derives their tips. Returns the number of log rows
    /// read, so the caller can go on at once when the batch was full.
    pub async fn run_once(&self) -> Result<usize, WarmError> {
        let from = self.cursor.load(Ordering::SeqCst);
        let rows = sqlx::query(
            "SELECT seq, repo_id, name, new FROM ref_log WHERE seq > $1 ORDER BY seq LIMIT $2",
        )
        .bind(from)
        .bind(MAX_ROWS)
        .fetch_all(self.registry.pool())
        .await?;
        let Some(last) = rows.last() else {
            return Ok(0);
        };
        let last: i64 = last.try_get("seq")?;
        // Only the newest tip of each ref matters; older ones are its ancestors' business.
        let mut newest: HashMap<(i64, String), Tip> = HashMap::new();
        for row in &rows {
            let (seq, repo): (i64, i64) = (row.try_get("seq")?, row.try_get("repo_id")?);
            let name: String = row.try_get("name")?;
            let new: Option<Vec<u8>> = row.try_get("new")?;
            if !warmable(&name) {
                continue;
            }
            let cs = match new.as_deref().map(RefTarget::from_bytes) {
                Some(Ok(RefTarget::Changeset(cs))) => cs,
                _ => {
                    newest.remove(&(repo, name));
                    continue;
                }
            };
            newest.insert(
                (repo, name),
                Tip {
                    repo,
                    cs,
                    land: false,
                    seq,
                },
            );
        }
        let mut lands: HashMap<i64, HashSet<String>> = HashMap::new();
        let mut tips: Vec<Tip> = Vec::with_capacity(newest.len());
        for ((repo, name), mut tip) in newest {
            if let std::collections::hash_map::Entry::Vacant(e) = lands.entry(repo) {
                e.insert(self.land_branches(repo).await?);
            }
            tip.land = lands.get(&repo).is_some_and(|l| l.contains(&name));
            tips.push(tip);
        }
        // Land-branch tips first, then oldest log row first.
        tips.sort_by_key(|t| (!t.land, t.seq));
        let dropped = tips.len().saturating_sub(MAX_TIPS);
        tips.truncate(MAX_TIPS);
        self.stats
            .dropped
            .fetch_add(dropped as u64, Ordering::Relaxed);
        let mut names: HashMap<i64, Option<RepoName>> = HashMap::new();
        let mut done: HashSet<(i64, ChangesetId)> = HashSet::new();
        for tip in tips {
            if !done.insert((tip.repo, tip.cs)) || self.warm.contains(tip.repo, tip.cs) {
                continue;
            }
            let name = match names.get(&tip.repo) {
                Some(n) => n.clone(),
                None => {
                    let n = self.repo_name(tip.repo).await?;
                    names.insert(tip.repo, n.clone());
                    n
                }
            };
            let Some(name) = name else {
                self.stats.failed.fetch_add(1, Ordering::Relaxed);
                continue;
            };
            match self.derive(&name, tip.repo, tip.cs).await {
                Ok(()) => {
                    self.stats.derived.fetch_add(1, Ordering::Relaxed);
                }
                Err(message) => {
                    self.stats.failed.fetch_add(1, Ordering::Relaxed);
                    tracing::debug!(repo = %name, changeset = %tip.cs, %message, "projects warm failed");
                }
            }
        }
        self.cursor.store(last, Ordering::SeqCst);
        Ok(rows.len())
    }

    async fn repo_name(&self, repo: i64) -> Result<Option<RepoName>, WarmError> {
        let row = sqlx::query("SELECT name FROM repos WHERE id = $1")
            .bind(repo)
            .fetch_optional(self.registry.pool())
            .await?;
        Ok(match row {
            Some(r) => RepoName::parse(&r.try_get::<String, _>("name")?).ok(),
            None => None,
        })
    }

    async fn derive(&self, name: &RepoName, repo: i64, cs: ChangesetId) -> Result<(), String> {
        let entry = self.registry.open(name).await.map_err(|e| e.to_string())?;
        entry
            .backend
            .engine()
            .project_graph(cs)
            .await
            .map_err(|e| e.to_string())?;
        self.warm.insert(repo, cs);
        Ok(())
    }

    /// Runs passes until aborted: back to back while the log has more, else every
    /// [`POLL_INTERVAL`].
    pub fn spawn(self: Arc<Self>) -> JoinHandle<()> {
        tokio::spawn(async move {
            if let Err(e) = self.start_at_head().await {
                tracing::error!(error = %e, "projects warmer cannot start");
                return;
            }
            let mut tick = tokio::time::interval(POLL_INTERVAL);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tick.tick().await;
                loop {
                    match self.run_once().await {
                        Ok(n) if n as i64 >= MAX_ROWS => {}
                        Ok(_) => break,
                        Err(e) => {
                            tracing::warn!(error = %e, "projects warmer pass failed");
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
    use super::*;

    #[test]
    fn warms_branches_and_pull_heads_only() {
        assert!(warmable("refs/heads/master"));
        assert!(warmable("refs/heads/feat/x"));
        assert!(warmable("refs/pull/12/head"));
        assert!(!warmable("refs/pull/12/merge"));
        assert!(!warmable("refs/pull//head"));
        assert!(!warmable("refs/pull/x/head"));
        assert!(!warmable("refs/tags/v1"));
        assert!(!warmable("refs/remotes/origin/master"));
    }
}
