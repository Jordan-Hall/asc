//! `[pack]` and `[import]` settings, and the background job that packs, merges and compacts
//! (docs/design/PACKED-OBJECTS.md).
//!
//! Reading packs is always on; everything that writes one waits for `[pack] enabled`. The job
//! runs once per `repack_interval` for every repository, holding a run lock so two servers do not
//! repack at once, and enters the GC fence only around the steps that delete loose objects or
//! retire a pack.

use std::sync::Arc;
use std::time::{Duration, SystemTime};

use asc_git::import::ImportPolicy;
use asc_rpc::server::RepoName;
use asc_store::objects::Error as StoreError;
use asc_store::pack::PackConfig;
use asc_store::pack::job::{PackReport, PackStatus, ReindexReport, VerifyReport};
use serde::Deserialize;
use sqlx::Row;
use tokio::task::JoinHandle;

use crate::config::{ByteSize, ConfigError, Secs};
use crate::gc::ServerFence;
use crate::registry::Registry;
use crate::update::PipelineHooks;

/// Postgres advisory lock key of the repack job (distinct from the GC keys).
const PACK_RUN_KEY: i64 = 0x6173_6370_6163_6b31;

/// `[pack]`: packed objects. Off unless `enabled`.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct PackSettings {
    /// Writes packs (imports, the repack job). Reading packs is always on.
    pub enabled: bool,
    /// Time between repack runs.
    pub repack_interval: Secs,
    /// Largest stored value that is packed.
    pub max_object: ByteSize,
    /// Largest pack file.
    pub max_size: ByteSize,
    /// Most objects in one pack.
    pub max_objects: usize,
    /// A loose object younger than this is not packed.
    pub min_age: Secs,
    /// A repository with fewer old loose small objects is left alone.
    pub min_loose: usize,
    /// Packs smaller than this are merged.
    pub merge_below: ByteSize,
    /// Merge when at least this many small packs exist.
    pub merge_count: usize,
    /// A pack with less than this percent of its bytes still referenced is compacted.
    pub compact_below_percent: u8,
    /// A retired pack file is deleted this long after it was retired.
    pub retire_age: Secs,
    /// A pack file no index knows is deleted when older than this.
    pub orphan_age: Secs,
    /// A bulk write of fewer small objects than this stays loose.
    pub min_batch: usize,
}

impl Default for PackSettings {
    fn default() -> Self {
        let d = PackConfig::default();
        Self {
            enabled: false,
            repack_interval: Secs(3600),
            max_object: ByteSize(d.max_object as u64),
            max_size: ByteSize(d.max_size),
            max_objects: d.max_objects,
            min_age: Secs(d.min_age.as_secs()),
            min_loose: d.min_loose,
            merge_below: ByteSize(d.merge_below),
            merge_count: d.merge_count,
            compact_below_percent: 50,
            retire_age: Secs(d.retire_age.as_secs()),
            orphan_age: Secs(d.orphan_age.as_secs()),
            min_batch: d.min_batch,
        }
    }
}

impl PackSettings {
    /// The policy these settings describe, or why they are refused.
    pub fn to_config(&self) -> Result<PackConfig, ConfigError> {
        let bad = |m: &str| Err(ConfigError::Invalid(format!("[pack] {m}")));
        if !(60..=7 * 86_400).contains(&self.repack_interval.0) {
            return bad("repack_interval must be 1m to 7d");
        }
        if !(64..=1 << 20).contains(&self.max_object.0) {
            return bad("max_object must be 64B to 1MiB");
        }
        if !((1 << 20)..=(1 << 30)).contains(&self.max_size.0) {
            return bad("max_size must be 1MiB to 1GiB");
        }
        if !(2..=1_000_000).contains(&self.max_objects) {
            return bad("max_objects must be 2 to 1000000");
        }
        if self.max_size.0 < self.max_object.0.saturating_mul(2) {
            return bad("max_size must hold at least two max_object values");
        }
        if self.min_age.0 < 60 {
            return bad("min_age must be at least 1m, so a push in flight is not touched");
        }
        if self.merge_count < 2 {
            return bad("merge_count must be at least 2");
        }
        if !(1..=100).contains(&self.compact_below_percent) {
            return bad("compact_below_percent must be 1 to 100");
        }
        if self.orphan_age.0 < 600 {
            return bad("orphan_age must be at least 10m");
        }
        if self.min_batch == 0 {
            return bad("min_batch must be at least 1");
        }
        Ok(PackConfig {
            enabled: self.enabled,
            max_object: self.max_object.0 as usize,
            max_size: self.max_size.0,
            max_objects: self.max_objects,
            min_age: Duration::from_secs(self.min_age.0),
            min_loose: self.min_loose,
            merge_below: self.merge_below.0,
            merge_count: self.merge_count,
            compact_below: f64::from(self.compact_below_percent) / 100.0,
            retire_age: Duration::from_secs(self.retire_age.0),
            orphan_age: Duration::from_secs(self.orphan_age.0),
            min_batch: self.min_batch,
            negative_ttl: PackConfig::default().negative_ttl,
        })
    }
}

/// `[import]`: how a git import writes.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct ImportSettings {
    /// Commits per batch: a batch's objects are written first, then its rows in one call each.
    /// `1` restores a write after every commit.
    pub batch_commits: usize,
    /// Stored bytes buffered before the objects are written, whatever the commit count.
    pub batch_bytes: ByteSize,
}

impl Default for ImportSettings {
    fn default() -> Self {
        Self {
            batch_commits: asc_git::import::BATCH_COMMITS,
            batch_bytes: ByteSize(asc_git::import::BATCH_STORED_BYTES),
        }
    }
}

impl ImportSettings {
    pub fn to_policy(&self) -> Result<ImportPolicy, ConfigError> {
        if !(1..=1_000_000).contains(&self.batch_commits) {
            return Err(ConfigError::Invalid(
                "[import] batch_commits must be 1 to 1000000".into(),
            ));
        }
        if !((1 << 20)..=(8u64 << 30)).contains(&self.batch_bytes.0) {
            return Err(ConfigError::Invalid(
                "[import] batch_bytes must be 1MiB to 8GiB".into(),
            ));
        }
        Ok(ImportPolicy {
            batch_commits: self.batch_commits,
            batch_bytes: self.batch_bytes.0,
            ..ImportPolicy::default()
        })
    }
}

/// Why a pack operation failed.
#[derive(Debug, thiserror::Error)]
pub enum PackError {
    #[error("{0}")]
    Busy(String),
    #[error("{0}: {1}")]
    Repo(String, String),
    #[error("store: {0}")]
    Store(#[from] StoreError),
    #[error("database: {0}")]
    Sql(#[from] sqlx::Error),
}

/// Holds the run lock until dropped or released.
struct RunGuard {
    _local: tokio::sync::OwnedMutexGuard<()>,
    pg: Option<sqlx::pool::PoolConnection<sqlx::Any>>,
}

impl RunGuard {
    async fn release(mut self) {
        if let Some(mut c) = self.pg.take() {
            let _ = sqlx::query("SELECT pg_advisory_unlock($1)")
                .bind(PACK_RUN_KEY)
                .execute(&mut *c)
                .await;
        }
    }
}

/// Packs, merges and compacts every repository.
pub struct PackService {
    registry: Arc<Registry>,
    hooks: Arc<PipelineHooks>,
    running: Arc<tokio::sync::Mutex<()>>,
}

impl PackService {
    pub fn new(registry: Arc<Registry>, hooks: Arc<PipelineHooks>) -> Self {
        Self {
            registry,
            hooks,
            running: Arc::default(),
        }
    }

    async fn acquire(&self) -> Result<RunGuard, PackError> {
        let local = self
            .running
            .clone()
            .try_lock_owned()
            .map_err(|_| PackError::Busy("a pack run is going in this server".into()))?;
        let pool = self.registry.pool();
        let postgres = pool
            .connect_options()
            .database_url
            .scheme()
            .starts_with("postgres");
        let pg = if postgres {
            let mut conn = pool.acquire().await?;
            let got: bool = sqlx::query_scalar("SELECT pg_try_advisory_lock($1)")
                .bind(PACK_RUN_KEY)
                .fetch_one(&mut *conn)
                .await?;
            if !got {
                return Err(PackError::Busy("another server is packing".into()));
            }
            Some(conn)
        } else {
            None
        };
        Ok(RunGuard { _local: local, pg })
    }

    fn fence(&self) -> ServerFence {
        ServerFence {
            hooks: self.hooks.clone(),
            pool: self.registry.pool().clone(),
        }
    }

    async fn open(&self, name: &RepoName) -> Result<Arc<crate::registry::RepoEntry>, PackError> {
        self.registry
            .open(name)
            .await
            .map_err(|e| PackError::Repo(name.to_string(), e.to_string()))
    }

    /// One maintenance pass over `name`. `force` packs even below `min_loose`.
    pub async fn run_repo(
        &self,
        name: &RepoName,
        now: SystemTime,
        force: bool,
    ) -> Result<PackReport, PackError> {
        let guard = self.acquire().await?;
        let out = async {
            let entry = self.open(name).await?;
            Ok(entry.objects.pack_run(&self.fence(), now, force).await?)
        }
        .await;
        guard.release().await;
        out
    }

    /// Every repository in turn; one failing does not stop the others.
    pub async fn run_all(
        &self,
        now: SystemTime,
        force: bool,
    ) -> Vec<(String, Result<PackReport, PackError>)> {
        let names = match self.names().await {
            Ok(n) => n,
            Err(e) => return vec![("(repos)".into(), Err(e))],
        };
        let mut out = Vec::new();
        for n in names {
            let res = match RepoName::parse(&n) {
                Ok(name) => self.run_repo(&name, now, force).await,
                Err(_) => Err(PackError::Repo(n.clone(), "the name is invalid".into())),
            };
            out.push((n, res));
        }
        out
    }

    async fn names(&self) -> Result<Vec<String>, PackError> {
        let rows = sqlx::query("SELECT name FROM repos ORDER BY id")
            .fetch_all(self.registry.pool())
            .await?;
        Ok(rows
            .iter()
            .filter_map(|r| r.try_get::<String, _>(0).ok())
            .collect())
    }

    /// Rewrites every pack holding a dead entry, after an operator purge.
    pub async fn compact_all(
        &self,
        name: &RepoName,
        now: SystemTime,
    ) -> Result<PackReport, PackError> {
        let entry = self.open(name).await?;
        Ok(entry.objects.pack_compact_all(&self.fence(), now).await?)
    }

    pub async fn status(&self, name: &RepoName) -> Result<PackStatus, PackError> {
        Ok(self.open(name).await?.objects.pack_status().await?)
    }

    pub async fn verify(&self, name: &RepoName) -> Result<VerifyReport, PackError> {
        Ok(self.open(name).await?.objects.pack_verify().await?)
    }

    pub async fn reindex(&self, name: &RepoName) -> Result<ReindexReport, PackError> {
        let guard = self.acquire().await?;
        let out = async { Ok(self.open(name).await?.objects.pack_reindex().await?) }.await;
        guard.release().await;
        out
    }

    /// Runs [`run_all`](Self::run_all) every `interval` until aborted. A pass decodes and hashes
    /// every object it packs, so it runs on a blocking thread and not on the async workers.
    pub fn spawn_worker(self: Arc<Self>, interval: Duration) -> JoinHandle<()> {
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(interval).await;
                let this = self.clone();
                let handle = tokio::runtime::Handle::current();
                let pass = tokio::task::spawn_blocking(move || {
                    handle.block_on(this.run_all(SystemTime::now(), false))
                })
                .await;
                let Ok(results) = pass else {
                    tracing::warn!("a pack pass did not finish");
                    continue;
                };
                for (name, res) in results {
                    match res {
                        Ok(r) if r.packs_written + r.deleted + r.orphans + r.emptied > 0 => {
                            tracing::info!(
                                repo = %name,
                                packed = r.packed,
                                packs = r.packs_written,
                                merged = r.merged,
                                compacted = r.compacted,
                                deleted = r.deleted,
                                "pack run finished"
                            );
                        }
                        Ok(_) => {}
                        Err(PackError::Busy(m)) => tracing::debug!(repo = %name, "{m}"),
                        Err(e) => tracing::warn!(repo = %name, error = %e, "pack run failed"),
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
    fn the_defaults_are_valid_and_match_the_library() {
        let c = PackSettings::default().to_config().unwrap();
        assert_eq!(c.max_object, PackConfig::default().max_object);
        assert!(!c.enabled);
        assert!((c.compact_below - 0.5).abs() < f64::EPSILON);
        let p = ImportSettings::default().to_policy().unwrap();
        assert_eq!(p.batch_commits, 500);
        assert_eq!(p.batch_bytes, 128 << 20);
    }

    #[test]
    fn nonsense_is_refused() {
        let s = |f: fn(&mut PackSettings)| {
            let mut s = PackSettings::default();
            f(&mut s);
            s.to_config().is_err()
        };
        assert!(s(|s| s.repack_interval = Secs(1)));
        assert!(s(|s| s.max_object = ByteSize(1)));
        assert!(s(|s| s.max_size = ByteSize(10)));
        assert!(s(|s| s.max_objects = 1));
        assert!(s(|s| s.min_age = Secs(0)));
        assert!(s(|s| s.merge_count = 1));
        assert!(s(|s| s.compact_below_percent = 0));
        assert!(s(|s| s.compact_below_percent = 101));
        assert!(s(|s| s.orphan_age = Secs(5)));
        assert!(s(|s| s.min_batch = 0));
        let bad = ImportSettings {
            batch_commits: 0,
            ..ImportSettings::default()
        };
        assert!(bad.to_policy().is_err());
    }

    #[test]
    fn the_toml_sections_parse_and_reject_unknown_keys() {
        let s: PackSettings =
            toml::from_str("enabled = true\nmin_age = \"15m\"\nmax_size = \"32MiB\"").unwrap();
        let c = s.to_config().unwrap();
        assert!(c.enabled);
        assert_eq!(c.min_age, Duration::from_secs(900));
        assert_eq!(c.max_size, 32 << 20);
        assert!(toml::from_str::<PackSettings>("enable = true").is_err());
        let i: ImportSettings = toml::from_str("batch_commits = 50").unwrap();
        assert_eq!(i.to_policy().unwrap().batch_commits, 50);
    }
}
