//! The indexing workers of a running server (SPEC §23.5, PLAN I13).
//!
//! [`start`] spawns one supervisor task that drives a [`Worker`] for every repository
//! namespace with ready jobs. A worker reads file bytes and writes derived objects through the
//! object store of its own namespace (`<root>/ns/<ns>`), so the supervisor keeps one worker
//! per namespace and builds it on first use. Workers of several replicas share the queue; the
//! leader lease in `ci_manifest` decides which replica runs a namespace.
//!
//! * **Config.** `[codeintel] enabled` starts the supervisor (with `cache.disk_dir`, which
//!   holds the scratch directory `<disk_dir>/ci-work`). `workers` caps the file jobs analysed
//!   at once; `memory` bounds the tantivy writer heaps (see [`heap_for`]).
//! * **Shutdown.** [`Indexers::stop`] stops taking new namespaces, commits what the workers
//!   buffered (so no staged job waits for its lease to expire), and returns; it gives up after
//!   [`SHUTDOWN_GRACE`] and aborts the task. Dropping [`Indexers`] asks the task to stop too.
//! * **Logging.** `tracing` only: `info` for start and stop, `debug` for a step that did work,
//!   `warn` for a failed step.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use asc_codeintel::index::segments::WRITER_HEAP;
use asc_store::bucket::BucketObjects;
use sqlx::AnyPool;
use tokio::sync::watch;
use tokio::task::JoinHandle;

use super::queue::Queue;
use super::worker::{EmbedModel, SqlManifest, Worker, WorkerConfig};
use crate::config::ServeConfig;
use crate::registry::{Ns, Registry};

/// Namespaces with ready jobs one pass looks at, at most.
const READY_LIMIT: i64 = 64;
/// Sleep between passes that found no work.
const IDLE_SLEEP: Duration = Duration::from_millis(250);
/// Workers kept; beyond this, idle ones are dropped (a worker keeps the indexed-ords bitmap of
/// its namespace).
pub const MAX_WORKERS: usize = 256;
/// Namespaces that are refused (and logged) once each, at most.
const MAX_REFUSED: usize = 1024;
/// How long [`Indexers::stop`] waits for the task before it aborts it.
pub const SHUTDOWN_GRACE: Duration = Duration::from_secs(20);
/// `memory / HEAP_SHARE` is the heap of one segment writer: a namespace buffers two writers,
/// so the default 2 GiB budget holds the writers of 32 busy namespaces.
const HEAP_SHARE: u64 = 64;

/// What the supervisor needs from the deployment.
#[derive(Clone, Debug)]
pub struct Config {
    /// File jobs analysed at once (`[codeintel] workers`).
    pub workers: usize,
    /// Tantivy writer heap in bytes.
    pub heap: usize,
    /// Scratch directory of the segment writers; created when missing.
    pub work_dir: PathBuf,
    /// This node's name in the leader lease.
    pub holder: String,
}

/// The writer heap for a `[codeintel] memory` budget: `memory / 64`, never more than the
/// default 32 MiB (the writer clamps the lower bound).
pub fn heap_for(memory: u64) -> usize {
    let share = memory / HEAP_SHARE;
    usize::try_from(share).map_or(WRITER_HEAP, |s| s.min(WRITER_HEAP))
}

/// The supervisor's config when `[codeintel] enabled` and `cache.disk_dir` names a disk for the
/// scratch directory; `None` otherwise (the server logs why when it starts `CodeIntel`).
pub fn config(cfg: &ServeConfig) -> Option<Config> {
    if !cfg.codeintel.enabled {
        return None;
    }
    let dir = cfg.cache.disk_dir.as_ref()?;
    Some(Config {
        workers: usize::try_from(cfg.codeintel.workers)
            .unwrap_or(usize::MAX)
            .max(1),
        heap: heap_for(cfg.codeintel.memory.0),
        work_dir: dir.join("ci-work"),
        holder: format!("{}#{}", cfg.listen, std::process::id()),
    })
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

fn stopped(rx: &watch::Receiver<bool>) -> bool {
    rx.has_changed().is_err() || *rx.borrow()
}

/// A running supervisor.
pub struct Indexers {
    stop: watch::Sender<bool>,
    task: Option<JoinHandle<()>>,
}

impl Indexers {
    /// Stops the workers: no new namespace is started, buffered documents are committed, then
    /// the task ends. After [`SHUTDOWN_GRACE`] the task is aborted instead (its leased jobs
    /// come back when their leases expire).
    pub async fn stop(mut self) {
        let _ = self.stop.send(true);
        let Some(mut task) = self.task.take() else {
            return;
        };
        match tokio::time::timeout(SHUTDOWN_GRACE, &mut task).await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => tracing::warn!(error = %e, "codeintel workers ended abnormally"),
            Err(_) => {
                tracing::warn!("codeintel workers did not stop in time; aborting them");
                task.abort();
            }
        }
    }
}

impl Drop for Indexers {
    fn drop(&mut self) {
        // Without `stop`, the task commits and ends on its own; the task is not aborted.
        let _ = self.stop.send(true);
    }
}

/// Starts the indexing workers over `registry`'s namespaces. `model` is the embedder (`None`
/// leaves chunks without vectors, as `Semantic` is then unavailable too).
pub fn start(
    registry: Arc<Registry>,
    cfg: Config,
    model: Option<Arc<dyn EmbedModel>>,
) -> std::io::Result<Indexers> {
    std::fs::create_dir_all(&cfg.work_dir)?;
    tracing::info!(
        workers = cfg.workers,
        heap = cfg.heap,
        holder = %cfg.holder,
        embeddings = model.is_some(),
        "codeintel workers starting"
    );
    let (stop, rx) = watch::channel(false);
    let fleet = Fleet::new(registry, cfg, model);
    let task = tokio::spawn(fleet.run(rx));
    Ok(Indexers {
        stop,
        task: Some(task),
    })
}

/// One worker per namespace, and what drives them.
struct Fleet {
    registry: Arc<Registry>,
    pool: AnyPool,
    queue: Queue,
    manifests: Arc<SqlManifest>,
    model: Option<Arc<dyn EmbedModel>>,
    cfg: WorkerConfig,
    workers: HashMap<Vec<u8>, Arc<Worker>>,
    refused: HashSet<Vec<u8>>,
}

impl Fleet {
    fn new(registry: Arc<Registry>, cfg: Config, model: Option<Arc<dyn EmbedModel>>) -> Self {
        let pool = registry.pool().clone();
        let mut wcfg = WorkerConfig::new(cfg.work_dir, &cfg.holder);
        wcfg.workers = cfg.workers;
        wcfg.heap = cfg.heap;
        Self {
            queue: Queue::new(pool.clone()),
            manifests: Arc::new(SqlManifest::new(pool.clone())),
            pool,
            registry,
            model,
            cfg: wcfg,
            workers: HashMap::new(),
            refused: HashSet::new(),
        }
    }

    /// The worker of `ns`, built on first use. `None` for a namespace that is not a repository
    /// namespace id (logged once).
    fn worker(&mut self, ns: &[u8]) -> Option<Arc<Worker>> {
        if let Some(w) = self.workers.get(ns) {
            return Some(w.clone());
        }
        let Some(id) = Ns::from_bytes(ns) else {
            if self.refused.len() < MAX_REFUSED && self.refused.insert(ns.to_vec()) {
                tracing::warn!(
                    len = ns.len(),
                    "codeintel jobs for a namespace that is not a repository id; skipped"
                );
            }
            return None;
        };
        let (store, root) = self.registry.namespace(id);
        let objects = Arc::new(BucketObjects::new(store, root));
        match Worker::new(
            self.pool.clone(),
            objects,
            self.manifests.clone(),
            self.model.clone(),
            self.cfg.clone(),
        ) {
            Ok(w) => {
                let w = Arc::new(w);
                self.workers.insert(ns.to_vec(), w.clone());
                Some(w)
            }
            Err(e) => {
                tracing::warn!(error = %e, "codeintel worker could not be created");
                None
            }
        }
    }

    async fn run(mut self, mut stop: watch::Receiver<bool>) {
        while !stopped(&stop) {
            if !self.pass(&stop).await {
                tokio::select! {
                    () = tokio::time::sleep(IDLE_SLEEP) => {}
                    _ = stop.changed() => {}
                }
            }
        }
        self.drain().await;
        tracing::info!("codeintel workers stopped");
    }

    /// One round over the namespaces with ready jobs and those holding uncommitted documents.
    /// True when a step leased jobs.
    async fn pass(&mut self, stop: &watch::Receiver<bool>) -> bool {
        let ready = match self
            .queue
            .ready_namespaces(now_ms_secs(), READY_LIMIT)
            .await
        {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(error = %e, "codeintel queue could not be read");
                return false;
            }
        };
        let mut todo = ready.clone();
        for (ns, w) in &self.workers {
            if !ready.contains(ns) && !w.is_idle(ns).await {
                todo.push(ns.clone());
            }
        }
        let mut worked = false;
        for ns in todo {
            if stopped(stop) {
                break;
            }
            let Some(w) = self.worker(&ns) else { continue };
            let now = now_ms();
            match w.step(&ns, now).await {
                Ok(r) if r.leased > 0 => {
                    worked = true;
                    tracing::debug!(
                        leased = r.leased,
                        files = r.files,
                        embedded = r.embedded,
                        skipped = r.skipped,
                        failed = r.failed,
                        committed = r.committed,
                        "codeintel step"
                    );
                }
                Ok(_) => {}
                Err(e) => tracing::warn!(error = %e, "codeintel worker step failed"),
            }
            if let Err(e) = w.merge_vectors(&ns, now_ms()).await {
                tracing::warn!(error = %e, "codeintel vector merge failed");
            }
        }
        self.evict(&ready).await;
        worked
    }

    /// Drops idle workers beyond [`MAX_WORKERS`] that have no ready jobs.
    async fn evict(&mut self, ready: &[Vec<u8>]) {
        if self.workers.len() <= MAX_WORKERS {
            return;
        }
        let mut drop_ns = Vec::new();
        for (ns, w) in &self.workers {
            if !ready.contains(ns) && w.is_idle(ns).await {
                drop_ns.push(ns.clone());
            }
        }
        for ns in drop_ns {
            if self.workers.len() <= MAX_WORKERS {
                break;
            }
            self.workers.remove(&ns);
        }
    }

    /// Commits everything the workers buffered, so no staged job waits out its lease.
    async fn drain(&mut self) {
        for (ns, w) in &self.workers {
            if let Err(e) = w.flush_all(ns, now_ms()).await {
                tracing::warn!(error = %e, "codeintel commit at shutdown failed");
            }
        }
    }
}

fn now_ms_secs() -> i64 {
    i64::try_from(now_ms() / 1000).unwrap_or(i64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(toml: &str) -> ServeConfig {
        ServeConfig::parse(&format!(
            "store = \"memory:///\"
db = \"sqlite::memory:\"
{toml}"
        ))
        .unwrap()
    }

    #[test]
    fn disabled_or_diskless_codeintel_starts_nothing() {
        assert!(config(&cfg("")).is_none());
        assert!(config(&cfg("[codeintel]\nenabled = true\n")).is_none());
    }

    #[test]
    fn enabled_codeintel_takes_workers_memory_and_scratch_from_config() {
        let c = config(&cfg(
            "cache = { disk_dir = \"/var/cache/asc\" }\n[codeintel]\nenabled = true\nworkers = 3\nmemory = \"512MiB\"\n",
        ))
        .unwrap();
        assert_eq!(c.workers, 3);
        assert_eq!(c.heap, 8 * 1024 * 1024);
        assert_eq!(c.work_dir, PathBuf::from("/var/cache/asc").join("ci-work"));
        assert!(c.holder.ends_with(&format!("#{}", std::process::id())));
    }

    #[test]
    fn the_default_budget_gives_the_default_writer_heap() {
        assert_eq!(heap_for(2 << 30), WRITER_HEAP);
        assert_eq!(heap_for(u64::MAX), WRITER_HEAP);
    }
}
