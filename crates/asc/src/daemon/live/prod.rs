//! The production side of the live driver (SPEC §21.3.8; B30): what the daemon starts.
//!
//! [`Supervisor`] watches `state.db` and keeps one [`drive`](super::drive) task per asc server
//! repository whose `asc.live.*` settings leave live updates on. Each driver gets
//!
//! * a [`ProdFeed`]: the repository's RPC client (`WatchRefs` and `ListRefs`), connected the way
//!   a command connects but never prompting, and dropped after a failure or [`KEEP`] so a changed
//!   credential is picked up;
//! * a [`ProdRunner`]: one `asc live run` child process per unpaused worktree and window (see
//!   [`pass`](super::pass) for what a pass does and why it is a child);
//! * the cloud wake: the sync task of the daemon says when the user's own drafts changed on
//!   another device ([`cloud_wake`](crate::daemon::cloud::cloud_wake)), which is the daemon's
//!   `Watch KIND_CLOUD`.
//!
//! Everything is bounded: [`MAX_REPOS`] drivers, [`MAX_WORKTREES`](super::MAX_WORKTREES) runs
//! per window, one child at a time per driver, [`RUN_LIMIT`] for a child and [`MAX_OUT`] bytes
//! of its output. A failing repository, feed or child is logged and retried; it never stops the
//! daemon or another repository.

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use asc_rpc::client::RpcClient;
use asc_state::state::repos::{self, RemoteKind};
use asc_state::state::workspaces;
use asc_state::{AscHome, StateDb};
use asc_store::refs::RepoKey;
use tokio::io::{AsyncRead, AsyncReadExt as _};
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;

use super::{EventStream, Feed, Mode, RpcFeed, Runner, Timing, TokioClock, drive};
use crate::credential::Prompt;
use crate::daemon::cloud::{cloud_wake, prod as cloud_prod};
use crate::daemon::lifecycle;
use crate::daemon::metrics::EventLog;
use crate::remote::server::{self, ServerRemote};

/// How often the supervisor looks for repositories that appeared, went or changed settings.
const SCAN_EVERY: Duration = Duration::from_secs(60);
/// How long an opened client (and the settings read with it) is kept.
const KEEP: Duration = Duration::from_secs(300);
/// Repositories driven at once.
const MAX_REPOS: usize = 64;
/// Workspace directories of one repository tried when looking for one that really holds it.
const MAX_CANDIDATES: usize = 16;
/// How long a stopping driver may take to end before it is cut off.
const STOP_WAIT: Duration = Duration::from_secs(5);
/// The longest one `asc live run` may take.
const RUN_LIMIT: Duration = Duration::from_secs(15 * 60);
/// The most output kept from one child; the rest is read and dropped.
const MAX_OUT: usize = 16 * 1024;
/// The shortest `asc.live.debounce` and `asc.live.pollInterval` accepted, and the longest.
const DEBOUNCE_RANGE: (Duration, Duration) = (Duration::ZERO, Duration::from_secs(3600));
const POLL_RANGE: (Duration, Duration) = (Duration::from_secs(10), Duration::from_secs(24 * 3600));

fn note(text: &str) -> String {
    text.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .take(256)
        .collect()
}

// ------------------------------------------------------------------------------ settings

/// A duration as `asc.live.*` writes it: digits and a unit of `ms`, `s`, `m` or `h` (a bare
/// number is seconds). `None` for anything else, including an overflow.
pub fn parse_duration(text: &str) -> Option<Duration> {
    let t = text.trim();
    let split = t.find(|c: char| !c.is_ascii_digit()).unwrap_or(t.len());
    let (digits, unit) = t.split_at(split);
    let n: u64 = digits.parse().ok()?;
    match unit.trim() {
        "ms" => Some(Duration::from_millis(n)),
        "" | "s" => Some(Duration::from_secs(n)),
        "m" => n.checked_mul(60).map(Duration::from_secs),
        "h" => n.checked_mul(3600).map(Duration::from_secs),
        _ => None,
    }
}

fn clamp(d: Duration, (lo, hi): (Duration, Duration)) -> Duration {
    d.max(lo).min(hi)
}

/// `asc.live.debounce` and `asc.live.pollInterval` (SPEC §21.6.1), each held to a sane range; a
/// value that does not parse is the default.
pub fn timing_of(get: &dyn Fn(&str) -> Option<String>) -> Timing {
    let base = Timing::default();
    Timing {
        debounce: get("asc.live.debounce")
            .and_then(|v| parse_duration(&v))
            .map_or(base.debounce, |d| clamp(d, DEBOUNCE_RANGE)),
        poll_interval: get("asc.live.pollInterval")
            .and_then(|v| parse_duration(&v))
            .map_or(base.poll_interval, |d| clamp(d, POLL_RANGE)),
    }
}

// ------------------------------------------------------------------------------ the feed

/// What opening a repository told us.
#[derive(Clone)]
struct Opened {
    /// `None` when `asc.live.*` leaves live updates off for the repository.
    client: Option<Arc<RpcClient>>,
    timing: Timing,
}

type Cached = (Instant, Result<Opened, String>);

/// The [`Feed`] of one repository, over a client opened on demand.
pub struct ProdFeed {
    home: AscHome,
    repo: RepoKey,
    url: String,
    paths: Mutex<Vec<PathBuf>>,
    cached: Mutex<Option<Cached>>,
}

impl ProdFeed {
    fn new(home: AscHome, repo: RepoKey, url: String, paths: Vec<PathBuf>) -> Self {
        Self {
            home,
            repo,
            url,
            paths: Mutex::new(paths),
            cached: Mutex::new(None),
        }
    }

    fn set_paths(&self, paths: Vec<PathBuf>) {
        *self.paths.lock().unwrap_or_else(|e| e.into_inner()) = paths;
    }

    /// Forgets the opened client, so the next call connects afresh.
    fn forget(&self) {
        *self.cached.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }

    /// Opens a context on the first workspace directory that really holds the repository, reads
    /// the settings and connects. Contexts are not `Send`, so this runs on a blocking thread.
    async fn opened(&self) -> Result<Opened, String> {
        let hit = self
            .cached
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
            .filter(|(at, _)| at.elapsed() < KEEP);
        if let Some((_, answer)) = hit {
            return answer;
        }
        let paths = self.paths.lock().unwrap_or_else(|e| e.into_inner()).clone();
        let (home, repo, url) = (self.home.clone(), self.repo, self.url.clone());
        let handle = tokio::runtime::Handle::current();
        let answer =
            tokio::task::spawn_blocking(move || handle.block_on(connect(home, paths, repo, url)))
                .await
                .map_err(|e| note(&e.to_string()))
                .and_then(|r| r);
        *self.cached.lock().unwrap_or_else(|e| e.into_inner()) =
            Some((Instant::now(), answer.clone()));
        answer
    }
}

/// The first of `paths` that holds `repo` with remote `url`, connected without a prompt.
async fn connect(
    home: AscHome,
    paths: Vec<PathBuf>,
    repo: RepoKey,
    url: String,
) -> Result<Opened, String> {
    let mut last = "no workspace directory holds this repository".to_owned();
    for path in paths.into_iter().take(MAX_CANDIDATES) {
        let ctx = match cloud_prod::open_ctx(&home, path).await {
            Ok(ctx) => ctx,
            Err(e) => {
                last = e;
                continue;
            }
        };
        let Ok(ws) = ctx.ws() else { continue };
        if !cloud_prod::binds(
            ws.repo.key,
            ws.repo.repo.remote_url.as_deref(),
            ws.repo.repo.remote_kind,
            repo,
            &url,
        ) {
            continue;
        }
        let cfg = super::pass::config_of(&ctx);
        let timing = timing_of(&|k| ctx.config.get(k));
        if !cfg.fetch {
            // Not an error: live updates are off here; the supervisor looks again later.
            return Ok(Opened {
                client: None,
                timing,
            });
        }
        let real = ctx.real_git().ok().cloned();
        let connected = server::connect(ctx.config.file(), &ws.db, real, None, &url, Prompt::Never)
            .await
            .map_err(|e| note(&format!("{e:?}")))?
            .ok_or_else(|| "the remote is not an asc server".to_owned())?;
        let remote = ServerRemote::new("live", connected);
        return Ok(Opened {
            client: Some(Arc::new(remote.client())),
            timing,
        });
    }
    Err(note(&last))
}

#[async_trait]
impl Feed for ProdFeed {
    async fn open(&self, resume: Option<u64>) -> Result<Box<dyn EventStream>, String> {
        let client = self
            .opened()
            .await?
            .client
            .ok_or_else(|| "live updates are off".to_owned())?;
        let out = RpcFeed::new(client).open(resume).await;
        if out.is_err() {
            self.forget();
        }
        out
    }

    async fn poll(&self) -> Result<Vec<(String, u64)>, String> {
        let client = self
            .opened()
            .await?
            .client
            .ok_or_else(|| "live updates are off".to_owned())?;
        let out = RpcFeed::new(client).poll().await;
        if out.is_err() {
            self.forget();
        }
        out
    }
}

// ------------------------------------------------------------------------------ the runner

/// The [`Runner`] of one repository: `asc live run` in each worktree. A worktree is named by its
/// workspace id, which is also the key of the pause file of `asc live pause`.
pub struct ProdRunner {
    home: AscHome,
    exe: PathBuf,
    repo: RepoKey,
    events: Arc<EventLog>,
    db: tokio::sync::OnceCell<StateDb>,
}

impl ProdRunner {
    fn new(home: AscHome, exe: PathBuf, repo: RepoKey, events: Arc<EventLog>) -> Self {
        Self {
            home,
            exe,
            repo,
            events,
            db: tokio::sync::OnceCell::new(),
        }
    }

    async fn db(&self) -> Result<&StateDb, String> {
        self.db
            .get_or_try_init(|| async { StateDb::open(&self.home).await })
            .await
            .map_err(|e| note(&e.to_string()))
    }

    /// The command: this very executable, in the worktree, with no terminal, no daemon and the
    /// hooks directory of the live pass (SPEC §21.3.8: no hook runs on a live update).
    fn command(&self, path: &str) -> Result<tokio::process::Command, String> {
        let hookless =
            crate::cli::live::ensure_hookless_dir(&self.home).map_err(|e| note(&e.to_string()))?;
        let inherited = std::env::var("GIT_CONFIG_COUNT")
            .ok()
            .and_then(|v| v.trim().parse::<usize>().ok())
            .unwrap_or(0);
        let mut cmd = tokio::process::Command::new(&self.exe);
        cmd.args(["live", "run"])
            .current_dir(path)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .env("ASC_NO_DAEMON", "1")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_CONFIG_COUNT", (inherited + 1).to_string())
            .env(format!("GIT_CONFIG_KEY_{inherited}"), "core.hooksPath")
            .env(format!("GIT_CONFIG_VALUE_{inherited}"), hookless);
        // Discovery starts from the directory, whatever the daemon was started with.
        for var in [
            "GIT_DIR",
            "GIT_WORK_TREE",
            "GIT_COMMON_DIR",
            "GIT_INDEX_FILE",
            "GIT_ASKPASS",
            "SSH_ASKPASS",
        ] {
            cmd.env_remove(var);
        }
        Ok(cmd)
    }

    fn failed(&self, id: &str, path: &str, why: &str) {
        let _ = crate::cli::live::record_run(&self.home, id, path, self.repo.0, "failed");
        self.events
            .emit("warn", "live.run", &[("ws", id), ("error", &note(why))]);
    }
}

/// Reads `r` to its end, keeping the first [`MAX_OUT`] bytes as text.
async fn drain(r: Option<impl AsyncRead + Unpin>) -> String {
    let Some(mut r) = r else {
        return String::new();
    };
    let mut kept = Vec::new();
    let mut buf = [0u8; 4096];
    while let Ok(n) = r.read(&mut buf).await {
        if n == 0 {
            break;
        }
        let room = MAX_OUT.saturating_sub(kept.len());
        kept.extend_from_slice(&buf[..n.min(room)]);
    }
    String::from_utf8_lossy(&kept).into_owned()
}

#[async_trait]
impl Runner for ProdRunner {
    async fn worktrees(&self) -> Vec<String> {
        let Ok(db) = self.db().await else {
            return Vec::new();
        };
        workspaces::list(db, Some(self.repo))
            .await
            .map(|rows| {
                rows.into_iter()
                    .filter(|w| std::path::Path::new(&w.ws.path).is_dir())
                    .map(|w| w.ws.id.to_string())
                    .collect()
            })
            .unwrap_or_default()
    }

    async fn paused(&self, worktree: &str) -> bool {
        crate::cli::live::paused(&self.home, worktree)
    }

    async fn run(&self, worktree: &str) -> Result<(), String> {
        let db = self.db().await?;
        let id = worktree
            .parse()
            .map_err(|_| "not a workspace id".to_owned())?;
        let row = workspaces::get(db, id)
            .await
            .map_err(|e| note(&e.to_string()))?
            .ok_or_else(|| "the workspace is gone".to_owned())?;
        let path = row.ws.path.clone();
        let mut child = match self
            .command(&path)
            .and_then(|mut c| c.spawn().map_err(|e| note(&e.to_string())))
        {
            Ok(child) => child,
            Err(e) => {
                self.failed(worktree, &path, &e);
                return Err(e);
            }
        };
        let (out, err) = (child.stdout.take(), child.stderr.take());
        let finished = tokio::time::timeout(RUN_LIMIT, async {
            let (status, out, err) = tokio::join!(child.wait(), drain(out), drain(err));
            (status, out, err)
        })
        .await;
        match finished {
            Ok((Ok(status), out, _)) if status.success() => {
                self.events.emit(
                    "info",
                    "live.run",
                    &[("ws", worktree), ("result", &note(out.trim()))],
                );
                Ok(())
            }
            Ok((Ok(status), _, err)) => {
                let why = format!(
                    "exit {:?}: {}",
                    status.code(),
                    err.lines().next().unwrap_or("")
                );
                self.failed(worktree, &path, &why);
                Err(note(&why))
            }
            Ok((Err(e), _, _)) => {
                self.failed(worktree, &path, &e.to_string());
                Err(note(&e.to_string()))
            }
            Err(_) => {
                // `kill_on_drop` ends the child with the future that owned it.
                self.failed(worktree, &path, "the run took too long");
                Err("the run took too long".into())
            }
        }
    }
}

// ------------------------------------------------------------------------------ the supervisor

/// One running driver.
struct Driver {
    url: String,
    stop: watch::Sender<bool>,
    join: JoinHandle<()>,
    forward: JoinHandle<()>,
    feed: Arc<ProdFeed>,
}

/// Stops drivers together. A driver sees the stop between windows; one that is inside a long
/// run is cut off after [`STOP_WAIT`] (its child dies with it), so the daemon never waits for a
/// fetch to finish.
async fn stop_all(drivers: Vec<Driver>) {
    let mut joins = Vec::with_capacity(drivers.len());
    for d in drivers {
        let _ = d.stop.send(true);
        d.forward.abort();
        joins.push(d.join);
    }
    let deadline = tokio::time::Instant::now() + STOP_WAIT;
    for mut join in joins {
        if tokio::time::timeout_at(deadline, &mut join).await.is_err() {
            join.abort();
            let _ = join.await;
        }
    }
}

/// Keeps one driver per live-enabled asc server repository for as long as the daemon runs.
pub struct Supervisor {
    stop: watch::Sender<bool>,
    join: JoinHandle<()>,
}

impl Supervisor {
    /// Starts the supervisor. `None` when the daemon is switched off for this process
    /// (`ASC_NO_DAEMON`), so a test or CI run never opens a stream.
    pub fn start(home: AscHome, asc_exe: PathBuf, events: Arc<EventLog>) -> Option<Self> {
        if lifecycle::disabled(&|k| std::env::var_os(k)) {
            return None;
        }
        let (stop, rx) = watch::channel(false);
        let join = tokio::spawn(supervise(home, asc_exe, events, rx));
        Some(Self { stop, join })
    }

    /// Stops every driver and waits for them.
    pub async fn stop(self) {
        let _ = self.stop.send(true);
        let _ = self.join.await;
    }
}

/// The asc server repositories of `state.db` with the directories of their workspaces.
async fn server_repos(db: &StateDb) -> Result<Vec<(RepoKey, String, Vec<PathBuf>)>, String> {
    let all = workspaces::list(db, None)
        .await
        .map_err(|e| note(&e.to_string()))?;
    let mut order = Vec::new();
    let mut seen = BTreeSet::new();
    let mut dirs: HashMap<i64, Vec<PathBuf>> = HashMap::new();
    for w in all {
        if seen.insert(w.ws.repo.0) {
            order.push(w.ws.repo);
        }
        let path = PathBuf::from(&w.ws.path);
        if path.is_dir() {
            dirs.entry(w.ws.repo.0).or_default().push(path);
        }
    }
    let mut out = Vec::new();
    for repo in order.into_iter().take(MAX_REPOS) {
        let Some(row) = repos::get(db, repo)
            .await
            .map_err(|e| note(&e.to_string()))?
        else {
            continue;
        };
        let Some(url) = row
            .repo
            .remote_url
            .clone()
            .filter(|_| row.repo.remote_kind == RemoteKind::Server)
        else {
            continue;
        };
        out.push((repo, url, dirs.remove(&repo.0).unwrap_or_default()));
    }
    Ok(out)
}

async fn supervise(
    home: AscHome,
    exe: PathBuf,
    events: Arc<EventLog>,
    mut stop: watch::Receiver<bool>,
) {
    let mut drivers: HashMap<i64, Driver> = HashMap::new();
    let mut db: Option<StateDb> = None;
    loop {
        if db.is_none() {
            match StateDb::open(&home).await {
                Ok(d) => db = Some(d),
                Err(e) => events.emit("error", "live.scan", &[("error", &note(&e.to_string()))]),
            }
        }
        if let Some(db) = &db {
            match server_repos(db).await {
                Ok(found) => reconcile(&home, &exe, &events, &mut drivers, found).await,
                Err(e) => events.emit("error", "live.scan", &[("error", &e)]),
            }
        }
        tokio::select! {
            r = stop.changed() => {
                if r.is_err() || *stop.borrow() {
                    break;
                }
            }
            () = tokio::time::sleep(SCAN_EVERY) => {}
        }
    }
    stop_all(drivers.drain().map(|(_, d)| d).collect()).await;
}

/// Starts the drivers that are missing and enabled; stops those whose repository is gone,
/// moved to another server, or turned live updates off.
async fn reconcile(
    home: &AscHome,
    exe: &Path,
    events: &Arc<EventLog>,
    drivers: &mut HashMap<i64, Driver>,
    found: Vec<(RepoKey, String, Vec<PathBuf>)>,
) {
    let live: BTreeSet<i64> = found.iter().map(|(k, _, _)| k.0).collect();
    let gone: Vec<i64> = drivers
        .keys()
        .copied()
        .filter(|k| !live.contains(k))
        .collect();
    stop_all(
        gone.into_iter()
            .filter_map(|k| drivers.remove(&k))
            .collect(),
    )
    .await;
    for (repo, url, paths) in found {
        if drivers.get(&repo.0).is_some_and(|d| d.url != url)
            && let Some(d) = drivers.remove(&repo.0)
        {
            stop_all(vec![d]).await;
        }
        if let Some(d) = drivers.get(&repo.0) {
            d.feed.set_paths(paths);
            // The settings are read again with the client; a driver whose feed now says "off"
            // (or cannot be opened for good) is stopped and started again when that changes.
            if matches!(d.feed.opened().await, Ok(o) if o.client.is_none())
                && let Some(d) = drivers.remove(&repo.0)
            {
                stop_all(vec![d]).await;
            }
            continue;
        }
        let feed = Arc::new(ProdFeed::new(home.clone(), repo, url.clone(), paths));
        let timing = match feed.opened().await {
            Ok(o) if o.client.is_some() => o.timing,
            Ok(_) => continue,
            Err(e) => {
                events.emit(
                    "warn",
                    "live.open",
                    &[("repo", &repo.0.to_string()), ("error", &e)],
                );
                continue;
            }
        };
        let runner = Arc::new(ProdRunner::new(
            home.clone(),
            exe.to_path_buf(),
            repo,
            events.clone(),
        ));
        let (stop, stop_rx) = watch::channel(false);
        let (mode, _) = watch::channel(Mode::Starting);
        let (wake_tx, wake_rx) = mpsc::unbounded_channel();
        let forward = tokio::spawn(forward_wakes(repo, wake_tx));
        let join = tokio::spawn(drive(
            feed.clone(),
            runner,
            Arc::new(TokioClock::new()),
            timing,
            stop_rx,
            mode,
            Some(wake_rx),
        ));
        drivers.insert(
            repo.0,
            Driver {
                url,
                stop,
                join,
                forward,
                feed,
            },
        );
    }
}

/// Turns the daemon-wide cloud wake into unit messages for the driver of `repo`. A lagging
/// receiver lost some keys, so it wakes the driver once: a spare window costs one fetch.
async fn forward_wakes(repo: RepoKey, tx: mpsc::UnboundedSender<()>) {
    use tokio::sync::broadcast::error::RecvError;
    let mut rx = cloud_wake().subscribe();
    loop {
        let wake = tokio::select! {
            () = tx.closed() => return,
            r = rx.recv() => match r {
                Ok(key) => key == repo,
                Err(RecvError::Lagged(_)) => true,
                Err(RecvError::Closed) => return,
            },
        };
        if wake && tx.send(()).is_err() {
            return;
        }
    }
}

#[cfg(test)]
mod tests;
