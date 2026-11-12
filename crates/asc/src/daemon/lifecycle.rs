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
//! * **Trust:** whatever answers on the endpoint is not trusted by name. Every connection checks
//!   the server process with the OS (`asc_rpc::ipc`: same user, else refused), and a kill only
//!   ever targets a pid that is *all* of: the OS-reported server pid, the pid in the pid file the
//!   daemon wrote into the private run directory, and (after `Ping`) the pid the daemon claims.
//!   Otherwise the endpoint is held by something that is not asc's daemon:
//!   [`LifecycleError::NotDaemon`], and nothing is killed.
//!
//! The process side effects (spawn, kill) sit behind [`Process`] so tests run daemons in
//! process.

use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use asc_rpc::ipc::{
    IpcEndpoint, IpcError, WorkspaceClient, connect, connect_stream, verified_server_pid,
};
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
    #[error("a process that is not asc's daemon holds the endpoint; nothing was killed")]
    NotDaemon,
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

/// The endpoint of the current user's daemon. `ASC_DAEMON_ENDPOINT` names another one (a pipe
/// name or a socket path); the plugin API tests use it so a test daemon never meets the
/// user's own. The variable is the user's own environment, like `ASC_HOME`.
pub fn endpoint_for_current_user() -> Result<IpcEndpoint, IpcError> {
    let root = AscHome::locate_root().map_err(|e| IpcError::NoRuntimeDir(e.to_string()))?;
    endpoint_for_home(&root)
}

/// The endpoint of the daemon serving the home at `root` (SPEC §9.1): `ASC_DAEMON_ENDPOINT`
/// when set; else the user's endpoint, narrowed by [`home_key`] unless `root` is the platform
/// default home. A daemon only ever answers for the home it opened.
pub fn endpoint_for_home(root: &Path) -> Result<IpcEndpoint, IpcError> {
    if let Ok(name) = std::env::var("ASC_DAEMON_ENDPOINT")
        && !name.is_empty()
    {
        return Ok(IpcEndpoint::from_raw(name));
    }
    let user = IpcEndpoint::for_current_user()?;
    let default = AscHome::resolve_root(
        |k| (k != "ASC_HOME").then(|| std::env::var_os(k)).flatten(),
        None,
    )
    .ok();
    Ok(match default {
        Some(d) if same_home(&d, root) => user,
        _ => user.for_home_key(&home_key(root)),
    })
}

/// A short digest of a home's absolute path, case-folded where the file system is.
pub fn home_key(root: &Path) -> String {
    let abs = std::path::absolute(root).unwrap_or_else(|_| root.to_owned());
    let text = abs.to_string_lossy();
    let text = if cfg!(windows) {
        text.to_lowercase()
    } else {
        text.into_owned()
    };
    blake3::hash(text.as_bytes()).to_hex()[..12].to_owned()
}

fn same_home(a: &Path, b: &Path) -> bool {
    let abs = |p: &Path| std::path::absolute(p).unwrap_or_else(|_| p.to_owned());
    if cfg!(windows) {
        abs(a).to_string_lossy().to_lowercase() == abs(b).to_string_lossy().to_lowercase()
    } else {
        abs(a) == abs(b)
    }
}

impl Config {
    /// The configuration for the current user and `home`.
    pub fn for_home(home: &AscHome) -> Result<Self, IpcError> {
        Ok(Self {
            endpoint: endpoint_for_home(home.root())?,
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

/// The first argument the daemon is started with. The binary may be running under another name
/// (the `git` shim starts the daemon too), and this argument makes it run as `asc daemon`
/// whatever that name is.
pub const DAEMON_ENTRY: &str = "--asc-daemon-entry";

/// `args` (argv0 first) as `asc` must read them when they start with [`DAEMON_ENTRY`]: the entry
/// argument becomes `daemon`. `None` for any other command line.
pub fn daemon_entry(args: &[std::ffi::OsString]) -> Option<Vec<std::ffi::OsString>> {
    if args.get(1).is_none_or(|a| a != DAEMON_ENTRY) {
        return None;
    }
    let mut out = args.to_vec();
    out[1] = "daemon".into();
    Some(out)
}

/// The daemon's environment: this process's, without `ASC_NO_DAEMON`, with `ASC_HOME` set to
/// `home`. Names compare case-insensitively on Windows.
fn daemon_env(
    vars: impl Iterator<Item = (std::ffi::OsString, std::ffi::OsString)>,
    home: &Path,
) -> Vec<(std::ffi::OsString, std::ffi::OsString)> {
    let same = |k: &std::ffi::OsStr, name: &str| {
        if cfg!(windows) {
            k.eq_ignore_ascii_case(name)
        } else {
            k == name
        }
    };
    let mut out: Vec<_> = vars
        .filter(|(k, _)| !same(k, "ASC_NO_DAEMON") && !same(k, "ASC_HOME"))
        .collect();
    out.push(("ASC_HOME".into(), home.as_os_str().to_owned()));
    out
}

/// Windows: the daemon is created with handle inheritance off. `std::process::Command` always
/// inherits every inheritable handle, so a daemon started by a process that was itself handed
/// someone's pipe (the git shim under a test harness, a script reading `asc` output) would keep
/// that pipe open for its whole life and the reader would never see the end of it.
#[cfg(windows)]
#[allow(unsafe_code)] // one CreateProcessW call with buffers owned by this function
mod detached {
    use std::ffi::OsString;
    use std::io;
    use std::os::windows::ffi::OsStrExt;
    use std::path::Path;

    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{
        CREATE_NEW_PROCESS_GROUP, CREATE_NO_WINDOW, CREATE_UNICODE_ENVIRONMENT, CreateProcessW,
        DETACHED_PROCESS, PROCESS_INFORMATION, STARTUPINFOW,
    };

    /// The command line of `exe` and `args`: the program quoted, each argument quoted when it
    /// holds a space or tab. Arguments with quotes are refused (none of asc's do).
    pub(super) fn command_line(exe: &Path, args: &[&str]) -> io::Result<Vec<u16>> {
        let exe = exe.as_os_str();
        if exe.encode_wide().any(|c| c == u16::from(b'"')) || args.iter().any(|a| a.contains('"')) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "a quote in the daemon command line",
            ));
        }
        let mut line: Vec<u16> = Vec::new();
        line.push(u16::from(b'"'));
        line.extend(exe.encode_wide());
        line.push(u16::from(b'"'));
        for a in args {
            line.push(u16::from(b' '));
            let quote = a.is_empty() || a.contains([' ', '\t']);
            if quote {
                line.push(u16::from(b'"'));
            }
            line.extend(a.encode_utf16());
            if quote {
                line.push(u16::from(b'"'));
            }
        }
        line.push(0);
        Ok(line)
    }

    /// The `CREATE_UNICODE_ENVIRONMENT` block: `name=value\0` pairs, sorted by name ignoring
    /// case, then a final `\0`. A pair whose name is empty or holds `=` is dropped.
    pub(super) fn env_block(vars: &[(OsString, OsString)]) -> Vec<u16> {
        let mut pairs: Vec<(Vec<u16>, Vec<u16>)> = vars
            .iter()
            .filter(|(k, _)| {
                let k: Vec<u16> = k.encode_wide().collect();
                !k.is_empty() && !k.contains(&u16::from(b'=')) && !k.contains(&0)
            })
            .map(|(k, v)| {
                (
                    k.encode_wide().collect(),
                    v.encode_wide().filter(|&c| c != 0).collect(),
                )
            })
            .collect();
        let upper = |s: &[u16]| String::from_utf16_lossy(s).to_uppercase();
        pairs.sort_by_key(|(k, _)| upper(k));
        let mut block = Vec::new();
        for (k, v) in pairs {
            block.extend(k);
            block.push(u16::from(b'='));
            block.extend(v);
            block.push(0);
        }
        if block.is_empty() {
            block.push(0);
        }
        block.push(0);
        block
    }

    pub(super) fn spawn(exe: &Path, args: &[&str], env: &[(OsString, OsString)]) -> io::Result<()> {
        let app: Vec<u16> = exe.as_os_str().encode_wide().chain([0]).collect();
        let mut line = command_line(exe, args)?;
        let block = env_block(env);
        // SAFETY: zeroed is the documented initial state of both structs.
        let mut si: STARTUPINFOW = unsafe { std::mem::zeroed() };
        si.cb = u32::try_from(std::mem::size_of::<STARTUPINFOW>()).unwrap_or(u32::MAX);
        let mut pi: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };
        // SAFETY: every pointer is to a NUL-terminated buffer owned here and alive for the
        // call; inheritance is off, so the child gets no handle of this process.
        let ok = unsafe {
            CreateProcessW(
                app.as_ptr(),
                line.as_mut_ptr(),
                std::ptr::null(),
                std::ptr::null(),
                0,
                DETACHED_PROCESS
                    | CREATE_NEW_PROCESS_GROUP
                    | CREATE_NO_WINDOW
                    | CREATE_UNICODE_ENVIRONMENT,
                block.as_ptr().cast(),
                std::ptr::null(),
                &si,
                &mut pi,
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: both handles were just returned by CreateProcessW and are closed once; the
        // child is not waited for, it outlives this process.
        unsafe {
            CloseHandle(pi.hThread);
            CloseHandle(pi.hProcess);
        }
        Ok(())
    }
}

/// The real processes: this executable, run detached, and the operating system's kill.
pub struct SystemProcess {
    pub exe: PathBuf,
    /// `$ASC_HOME`, handed to the daemon so it never resolves a different one.
    pub home: PathBuf,
}

impl Process for SystemProcess {
    #[cfg(windows)]
    fn spawn(&self) -> io::Result<()> {
        let env = daemon_env(std::env::vars_os(), &self.home);
        detached::spawn(&self.exe, &[DAEMON_ENTRY, "run"], &env)
    }

    #[cfg(not(windows))]
    fn spawn(&self) -> io::Result<()> {
        use std::os::unix::process::CommandExt;
        use std::process::{Command, Stdio};
        let mut cmd = Command::new(&self.exe);
        cmd.args([DAEMON_ENTRY, "run"])
            .env_clear()
            .envs(daemon_env(std::env::vars_os(), &self.home))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0);
        // The child is not waited for: it outlives this process.
        cmd.spawn().map(drop)
    }

    fn kill(&self, pid: u32) -> io::Result<()> {
        use std::process::{Command, Stdio};
        if pid <= 1 || pid == std::process::id() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "refusing to kill that pid",
            ));
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
        let status = cmd
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()?;
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
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let path = cfg.log_dir.join(format!("daemon-hang-{now}-{pid}.txt"));
    let mut out = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)?;
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
    let Ok(entries) = std::fs::read_dir(&tasks) else {
        return;
    }; // lint:allow-worktree-fs /proc, not a work tree
    for entry in entries.flatten().take(512) {
        let read = |name: &str| {
            std::fs::read_to_string(entry.path().join(name))
                .map_or_else(|_| "?".into(), |s| s.trim().to_owned())
        };
        let _ = writeln!(
            out,
            "thread {} {} wchan={}",
            entry.file_name().to_string_lossy(),
            read("comm"),
            read("wchan")
        );
    }
}

/// Fails with [`LifecycleError::NotDaemon`] when the endpoint is held by a server that the OS
/// does not show to be this user's. Nothing is sent to such a server.
async fn refuse_foreign(cfg: &Config) -> Result<(), LifecycleError> {
    match connect_stream(&cfg.endpoint).await {
        Err(e) if e.kind() == io::ErrorKind::PermissionDenied => Err(LifecycleError::NotDaemon),
        _ => Ok(()),
    }
}

/// The one pid a kill may target, or `None` when the server is already gone.
///
/// The pid is what the OS says serves the endpoint (a user-owned process: see `asc_rpc::ipc`),
/// and it must equal the pid file in the private run directory and, if given, the pid the
/// server claims. Any mismatch is [`LifecycleError::NotDaemon`].
async fn kill_target(cfg: &Config, claimed: Option<u32>) -> Result<Option<u32>, LifecycleError> {
    let pid = match verified_server_pid(&cfg.endpoint).await {
        Ok(pid) => pid,
        Err(e) if e.kind() == io::ErrorKind::PermissionDenied => {
            return Err(LifecycleError::NotDaemon);
        }
        Err(_) => return Ok(None),
    };
    if read_pid(&cfg.run_dir) != Some(pid) || claimed.is_some_and(|c| c != pid) {
        return Err(LifecycleError::NotDaemon);
    }
    Ok(Some(pid))
}

/// Stops a daemon that answers `Ping`: `Shutdown`, then a kill if it stays.
async fn stop_up(cfg: &Config, proc: &dyn Process, info: &Info) -> Result<(), LifecycleError> {
    let mut client = WorkspaceClient::new(connect(&cfg.endpoint));
    // The RPC may fail because the daemon closes the connection as it goes; the wait decides.
    let _ = tokio::time::timeout(
        cfg.ping_timeout,
        client.shutdown(wire::ShutdownRequest::default()),
    )
    .await;
    if wait_down(cfg).await {
        return Ok(());
    }
    // `info.pid` came from whatever answered; it is only ever compared, never trusted.
    let Some(pid) = kill_target(cfg, Some(info.pid)).await? else {
        return Ok(());
    };
    proc.kill(pid)?;
    if wait_down(cfg).await {
        Ok(())
    } else {
        Err(LifecycleError::StopFailed(pid))
    }
}

/// Kills a daemon that does not answer, after writing the dump. The pid comes from the pid file
/// in the user's private `run/` directory and must be the one the OS says serves the endpoint.
async fn kill_hung(cfg: &Config, proc: &dyn Process) -> Result<(), LifecycleError> {
    read_pid(&cfg.run_dir).ok_or(LifecycleError::UnknownPid)?;
    let Some(pid) = kill_target(cfg, None).await? else {
        return Ok(());
    };
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
            refuse_foreign(cfg).await?;
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
        Probe::Down => refuse_foreign(cfg).await.map(|()| false),
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
        Probe::Down => refuse_foreign(cfg).await.map(|()| Stopped::NotRunning),
        Probe::Up(info) => stop_up(cfg, proc, &info).await.map(|()| Stopped::Stopped),
        Probe::Hung => kill_hung(cfg, proc).await.map(|()| Stopped::Stopped),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    use asc_rpc::ipc::{IpcListener, Workspace, WorkspaceServer, serve_incoming};
    use tokio::task::JoinHandle;
    use tonic::{Request, Response, Status};

    use super::*;
    use crate::daemon::{Never, RunConfig, not_served, run};

    static NEXT: AtomicUsize = AtomicUsize::new(0);

    #[test]
    fn only_the_hidden_entry_flag_forces_the_daemon() {
        let os = |v: &[&str]| v.iter().map(std::ffi::OsString::from).collect::<Vec<_>>();
        assert_eq!(daemon_entry(&os(&["git", "status"])), None);
        assert_eq!(daemon_entry(&os(&["git"])), None);
        assert_eq!(daemon_entry(&os(&["git", "run", DAEMON_ENTRY])), None);
        assert_eq!(
            daemon_entry(&os(&["git.exe", DAEMON_ENTRY, "run"])),
            Some(os(&["git.exe", "daemon", "run"]))
        );
    }

    #[test]
    fn the_daemon_env_sets_the_home_and_drops_the_off_switch() {
        let vars = [
            ("PATH", "/bin"),
            ("ASC_NO_DAEMON", "1"),
            ("ASC_HOME", "/old"),
        ]
        .into_iter()
        .map(|(k, v)| (k.into(), v.into()));
        let got = daemon_env(vars, Path::new("/new"));
        let want: Vec<(std::ffi::OsString, std::ffi::OsString)> = vec![
            ("PATH".into(), "/bin".into()),
            ("ASC_HOME".into(), "/new".into()),
        ];
        assert_eq!(got, want);
    }

    #[cfg(windows)]
    #[test]
    fn the_detached_command_line_and_env_block_are_well_formed() {
        let wide = |s: &str| s.encode_utf16().collect::<Vec<u16>>();
        let exe = "C:/a b/git.exe";
        let line = detached::command_line(Path::new(exe), &[DAEMON_ENTRY, "run"]).unwrap();
        let mut want = wide(&format!("\"{exe}\" --asc-daemon-entry run"));
        want.push(0);
        assert_eq!(line, want);
        assert!(detached::command_line(Path::new("x.exe"), &["a\"b"]).is_err());

        let vars: Vec<(std::ffi::OsString, std::ffi::OsString)> = vec![
            ("b".into(), "2".into()),
            ("A".into(), "1".into()),
            ("=C:".into(), "skip".into()),
        ];
        let mut want = wide("A=1");
        want.push(0);
        want.extend(wide("b=2"));
        want.extend([0, 0]);
        assert_eq!(detached::env_block(&vars), want);
        assert_eq!(detached::env_block(&[]), vec![0, 0]);
    }

    #[test]
    fn homes_get_their_own_key() {
        let d = tempfile::tempdir().unwrap();
        let (a, b) = (d.path().join("a"), d.path().join("b"));
        assert_ne!(home_key(&a), home_key(&b));
        assert_eq!(home_key(&a), home_key(&a));
        assert_eq!(home_key(&a).len(), 12);
        if cfg!(windows) {
            let upper = std::path::PathBuf::from(a.to_string_lossy().to_uppercase());
            assert!(same_home(&a, &upper));
            assert_eq!(home_key(&a), home_key(&upper));
        }
        assert!(!same_home(&a, &b));
    }

    fn config(dir: &Path, version: &str) -> Config {
        let endpoint = if cfg!(windows) {
            IpcEndpoint::from_raw(format!(
                "asc-u53a-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ))
        } else {
            IpcEndpoint::from_raw(
                dir.join("sock")
                    .join("asc.sock")
                    .to_string_lossy()
                    .into_owned(),
            )
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
        assert_eq!(
            read_pid(&cfg.run_dir),
            None,
            "a clean stop removes the pid file"
        );

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
        assert!(
            matches!(probe(&cfg).await, Probe::Up(_)),
            "the first daemon is unharmed"
        );
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
        assert!(
            pid_file(&cfg.run_dir).exists(),
            "a crash leaves a stale pid file"
        );

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
        assert!(
            proc.killed.lock().unwrap().is_empty(),
            "a daemon that shuts down cleanly is not killed"
        );
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
        let me = std::process::id();
        write_pid_file(&cfg, me);
        assert_eq!(probe(&cfg).await, Probe::Hung);

        let info = ensure_running(&cfg, &*proc).await.unwrap();
        assert_eq!(info.version, "1.0.0");
        assert_eq!(
            *proc.killed.lock().unwrap(),
            [me],
            "the pid file names the process to kill"
        );
        let dumps: Vec<_> = std::fs::read_dir(&cfg.log_dir).unwrap().flatten().collect();
        assert_eq!(dumps.len(), 1, "one dump per hang");
        let text = std::fs::read_to_string(dumps[0].path()).unwrap();
        assert!(text.contains(&format!("pid: {me}")), "{text}");
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

    // ------------------------------------------------------------------ a squatter on the endpoint

    /// A server that took the endpoint and lies in `Ping`: its version differs, so asc wants to
    /// replace it, and it names `claimed_pid` as its own. It ignores `Shutdown`.
    struct Fake {
        version: String,
        claimed_pid: u32,
    }

    #[tonic::async_trait]
    impl Workspace for Fake {
        async fn status(
            &self,
            _: Request<wire::WsStatusRequest>,
        ) -> Result<Response<wire::WsStatusResponse>, Status> {
            not_served()
        }

        type DiffStream = Never<wire::DiffResponse>;
        async fn diff(
            &self,
            _: Request<wire::WsDiffRequest>,
        ) -> Result<Response<Self::DiffStream>, Status> {
            not_served()
        }

        type LogStream = Never<wire::LogResponse>;
        async fn log(
            &self,
            _: Request<wire::WsLogRequest>,
        ) -> Result<Response<Self::LogStream>, Status> {
            not_served()
        }

        type BlameStream = Never<wire::BlameResponse>;
        async fn blame(
            &self,
            _: Request<wire::WsBlameRequest>,
        ) -> Result<Response<Self::BlameStream>, Status> {
            not_served()
        }

        async fn affected(
            &self,
            _: Request<wire::WsAffectedRequest>,
        ) -> Result<Response<wire::WsAffectedResponse>, Status> {
            not_served()
        }

        async fn inputs(
            &self,
            _: Request<wire::WsInputsRequest>,
        ) -> Result<Response<wire::WsInputsResponse>, Status> {
            not_served()
        }

        type WatchStream = Never<wire::WatchEvent>;
        async fn watch(
            &self,
            _: Request<wire::WatchRequest>,
        ) -> Result<Response<Self::WatchStream>, Status> {
            not_served()
        }

        async fn stage(
            &self,
            _: Request<wire::WsStageRequest>,
        ) -> Result<Response<wire::WsStageResponse>, Status> {
            not_served()
        }

        type SearchStream = Never<wire::SearchResponse>;
        async fn search(
            &self,
            _: Request<wire::WsSearchRequest>,
        ) -> Result<Response<Self::SearchStream>, Status> {
            not_served()
        }

        async fn symbols(
            &self,
            _: Request<wire::WsSymbolsRequest>,
        ) -> Result<Response<wire::SymbolsResponse>, Status> {
            not_served()
        }

        async fn semantic(
            &self,
            _: Request<wire::WsSemanticRequest>,
        ) -> Result<Response<wire::SemanticResponse>, Status> {
            not_served()
        }

        async fn index_status(
            &self,
            _: Request<wire::WsIndexStatusRequest>,
        ) -> Result<Response<wire::IndexStatusResponse>, Status> {
            not_served()
        }

        async fn stacks(
            &self,
            _: Request<wire::WsStacksRequest>,
        ) -> Result<Response<wire::WsStacksResponse>, Status> {
            not_served()
        }

        async fn worktrees(
            &self,
            _: Request<wire::WsWorktreesRequest>,
        ) -> Result<Response<wire::WsWorktreesResponse>, Status> {
            not_served()
        }

        async fn conflicts(
            &self,
            _: Request<wire::WsConflictsRequest>,
        ) -> Result<Response<wire::WsConflictsResponse>, Status> {
            not_served()
        }

        type CommitGraphStream = Never<wire::CommitGraphResponse>;
        async fn commit_graph(
            &self,
            _: Request<wire::WsCommitGraphRequest>,
        ) -> Result<Response<Self::CommitGraphStream>, Status> {
            not_served()
        }

        type ReadFileStream = Never<wire::ReadFileResponse>;
        async fn read_file(
            &self,
            _: Request<wire::WsReadFileRequest>,
        ) -> Result<Response<Self::ReadFileStream>, Status> {
            not_served()
        }

        async fn ping(
            &self,
            _: Request<wire::PingRequest>,
        ) -> Result<Response<wire::PingResponse>, Status> {
            Ok(Response::new(wire::PingResponse {
                version: self.version.clone(),
                pid: self.claimed_pid,
                uptime_ms: 1,
            }))
        }

        async fn mount(
            &self,
            _: Request<wire::MountRequest>,
        ) -> Result<Response<wire::MountResponse>, Status> {
            not_served()
        }

        async fn unmount(
            &self,
            _: Request<wire::UnmountRequest>,
        ) -> Result<Response<wire::UnmountResponse>, Status> {
            not_served()
        }

        async fn switch(
            &self,
            _: Request<wire::SwitchRequest>,
        ) -> Result<Response<wire::SwitchResponse>, Status> {
            not_served()
        }

        async fn retarget(
            &self,
            _: Request<wire::RetargetRequest>,
        ) -> Result<Response<wire::RetargetResponse>, Status> {
            not_served()
        }

        async fn journal(
            &self,
            _: Request<wire::JournalRequest>,
        ) -> Result<Response<wire::JournalResponse>, Status> {
            not_served()
        }

        async fn clear_journal(
            &self,
            _: Request<wire::ClearJournalRequest>,
        ) -> Result<Response<wire::ClearJournalResponse>, Status> {
            not_served()
        }

        async fn dehydrate(
            &self,
            _: Request<wire::DehydrateRequest>,
        ) -> Result<Response<wire::DehydrateResponse>, Status> {
            not_served()
        }

        async fn prefetch(
            &self,
            _: Request<wire::PrefetchRequest>,
        ) -> Result<Response<wire::PrefetchResponse>, Status> {
            not_served()
        }

        async fn stats(
            &self,
            _: Request<wire::StatsRequest>,
        ) -> Result<Response<wire::StatsResponse>, Status> {
            not_served()
        }

        async fn shutdown(
            &self,
            _: Request<wire::ShutdownRequest>,
        ) -> Result<Response<wire::ShutdownResponse>, Status> {
            // Ignored: the squatter stays, so the caller is tempted to kill it.
            Ok(Response::new(wire::ShutdownResponse::default()))
        }
    }

    fn serve_fake(cfg: &Config, claimed_pid: u32) -> JoinHandle<()> {
        let incoming = serve_incoming(IpcListener::bind(&cfg.endpoint).unwrap());
        let fake = Fake {
            version: "0.0.1-squatter".into(),
            claimed_pid,
        };
        tokio::spawn(async move {
            let _ = tonic::transport::Server::builder()
                .add_service(WorkspaceServer::new(fake))
                .serve_with_incoming(incoming)
                .await;
        })
    }

    /// An innocent process owned by this user, that must survive.
    struct Victim(std::process::Child);

    impl Victim {
        fn start() -> Self {
            use std::process::{Command, Stdio};
            let mut cmd = if cfg!(windows) {
                let mut c = Command::new("ping");
                c.args(["-n", "120", "127.0.0.1"]);
                c
            } else {
                let mut c = Command::new("sleep");
                c.arg("120");
                c
            };
            Self(
                cmd.stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .spawn()
                    .unwrap(),
            )
        }

        fn pid(&self) -> u32 {
            self.0.id()
        }

        fn alive(&mut self) -> bool {
            self.0.try_wait().unwrap().is_none()
        }
    }

    impl Drop for Victim {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    /// Records every kill and, when `really`, carries it out with the operating system's kill.
    struct Recording {
        really: bool,
        killed: Mutex<Vec<u32>>,
    }

    impl Recording {
        fn new(really: bool) -> Self {
            Self {
                really,
                killed: Mutex::default(),
            }
        }
    }

    impl Process for Recording {
        fn spawn(&self) -> io::Result<()> {
            Err(io::Error::other("the test does not start daemons"))
        }

        fn kill(&self, pid: u32) -> io::Result<()> {
            self.killed.lock().unwrap().push(pid);
            if self.really {
                let sys = SystemProcess {
                    exe: PathBuf::from("asc"),
                    home: PathBuf::new(),
                };
                return sys.kill(pid);
            }
            Ok(())
        }
    }

    fn write_pid_file(cfg: &Config, pid: u32) {
        std::fs::create_dir_all(&cfg.run_dir).unwrap();
        std::fs::write(pid_file(&cfg.run_dir), format!("{pid}\n")).unwrap();
    }

    #[tokio::test]
    async fn a_squatter_cannot_make_asc_kill_another_process() {
        let me = std::process::id();
        // (pid the squatter claims, pid in the pid file): the OS says the server is this process,
        // so each of these names a process that is not the server.
        for (claimed, in_file) in [(None, None), (None, Some(me)), (Some(me), None)] {
            let mut victim = Victim::start();
            let dir = tempfile::tempdir().unwrap();
            let cfg = Config {
                stop_wait: Duration::from_millis(300),
                ..config(dir.path(), "1.0.0")
            };
            let squatter = serve_fake(&cfg, claimed.unwrap_or_else(|| victim.pid()));
            write_pid_file(&cfg, in_file.unwrap_or_else(|| victim.pid()));
            let proc = Recording::new(true);

            let probed = probe(&cfg).await;
            assert!(
                matches!(&probed, Probe::Up(i) if i.version != "1.0.0"),
                "{probed:?}"
            );
            let err = ensure_running(&cfg, &proc).await.unwrap_err();
            assert!(matches!(err, LifecycleError::NotDaemon), "{err}");
            let err = stop(&cfg, &proc).await.unwrap_err();
            assert!(matches!(err, LifecycleError::NotDaemon), "{err}");
            let err = stop_skewed(&cfg, &proc).await.unwrap_err();
            assert!(matches!(err, LifecycleError::NotDaemon), "{err}");

            assert!(proc.killed.lock().unwrap().is_empty(), "nothing is killed");
            assert!(
                victim.alive(),
                "the victim survives (claimed {claimed:?}, file {in_file:?})"
            );
            squatter.abort();
        }
    }

    #[tokio::test]
    async fn a_hung_endpoint_whose_pid_file_names_another_process_kills_nothing() {
        let mut victim = Victim::start();
        let dir = tempfile::tempdir().unwrap();
        let cfg = Config {
            stop_wait: Duration::from_millis(300),
            ..config(dir.path(), "1.0.0")
        };
        let proc = InProcess::new(&cfg);
        proc.adopt(hung_daemon(&cfg));
        write_pid_file(&cfg, victim.pid());
        let killer = Recording::new(true);

        let err = ensure_running(&cfg, &killer).await.unwrap_err();
        assert!(matches!(err, LifecycleError::NotDaemon), "{err}");
        assert!(killer.killed.lock().unwrap().is_empty());
        assert!(victim.alive());
        assert!(
            pid_file(&cfg.run_dir).exists(),
            "the pid file is left as found"
        );
        assert!(
            !cfg.log_dir.exists(),
            "no dump is written for a process that is not the daemon"
        );
        proc.crash();
    }

    #[tokio::test]
    async fn a_daemon_that_ignores_shutdown_is_killed_when_every_pid_agrees() {
        let me = std::process::id();
        let dir = tempfile::tempdir().unwrap();
        let cfg = Config {
            stop_wait: Duration::from_millis(300),
            ..config(dir.path(), "1.0.0")
        };
        let squatter = serve_fake(&cfg, me);
        write_pid_file(&cfg, me);
        let proc = Recording::new(false);

        // The OS-verified pid, the pid file and the claimed pid are all this process.
        let err = ensure_running(&cfg, &proc).await.unwrap_err();
        assert!(
            matches!(err, LifecycleError::StopFailed(p) if p == me),
            "{err}"
        );
        assert_eq!(*proc.killed.lock().unwrap(), [me]);
        squatter.abort();
    }

    #[test]
    fn no_daemon_switch() {
        let env = |v: &'static str| {
            move |k: &str| (k == "ASC_NO_DAEMON").then(|| std::ffi::OsString::from(v))
        };
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
