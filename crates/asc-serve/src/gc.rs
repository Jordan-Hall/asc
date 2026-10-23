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
//! until its transaction commits (step 8). On Postgres a sweep batch also takes
//! `pg_advisory_xact_lock(asc_gc)`, which the step-8 transaction's shared lock conflicts with, so
//! a second server process cannot commit a push during the batch either.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use async_trait::async_trait;
use asc_core::id::RefTarget;
use asc_rpc::server::RepoName;
use asc_store::gc::{
    Collector, Fence, GcConfig, GcError, GcReport, GcRootSource, MarkRun, MarkStore, RefRoots,
    SweepObserver, Tier,
};
use asc_store::key::ObjKey;
use asc_store::refs::{Refs, RepoKey};
use asc_store::sqlrefs::SqlRefs;
use serde::Deserialize;
use sqlx::any::Any;
use sqlx::{AnyPool, Row, Transaction};
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

/// The sweep fence: the exclusive `UpdateRefs` GC lock (and the Postgres advisory lock).
pub struct ServerFence {
    pub hooks: Arc<PipelineHooks>,
    pub pool: AnyPool,
}

#[async_trait]
impl Fence for ServerFence {
    async fn enter(&self) -> Result<Box<dyn Send>, GcError> {
        let local = self.hooks.gc().exclusive().await;
        let tx: Option<Transaction<'static, Any>> = if is_postgres(&self.pool) {
            let mut tx = self.pool.begin().await.map_err(|e| sql("gc fence", e))?;
            sqlx::query("SELECT pg_advisory_xact_lock($1)")
                .bind(GC_LOCK_KEY)
                .execute(&mut *tx)
                .await
                .map_err(|e| sql("gc fence", e))?;
            Some(tx)
        } else {
            None
        };
        Ok(Box::new((local, tx)))
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
                let (own, id): (Option<&'static str>, Vec<u8>) = match key {
                    ObjKey::Changeset(c) => (
                        Some("DELETE FROM cs_graph WHERE ns = $1 AND cs = $2"),
                        c.as_bytes().to_vec(),
                    ),
                    ObjKey::File(f) | ObjKey::ChunkList(f) => (
                        Some("DELETE FROM file_meta WHERE ns = $1 AND file = $2"),
                        f.as_bytes().to_vec(),
                    ),
                    ObjKey::Tree(t) => (None, t.as_bytes().to_vec()),
                    ObjKey::Tag(t) => (None, t.as_bytes().to_vec()),
                    _ => continue,
                };
                let stmts = own
                    .into_iter()
                    .chain(["DELETE FROM git_map WHERE ns = $1 AND asc = $2"]);
                for stmt in stmts {
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
        Collector {
            ns: &ns,
            objects: &*entry.objects,
            refs: &roots,
            extra: &extra,
            marks: &marks,
            observer: Some(&observer),
            cfg: &self.cfg,
        }
        .collect(now, dry_run, &fence)
        .await
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
    use asc_store::key::ObjKey;
    use asc_store::objects::Objects;
    use asc_store::refs::{Actor, Expect, RefTxn, RefUpdate};
    use uuid::Uuid;

    use super::*;
    use crate::ingest::testutil::{commit, registry, repo};
    use crate::registry::RepoEntry;

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
        let push = s.hooks.gc().shared().await;
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
