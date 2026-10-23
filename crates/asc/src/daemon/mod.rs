//! The per-user daemon (SPEC §9.1): the process side of `asc daemon run`.
//!
//! This unit (U53a) owns the process lifecycle: a single instance bound to the user's IPC
//! endpoint, a pid file in `$ASC_HOME/run`, `Ping` and `Shutdown`, and the watchdog that aborts
//! the process when a provider callback hangs. The `Workspace` service is in [`service`] and the
//! background tasks in [`scheduler`] (U53b); the observability endpoint and log are in
//! [`metrics`]. The client half (autostart, version skew, liveness) is in [`lifecycle`].
//!
//! Every connection is local and already restricted to the current user by the transport (a
//! DACL on the Windows pipe, uid check on the Unix socket), so each RPC is authorised by that
//! check before it reaches [`Service`].

pub mod codeintel_overlay;
pub mod install;
pub mod lifecycle;
pub mod metrics;
pub mod plugin_api;
pub mod plugin_api_stack;
pub mod scheduler;
pub mod service;
pub mod workspace_factory;

pub use service::Service;

use std::collections::HashMap;
use std::future::Future;
use std::io;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use asc_rpc::ipc::{IpcEndpoint, IpcError, IpcListener, WorkspaceServer, serve_incoming};
#[cfg(test)]
use futures::stream::Empty;
use tokio::sync::Notify;
#[cfg(test)]
use tonic::Status;

/// The daemon's version: `Ping.version`, compared by the CLI (SPEC §9.1, "Version skew").
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// A provider callback that runs longer than this aborts the process (SPEC §9.1, "Liveness").
pub const CALLBACK_LIMIT: Duration = Duration::from_secs(60);

/// How often the watchdog thread looks at the running callbacks.
const WATCHDOG_TICK: Duration = Duration::from_secs(1);

/// Largest pid file read.
const MAX_PID_FILE: u64 = 32;

/// Most concurrent HTTP/2 streams per connection; bounds the work one local client can queue.
const MAX_STREAMS: u32 = 64;

/// Why the daemon could not run.
#[derive(Debug, thiserror::Error)]
pub enum DaemonError {
    #[error(transparent)]
    Ipc(#[from] IpcError),
    #[error("daemon: {0}")]
    Io(#[from] io::Error),
    #[error("daemon: {0}")]
    Serve(#[from] tonic::transport::Error),
}

// ------------------------------------------------------------------------------ watchdog

#[derive(Default)]
struct Running {
    next: AtomicU64,
    active: Mutex<HashMap<u64, (Instant, &'static str)>>,
}

impl Running {
    fn longest(&self) -> Option<(&'static str, Duration)> {
        let active = self.active.lock().unwrap_or_else(|e| e.into_inner());
        active
            .values()
            .map(|(since, what)| (*what, since.elapsed()))
            .max_by_key(|(_, age)| *age)
    }
}

/// Watches provider callbacks (ProjFS and FUSE entry points). A callback holds a
/// [`CallbackGuard`] while it runs; when one outlives the limit the hang action runs, which in
/// the daemon aborts the process so the service manager (or the CLI) restarts it.
pub struct Watchdog {
    running: Arc<Running>,
}

impl Watchdog {
    /// Starts the watchdog thread; `on_hang(what, age)` runs on every tick while a callback is
    /// over `limit`. The thread ends when the last [`Watchdog`] handle is dropped.
    pub fn spawn(
        limit: Duration,
        tick: Duration,
        on_hang: impl Fn(&'static str, Duration) + Send + 'static,
    ) -> io::Result<Self> {
        let running = Arc::new(Running::default());
        let weak = Arc::downgrade(&running);
        std::thread::Builder::new()
            .name("asc-watchdog".into())
            .spawn(move || {
                loop {
                    std::thread::sleep(tick);
                    let Some(running) = weak.upgrade() else { break };
                    if let Some((what, age)) = running.longest().filter(|(_, age)| *age > limit) {
                        on_hang(what, age);
                    }
                }
            })?;
        Ok(Self { running })
    }

    /// The daemon's watchdog: aborts the process after [`CALLBACK_LIMIT`].
    pub fn abort_on_hang() -> io::Result<Self> {
        Self::spawn(CALLBACK_LIMIT, WATCHDOG_TICK, |what, age| {
            eprintln!("asc daemon: {what} has run for {}s; aborting", age.as_secs());
            std::process::abort();
        })
    }

    /// Marks the start of a callback; it ends when the guard drops.
    pub fn enter(&self, what: &'static str) -> CallbackGuard {
        let id = self.running.next.fetch_add(1, Ordering::Relaxed);
        self.running
            .active
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(id, (Instant::now(), what));
        CallbackGuard {
            running: self.running.clone(),
            id,
        }
    }
}

/// A callback in progress, as far as the [`Watchdog`] is concerned.
pub struct CallbackGuard {
    running: Arc<Running>,
    id: u64,
}

impl Drop for CallbackGuard {
    fn drop(&mut self) {
        self.running
            .active
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&self.id);
    }
}

// ------------------------------------------------------------------------------ pid file

/// `<run_dir>/daemon.pid`.
pub fn pid_file(run_dir: &Path) -> PathBuf {
    run_dir.join("daemon.pid")
}

/// The pid in the pid file, if it holds one. The file may be stale; only `Ping` says whether
/// a daemon answers.
pub fn read_pid(run_dir: &Path) -> Option<u32> {
    use std::io::Read;
    let mut text = String::new();
    std::fs::File::open(pid_file(run_dir))
        .ok()?
        .take(MAX_PID_FILE)
        .read_to_string(&mut text)
        .ok()?;
    text.trim().parse().ok()
}

fn write_pid(run_dir: &Path) -> io::Result<()> {
    std::fs::create_dir_all(run_dir)?;
    let pid = std::process::id();
    let tmp = run_dir.join(format!("daemon.pid.{pid}.tmp"));
    std::fs::write(&tmp, format!("{pid}\n"))?;
    std::fs::rename(&tmp, pid_file(run_dir)).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })
}

fn remove_own_pid(run_dir: &Path) {
    if read_pid(run_dir) == Some(std::process::id()) {
        let _ = std::fs::remove_file(pid_file(run_dir));
    }
}

// ------------------------------------------------------------------------------ service

#[cfg(test)]
const NOT_SERVED: &str = "this daemon RPC is not served yet";

#[cfg(test)]
fn not_served<T>() -> Result<T, Status> {
    Err(Status::unimplemented(NOT_SERVED))
}

#[cfg(test)]
type Never<T> = Empty<Result<T, Status>>;

// ------------------------------------------------------------------------------ run

/// What [`run`] needs.
pub struct RunConfig {
    pub endpoint: IpcEndpoint,
    /// `$ASC_HOME/run`, where the pid file lives.
    pub run_dir: PathBuf,
    /// The version `Ping` reports.
    pub version: String,
}

/// What a daemon that hosts workspaces needs on top of [`RunConfig`].
pub struct Hosting {
    /// The user's home: `state.db`, the cache and the logs are in it.
    pub home: asc_state::AscHome,
    /// `--metrics-listen`: a loopback address, or none (the default).
    pub metrics_listen: Option<SocketAddr>,
    /// The OS backend for virtual workspaces, when the build has one.
    pub backend: Option<Arc<dyn service::VirtualBackend>>,
    pub settings: scheduler::Settings,
    pub hooks: scheduler::Hooks,
    /// Tasks of later units (the uploader, the live subscription) added to the built-in ones.
    pub extra_jobs: Vec<(scheduler::Spec, Arc<dyn scheduler::Job>)>,
}

/// Runs the daemon until `Shutdown` arrives or `interrupt` resolves. Fails with
/// [`IpcError::InUse`] when another daemon already answers on the endpoint (single instance).
/// The socket and the pid file are removed on the way out. This daemon hosts no workspaces; see
/// [`run_hosting`].
pub async fn run(cfg: RunConfig, interrupt: impl Future<Output = ()>) -> Result<(), DaemonError> {
    run_with(cfg, None, interrupt).await
}

/// [`run`] for the real daemon: opens `state.db`, re-attaches the virtual workspaces, starts the
/// scheduler and (when asked) the metrics endpoint, and stops all of it on the way out.
pub async fn run_hosting(
    cfg: RunConfig,
    hosting: Hosting,
    interrupt: impl Future<Output = ()>,
) -> Result<(), DaemonError> {
    run_with(cfg, Some(hosting), interrupt).await
}

async fn run_with(
    cfg: RunConfig,
    hosting: Option<Hosting>,
    interrupt: impl Future<Output = ()>,
) -> Result<(), DaemonError> {
    let listener = IpcListener::bind(&cfg.endpoint)?;
    // Bind the metrics port before the daemon announces itself, so a bad port fails the start.
    let metrics_listener = match hosting.as_ref().and_then(|h| h.metrics_listen) {
        Some(addr) => Some(metrics::bind(addr).await?),
        None => None,
    };
    write_pid(&cfg.run_dir)?;
    let stop = Arc::new(Notify::new());

    let mut running = None;
    let mut metrics_stop = None;
    let state = match hosting {
        None => None,
        Some(h) => {
            let db = asc_state::StateDb::open(&h.home).await.map_err(|e| io::Error::other(e.to_string()))?;
            let events = Arc::new(
                metrics::EventLog::open(&h.home.log()).unwrap_or_else(|_| metrics::EventLog::none()),
            );
            let meter = Arc::new(metrics::Metrics::new());
            let state = service::State::new(db, h.home, h.backend, meter.clone(), events.clone());
            state.restore().await;
            let mut tasks = scheduler::builtin(state.clone(), &h.settings, &h.hooks);
            for (spec, job) in h.extra_jobs {
                if let Err(e) = tasks.register(spec, job) {
                    events.emit("error", "daemon.task", &[("error", &e)]);
                }
            }
            running = Some(tasks.start(meter.clone(), events.clone()));
            if let Some(listener) = metrics_listener {
                let (tx, rx) = tokio::sync::watch::channel(false);
                tokio::spawn(metrics::serve(listener, meter, state.clone(), rx));
                metrics_stop = Some(tx);
            }
            events.emit("info", "daemon.start", &[("version", &cfg.version)]);
            Some(state)
        }
    };

    let service = Service::new(cfg.version, stop.clone(), state.clone());
    let done = async {
        tokio::select! {
            () = stop.notified() => {}
            () = interrupt => {}
        }
    };
    let served = tonic::transport::Server::builder()
        .max_concurrent_streams(MAX_STREAMS)
        .add_service(WorkspaceServer::new(service))
        .serve_with_incoming_shutdown(serve_incoming(listener), done)
        .await;
    if let Some(tx) = metrics_stop {
        let _ = tx.send(true);
    }
    if let Some(running) = running {
        running.stop().await;
    }
    if let Some(state) = &state {
        state.shutdown().await;
    }
    remove_own_pid(&cfg.run_dir);
    served.map_err(Into::into)
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicBool;

    use super::*;

    #[test]
    fn watchdog_fires_only_for_a_callback_over_the_limit() {
        let fired = Arc::new(AtomicBool::new(false));
        let seen = fired.clone();
        let dog = Watchdog::spawn(Duration::from_millis(60), Duration::from_millis(10), move |what, _| {
            assert_eq!(what, "read");
            seen.store(true, Ordering::SeqCst);
        })
        .unwrap();
        {
            let _quick = dog.enter("read");
        }
        std::thread::sleep(Duration::from_millis(150));
        assert!(!fired.load(Ordering::SeqCst), "a finished callback must not trip the watchdog");
        let _slow = dog.enter("read");
        std::thread::sleep(Duration::from_millis(250));
        assert!(fired.load(Ordering::SeqCst), "a callback over the limit trips it");
    }

    #[test]
    fn pid_file_round_trips_and_is_bounded() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(read_pid(dir.path()), None);
        write_pid(dir.path()).unwrap();
        assert_eq!(read_pid(dir.path()), Some(std::process::id()));
        std::fs::write(pid_file(dir.path()), "12 garbage").unwrap();
        assert_eq!(read_pid(dir.path()), None);
        std::fs::write(pid_file(dir.path()), "1".repeat(4096)).unwrap();
        assert_eq!(read_pid(dir.path()), None, "an oversized file is not a pid");
        remove_own_pid(dir.path());
        assert!(pid_file(dir.path()).exists(), "another process's pid file is left alone");
    }
}
