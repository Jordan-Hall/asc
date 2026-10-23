//! The per-user daemon (SPEC §9.1): the process side of `asc daemon run`.
//!
//! This unit (U53a) owns the process lifecycle: a single instance bound to the user's IPC
//! endpoint, a pid file in `$ASC_HOME/run`, `Ping` and `Shutdown`, and the watchdog that aborts
//! the process when a provider callback hangs. The remaining `Workspace` RPCs answer
//! `UNIMPLEMENTED` until U53b (service and scheduler) replaces [`Service`]'s bodies. The client
//! half (autostart, version skew, liveness) is in [`lifecycle`].
//!
//! Every connection is local and already restricted to the current user by the transport (a
//! DACL on the Windows pipe, uid check on the Unix socket), so each RPC is authorised by that
//! check before it reaches [`Service`].

pub mod install;
pub mod lifecycle;
pub mod workspace_factory;

use std::collections::HashMap;
use std::future::Future;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use asc_rpc::ipc::{IpcEndpoint, IpcError, IpcListener, Workspace, WorkspaceServer, serve_incoming};
use asc_rpc::server::wire;
use futures::stream::Empty;
use tokio::sync::Notify;
use tonic::{Request, Response, Status};

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

const NOT_SERVED: &str = "this daemon RPC is not served yet";

fn not_served<T>() -> Result<T, Status> {
    Err(Status::unimplemented(NOT_SERVED))
}

type Never<T> = Empty<Result<T, Status>>;

/// The `Workspace` service of the daemon. `Ping` and `Shutdown` are served here (U53a); the
/// rest is U53b.
pub struct Service {
    version: String,
    started: Instant,
    stop: Arc<Notify>,
}

impl Service {
    fn new(version: String, stop: Arc<Notify>) -> Self {
        Self {
            version,
            started: Instant::now(),
            stop,
        }
    }
}

#[tonic::async_trait]
impl Workspace for Service {
    async fn status(&self, _: Request<wire::WsStatusRequest>) -> Result<Response<wire::WsStatusResponse>, Status> {
        not_served()
    }

    type DiffStream = Never<wire::DiffResponse>;
    async fn diff(&self, _: Request<wire::WsDiffRequest>) -> Result<Response<Self::DiffStream>, Status> {
        not_served()
    }

    type LogStream = Never<wire::LogResponse>;
    async fn log(&self, _: Request<wire::WsLogRequest>) -> Result<Response<Self::LogStream>, Status> {
        not_served()
    }

    type BlameStream = Never<wire::BlameResponse>;
    async fn blame(&self, _: Request<wire::WsBlameRequest>) -> Result<Response<Self::BlameStream>, Status> {
        not_served()
    }

    async fn affected(&self, _: Request<wire::WsAffectedRequest>) -> Result<Response<wire::WsAffectedResponse>, Status> {
        not_served()
    }

    type WatchStream = Never<wire::WatchEvent>;
    async fn watch(&self, _: Request<wire::WatchRequest>) -> Result<Response<Self::WatchStream>, Status> {
        not_served()
    }

    async fn stage(&self, _: Request<wire::WsStageRequest>) -> Result<Response<wire::WsStageResponse>, Status> {
        not_served()
    }

    async fn ping(&self, _: Request<wire::PingRequest>) -> Result<Response<wire::PingResponse>, Status> {
        Ok(Response::new(wire::PingResponse {
            version: self.version.clone(),
            pid: std::process::id(),
            uptime_ms: u64::try_from(self.started.elapsed().as_millis()).unwrap_or(u64::MAX),
        }))
    }

    async fn mount(&self, _: Request<wire::MountRequest>) -> Result<Response<wire::MountResponse>, Status> {
        not_served()
    }

    async fn unmount(&self, _: Request<wire::UnmountRequest>) -> Result<Response<wire::UnmountResponse>, Status> {
        not_served()
    }

    async fn switch(&self, _: Request<wire::SwitchRequest>) -> Result<Response<wire::SwitchResponse>, Status> {
        not_served()
    }

    async fn journal(&self, _: Request<wire::JournalRequest>) -> Result<Response<wire::JournalResponse>, Status> {
        not_served()
    }

    async fn clear_journal(&self, _: Request<wire::ClearJournalRequest>) -> Result<Response<wire::ClearJournalResponse>, Status> {
        not_served()
    }

    async fn dehydrate(&self, _: Request<wire::DehydrateRequest>) -> Result<Response<wire::DehydrateResponse>, Status> {
        not_served()
    }

    async fn prefetch(&self, _: Request<wire::PrefetchRequest>) -> Result<Response<wire::PrefetchResponse>, Status> {
        not_served()
    }

    async fn stats(&self, _: Request<wire::StatsRequest>) -> Result<Response<wire::StatsResponse>, Status> {
        not_served()
    }

    async fn shutdown(&self, _: Request<wire::ShutdownRequest>) -> Result<Response<wire::ShutdownResponse>, Status> {
        // `notify_one` keeps a permit, so a stop requested before the server awaits is not lost.
        self.stop.notify_one();
        Ok(Response::new(wire::ShutdownResponse::default()))
    }
}

// ------------------------------------------------------------------------------ run

/// What [`run`] needs.
pub struct RunConfig {
    pub endpoint: IpcEndpoint,
    /// `$ASC_HOME/run`, where the pid file lives.
    pub run_dir: PathBuf,
    /// The version `Ping` reports.
    pub version: String,
}

/// Runs the daemon until `Shutdown` arrives or `interrupt` resolves. Fails with
/// [`IpcError::InUse`] when another daemon already answers on the endpoint (single instance).
/// The socket and the pid file are removed on the way out.
pub async fn run(cfg: RunConfig, interrupt: impl Future<Output = ()>) -> Result<(), DaemonError> {
    let listener = IpcListener::bind(&cfg.endpoint)?;
    write_pid(&cfg.run_dir)?;
    let stop = Arc::new(Notify::new());
    let service = Service::new(cfg.version, stop.clone());
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
