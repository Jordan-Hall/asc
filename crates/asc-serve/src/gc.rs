//! Server garbage collection (SPEC §3.6, unit O01).
//!
//! [`GcService`] runs `asc_store::gc::Collector` for every repository namespace with the server's
//! pieces: refs and the `ref_log` table as roots, `gc_marks` for the marks, the `UpdateRefs`
//! GC lock as the sweep fence, and `GcRootSource`s that other units register on
//! [`PipelineHooks::gc_roots`] (change versions, commit-cloud heads, Actions manifests).
//!
//! There is no network entry point: collection starts from the `[gc]` section of the server
//! configuration (a worker) or from an operator calling the service in process.
//!
//! The fence is the exclusive side of the lock `UpdateRefs` holds shared from ingest (step 4)
//! until its transaction commits (step 8). The lock has two halves taken in the same order by
//! both sides: an in-process `RwLock`, and on Postgres a session-level advisory lock
//! ([`PgClusterLock`], `pg_advisory_lock_shared` for a push and `pg_advisory_lock` for a sweep
//! batch) held on a dedicated pooled connection for the whole interval. A sweep in one server
//! process therefore waits for every push of every other process that has ingested and not yet
//! committed. SQLite is a single-process database, so the in-process half is the whole fence.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use asc_core::id::RefTarget;
use asc_rpc::server::RepoName;
use asc_store::gc::{
    Collector, Fence, GcConfig, GcError, GcReport, GcRootSource, MarkRun, MarkStore, RefRoots,
    SweepObserver, Tier,
};
use asc_store::key::ObjKey;
use asc_store::refs::{Refs, RepoKey};
use asc_store::sqlrefs::SqlRefs;
use async_trait::async_trait;
use serde::Deserialize;
use sqlx::any::Any;
use sqlx::pool::PoolConnection;
use sqlx::{AnyPool, Row};
use tokio::task::JoinHandle;

use crate::config::ConfigError;
use crate::registry::Registry;
use crate::update::{GC_LOCK_KEY, PipelineHooks};

/// Postgres advisory lock that keeps two servers from collecting at once.
const GC_RUN_KEY: i64 = 0x0063_675f_6772;
/// Mark rows per `INSERT`.
const MARK_ROWS: usize = 300;
/// `gc_marks.key` of the row that seals a run (no tier byte is `0xFF`).
const DONE_KEY: &[u8] = b"\xFFdone";
/// Statements per sweep-observer transaction.
const OBSERVER_CHUNK: usize = 500;

// ------------------------------------------------------------------------------ configuration

/// `[gc]`: off unless `enabled`, because collection deletes data.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct GcSettings {
    pub enabled: bool,
    /// Hours between collections.
    pub interval_hours: u64,
    /// `asc.gc.pitrDays`: ref-log targets this recent are roots. Must be at least the Postgres
    /// point-in-time-recovery window.
    pub pitr_days: u64,
    /// Most keys held in memory for one mark.
    pub max_marks: usize,
}

impl Default for GcSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            interval_hours: 24,
            pitr_days: 14,
            max_marks: 16_000_000,
        }
    }
}

impl GcSettings {
    /// The collector policy for these settings.
    pub fn to_config(&self) -> Result<GcConfig, ConfigError> {
        let bad = |m: &str| Err(ConfigError::Invalid(format!("[gc] {m}")));
        if !(1..=24 * 365).contains(&self.interval_hours) {
            return bad("interval_hours must be 1 to 8760");
        }
        if !(1..=3650).contains(&self.pitr_days) {
            return bad("pitr_days must be 1 to 3650");
        }
        if self.max_marks < 1000 {
            return bad("max_marks must be at least 1000");
        }
        Ok(GcConfig {
            pitr: Duration::from_secs(self.pitr_days * 24 * 3600),
            max_marks: self.max_marks,
            ..GcConfig::default()
        })
    }
}

// ------------------------------------------------------------------------------ extra roots

/// The registered [`GcRootSource`]s. Units register once at startup.
#[derive(Default)]
pub struct GcRoots {
    sources: Mutex<Vec<Arc<dyn GcRootSource>>>,
}

impl GcRoots {
    pub fn register(&self, source: Arc<dyn GcRootSource>) {
        self.sources
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(source);
    }

    pub fn all(&self) -> Vec<Arc<dyn GcRootSource>> {
        self.sources
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
}

// ------------------------------------------------------------------------------ SQL pieces

fn is_postgres(pool: &AnyPool) -> bool {
    matches!(
        pool.connect_options().database_url.scheme(),
        "postgres" | "postgresql"
    )
}

fn sql(what: &str, e: impl std::fmt::Display) -> GcError {
    GcError::Marks(format!("{what}: {e}"))
}

/// Refs and the ref log of one repository as roots.
pub struct SqlRefRoots {
    pub pool: AnyPool,
    pub refs: Arc<SqlRefs>,
    pub repo: RepoKey,
}

fn target(bytes: &[u8]) -> Result<RefTarget, GcError> {
    RefTarget::from_bytes(bytes).map_err(|e| GcError::Refs(format!("corrupt ref target: {e}")))
}

#[async_trait]
impl RefRoots for SqlRefRoots {
    async fn current(&self) -> Result<Vec<RefTarget>, GcError> {
        let all = self.refs.list(self.repo, "").await?;
        Ok(all.into_iter().map(|e| e.target).collect())
    }

    async fn logged_since(&self, since: i64) -> Result<Vec<RefTarget>, GcError> {
        let mut out = Vec::new();
        // Both columns: the value a ref held when the window opened is the `old` of its first
        // change inside it.
        const SQLS: [&str; 2] = [
            "SELECT DISTINCT new FROM ref_log WHERE repo_id = $1 AND at >= $2 AND new IS NOT NULL",
            "SELECT DISTINCT old FROM ref_log WHERE repo_id = $1 AND at >= $2 AND old IS NOT NULL",
        ];
        for q in SQLS {
            let rows = sqlx::query(q)
                .bind(self.repo.0)
                .bind(since)
                .fetch_all(&self.pool)
                .await
                .map_err(|e| sql("ref_log", e))?;
            for row in rows {
                let bytes: Vec<u8> = row.try_get(0).map_err(|e| sql("ref_log", e))?;
                out.push(target(&bytes)?);
            }
        }
        Ok(out)
    }
}

/// `gc_marks` for one namespace.
pub struct SqlMarks {
    pub pool: AnyPool,
    pub ns: Vec<u8>,
}

fn mark_key(tier: Tier, key: &ObjKey) -> Vec<u8> {
    let mut v = vec![tier as u8];
    v.extend_from_slice(key.path().as_bytes());
    v
}

fn parse_mark(bytes: &[u8]) -> Option<(Tier, ObjKey)> {
    let (&t, path) = bytes.split_first()?;
    let tier = match t {
        0 => Tier::Strict,
        1 => Tier::Recent,
        _ => return None,
    };
    Some((tier, ObjKey::parse(std::str::from_utf8(path).ok()?)?))
}

#[async_trait]
impl MarkStore for SqlMarks {
    async fn runs(&self) -> Result<Vec<MarkRun>, GcError> {
        let all = sqlx::query("SELECT DISTINCT run FROM gc_marks WHERE ns = $1")
            .bind(self.ns.clone())
            .fetch_all(&self.pool)
            .await
            .map_err(|e| sql("gc_marks", e))?;
        let done = sqlx::query("SELECT run FROM gc_marks WHERE ns = $1 AND key = $2")
            .bind(self.ns.clone())
            .bind(DONE_KEY.to_vec())
            .fetch_all(&self.pool)
            .await
            .map_err(|e| sql("gc_marks", e))?;
        let mut sealed = std::collections::HashSet::new();
        for r in done {
            sealed.insert(r.try_get::<i64, _>(0).map_err(|e| sql("gc_marks", e))?);
        }
        let mut out = Vec::new();
        for r in all {
            let run: i64 = r.try_get(0).map_err(|e| sql("gc_marks", e))?;
            out.push(MarkRun {
                run,
                complete: sealed.contains(&run),
            });
        }
        Ok(out)
    }

    async fn append(&self, run: i64, tier: Tier, keys: &[ObjKey]) -> Result<(), GcError> {
        let mut tx = self.pool.begin().await.map_err(|e| sql("gc_marks", e))?;
        for chunk in keys.chunks(MARK_ROWS) {
            let values: Vec<String> = (0..chunk.len())
                .map(|i| format!("(${}, ${}, ${})", i * 3 + 1, i * 3 + 2, i * 3 + 3))
                .collect();
            let q = format!(
                "INSERT INTO gc_marks (run, ns, key) VALUES {}",
                values.join(", ")
            );
            // Only `$n` placeholders are interpolated, never data.
            let mut query = sqlx::query(sqlx::AssertSqlSafe(q));
            for k in chunk {
                query = query
                    .bind(run)
                    .bind(self.ns.clone())
                    .bind(mark_key(tier, k));
            }
            query
                .execute(&mut *tx)
                .await
                .map_err(|e| sql("gc_marks", e))?;
        }
        tx.commit().await.map_err(|e| sql("gc_marks", e))
    }

    async fn complete(&self, run: i64) -> Result<(), GcError> {
        sqlx::query("INSERT INTO gc_marks (run, ns, key) VALUES ($1, $2, $3)")
            .bind(run)
            .bind(self.ns.clone())
            .bind(DONE_KEY.to_vec())
            .execute(&self.pool)
            .await
            .map_err(|e| sql("gc_marks", e))?;
        Ok(())
    }

    async fn load(&self, run: i64, cap: usize) -> Result<HashMap<ObjKey, Tier>, GcError> {
        use futures::TryStreamExt;
        let mut rows = sqlx::query("SELECT key FROM gc_marks WHERE run = $1 AND ns = $2")
            .bind(run)
            .bind(self.ns.clone())
            .fetch(&self.pool);
        let mut out = HashMap::new();
        while let Some(row) = rows.try_next().await.map_err(|e| sql("gc_marks", e))? {
            let bytes: Vec<u8> = row.try_get(0).map_err(|e| sql("gc_marks", e))?;
            if bytes == DONE_KEY {
                continue;
            }
            let (tier, key) = parse_mark(&bytes).ok_or_else(|| sql("gc_marks", "bad mark row"))?;
            if out.len() >= cap && !out.contains_key(&key) {
                return Err(GcError::TooManyMarks(cap));
            }
            let slot = out.entry(key).or_insert(tier);
            if tier == Tier::Strict {
                *slot = Tier::Strict;
            }
        }
        Ok(out)
    }

    async fn drop_run(&self, run: i64) -> Result<(), GcError> {
        sqlx::query("DELETE FROM gc_marks WHERE run = $1 AND ns = $2")
            .bind(run)
            .bind(self.ns.clone())
            .execute(&self.pool)
            .await
            .map_err(|e| sql("gc_marks", e))?;
        Ok(())
    }
}

// ------------------------------------------------------------------------------ cross-process fence

/// The cross-process half of the GC fence (see [`crate::update::GcLock`]). A guard holds the lock
/// until it is dropped, including when the future holding it is cancelled.
#[async_trait]
pub trait ClusterLock: Send + Sync {
    /// Taken by a push before its ingest; any number may hold it at once.
    async fn shared(&self) -> Result<Box<dyn Send>, sqlx::Error>;
    /// Taken by a sweep batch; waits for every shared holder in every process and excludes new ones.
    async fn exclusive(&self) -> Result<Box<dyn Send>, sqlx::Error>;
}

/// [`ClusterLock`] as a Postgres session-level advisory lock on `GC_LOCK_KEY`. Each guard owns one
/// pooled connection for as long as it lives; at most half the pool is lent out this way so that
/// pushes holding the fence can still reach the database to ingest and commit.
pub struct PgClusterLock {
    pool: AnyPool,
    slots: Arc<tokio::sync::Semaphore>,
}

impl PgClusterLock {
    pub fn new(pool: AnyPool) -> Self {
        let max = usize::try_from(pool.options().get_max_connections()).unwrap_or(1);
        Self {
            slots: Arc::new(tokio::sync::Semaphore::new((max / 2).max(1))),
            pool,
        }
    }

    async fn take(&self, shared: bool) -> Result<Box<dyn Send>, sqlx::Error> {
        let slot = self
            .slots
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| sqlx::Error::PoolClosed)?;
        let conn = self.pool.acquire().await?;
        // The guard exists before the statement runs: a cancelled wait must not hand a connection
        // that may yet receive the lock back to the pool.
        let mut held = PgHeld {
            conn: Some(conn),
            shared,
            locked: false,
            slot: Some(slot),
        };
        let q = if shared {
            "SELECT pg_advisory_lock_shared($1)"
        } else {
            "SELECT pg_advisory_lock($1)"
        };
        if let Some(conn) = held.conn.as_mut() {
            sqlx::query(q)
                .bind(GC_LOCK_KEY)
                .execute(&mut **conn)
                .await?;
        }
        held.locked = true;
        Ok(Box::new(held))
    }
}

#[async_trait]
impl ClusterLock for PgClusterLock {
    async fn shared(&self) -> Result<Box<dyn Send>, sqlx::Error> {
        self.take(true).await
    }

    async fn exclusive(&self) -> Result<Box<dyn Send>, sqlx::Error> {
        self.take(false).await
    }
}

/// One held (or being acquired) advisory lock and the connection it lives on.
struct PgHeld {
    conn: Option<PoolConnection<Any>>,
    shared: bool,
    locked: bool,
    slot: Option<tokio::sync::OwnedSemaphorePermit>,
}

impl Drop for PgHeld {
    fn drop(&mut self) {
        let Some(mut conn) = self.conn.take() else {
            return;
        };
        let slot = self.slot.take();
        // Closing the connection ends the session, which drops any lock it holds or is waiting
        // for; that is the answer whenever an orderly unlock is not certain.
        if !self.locked {
            conn.close_on_drop();
            return;
        }
        let q = if self.shared {
            "SELECT pg_advisory_unlock_shared($1)"
        } else {
            "SELECT pg_advisory_unlock($1)"
        };
        let Ok(rt) = tokio::runtime::Handle::try_current() else {
            conn.close_on_drop();
            return;
        };
        rt.spawn(async move {
            let unlocked = sqlx::query_scalar::<_, bool>(q)
                .bind(GC_LOCK_KEY)
                .fetch_one(&mut *conn)
                .await;
            if !matches!(unlocked, Ok(true)) {
                conn.close_on_drop();
            }
            drop(conn);
            drop(slot);
        });
    }
}

/// The sweep fence: the exclusive `UpdateRefs` GC lock, in this process and (on Postgres) in all.
pub struct ServerFence {
    pub hooks: Arc<PipelineHooks>,
    pub pool: AnyPool,
}

#[async_trait]
impl Fence for ServerFence {
    async fn enter(&self) -> Result<Box<dyn Send>, GcError> {
        self.hooks.gc().attach_pool(&self.pool);
        let hold = self
            .hooks
            .gc()
            .exclusive()
            .await
            .map_err(|e| sql("gc fence", e))?;
        Ok(Box::new(hold))
    }
}

/// Removes the derived rows of objects about to be deleted. `cs_graph` and `file_meta` answer
/// "already stored" during ingest, so a row that outlived its object would let a later push
/// skip an upload the store needs.
pub struct SqlSweepObserver {
    pub pool: AnyPool,
    pub ns: Vec<u8>,
}

#[async_trait]
impl SweepObserver for SqlSweepObserver {
    async fn before_delete(&self, keys: &[ObjKey]) -> Result<(), GcError> {
        for chunk in keys.chunks(OBSERVER_CHUNK) {
            let mut tx = self.pool.begin().await.map_err(|e| sql("sweep", e))?;
            for key in chunk {
                // A meta is collected with its changeset (SPEC §3.6), so the `changes` row goes
                // with either object: a row never names a swept changeset or a missing meta.
                let (own, id): (&[&'static str], Vec<u8>) = match key {
                    ObjKey::Changeset(c) => (
                        &[
                            "DELETE FROM cs_graph WHERE ns = $1 AND cs = $2",
                            "DELETE FROM changes WHERE ns = $1 AND cs = $2",
                        ],
                        c.as_bytes().to_vec(),
                    ),
                    ObjKey::ChangeMeta(m) => (
                        &["DELETE FROM changes WHERE ns = $1 AND meta = $2"],
                        m.as_bytes().to_vec(),
                    ),
                    ObjKey::File(f) | ObjKey::ChunkList(f) => (
                        &["DELETE FROM file_meta WHERE ns = $1 AND file = $2"],
                        f.as_bytes().to_vec(),
                    ),
                    ObjKey::Tree(t) => (&[], t.as_bytes().to_vec()),
                    ObjKey::Tag(t) => (&[], t.as_bytes().to_vec()),
                    _ => continue,
                };
                let git_map = (!matches!(key, ObjKey::ChangeMeta(_)))
                    .then_some("DELETE FROM git_map WHERE ns = $1 AND asc = $2");
                for stmt in own.iter().copied().chain(git_map) {
                    sqlx::query(stmt)
                        .bind(self.ns.clone())
                        .bind(id.clone())
                        .execute(&mut *tx)
                        .await
                        .map_err(|e| sql("sweep", e))?;
                }
            }
            tx.commit().await.map_err(|e| sql("sweep", e))?;
        }
        Ok(())
    }
}

// ------------------------------------------------------------------------------ service

/// Holds the collection-wide lock: in process always, and across servers on Postgres.
struct RunGuard {
    _local: tokio::sync::OwnedMutexGuard<()>,
    pg: Option<sqlx::pool::PoolConnection<Any>>,
}

impl RunGuard {
    async fn release(mut self) {
        if let Some(mut conn) = self.pg.take() {
            let _ = sqlx::query("SELECT pg_advisory_unlock($1)")
                .bind(GC_RUN_KEY)
                .execute(&mut *conn)
                .await;
        }
    }
}

/// Collects every repository namespace of one server.
pub struct GcService {
    registry: Arc<Registry>,
    hooks: Arc<PipelineHooks>,
    cfg: GcConfig,
    running: Arc<tokio::sync::Mutex<()>>,
}

impl GcService {
    pub fn new(registry: Arc<Registry>, hooks: Arc<PipelineHooks>, cfg: GcConfig) -> Self {
        Self {
            registry,
            hooks,
            cfg,
            running: Arc::default(),
        }
    }

    async fn acquire(&self) -> Result<RunGuard, GcError> {
        let local = self
            .running
            .clone()
            .try_lock_owned()
            .map_err(|_| GcError::Busy("a collection is running in this server".into()))?;
        let pool = self.registry.pool();
        let pg = if is_postgres(pool) {
            let mut conn = pool.acquire().await.map_err(|e| sql("gc lock", e))?;
            let got: bool = sqlx::query_scalar("SELECT pg_try_advisory_lock($1)")
                .bind(GC_RUN_KEY)
                .fetch_one(&mut *conn)
                .await
                .map_err(|e| sql("gc lock", e))?;
            if !got {
                return Err(GcError::Busy("another server is collecting".into()));
            }
            Some(conn)
        } else {
            None
        };
        Ok(RunGuard { _local: local, pg })
    }

    /// Collects one repository. `now` is the mark time; it is the wall clock in production.
    pub async fn run_repo(
        &self,
        name: &RepoName,
        now: SystemTime,
        dry_run: bool,
    ) -> Result<GcReport, GcError> {
        let guard = self.acquire().await?;
        let out = self.collect_locked(name, now, dry_run).await;
        guard.release().await;
        out
    }

    async fn collect_locked(
        &self,
        name: &RepoName,
        now: SystemTime,
        dry_run: bool,
    ) -> Result<GcReport, GcError> {
        let entry = self
            .registry
            .open(name)
            .await
            .map_err(|e| GcError::Other(format!("{name}: {e}")))?;
        let pool = self.registry.pool();
        let ns = entry.ns.as_bytes().to_vec();
        let roots = SqlRefRoots {
            pool: pool.clone(),
            refs: self.registry.refs().clone(),
            repo: entry.key,
        };
        let marks = SqlMarks {
            pool: pool.clone(),
            ns: ns.clone(),
        };
        let observer = SqlSweepObserver {
            pool: pool.clone(),
            ns: ns.clone(),
        };
        let fence = ServerFence {
            hooks: self.hooks.clone(),
            pool: pool.clone(),
        };
        let extra = self.hooks.gc_roots().all();
        let report = Collector {
            ns: &ns,
            objects: &*entry.objects,
            refs: &roots,
            extra: &extra,
            marks: &marks,
            observer: Some(&observer),
            cfg: &self.cfg,
        }
        .collect(now, dry_run, &fence)
        .await?;
        // A purge means the bytes must go. A purged object that sat in a pack is only forgotten
        // by the sweep; rewrite the packs now rather than wait for half of one to die.
        if report.sweep.purged > 0
            && !dry_run
            && entry.objects.pack_layer().is_some()
            && let Err(e) = entry.objects.pack_compact_all(&fence, now).await
        {
            tracing::warn!(repo = %name, error = %e, "compacting packs after a purge failed");
        }
        Ok(report)
    }

    /// Collects every repository in turn. One repository failing does not stop the others.
    pub async fn run_all(
        &self,
        now: SystemTime,
        dry_run: bool,
    ) -> Vec<(String, Result<GcReport, GcError>)> {
        let names: Vec<String> = match sqlx::query("SELECT name FROM repos ORDER BY id")
            .fetch_all(self.registry.pool())
            .await
        {
            Ok(rows) => rows
                .iter()
                .filter_map(|r| r.try_get::<String, _>(0).ok())
                .collect(),
            Err(e) => return vec![("(repos)".into(), Err(sql("repos", e)))],
        };
        let mut out = Vec::new();
        for n in names {
            let res = match RepoName::parse(&n) {
                Ok(name) => self.run_repo(&name, now, dry_run).await,
                Err(_) => Err(GcError::Other(format!("repository name {n:?} is invalid"))),
            };
            out.push((n, res));
        }
        out
    }

    /// Runs [`run_all`](Self::run_all) every `interval` until aborted.
    pub fn spawn_worker(self: Arc<Self>, interval: Duration) -> JoinHandle<()> {
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(interval).await;
                for (name, res) in self.run_all(SystemTime::now(), false).await {
                    match res {
                        Ok(r) => tracing::info!(
                            repo = %name,
                            deleted = r.sweep.deleted,
                            scanned = r.sweep.scanned,
                            "gc finished"
                        ),
                        Err(e) => tracing::warn!(repo = %name, error = %e, "gc failed"),
                    }
                }
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use asc_core::id::{ChangesetId, FileId, RefTarget};
    use asc_core::path::RefName;
    use asc_rpc::server::{CallCtx, RefsHandler, wire};
    use asc_store::key::ObjKey;
    use asc_store::objects::Objects;
    use asc_store::refs::{Actor, Expect, RefTxn, RefUpdate};
    use uuid::Uuid;

    use super::*;
    use crate::ingest::testutil::{commit, registry, repo};
    use crate::registry::RepoEntry;
    use crate::update::{HookRejected, HookRequest, PushHooks, ServerRefs};

    const D15: Duration = Duration::from_secs(15 * 24 * 3600);
    const H25: Duration = Duration::from_secs(25 * 3600);

    struct Setup {
        reg: Arc<Registry>,
        entry: Arc<RepoEntry>,
        hooks: Arc<PipelineHooks>,
        svc: Arc<GcService>,
    }

    async fn setup() -> Setup {
        let reg = registry().await;
        let entry = repo(&reg).await;
        let hooks = Arc::new(PipelineHooks::default());
        let svc = Arc::new(GcService::new(
            reg.clone(),
            hooks.clone(),
            GcConfig::default(),
        ));
        Setup {
            reg,
            entry,
            hooks,
            svc,
        }
    }

    fn name() -> RepoName {
        RepoName::parse("o/r").unwrap()
    }

    /// As `setup`, with packing on and no minimum age, so a pack run packs everything.
    async fn setup_packed() -> Setup {
        let reg = registry().await;
        assert!(reg.set_pack_config(asc_store::pack::PackConfig {
            enabled: true,
            min_age: Duration::ZERO,
            min_loose: 1,
            min_batch: 1,
            ..Default::default()
        }));
        let entry = repo(&reg).await;
        let hooks = Arc::new(PipelineHooks::default());
        let svc = Arc::new(GcService::new(
            reg.clone(),
            hooks.clone(),
            GcConfig::default(),
        ));
        Setup {
            reg,
            entry,
            hooks,
            svc,
        }
    }

    async fn set_ref(s: &Setup, name: &str, to: Option<ChangesetId>) {
        let rn = RefName::new(name.as_bytes().to_vec()).unwrap();
        let cur = s.reg.refs().get(s.entry.key, &rn).await.unwrap();
        let txn = RefTxn {
            updates: vec![RefUpdate {
                name: rn,
                expect: cur.map_or(Expect::Absent, |c| Expect::Value(c.target)),
                new: to.map(RefTarget::Changeset),
                expires_at: None,
            }],
            actor: Actor {
                id: "t".into(),
                via: "test".into(),
            },
            reason: "test".into(),
            op_id: Uuid::new_v4(),
        };
        s.reg.refs().update(s.entry.key, &txn).await.unwrap();
    }

    async fn has(s: &Setup, k: ObjKey) -> bool {
        s.entry.objects.has_many(&[k]).await.unwrap()[0]
    }

    async fn count(s: &Setup, table: &str) -> i64 {
        let q = match table {
            "cs_graph" => "SELECT COUNT(*) FROM cs_graph",
            "file_meta" => "SELECT COUNT(*) FROM file_meta",
            "changes" => "SELECT COUNT(*) FROM changes",
            _ => unreachable!(),
        };
        sqlx::query_scalar(q).fetch_one(s.reg.pool()).await.unwrap()
    }

    #[tokio::test]
    async fn two_marks_delete_an_orphan_and_its_rows_and_keep_the_rest() {
        let s = setup().await;
        let keep = commit(&s.entry, &[], &[("a", b"kept")]).await;
        let lost = commit(&s.entry, &[], &[("b", b"lost")]).await;
        set_ref(&s, "refs/heads/master", Some(keep)).await;
        let lost_file = FileId::of(b"lost");
        // Rows the ingest keeps for the orphan.
        let ns = s.entry.ns.as_bytes().to_vec();
        sqlx::query("INSERT INTO cs_graph (ns, cs, gen, parents) VALUES ($1, $2, 1, $3)")
            .bind(ns.clone())
            .bind(lost.as_bytes().to_vec())
            .bind(Vec::<u8>::new())
            .execute(s.reg.pool())
            .await
            .unwrap();
        sqlx::query("INSERT INTO file_meta (ns, file, size, git) VALUES ($1, $2, 4, $3)")
            .bind(ns)
            .bind(lost_file.as_bytes().to_vec())
            .bind(vec![0u8; 20])
            .execute(s.reg.pool())
            .await
            .unwrap();

        let t1 = SystemTime::now() + D15;
        let first = s.svc.run_repo(&name(), t1, false).await.unwrap();
        assert!(first.sweep.waiting_for_second_mark);
        assert!(has(&s, ObjKey::Changeset(lost)).await);
        assert_eq!(count(&s, "cs_graph").await, 1);

        let second = s.svc.run_repo(&name(), t1 + H25, false).await.unwrap();
        assert_eq!(second.sweep.deleted, 3, "{second}");
        assert!(!has(&s, ObjKey::Changeset(lost)).await);
        assert!(!has(&s, ObjKey::File(lost_file)).await);
        assert!(has(&s, ObjKey::Changeset(keep)).await);
        assert_eq!(
            count(&s, "cs_graph").await,
            0,
            "the orphan's graph row went with it"
        );
        assert_eq!(count(&s, "file_meta").await, 0);
        // Marks are pruned to what a later sweep can use.
        let marks = SqlMarks {
            pool: s.reg.pool().clone(),
            ns: s.entry.ns.as_bytes().to_vec(),
        };
        let runs = marks.runs().await.unwrap();
        assert!(!runs.is_empty() && runs.len() <= 3 && runs.iter().all(|r| r.complete));
    }

    #[tokio::test]
    async fn sweeping_a_changeset_and_its_meta_removes_their_changes_rows() {
        use asc_core::change::{ChangeId, ChangeMeta, RewriteOp};
        use asc_core::codec::Enveloped;
        use asc_store::codec::encode_stored;
        let s = setup().await;
        let keep = commit(&s.entry, &[], &[("a", b"kept")]).await;
        let lost = commit(&s.entry, &[], &[("b", b"lost")]).await;
        set_ref(&s, "refs/heads/master", Some(keep)).await;
        let ns = s.entry.ns.as_bytes().to_vec();
        let mut metas = Vec::new();
        for (cs, n) in [(keep, 1u8), (lost, 2)] {
            let m = ChangeMeta::new(cs, ChangeId::from_bytes([n; 16]), vec![], RewriteOp::New);
            s.entry
                .objects
                .put(&ObjKey::ChangeMeta(m.id()), encode_stored(&m.to_envelope()))
                .await
                .unwrap();
            sqlx::query(
                "INSERT INTO changes (ns, cs, change, meta, op, preds) VALUES ($1, $2, $3, $4, 0, $5)",
            )
            .bind(ns.clone())
            .bind(cs.as_bytes().to_vec())
            .bind(vec![n; 16])
            .bind(m.id().as_bytes().to_vec())
            .bind(Vec::<u8>::new())
            .execute(s.reg.pool())
            .await
            .unwrap();
            metas.push(m.id());
        }
        assert_eq!(count(&s, "changes").await, 2);
        let t1 = SystemTime::now() + D15;
        s.svc.run_repo(&name(), t1, false).await.unwrap();
        s.svc.run_repo(&name(), t1 + H25, false).await.unwrap();
        assert!(!has(&s, ObjKey::Changeset(lost)).await);
        assert!(!has(&s, ObjKey::ChangeMeta(metas[1])).await);
        assert!(has(&s, ObjKey::ChangeMeta(metas[0])).await);
        let left: Vec<Vec<u8>> = sqlx::query_scalar("SELECT cs FROM changes")
            .fetch_all(s.reg.pool())
            .await
            .unwrap();
        assert_eq!(
            left,
            vec![keep.as_bytes().to_vec()],
            "only the live row stays"
        );
    }

    #[tokio::test]
    async fn sweep_race_push_after_mark2() {
        let s = setup().await;
        let old = commit(&s.entry, &[], &[("a", b"old")]).await;
        let t1 = SystemTime::now() + D15;
        s.svc.run_repo(&name(), t1, false).await.unwrap();

        let ns = s.entry.ns.as_bytes().to_vec();
        let roots = SqlRefRoots {
            pool: s.reg.pool().clone(),
            refs: s.reg.refs().clone(),
            repo: s.entry.key,
        };
        let marks = SqlMarks {
            pool: s.reg.pool().clone(),
            ns: ns.clone(),
        };
        let cfg = GcConfig::default();
        let c = Collector {
            ns: &ns,
            objects: &*s.entry.objects,
            refs: &roots,
            extra: &[],
            marks: &marks,
            observer: None,
            cfg: &cfg,
        };
        let fence = ServerFence {
            hooks: s.hooks.clone(),
            pool: s.reg.pool().clone(),
        };
        let t2 = t1 + H25;
        let mark2 = c.mark(t2).await.unwrap();
        assert_eq!(mark2.strict, 0);
        // The push commits between mark 2 and the sweep.
        set_ref(&s, "refs/heads/master", Some(old)).await;
        let swept = c.sweep(&mark2, t2, false, &fence).await.unwrap();
        assert_eq!(swept.candidates, 3);
        assert_eq!(swept.rescued, 3);
        assert_eq!(swept.deleted, 0);
        assert!(has(&s, ObjKey::Changeset(old)).await);
    }

    #[tokio::test]
    async fn pitr_restore_closure() {
        let s = setup().await;
        let a = commit(&s.entry, &[], &[("a", b"one")]).await;
        let b = commit(&s.entry, &[a], &[("b", b"two")]).await;
        set_ref(&s, "refs/heads/master", Some(a)).await;
        set_ref(&s, "refs/heads/master", Some(b)).await;
        set_ref(&s, "refs/heads/master", None).await;
        let cfg = GcConfig {
            min_age: Duration::ZERO,
            ..GcConfig::default()
        };
        let svc = GcService::new(s.reg.clone(), s.hooks.clone(), cfg);
        let t1 = SystemTime::now() + Duration::from_secs(3600);
        svc.run_repo(&name(), t1, false).await.unwrap();
        let r = svc.run_repo(&name(), t1 + H25, false).await.unwrap();
        assert_eq!(r.sweep.deleted, 0, "{r}");
        assert!(has(&s, ObjKey::Changeset(a)).await);
        assert!(has(&s, ObjKey::Changeset(b)).await);

        // A window that has passed no longer protects them.
        let short = GcService::new(
            s.reg.clone(),
            s.hooks.clone(),
            GcConfig {
                min_age: Duration::ZERO,
                pitr: Duration::from_secs(60),
                ..GcConfig::default()
            },
        );
        let t3 = t1 + 3 * H25;
        short.run_repo(&name(), t3, false).await.unwrap();
        let r = short.run_repo(&name(), t3 + H25, false).await.unwrap();
        assert!(r.sweep.deleted >= 4, "{r}");
        assert!(!has(&s, ObjKey::Changeset(a)).await);
    }

    #[tokio::test]
    async fn the_sweep_waits_for_a_push_in_flight() {
        let s = setup().await;
        let lost = commit(&s.entry, &[], &[("a", b"lost")]).await;
        let t1 = SystemTime::now() + D15;
        s.svc.run_repo(&name(), t1, false).await.unwrap();

        // `UpdateRefs` holds the shared lock between ingest and commit.
        let push = s.hooks.gc().shared().await.unwrap();
        let svc = s.svc.clone();
        let sweep = tokio::spawn(async move { svc.run_repo(&name(), t1 + H25, false).await });
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(
            !sweep.is_finished(),
            "a sweep batch needs the exclusive lock"
        );
        assert!(has(&s, ObjKey::Changeset(lost)).await);
        drop(push);
        let r = tokio::time::timeout(Duration::from_secs(10), sweep)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(r.sweep.deleted, 3);
        assert!(!has(&s, ObjKey::Changeset(lost)).await);
    }

    /// Stands in for the Postgres advisory lock that every server process on one database
    /// shares: the harness has no Postgres, so two "processes" share this instead.
    #[derive(Default)]
    struct SharedLock(Arc<tokio::sync::RwLock<()>>);

    #[async_trait]
    impl ClusterLock for SharedLock {
        async fn shared(&self) -> Result<Box<dyn Send>, sqlx::Error> {
            Ok(Box::new(self.0.clone().read_owned().await))
        }
        async fn exclusive(&self) -> Result<Box<dyn Send>, sqlx::Error> {
            Ok(Box::new(self.0.clone().write_owned().await))
        }
    }

    /// Parks every push in step 7 (after ingest, before the commit) until released.
    struct ParkedPush {
        reached: Arc<tokio::sync::Notify>,
        go: Arc<tokio::sync::Notify>,
    }

    #[async_trait]
    impl PushHooks for ParkedPush {
        async fn pre_receive(&self, _: &HookRequest<'_>) -> Result<(), HookRejected> {
            self.reached.notify_one();
            self.go.notified().await;
            Ok(())
        }
    }

    fn push_request(name: &str, to: ChangesetId) -> wire::UpdateRefsRequest {
        wire::UpdateRefsRequest {
            repo: Some(wire::RepoRef {
                name: "o/r".into(),
                id_mode: 0,
            }),
            updates: vec![wire::RefUpdate {
                name: name.into(),
                expect: Some(wire::ref_update::Expect::Absent(true)),
                new: Some(wire::RefTarget::from(&RefTarget::Changeset(to))),
                delete: false,
            }],
            mode: wire::UpdateMode::FastForward as i32,
            ..wire::UpdateRefsRequest::default()
        }
    }

    #[tokio::test]
    async fn a_sweep_in_another_process_waits_for_an_ingested_push() {
        let s = setup().await;
        // Two server processes on one database: their own in-process locks, one shared cluster
        // lock between them.
        let cluster = Arc::new(SharedLock::default());
        let hooks_b = Arc::new(PipelineHooks::default());
        s.hooks.gc().attach(cluster.clone());
        hooks_b.gc().attach(cluster);
        let svc_b = Arc::new(GcService::new(s.reg.clone(), hooks_b, GcConfig::default()));

        // An old orphan, seen unreachable by mark 1.
        let lost = commit(&s.entry, &[], &[("a", b"lost")]).await;
        let t1 = SystemTime::now() + D15;
        svc_b.run_repo(&name(), t1, false).await.unwrap();

        // Process A ingests it for a push and parks before the ref commit.
        let reached = Arc::new(tokio::sync::Notify::new());
        let go = Arc::new(tokio::sync::Notify::new());
        s.hooks
            .set_push_hooks(Arc::new(ParkedPush {
                reached: reached.clone(),
                go: go.clone(),
            }))
            .ok()
            .unwrap();
        let a = ServerRefs::new(s.reg.clone(), s.hooks.clone());
        let ctx = CallCtx {
            repo: name(),
            id_mode: asc_rpc::IdMode::Asc,
            principal: asc_rpc::auth::Principal { id: "alice".into() },
            op_id: Some(Uuid::new_v4()),
            client: None,
            admin: Default::default(),
        };
        let push = tokio::spawn(async move {
            a.update_refs(&ctx, push_request("refs/heads/master", lost))
                .await
        });
        reached.notified().await;

        // Process B sweeps. Its batch must wait: `lost` is ingested but no ref names it yet.
        let sweeping = svc_b.clone();
        let sweep = tokio::spawn(async move { sweeping.run_repo(&name(), t1 + H25, false).await });
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(
            !sweep.is_finished(),
            "a sweep in another process ran between a push's ingest and its commit"
        );
        assert!(has(&s, ObjKey::Changeset(lost)).await);

        go.notify_one();
        push.await.unwrap().unwrap();
        let report = tokio::time::timeout(Duration::from_secs(10), sweep)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(report.sweep.deleted, 0, "{report}");
        assert!(has(&s, ObjKey::Changeset(lost)).await);
        let rn = RefName::new(b"refs/heads/master".to_vec()).unwrap();
        let got = s.reg.refs().get(s.entry.key, &rn).await.unwrap().unwrap();
        assert_eq!(got.target, RefTarget::Changeset(lost));
    }

    #[tokio::test]
    async fn concurrent_runs_are_refused() {
        let s = setup().await;
        let held = s.svc.acquire().await.unwrap();
        let err = s
            .svc
            .run_repo(&name(), SystemTime::now(), false)
            .await
            .unwrap_err();
        assert!(matches!(err, GcError::Busy(_)), "{err}");
        held.release().await;
        s.svc
            .run_repo(&name(), SystemTime::now(), false)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn registered_roots_keep_objects_and_purges_are_swept_once() {
        struct Src(Vec<ObjKey>, Vec<ObjKey>);
        #[async_trait]
        impl GcRootSource for Src {
            async fn roots(&self, _: &[u8]) -> Result<Vec<ObjKey>, GcError> {
                Ok(self.0.clone())
            }
            async fn purged(&self, _: &[u8]) -> Result<Vec<ObjKey>, GcError> {
                Ok(self.1.clone())
            }
        }
        let s = setup().await;
        let rooted = commit(&s.entry, &[], &[("a", b"rooted")]).await;
        let secret = commit(&s.entry, &[], &[("b", b"secret")]).await;
        s.hooks.gc_roots().register(Arc::new(Src(
            vec![ObjKey::Changeset(rooted)],
            vec![ObjKey::File(FileId::of(b"secret"))],
        )));
        let t1 = SystemTime::now() + D15;
        s.svc.run_repo(&name(), t1, false).await.unwrap();
        let r = s.svc.run_repo(&name(), t1 + H25, false).await.unwrap();
        assert!(has(&s, ObjKey::Changeset(rooted)).await);
        assert!(has(&s, ObjKey::File(FileId::of(b"rooted"))).await);
        assert!(!has(&s, ObjKey::File(FileId::of(b"secret"))).await);
        assert!(
            !has(&s, ObjKey::Changeset(secret)).await,
            "unreachable and old: {r}"
        );
    }

    #[tokio::test]
    async fn packed_objects_are_swept_and_the_rest_of_the_pack_stays_readable() {
        let s = setup_packed().await;
        let keep = commit(&s.entry, &[], &[("a", b"kept")]).await;
        let lost = commit(&s.entry, &[], &[("b", b"lost")]).await;
        set_ref(&s, "refs/heads/master", Some(keep)).await;
        let t1 = SystemTime::now() + D15;
        let packs = crate::pack_service::PackService::new(s.reg.clone(), s.hooks.clone());
        let packed = packs.run_repo(&name(), t1, true).await.unwrap();
        assert!(packed.packed >= 6, "{packed:?}");
        assert_eq!(packed.loose_deleted, packed.packed);

        s.svc.run_repo(&name(), t1, false).await.unwrap();
        let second = s.svc.run_repo(&name(), t1 + H25, false).await.unwrap();
        assert_eq!(second.sweep.deleted, 3, "{second}");
        assert!(!has(&s, ObjKey::Changeset(lost)).await);
        assert!(!has(&s, ObjKey::File(FileId::of(b"lost"))).await);
        assert!(has(&s, ObjKey::Changeset(keep)).await);
        assert!(has(&s, ObjKey::File(FileId::of(b"kept"))).await);
        // The engine still reads what is left, through the pack.
        let cs = s.entry.backend.engine().get_changeset(keep).await.unwrap();
        s.entry.backend.engine().get_tree(cs.root).await.unwrap();
        assert!(packs.verify(&name()).await.unwrap().problems.is_empty());
        // The pack now holds dead bytes; the next run compacts it (the half-live rule or a
        // purge), and the survivors are still there after.
        let status = packs.status(&name()).await.unwrap();
        assert!(status.dead_bytes > 0);
        packs.compact_all(&name(), t1 + H25).await.unwrap();
        assert_eq!(packs.status(&name()).await.unwrap().dead_bytes, 0);
        assert!(has(&s, ObjKey::Changeset(keep)).await);
        assert!(packs.verify(&name()).await.unwrap().problems.is_empty());
    }

    #[tokio::test]
    async fn a_purge_that_swept_packed_bytes_compacts_their_pack_at_once() {
        struct Src(Vec<ObjKey>);
        #[async_trait]
        impl GcRootSource for Src {
            async fn roots(&self, _: &[u8]) -> Result<Vec<ObjKey>, GcError> {
                Ok(Vec::new())
            }
            async fn purged(&self, _: &[u8]) -> Result<Vec<ObjKey>, GcError> {
                Ok(self.0.clone())
            }
        }
        let s = setup_packed().await;
        let a = commit(&s.entry, &[], &[("a", b"first")]).await;
        let secret = FileId::of(b"purge me please");
        commit(&s.entry, &[], &[("b", b"purge me please")]).await;
        set_ref(&s, "refs/heads/master", Some(a)).await;
        s.hooks
            .gc_roots()
            .register(Arc::new(Src(vec![ObjKey::File(secret)])));
        let t1 = SystemTime::now() + D15;
        let packs = crate::pack_service::PackService::new(s.reg.clone(), s.hooks.clone());
        packs.run_repo(&name(), t1, true).await.unwrap();
        // A purge needs no second mark: the first sweep deletes the object.
        let r = s.svc.run_repo(&name(), t1, false).await.unwrap();
        assert!(r.sweep.purged >= 1, "{r}");
        assert!(!has(&s, ObjKey::File(secret)).await);
        // The purge did not wait for half the pack to die.
        assert_eq!(packs.status(&name()).await.unwrap().dead_bytes, 0);
        assert!(has(&s, ObjKey::Changeset(a)).await);
    }

    #[tokio::test]
    async fn dry_run_changes_nothing() {
        let s = setup().await;
        let lost = commit(&s.entry, &[], &[("a", b"lost")]).await;
        let t1 = SystemTime::now() + D15;
        s.svc.run_repo(&name(), t1, false).await.unwrap();
        let r = s.svc.run_repo(&name(), t1 + H25, true).await.unwrap();
        assert_eq!(r.sweep.candidates, 3);
        assert_eq!(r.sweep.deleted, 0);
        assert!(has(&s, ObjKey::Changeset(lost)).await);
    }

    #[tokio::test]
    async fn run_all_reports_each_repository() {
        let s = setup().await;
        let out = s.svc.run_all(SystemTime::now(), false).await;
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].0, "o/r");
        assert!(out[0].1.is_ok());
    }

    #[test]
    fn settings_are_validated() {
        assert!(GcSettings::default().to_config().is_ok());
        let bad = GcSettings {
            pitr_days: 0,
            ..GcSettings::default()
        };
        assert!(bad.to_config().is_err());
        let bad = GcSettings {
            max_marks: 5,
            ..GcSettings::default()
        };
        assert!(bad.to_config().is_err());
        let unknown: Result<GcSettings, _> = toml::from_str("enable = true");
        assert!(unknown.is_err(), "unknown keys are refused");
    }

    #[tokio::test]
    async fn mark_rows_round_trip_and_seal() {
        let s = setup().await;
        let marks = SqlMarks {
            pool: s.reg.pool().clone(),
            ns: vec![1; 16],
        };
        let keys: Vec<ObjKey> = (0..700u32)
            .map(|i| ObjKey::File(FileId::of(&i.to_le_bytes())))
            .collect();
        marks.append(7, Tier::Strict, &keys).await.unwrap();
        marks.append(7, Tier::Recent, &keys[..3]).await.unwrap();
        assert_eq!(
            marks.runs().await.unwrap(),
            vec![MarkRun {
                run: 7,
                complete: false
            }]
        );
        marks.complete(7).await.unwrap();
        let loaded = marks.load(7, 1000).await.unwrap();
        assert_eq!(loaded.len(), 700);
        assert!(loaded.values().all(|&t| t == Tier::Strict));
        assert!(matches!(
            marks.load(7, 10).await,
            Err(GcError::TooManyMarks(10))
        ));
        marks.drop_run(7).await.unwrap();
        assert!(marks.runs().await.unwrap().is_empty());
    }
}
