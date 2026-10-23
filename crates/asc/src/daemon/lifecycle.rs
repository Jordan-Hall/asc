//! The CLI side of the daemon lifecycle (SPEC §9.1): autostart, single instance, version-skew
//! restart and the liveness check.
//!
//! * **Autostart:** [`ensure_running`] spawns `asc daemon run` detached and waits up to
//!   [`Config::start_wait`] (3 s) for a `Ping`. Two CLIs racing to start one is harmless: the
//!   loser fails to bind the endpoint and exits.
//! * **Version skew:** a daemon whose `Ping.version` differs is asked to `Shutdown` and, if it
//!   does not go, killed, then the new binary is started. [`stop_skewed`] does the stop half
//!   alone; `Ctx::open` calls it before `state.db` is opened, so no migration runs while an old
//!   daemon holds the database.
//! * **Liveness:** a `Ping` with no answer within [`Config::ping_timeout`] (2 s) writes a dump
//!   to `log/`, kills the process named by the pid file and starts a new daemon.
//!
//! The process side effects (spawn, kill) sit behind [`Process`] so tests run daemons in
//! process.

use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use asc_rpc::ipc::{IpcEndpoint, IpcError, WorkspaceClient, connect};
use asc_rpc::server::wire;
use asc_state::AscHome;

use super::{pid_file, read_pid};

/// Why the daemon could not be reached, started or stopped.
#[derive(Debug, thiserror::Error)]
pub enum LifecycleError {
    #[error("the daemon is disabled (ASC_NO_DAEMON)")]
    Disabled,
    #[error(transparent)]
    Ipc(#[from] IpcError),
    #[error("daemon: {0}")]
    Io(#[from] io::Error),
    #[error("the daemon did not answer within {0} ms of starting")]
    StartTimeout(u128),
    #[error("the daemon does not answer and its pid is unknown; stop it by hand")]
    UnknownPid,
    #[error("the daemon (pid {0}) did not stop")]
    StopFailed(u32),
}

/// Timeouts and locations of one daemon.
#[derive(Clone, Debug)]
pub struct Config {
    pub endpoint: IpcEndpoint,
    /// Holds the pid file.
    pub run_dir: PathBuf,
    /// Receives hang dumps.
    pub log_dir: PathBuf,
    /// This binary's version; a daemon that reports another one is replaced.
    pub version: String,
    /// How long `Ping` may take before the daemon counts as hung (SPEC §9.1: 2 s).
    pub ping_timeout: Duration,
    /// How long a new daemon has to answer `Ping` (SPEC §9.1: 3 s).
    pub start_wait: Duration,
    /// How long a daemon has to go away after `Shutdown` or a kill.
    pub stop_wait: Duration,
    /// Delay between probes while waiting.
    pub poll: Duration,
}

impl Config {
    /// The configuration for the current user and `home`.
    pub fn for_home(home: &AscHome) -> Result<Self, IpcError> {
        Ok(Self {
            endpoint: IpcEndpoint::for_current_user()?,
            run_dir: home.run(),
            log_dir: home.log(),
            version: super::VERSION.to_owned(),
            ping_timeout: Duration::from_secs(2),
            start_wait: Duration::from_secs(3),
            stop_wait: Duration::from_secs(5),
            poll: Duration::from_millis(25),
        })
    }
}

impl Config {
    /// A configuration that can probe and stop a daemon but never starts one or touches the
    /// file system (no pid file, no dumps): what `Ctx::open` uses before a home may exist.
    pub fn endpoint_only(endpoint: IpcEndpoint) -> Self {
        Self {
            endpoint,
            run_dir: PathBuf::new(),
            log_dir: PathBuf::new(),
            version: super::VERSION.to_owned(),
            ping_timeout: Duration::from_secs(2),
            start_wait: Duration::from_secs(3),
            stop_wait: Duration::from_secs(5),
            poll: Duration::from_millis(25),
        }
    }
}

/// Whether `ASC_NO_DAEMON` turns the daemon off (SPEC §9.1, "No daemon"): set and not empty,
/// `0` or `false`.
pub fn disabled(get: &dyn Fn(&str) -> Option<std::ffi::OsString>) -> bool {
    get("ASC_NO_DAEMON").is_some_and(|v| {
        let v = v.to_string_lossy();
        !(v.is_empty() || v == "0" || v.eq_ignore_ascii_case("false"))
    })
}

/// What `Ping` reported.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Info {
    pub version: String,
    pub pid: u32,
    pub uptime_ms: u64,
}

/// What one `Ping` found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Probe {
    Up(Info),
    /// A connection was made, but no answer came in time.
    Hung,
    /// Nothing listens.
    Down,
}

/// Starts and kills daemon processes.
pub trait Process: Send + Sync {
    /// Starts `asc daemon run` detached from this process.
    fn spawn(&self) -> io::Result<()>;
    /// Kills process `pid` outright.
    fn kill(&self, pid: u32) -> io::Result<()>;
}

/// The real processes: this executable, run detached, and the operating system's kill.
pub struct SystemProcess {
    pub exe: PathBuf,
    /// `$ASC_HOME`, handed to the daemon so it never resolves a different one.
    pub home: PathBuf,
}

impl Process for SystemProcess {
    fn spawn(&self) -> io::Result<()> {
        use std::process::{Command, Stdio};
        let mut cmd = Command::new(&self.exe);
        cmd.args(["daemon", "run"])
            .env("ASC_HOME", &self.home)
            .env_remove("ASC_NO_DAEMON")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            // DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW
            cmd.creation_flags(0x0000_0008 | 0x0000_0200 | 0x0800_0000);
        }
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            cmd.process_group(0);
        }
        // The child is not waited for: it outlives this process.
        cmd.spawn().map(drop)
    }

    fn kill(&self, pid: u32) -> io::Result<()> {
        use std::process::{Command, Stdio};
        if pid <= 1 || pid == std::process::id() {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "refusing to kill that pid"));
        }
        let mut cmd = if cfg!(windows) {
            let mut c = Command::new("taskkill");
            c.args(["/F", "/PID", &pid.to_string()]);
            c
        } else {
            let mut c = Command::new("kill");
            c.args(["-9", &pid.to_string()]);
            c
        };
        let status = cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).status()?;
        if status.success() {
            Ok(())
        } else {
            Err(io::Error::other(format!("could not kill pid {pid}")))
        }
    }
}

/// Sends one `Ping` and classifies the result.
pub async fn probe(cfg: &Config) -> Probe {
    let mut client = WorkspaceClient::new(connect(&cfg.endpoint));
    match tokio::time::timeout(cfg.ping_timeout, client.ping(wire::PingRequest::default())).await {
        Err(_) => Probe::Hung,
        Ok(Err(_)) => Probe::Down,
        Ok(Ok(r)) => {
            let r = r.into_inner();
            Probe::Up(Info {
                version: r.version,
                pid: r.pid,
                uptime_ms: r.uptime_ms,
            })
        }
    }
}

async fn wait_down(cfg: &Config) -> bool {
    let deadline = Instant::now() + cfg.stop_wait;
    loop {
        if probe(cfg).await == Probe::Down {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(cfg.poll).await;
    }
}

/// Writes what the CLI can learn about a hung daemon to `log/daemon-hang-<time>-<pid>.txt` and
/// returns its path. On Linux it lists the daemon's threads and where each one waits; the file
/// is created exclusively, so a planted link is never followed.
pub fn write_dump(cfg: &Config, pid: u32) -> io::Result<PathBuf> {
    use std::io::Write;
    std::fs::create_dir_all(&cfg.log_dir)?;
    let now = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
    let path = cfg.log_dir.join(format!("daemon-hang-{now}-{pid}.txt"));
    let mut out = std::fs::OpenOptions::new().write(true).create_new(true).open(&path)?;
    writeln!(
        out,
        "asc daemon did not answer Ping within {} ms\npid: {pid}\nunix time: {now}\nversion: {}",
        cfg.ping_timeout.as_millis(),
        cfg.version
    )?;
    if cfg!(target_os = "linux") {
        write_linux_threads(&mut out, pid);
    }
    Ok(path)
}

/// One line per thread of `pid`: id, name and the kernel function it waits in. Bounded to 512.
fn write_linux_threads(out: &mut impl io::Write, pid: u32) {
    let tasks = Path::new("/proc").join(pid.to_string()).join("task");
    let Ok(entries) = std::fs::read_dir(&tasks) else { return }; // lint:allow-worktree-fs /proc, not a work tree
    for entry in entries.flatten().take(512) {
        let read = |name: &str| {
            std::fs::read_to_string(entry.path().join(name)).map_or_else(|_| "?".into(), |s| s.trim().to_owned())
        };
        let _ = writeln!(out, "thread {} {} wchan={}", entry.file_name().to_string_lossy(), read("comm"), read("wchan"));
    }
}

/// Stops a daemon that answers `Ping`: `Shutdown`, then a kill if it stays.
async fn stop_up(cfg: &Config, proc: &dyn Process, info: &Info) -> Result<(), LifecycleError> {
    let mut client = WorkspaceClient::new(connect(&cfg.endpoint));
    // The RPC may fail because the daemon closes the connection as it goes; the wait decides.
    let _ = tokio::time::timeout(cfg.ping_timeout, client.shutdown(wire::ShutdownRequest::default())).await;
    if wait_down(cfg).await {
        return Ok(());
    }
    proc.kill(info.pid)?;
    if wait_down(cfg).await {
        Ok(())
    } else {
        Err(LifecycleError::StopFailed(info.pid))
    }
}

/// Kills a daemon that does not answer, after writing the dump. The pid comes from the pid file
/// in the user's private `run/` directory.
async fn kill_hung(cfg: &Config, proc: &dyn Process) -> Result<(), LifecycleError> {
    let pid = read_pid(&cfg.run_dir).ok_or(LifecycleError::UnknownPid)?;
    // The dump is diagnostics; failing to write it must not keep a hung daemon alive.
    let _ = write_dump(cfg, pid);
    proc.kill(pid)?;
    let _ = std::fs::remove_file(pid_file(&cfg.run_dir));
    if wait_down(cfg).await {
        Ok(())
    } else {
        Err(LifecycleError::StopFailed(pid))
    }
}

async fn start(cfg: &Config, proc: &dyn Process) -> Result<Info, LifecycleError> {
    proc.spawn()?;
    let started = Instant::now();
    loop {
        if let Probe::Up(info) = probe(cfg).await
            && info.version == cfg.version
        {
            return Ok(info);
        }
        if started.elapsed() >= cfg.start_wait {
            return Err(LifecycleError::StartTimeout(cfg.start_wait.as_millis()));
        }
        tokio::time::sleep(cfg.poll).await;
    }
}

/// Makes sure a daemon of this version answers, starting, replacing or restarting one as
/// needed, and returns what it reported.
pub async fn ensure_running(cfg: &Config, proc: &dyn Process) -> Result<Info, LifecycleError> {
    match probe(cfg).await {
        Probe::Up(info) if info.version == cfg.version => return Ok(info),
        Probe::Up(info) => stop_up(cfg, proc, &info).await?,
        Probe::Hung => kill_hung(cfg, proc).await?,
        Probe::Down => {
            // A pid file with nobody behind it is stale.
            let _ = std::fs::remove_file(pid_file(&cfg.run_dir));
        }
    }
    start(cfg, proc).await
}

/// Stops a daemon of another version and returns whether it did. A daemon of this version, a
/// hung one and no daemon at all are left alone. `Ctx::open` calls this before it opens
/// `state.db`, so migrations never run under an old daemon (SPEC §9.1).
pub async fn stop_skewed(cfg: &Config, proc: &dyn Process) -> Result<bool, LifecycleError> {
    match probe(cfg).await {
        Probe::Up(info) if info.version != cfg.version => {
            stop_up(cfg, proc, &info).await?;
            Ok(true)
        }
        _ => Ok(false),
    }
}

/// What [`stop`] did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Stopped {
    NotRunning,
    Stopped,
}

/// `asc daemon stop`: asks the daemon to shut down; a hung one is killed.
pub async fn stop(cfg: &Config, proc: &dyn Process) -> Result<Stopped, LifecycleError> {
    match probe(cfg).await {
        Probe::Down => Ok(Stopped::NotRunning),
        Probe::Up(info) => stop_up(cfg, proc, &info).await.map(|()| Stopped::Stopped),
        Probe::Hung => kill_hung(cfg, proc).await.map(|()| Stopped::Stopped),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    use asc_rpc::ipc::IpcListener;
    use tokio::task::JoinHandle;

    use super::*;
    use crate::daemon::{RunConfig, run};

    static NEXT: AtomicUsize = AtomicUsize::new(0);

    fn config(dir: &Path, version: &str) -> Config {
        let endpoint = if cfg!(windows) {
            IpcEndpoint::from_raw(format!("asc-u53a-{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)))
        } else {
            IpcEndpoint::from_raw(dir.join("sock").join("asc.sock").to_string_lossy().into_owned())
        };
        Config {
            endpoint,
            run_dir: dir.join("run"),
            log_dir: dir.join("log"),
            version: version.to_owned(),
            ping_timeout: Duration::from_millis(300),
            start_wait: Duration::from_secs(3),
            stop_wait: Duration::from_secs(3),
            poll: Duration::from_millis(10),
        }
    }

    /// Runs daemons as tasks of the test.
    struct InProcess {
        cfg: Config,
        version: Mutex<String>,
        tasks: Mutex<Vec<JoinHandle<()>>>,
        spawned: AtomicUsize,
        killed: Mutex<Vec<u32>>,
    }

    impl InProcess {
        fn new(cfg: &Config) -> Arc<Self> {
            Arc::new(Self {
                cfg: cfg.clone(),
                version: Mutex::new(cfg.version.clone()),
                tasks: Mutex::default(),
                spawned: AtomicUsize::new(0),
                killed: Mutex::default(),
            })
        }

        fn adopt(&self, task: JoinHandle<()>) {
            self.tasks.lock().unwrap().push(task);
        }

        /// A crash: the tasks stop with no clean-up (the pid file stays).
        fn crash(&self) {
            for t in self.tasks.lock().unwrap().drain(..) {
                t.abort();
            }
        }
    }

    impl Process for InProcess {
        fn spawn(&self) -> io::Result<()> {
            self.spawned.fetch_add(1, Ordering::SeqCst);
            let cfg = RunConfig {
                endpoint: self.cfg.endpoint.clone(),
                run_dir: self.cfg.run_dir.clone(),
                version: self.version.lock().unwrap().clone(),
            };
            self.adopt(tokio::spawn(async move {
                let _ = run(cfg, std::future::pending()).await;
            }));
            Ok(())
        }

        fn kill(&self, pid: u32) -> io::Result<()> {
            self.killed.lock().unwrap().push(pid);
            self.crash();
            Ok(())
        }
    }

    #[tokio::test]
    async fn start_stop_restart_cycle() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = config(dir.path(), "1.0.0");
        let proc = InProcess::new(&cfg);

        assert_eq!(probe(&cfg).await, Probe::Down);
        assert_eq!(stop(&cfg, &*proc).await.unwrap(), Stopped::NotRunning);

        let first = ensure_running(&cfg, &*proc).await.unwrap();
        assert_eq!(first.version, "1.0.0");
        assert_eq!(read_pid(&cfg.run_dir), Some(std::process::id()));
        // A second call finds the running daemon and starts nothing.
        ensure_running(&cfg, &*proc).await.unwrap();
        assert_eq!(proc.spawned.load(Ordering::SeqCst), 1);

        assert_eq!(stop(&cfg, &*proc).await.unwrap(), Stopped::Stopped);
        assert_eq!(probe(&cfg).await, Probe::Down);
        for _ in 0..100 {
            if read_pid(&cfg.run_dir).is_none() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(read_pid(&cfg.run_dir), None, "a clean stop removes the pid file");

        ensure_running(&cfg, &*proc).await.unwrap();
        assert_eq!(proc.spawned.load(Ordering::SeqCst), 2);
        stop(&cfg, &*proc).await.unwrap();
    }

    #[tokio::test]
    async fn a_second_daemon_cannot_take_the_endpoint() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = config(dir.path(), "1.0.0");
        let proc = InProcess::new(&cfg);
        ensure_running(&cfg, &*proc).await.unwrap();
        let second = run(
            RunConfig {
                endpoint: cfg.endpoint.clone(),
                run_dir: cfg.run_dir.clone(),
                version: "1.0.0".into(),
            },
            std::future::pending(),
        )
        .await;
        assert!(second.is_err(), "single instance: the endpoint is taken");
        assert!(matches!(probe(&cfg).await, Probe::Up(_)), "the first daemon is unharmed");
        stop(&cfg, &*proc).await.unwrap();
    }

    #[tokio::test]
    async fn a_killed_daemon_is_restarted() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = config(dir.path(), "1.0.0");
        let proc = InProcess::new(&cfg);
        ensure_running(&cfg, &*proc).await.unwrap();
        proc.crash();
        assert!(wait_down(&cfg).await);
        assert!(pid_file(&cfg.run_dir).exists(), "a crash leaves a stale pid file");

        let again = ensure_running(&cfg, &*proc).await.unwrap();
        assert_eq!(again.version, "1.0.0");
        assert_eq!(proc.spawned.load(Ordering::SeqCst), 2);
        stop(&cfg, &*proc).await.unwrap();
    }

    #[tokio::test]
    async fn a_version_mismatch_triggers_restart() {
        let dir = tempfile::tempdir().unwrap();
        let old = config(dir.path(), "1.0.0");
        let proc = InProcess::new(&old);
        ensure_running(&old, &*proc).await.unwrap();

        let new = Config {
            version: "2.0.0".into(),
            ..old.clone()
        };
        // The old daemon is left alone by a caller of its own version...
        assert!(!stop_skewed(&old, &*proc).await.unwrap());
        // ...and stopped by one of another version.
        assert!(stop_skewed(&new, &*proc).await.unwrap());
        assert_eq!(probe(&new).await, Probe::Down);

        // `ensure_running` replaces a daemon of another version in one step.
        ensure_running(&old, &*proc).await.unwrap();
        *proc.version.lock().unwrap() = "2.0.0".into();
        let replaced = ensure_running(&new, &*proc).await.unwrap();
        assert_eq!(replaced.version, "2.0.0");
        assert_eq!(proc.spawned.load(Ordering::SeqCst), 3);
        assert!(proc.killed.lock().unwrap().is_empty(), "a daemon that shuts down cleanly is not killed");
        stop(&new, &*proc).await.unwrap();
    }

    /// A daemon that accepts connections and never answers.
    fn hung_daemon(cfg: &Config) -> JoinHandle<()> {
        let listener = IpcListener::bind(&cfg.endpoint).unwrap();
        tokio::spawn(async move {
            let mut held = Vec::new();
            while let Ok(stream) = listener.accept().await {
                held.push(stream);
            }
        })
    }

    #[tokio::test]
    async fn hung_daemon_is_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = config(dir.path(), "1.0.0");
        let proc = InProcess::new(&cfg);
        proc.adopt(hung_daemon(&cfg));
        std::fs::create_dir_all(&cfg.run_dir).unwrap();
        std::fs::write(pid_file(&cfg.run_dir), "4242\n").unwrap();
        assert_eq!(probe(&cfg).await, Probe::Hung);

        let info = ensure_running(&cfg, &*proc).await.unwrap();
        assert_eq!(info.version, "1.0.0");
        assert_eq!(*proc.killed.lock().unwrap(), [4242], "the pid file names the process to kill");
        let dumps: Vec<_> = std::fs::read_dir(&cfg.log_dir).unwrap().flatten().collect();
        assert_eq!(dumps.len(), 1, "one dump per hang");
        let text = std::fs::read_to_string(dumps[0].path()).unwrap();
        assert!(text.contains("pid: 4242"), "{text}");
        stop(&cfg, &*proc).await.unwrap();
    }

    #[tokio::test]
    async fn a_hung_daemon_with_no_pid_is_reported_not_guessed() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = config(dir.path(), "1.0.0");
        let proc = InProcess::new(&cfg);
        proc.adopt(hung_daemon(&cfg));
        let err = ensure_running(&cfg, &*proc).await.unwrap_err();
        assert!(matches!(err, LifecycleError::UnknownPid), "{err}");
        assert!(proc.killed.lock().unwrap().is_empty());
        proc.crash();
    }

    #[test]
    fn no_daemon_switch() {
        let env = |v: &'static str| move |k: &str| (k == "ASC_NO_DAEMON").then(|| std::ffi::OsString::from(v));
        assert!(disabled(&env("1")));
        assert!(!disabled(&env("0")));
        assert!(!disabled(&env("")));
        assert!(!disabled(&env("false")));
        assert!(!disabled(&|_| None));
    }

    #[test]
    fn the_system_process_refuses_to_kill_itself_or_init() {
        let p = SystemProcess {
            exe: PathBuf::from("asc"),
            home: PathBuf::from("home"),
        };
        assert!(p.kill(std::process::id()).is_err());
        assert!(p.kill(1).is_err());
        assert!(p.kill(0).is_err());
    }
}
