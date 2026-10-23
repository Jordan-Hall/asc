//! The daemon's scheduler (SPEC §9.1; U53b): periodic background work.
//!
//! Every task has its own tokio task, so one slow task never delays another, and runs one at a
//! time: the next run is timed from the end of the last one. A run is cut off after its
//! timeout, a panic is caught and counted, and a failing task backs off (2^n × its period, up
//! to 16×, never beyond an hour) instead of failing in a tight loop. Every run is counted in
//! [`Metrics`] and logged as a `daemon.task` event.
//!
//! The built-in tasks are [`builtin`]: hourly cache GC, idle dehydration, ephemeral worktree
//! expiry, pending-path retries every 5 s, reconcile at start, the post-clone background
//! prefetch (SPEC §10.2 item 5a), the daily update check (SPEC §16.2) and seed unpack for a
//! cold cache (SPEC §12.3). The prefetcher, the release source and the seed source belong to
//! units that are not part of the daemon (U40b, the release channel, U72): they plug in through
//! [`Hooks`]. Later units (B14, B30) register their own tasks with [`Scheduler::register`].

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use asc_state::state::workspaces::{self, Workspace};
use tokio::sync::watch;
use tokio::task::JoinHandle;

use super::metrics::{EventLog, Metrics};
use super::service::{Mounted, State};

/// The longest wait between two runs of a failing task.
const MAX_BACKOFF: Duration = Duration::from_secs(3600);
/// Entries visited when measuring a directory.
const MAX_WALK: u64 = 5_000_000;
/// Longest `latest` version text accepted from a release source.
const MAX_VERSION: usize = 64;

/// A unit of background work.
#[async_trait]
pub trait Job: Send + Sync + 'static {
    /// Does one run. `Ok` carries a short note for the log.
    async fn run(&self) -> Result<String, String>;
}

/// When a task runs.
#[derive(Clone, Copy, Debug)]
pub struct Spec {
    /// A fixed name (a metrics label).
    pub name: &'static str,
    /// The period; `None` runs the task once.
    pub every: Option<Duration>,
    /// The wait before the first run.
    pub first: Duration,
    /// A run longer than this is cut off and counts as a failure.
    pub timeout: Duration,
}

/// The periods of the built-in tasks.
#[derive(Clone, Copy, Debug)]
pub struct Timing {
    pub cache_gc: Duration,
    pub idle_dehydrate: Duration,
    pub ephemeral_expiry: Duration,
    pub pending_retry: Duration,
    pub prefetch: Duration,
    pub update_check: Duration,
}

impl Default for Timing {
    fn default() -> Self {
        Self {
            cache_gc: Duration::from_secs(3600),
            idle_dehydrate: Duration::from_secs(300),
            ephemeral_expiry: Duration::from_secs(60),
            pending_retry: Duration::from_secs(5),
            prefetch: Duration::from_secs(5),
            update_check: Duration::from_secs(24 * 3600),
        }
    }
}

/// A size as git's `--int` config values spell it: digits with an optional `k`, `m` or `g`
/// (binary multiples). `None` for anything else, zero, or an overflow.
pub fn parse_size(text: &str) -> Option<u64> {
    let t = text.trim();
    let (digits, unit) = match t.char_indices().last()? {
        (i, c) if c.is_ascii_alphabetic() => (
            &t[..i],
            match c.to_ascii_lowercase() {
                'k' => 1u64 << 10,
                'm' => 1 << 20,
                'g' => 1 << 30,
                _ => return None,
            },
        ),
        _ => (t, 1),
    };
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    digits
        .parse::<u64>()
        .ok()?
        .checked_mul(unit)
        .filter(|n| *n > 0)
}

/// What the built-in tasks need to know.
#[derive(Clone, Debug)]
pub struct Settings {
    /// `asc.cache.maxSize` (SPEC §3.5); the cache is evicted down to 80% of it.
    pub cache_max_bytes: u64,
    /// How long no client may have asked for anything before idle dehydration runs.
    pub idle_after: Duration,
    /// The `asc` that runs `worktree prune`: the daemon's own binary, never found on `PATH`.
    pub asc_exe: PathBuf,
    pub timing: Timing,
}

impl Settings {
    /// Settings for a daemon that runs from `asc_exe`.
    pub fn new(asc_exe: PathBuf) -> Self {
        Self {
            cache_max_bytes: 20 * 1024 * 1024 * 1024,
            idle_after: Duration::from_secs(300),
            asc_exe,
            timing: Timing::default(),
        }
    }
}

/// The post-clone background prefetch (SPEC §10.2 item 5a): fills the machine cache from the
/// `.asc/prefetch` globs of a newly mounted workspace. `asc prefetch` is U40b.
#[async_trait]
pub trait Prefetcher: Send + Sync {
    async fn prefetch(&self, ws: &Workspace) -> Result<String, String>;
}

/// Where the daily update check asks for the newest release (SPEC §16.2).
#[async_trait]
pub trait UpdateSource: Send + Sync {
    async fn latest(&self) -> Result<String, String>;
}

/// Where a cold cache is seeded from (SPEC §12.3; `seed::unpack` is U72).
#[async_trait]
pub trait SeedSource: Send + Sync {
    /// Fills `cache`; the daemon calls it only for an empty cache.
    async fn unpack(&self, cache: &Path) -> Result<String, String>;
}

/// The collaborators the built-in tasks call. A missing one turns its task into a no-op (the
/// update check and seed unpack are not registered at all).
#[derive(Clone, Default)]
pub struct Hooks {
    pub prefetch: Option<Arc<dyn Prefetcher>>,
    pub updates: Option<Arc<dyn UpdateSource>>,
    pub seeds: Option<Arc<dyn SeedSource>>,
}

// ---------------------------------------------------------------------------------- engine

/// A set of tasks, not yet running.
#[derive(Default)]
pub struct Scheduler {
    tasks: Vec<(Spec, Arc<dyn Job>)>,
}

/// Running tasks.
pub struct Running {
    stop: watch::Sender<bool>,
    joins: Vec<JoinHandle<()>>,
}

impl Scheduler {
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds a task. A name already taken is refused so two tasks cannot share counters.
    pub fn register(&mut self, spec: Spec, job: Arc<dyn Job>) -> Result<(), String> {
        if self.tasks.iter().any(|(s, _)| s.name == spec.name) {
            return Err(format!(
                "a task named '{}' is already registered",
                spec.name
            ));
        }
        self.tasks.push((spec, job));
        Ok(())
    }

    pub fn names(&self) -> Vec<&'static str> {
        self.tasks.iter().map(|(s, _)| s.name).collect()
    }

    /// Starts every task.
    pub fn start(self, metrics: Arc<Metrics>, events: Arc<EventLog>) -> Running {
        let (stop, rx) = watch::channel(false);
        let joins = self
            .tasks
            .into_iter()
            .map(|(spec, job)| {
                tokio::spawn(drive(
                    spec,
                    job,
                    metrics.clone(),
                    events.clone(),
                    rx.clone(),
                ))
            })
            .collect();
        Running { stop, joins }
    }
}

impl Running {
    /// Stops the tasks and waits for them; a run in progress is cut off.
    pub async fn stop(self) {
        let _ = self.stop.send(true);
        for j in self.joins {
            let _ = j.await;
        }
    }
}

fn backoff(every: Duration, failures: u32) -> Duration {
    every
        .saturating_mul(1 << failures.min(4))
        .min(MAX_BACKOFF.max(every))
}

async fn drive(
    spec: Spec,
    job: Arc<dyn Job>,
    metrics: Arc<Metrics>,
    events: Arc<EventLog>,
    mut stop: watch::Receiver<bool>,
) {
    let mut failures = 0u32;
    let mut wait = spec.first;
    loop {
        tokio::select! {
            () = tokio::time::sleep(wait) => {}
            _ = stop.changed() => return,
        }
        if *stop.borrow() {
            return;
        }
        let began = Instant::now();
        // Its own task: a panic is caught here, and an overrun can be aborted.
        let handle = {
            let job = job.clone();
            tokio::spawn(async move { job.run().await })
        };
        let abort = handle.abort_handle();
        let outcome = tokio::select! {
            r = tokio::time::timeout(spec.timeout, handle) => match r {
                Ok(Ok(result)) => result,
                Ok(Err(e)) if e.is_panic() => Err("the task panicked".to_owned()),
                Ok(Err(e)) => Err(format!("the task was cancelled: {e}")),
                Err(_) => {
                    abort.abort();
                    Err(format!("the task ran over {} s", spec.timeout.as_secs()))
                }
            },
            _ = stop.changed() => {
                abort.abort();
                return;
            }
        };
        let took = began.elapsed();
        metrics.task_run(spec.name, outcome.is_ok(), took);
        match &outcome {
            Ok(note) if note.is_empty() => {}
            Ok(note) => events.emit(
                "info",
                "daemon.task",
                &[("task", spec.name), ("outcome", "ok"), ("note", note)],
            ),
            Err(why) => events.emit(
                "error",
                "daemon.task",
                &[("task", spec.name), ("outcome", "failed"), ("error", why)],
            ),
        }
        let Some(every) = spec.every else { return };
        failures = if outcome.is_ok() {
            0
        } else {
            failures.saturating_add(1)
        };
        wait = backoff(every, failures);
    }
}

// ---------------------------------------------------------------------------------- built-ins

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

/// The built-in tasks of SPEC §9.1.
pub fn builtin(state: Arc<State>, settings: &Settings, hooks: &Hooks) -> Scheduler {
    let t = settings.timing;
    let mut s = Scheduler::new();
    let mut add = |name: &'static str,
                   every: Option<Duration>,
                   first: Duration,
                   timeout: Duration,
                   job: Arc<dyn Job>| {
        s.register(
            Spec {
                name,
                every,
                first,
                timeout,
            },
            job,
        )
        .expect("built-in task names are distinct");
    };
    let minutes = |m: u64| Duration::from_secs(m * 60);

    add(
        "reconcile_start",
        None,
        Duration::ZERO,
        minutes(30),
        Arc::new(Reconcile(state.clone())),
    );
    add(
        "pending_retry",
        Some(t.pending_retry),
        t.pending_retry,
        minutes(1),
        Arc::new(PendingRetry(state.clone())),
    );
    add(
        "cache_gc",
        Some(t.cache_gc),
        minutes(1).min(t.cache_gc),
        minutes(30),
        Arc::new(CacheGc {
            state: state.clone(),
            max: settings.cache_max_bytes,
        }),
    );
    add(
        "idle_dehydrate",
        Some(t.idle_dehydrate),
        t.idle_dehydrate,
        minutes(30),
        Arc::new(IdleDehydrate {
            state: state.clone(),
            idle_after: settings.idle_after,
        }),
    );
    add(
        "ephemeral_expiry",
        Some(t.ephemeral_expiry),
        t.ephemeral_expiry,
        minutes(5),
        Arc::new(EphemeralExpiry {
            state: state.clone(),
            asc_exe: settings.asc_exe.clone(),
        }),
    );
    add(
        "post_clone_prefetch",
        Some(t.prefetch),
        t.prefetch,
        minutes(30),
        Arc::new(PostCloneFetch {
            state: state.clone(),
            prefetcher: hooks.prefetch.clone(),
        }),
    );
    if let Some(source) = &hooks.updates {
        add(
            "update_check",
            Some(t.update_check),
            minutes(10).min(t.update_check),
            minutes(2),
            Arc::new(UpdateCheck {
                state: state.clone(),
                source: source.clone(),
                period: t.update_check,
            }),
        );
    }
    if let Some(source) = &hooks.seeds {
        add(
            "seed_unpack",
            None,
            Duration::ZERO,
            minutes(60),
            Arc::new(SeedUnpack {
                state: state.clone(),
                source: source.clone(),
            }),
        );
    }
    s
}

/// Runs `f` on a mounted workspace unless it is busy with a change to its work tree.
async fn each_idle_mount<F, Fut>(state: &State, f: F) -> (u64, Vec<String>)
where
    F: Fn(Arc<Mounted>) -> Fut,
    Fut: std::future::Future<Output = Result<u64, String>>,
{
    let (mut sum, mut errors) = (0, Vec::new());
    for m in state.mounts.all() {
        let Ok(_held) = m.gate.try_lock() else {
            continue;
        };
        match f(m.clone()).await {
            Ok(n) => sum += n,
            Err(e) => errors.push(format!("{}: {e}", m.id)),
        }
    }
    (sum, errors)
}

fn verdict(done: String, errors: Vec<String>) -> Result<String, String> {
    if errors.is_empty() {
        Ok(done)
    } else {
        Err(errors.join("; "))
    }
}

/// Reconcile at start (SPEC §9.1): compares disk with the journal for every mounted workspace.
struct Reconcile(Arc<State>);

#[async_trait]
impl Job for Reconcile {
    async fn run(&self) -> Result<String, String> {
        let state = &self.0;
        let (paths, errors) = each_idle_mount(state, async |m| {
            m.hosted.reconcile().await.map_err(|e| {
                state.metrics.provider_error("reconcile");
                format!("{e:?}")
            })
        })
        .await;
        state.metrics.add_reconciled(paths);
        verdict(format!("reconciled {paths} paths"), errors)
    }
}

/// Retries the paths a switch left pending, every 5 s.
struct PendingRetry(Arc<State>);

#[async_trait]
impl Job for PendingRetry {
    async fn run(&self) -> Result<String, String> {
        let state = &self.0;
        let (_, errors) = each_idle_mount(state, async |m| {
            if m.hosted.pending() == 0 {
                return Ok(0);
            }
            m.hosted.retry_pending().await.map(|()| 0).map_err(|e| {
                state.metrics.provider_error("retry_pending");
                format!("{e:?}")
            })
        })
        .await;
        verdict(String::new(), errors)
    }
}

/// The idle dehydration hook: when no client has asked for anything for a while, each hosted
/// workspace gets one dehydration pass (its backend applies the SPEC §10.2 item 9 rules: not
/// recently read, not open, not modified).
struct IdleDehydrate {
    state: Arc<State>,
    idle_after: Duration,
}

#[async_trait]
impl Job for IdleDehydrate {
    async fn run(&self) -> Result<String, String> {
        let state = &self.state;
        if state.mounts.is_empty() || state.mounts.idle_for() < self.idle_after {
            return Ok(String::new());
        }
        let (files, errors) = each_idle_mount(state, async |m| {
            m.hosted
                .dehydrate_idle()
                .await
                .map(|(files, _)| files)
                .map_err(|e| {
                    state.metrics.provider_error("dehydrate");
                    format!("{e:?}")
                })
        })
        .await;
        verdict(
            if files == 0 {
                String::new()
            } else {
                format!("dehydrated {files} files")
            },
            errors,
        )
    }
}

/// Hourly cache GC: evicts the least recently used files above `asc.cache.maxSize` down to 80%
/// of it (SPEC §3.5), then measures what is left for `asc_daemon_cache_bytes`.
struct CacheGc {
    state: Arc<State>,
    max: u64,
}

impl CacheGc {
    async fn evict(&self) -> Result<u64, String> {
        let home = &self.state.home;
        let cache = asc_store::cachedir::CacheDir::open(home.cache(), home.tmp())
            .map_err(|e| e.to_string())?;
        cache.evict_to(self.max).await.map_err(|e| e.to_string())
    }
}

#[async_trait]
impl Job for CacheGc {
    async fn run(&self) -> Result<String, String> {
        let freed = self.evict().await?;
        let root = self.state.home.cache();
        let left = tokio::task::spawn_blocking(move || dir_size(&root))
            .await
            .map_err(|e| e.to_string())?;
        self.state.metrics.set_cache_bytes(left);
        Ok(if freed == 0 {
            String::new()
        } else {
            format!("evicted {freed} bytes; {left} left")
        })
    }
}

/// Total size of the regular files under `root`, links not followed, at most [`MAX_WALK`]
/// entries visited.
pub fn dir_size(root: &Path) -> u64 {
    let (mut total, mut seen) = (0u64, 0u64);
    let mut stack = vec![root.to_owned()];
    while let Some(dir) = stack.pop() {
        let Ok(read) = std::fs::read_dir(&dir) else {
            continue;
        }; // lint:allow-worktree-fs: the cache, not a work tree
        for entry in read.flatten() {
            seen += 1;
            if seen > MAX_WALK {
                return total;
            }
            let Ok(meta) = std::fs::symlink_metadata(entry.path()) else {
                continue;
            };
            if asc_wc::safe_path::is_link(&meta) {
                continue;
            }
            if meta.is_dir() {
                stack.push(entry.path());
            } else {
                total = total.saturating_add(meta.len());
            }
        }
    }
    total
}

/// Whether any regular file is under `root` (the cache's class directories are always there,
/// so an empty cache is a tree of empty directories). Links are not followed.
fn has_files(root: &Path) -> bool {
    let (mut seen, mut stack) = (0u64, vec![root.to_owned()]);
    while let Some(dir) = stack.pop() {
        let Ok(read) = std::fs::read_dir(&dir) else {
            continue;
        }; // lint:allow-worktree-fs: the cache, not a work tree
        for entry in read.flatten() {
            seen += 1;
            let Ok(meta) = std::fs::symlink_metadata(entry.path()) else {
                continue;
            };
            if asc_wc::safe_path::is_link(&meta) {
                continue;
            }
            if meta.is_dir() && seen <= MAX_WALK {
                stack.push(entry.path());
            } else if !meta.is_dir() {
                return true;
            }
        }
    }
    false
}

/// Ephemeral worktree expiry (`worktree add --ephemeral --ttl`): once a worktree is past its
/// time, `asc worktree prune` in a sibling worktree of the same repository removes it when it is
/// clean and unlocked (a dirty one stays), and the daemon then stops hosting what is gone.
struct EphemeralExpiry {
    state: Arc<State>,
    asc_exe: PathBuf,
}

impl EphemeralExpiry {
    async fn prune(&self, anchor: &Workspace) -> Result<(), String> {
        let dir = PathBuf::from(&anchor.ws.mount);
        let check = dir.clone();
        tokio::task::spawn_blocking(move || super::service::check_mount_root(&check))
            .await
            .map_err(|e| e.to_string())??;
        let mut cmd = tokio::process::Command::new(&self.asc_exe);
        cmd.args(["worktree", "prune"])
            .current_dir(&dir)
            // The child must not call back into this daemon, and must not take its repository
            // from the daemon's environment.
            .env("ASC_NO_DAEMON", "1")
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_COMMON_DIR")
            .env_remove("GIT_INDEX_FILE")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let out = tokio::time::timeout(Duration::from_secs(120), cmd.output())
            .await
            .map_err(|_| "worktree prune ran over 120 s".to_owned())?
            .map_err(|e| format!("cannot run worktree prune: {e}"))?;
        if out.status.success() {
            Ok(())
        } else {
            let text = String::from_utf8_lossy(&out.stderr);
            Err(format!(
                "worktree prune failed: {}",
                text.trim().chars().take(300).collect::<String>()
            ))
        }
    }
}

#[async_trait]
impl Job for EphemeralExpiry {
    async fn run(&self) -> Result<String, String> {
        let now = unix_now();
        let rows = workspaces::list(&self.state.db, None)
            .await
            .map_err(|e| e.to_string())?;
        let mut by_repo: HashMap<i64, Vec<&Workspace>> = HashMap::new();
        for r in &rows {
            by_repo.entry(r.ws.repo.0).or_default().push(r);
        }
        let mut errors = Vec::new();
        let mut asked = 0;
        for group in by_repo.values() {
            let expired = group
                .iter()
                .any(|r| r.locked.is_none() && r.ws.ephemeral_until.is_some_and(|t| t < now));
            if !expired {
                continue;
            }
            // The oldest worktree of the repository that does not itself expire.
            let Some(anchor) = group
                .iter()
                .filter(|r| r.ws.ephemeral_until.is_none() && Path::new(&r.ws.mount).is_dir())
                .min_by_key(|r| r.created)
            else {
                continue;
            };
            asked += 1;
            if let Err(e) = self.prune(anchor).await {
                errors.push(e);
            }
        }
        let gone = self
            .state
            .forget_unregistered()
            .await
            .map_err(|e| format!("unmount: {e}"))?;
        verdict(
            if asked == 0 && gone == 0 {
                String::new()
            } else {
                format!("pruned in {asked} repos; unmounted {gone}")
            },
            errors,
        )
    }
}

/// The post-clone background prefetch: each newly mounted workspace is handed to the
/// prefetcher once. Without a prefetcher the queue is just dropped.
struct PostCloneFetch {
    state: Arc<State>,
    prefetcher: Option<Arc<dyn Prefetcher>>,
}

#[async_trait]
impl Job for PostCloneFetch {
    async fn run(&self) -> Result<String, String> {
        let fresh = self.state.mounts.take_fresh();
        let Some(prefetcher) = &self.prefetcher else {
            return Ok(String::new());
        };
        let (mut done, mut errors) = (0, Vec::new());
        for id in fresh {
            let Some(m) = self.state.mounts.get(id) else {
                continue;
            };
            match prefetcher.prefetch(&m.row).await {
                Ok(_) => done += 1,
                Err(e) => errors.push(format!("{id}: {e}")),
            }
        }
        verdict(
            if done == 0 {
                String::new()
            } else {
                format!("prefetched {done} workspaces")
            },
            errors,
        )
    }
}

/// `<run>/update.json`: what the daily check found. `asc status` reads it for its hint.
pub fn update_file(run_dir: &Path) -> PathBuf {
    run_dir.join("update.json")
}

/// A release version is plain text of digits, letters and `.+-`.
fn valid_version(v: &str) -> bool {
    !v.is_empty()
        && v.len() <= MAX_VERSION
        && v.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'+' | b'-'))
}

struct UpdateCheck {
    state: Arc<State>,
    source: Arc<dyn UpdateSource>,
    period: Duration,
}

#[async_trait]
impl Job for UpdateCheck {
    async fn run(&self) -> Result<String, String> {
        let dir = self.state.home.run();
        let file = update_file(&dir);
        // A daemon that restarts often must not ask more than once a day.
        let recent = std::fs::metadata(&file)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.elapsed().ok())
            .is_some_and(|age| age < self.period);
        if recent {
            return Ok(String::new());
        }
        let latest = self.source.latest().await?;
        if !valid_version(&latest) {
            return Err("the release source returned something that is not a version".into());
        }
        let body = serde_json::json!({ "checked": unix_now(), "current": super::VERSION, "latest": latest });
        std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        let tmp = dir.join(format!("update.json.{}.tmp", std::process::id()));
        std::fs::write(&tmp, body.to_string()).map_err(|e| e.to_string())?;
        std::fs::rename(&tmp, &file).map_err(|e| {
            let _ = std::fs::remove_file(&tmp);
            e.to_string()
        })?;
        Ok(format!("latest release is {latest}"))
    }
}

/// Seed unpack for a cold cache (SPEC §12.3): only an empty cache is seeded, so a seed never
/// overwrites objects the cache already verified.
struct SeedUnpack {
    state: Arc<State>,
    source: Arc<dyn SeedSource>,
}

#[async_trait]
impl Job for SeedUnpack {
    async fn run(&self) -> Result<String, String> {
        let cache = self.state.home.cache();
        let probe = cache.clone();
        let cold = tokio::task::spawn_blocking(move || !has_files(&probe))
            .await
            .map_err(|e| e.to_string())?;
        if !cold {
            return Ok(String::new());
        }
        self.source.unpack(&cache).await
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU32, Ordering};

    use bytes::Bytes;
    use asc_core::id::FileId;
    use asc_state::state::workspaces::Mode;
    use asc_store::key::ObjKey;
    use asc_store::objects::Objects;

    use super::super::service::fixtures::fixture;
    use super::super::service::{HostError, Hosted, VirtualBackend};
    use super::*;

    const FAST: Duration = Duration::from_millis(10);

    fn spec(name: &'static str, every: Option<Duration>, timeout: Duration) -> Spec {
        Spec {
            name,
            every,
            first: Duration::ZERO,
            timeout,
        }
    }

    async fn until(what: &str, mut ok: impl FnMut() -> bool) {
        for _ in 0..400 {
            if ok() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("timed out waiting for {what}");
    }

    struct Count(Arc<AtomicU32>);

    #[async_trait]
    impl Job for Count {
        async fn run(&self) -> Result<String, String> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(String::new())
        }
    }

    struct Boom(Arc<AtomicU32>);

    #[async_trait]
    impl Job for Boom {
        async fn run(&self) -> Result<String, String> {
            if self.0.fetch_add(1, Ordering::SeqCst) == 0 {
                panic!("boom");
            }
            Err("still failing".into())
        }
    }

    struct Slow;

    #[async_trait]
    impl Job for Slow {
        async fn run(&self) -> Result<String, String> {
            tokio::time::sleep(Duration::from_secs(60)).await;
            Ok(String::new())
        }
    }

    #[tokio::test]
    async fn tasks_run_on_their_period_once_or_forever() {
        let (every, once) = (Arc::new(AtomicU32::new(0)), Arc::new(AtomicU32::new(0)));
        let mut s = Scheduler::new();
        s.register(
            spec("every", Some(FAST), Duration::from_secs(5)),
            Arc::new(Count(every.clone())),
        )
        .unwrap();
        s.register(
            spec("once", None, Duration::from_secs(5)),
            Arc::new(Count(once.clone())),
        )
        .unwrap();
        assert!(
            s.register(spec("once", None, FAST), Arc::new(Count(once.clone())))
                .is_err(),
            "names are unique"
        );
        let metrics = Arc::new(Metrics::new());
        let running = s.start(metrics.clone(), Arc::new(EventLog::none()));
        until("three runs", || every.load(Ordering::SeqCst) >= 3).await;
        running.stop().await;
        assert_eq!(once.load(Ordering::SeqCst), 1);
        let after = every.load(Ordering::SeqCst);
        tokio::time::sleep(Duration::from_millis(60)).await;
        assert_eq!(
            every.load(Ordering::SeqCst),
            after,
            "a stopped scheduler runs nothing"
        );
        assert!(metrics.task("every").runs >= 3 && metrics.task("every").failures == 0);
    }

    #[tokio::test]
    async fn a_panic_and_an_overrun_fail_one_run_not_the_scheduler() {
        let (boom, ok) = (Arc::new(AtomicU32::new(0)), Arc::new(AtomicU32::new(0)));
        let mut s = Scheduler::new();
        s.register(
            spec("boom", Some(FAST), Duration::from_secs(5)),
            Arc::new(Boom(boom.clone())),
        )
        .unwrap();
        s.register(
            spec("slow", Some(FAST), Duration::from_millis(50)),
            Arc::new(Slow),
        )
        .unwrap();
        s.register(
            spec("ok", Some(FAST), Duration::from_secs(5)),
            Arc::new(Count(ok.clone())),
        )
        .unwrap();
        let metrics = Arc::new(Metrics::new());
        let running = s.start(metrics.clone(), Arc::new(EventLog::none()));
        until("the failures", || {
            metrics.task("boom").failures >= 2 && metrics.task("slow").failures >= 1
        })
        .await;
        until("the healthy task to keep running", || {
            ok.load(Ordering::SeqCst) >= 5
        })
        .await;
        running.stop().await;
        assert_eq!(metrics.task("boom").runs, metrics.task("boom").failures);
    }

    #[test]
    fn sizes_parse_like_git_ints() {
        assert_eq!(parse_size("20g"), Some(20 << 30));
        assert_eq!(parse_size("512M"), Some(512 << 20));
        assert_eq!(parse_size("4k"), Some(4096));
        assert_eq!(parse_size("1000"), Some(1000));
        for bad in ["", "g", "0", "-1", "1.5g", "10x", "99999999999g", "1 g"] {
            assert_eq!(parse_size(bad), None, "{bad}");
        }
    }

    #[test]
    fn failures_back_off_up_to_sixteen_times_and_an_hour() {
        let five = Duration::from_secs(5);
        assert_eq!(backoff(five, 0), five);
        assert_eq!(backoff(five, 1), Duration::from_secs(10));
        assert_eq!(backoff(five, 4), Duration::from_secs(80));
        assert_eq!(backoff(five, 30), Duration::from_secs(80));
        assert_eq!(
            backoff(Duration::from_secs(3600), 3),
            Duration::from_secs(3600)
        );
        assert_eq!(
            backoff(Duration::from_secs(7200), 3),
            Duration::from_secs(7200)
        );
    }

    #[test]
    fn the_builtin_tasks_are_the_spec_list() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let f = fixture(None).await;
            let settings = Settings::new(PathBuf::from("asc"));
            let bare = builtin(f.state.clone(), &settings, &Hooks::default()).names();
            for name in [
                "reconcile_start",
                "pending_retry",
                "cache_gc",
                "idle_dehydrate",
                "ephemeral_expiry",
                "post_clone_prefetch",
            ] {
                assert!(bare.contains(&name), "{name}");
            }
            assert!(
                !bare.contains(&"update_check") && !bare.contains(&"seed_unpack"),
                "no source, no task"
            );

            struct Never;
            #[async_trait]
            impl UpdateSource for Never {
                async fn latest(&self) -> Result<String, String> {
                    Err("offline".into())
                }
            }
            #[async_trait]
            impl SeedSource for Never {
                async fn unpack(&self, _: &Path) -> Result<String, String> {
                    Err("offline".into())
                }
            }
            let hooks = Hooks {
                prefetch: None,
                updates: Some(Arc::new(Never)),
                seeds: Some(Arc::new(Never)),
            };
            let all = builtin(f.state.clone(), &settings, &hooks).names();
            assert!(all.contains(&"update_check") && all.contains(&"seed_unpack"));
            let t = Timing::default();
            assert_eq!(t.cache_gc, Duration::from_secs(3600));
            assert_eq!(t.pending_retry, Duration::from_secs(5));
            assert_eq!(t.update_check, Duration::from_secs(86_400));
        });
    }

    #[tokio::test]
    async fn cache_gc_evicts_the_oldest_down_to_eighty_percent() {
        let f = fixture(None).await;
        let cache =
            asc_store::cachedir::CacheDir::open(f.state.home.cache(), f.state.home.tmp()).unwrap();
        let mut keys = Vec::new();
        for i in 0..10u8 {
            let data = vec![i; 100];
            let key = ObjKey::File(FileId::of(&data));
            let mut stored = vec![0u8];
            stored.extend_from_slice(&data);
            cache.put(&key, Bytes::from(stored)).await.unwrap();
            keys.push(key);
        }
        let job = CacheGc {
            state: f.state.clone(),
            max: 500,
        };
        let note = job.run().await.unwrap();
        assert!(note.contains("evicted"), "{note}");
        let left = dir_size(&f.state.home.cache());
        assert!(left <= 400, "{left}");
        assert!(
            f.state
                .metrics
                .render(&Default::default())
                .contains(&format!("asc_daemon_cache_bytes{{dir=\"cache\"}} {left}"))
        );
        // Within the limit nothing goes.
        assert_eq!(
            CacheGc {
                state: f.state.clone(),
                max: 1_000_000
            }
            .run()
            .await
            .unwrap(),
            ""
        );
    }

    #[tokio::test]
    async fn dir_size_skips_links() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("a/b")).unwrap();
        std::fs::write(dir.path().join("a/x"), [0u8; 10]).unwrap();
        std::fs::write(dir.path().join("a/b/y"), [0u8; 5]).unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("big"), [0u8; 1000]).unwrap();
        #[cfg(unix)]
        let linked = std::os::unix::fs::symlink(outside.path(), dir.path().join("a/link")).is_ok();
        #[cfg(windows)]
        let linked =
            std::os::windows::fs::symlink_dir(outside.path(), dir.path().join("a/link")).is_ok();
        // Without the symlink privilege there is no link to skip; the size is the same.
        let _ = linked;
        assert_eq!(dir_size(dir.path()), 15, "a link is not followed");
    }

    /// A backend that counts what the scheduler asks of it.
    #[derive(Default)]
    struct Spy {
        pending: AtomicU32,
        retries: AtomicU32,
        idle: AtomicU32,
        reconciled: AtomicU32,
        unmounted: AtomicU32,
    }

    #[async_trait]
    impl Hosted for Arc<Spy> {
        fn pending(&self) -> u64 {
            u64::from(self.pending.load(Ordering::SeqCst))
        }
        async fn retry_pending(&self) -> Result<(), HostError> {
            self.retries.fetch_add(1, Ordering::SeqCst);
            self.pending.store(0, Ordering::SeqCst);
            Ok(())
        }
        async fn dehydrate_idle(&self) -> Result<(u64, u64), HostError> {
            self.idle.fetch_add(1, Ordering::SeqCst);
            Ok((3, 30))
        }
        async fn reconcile(&self) -> Result<u64, HostError> {
            self.reconciled.fetch_add(1, Ordering::SeqCst);
            Ok(7)
        }
        async fn unmount(&self) -> Result<(), HostError> {
            self.unmounted.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    struct SpyBackend(Arc<Spy>);

    #[async_trait]
    impl VirtualBackend for SpyBackend {
        async fn host(
            &self,
            _: super::super::service::HostContext,
        ) -> Result<Arc<dyn Hosted>, HostError> {
            Ok(Arc::new(self.0.clone()))
        }
    }

    #[tokio::test]
    async fn mounted_workspaces_get_retries_idle_dehydration_and_reconcile() {
        let spy = Arc::new(Spy::default());
        let f = fixture(Some(Arc::new(SpyBackend(spy.clone())))).await;
        let ws = f.workspace(Mode::Virtual, None).await;
        f.state.mount(ws.ws.id).await.unwrap();
        spy.pending.store(2, Ordering::SeqCst);

        assert_eq!(PendingRetry(f.state.clone()).run().await.unwrap(), "");
        assert_eq!(spy.retries.load(Ordering::SeqCst), 1);
        assert_eq!(PendingRetry(f.state.clone()).run().await.unwrap(), "");
        assert_eq!(
            spy.retries.load(Ordering::SeqCst),
            1,
            "nothing pending, nothing retried"
        );

        let busy = IdleDehydrate {
            state: f.state.clone(),
            idle_after: Duration::from_secs(3600),
        };
        busy.run().await.unwrap();
        assert_eq!(
            spy.idle.load(Ordering::SeqCst),
            0,
            "a daemon that was just asked something is not idle"
        );
        let idle = IdleDehydrate {
            state: f.state.clone(),
            idle_after: Duration::ZERO,
        };
        assert_eq!(idle.run().await.unwrap(), "dehydrated 3 files");

        assert_eq!(
            Reconcile(f.state.clone()).run().await.unwrap(),
            "reconciled 7 paths"
        );
        assert!(
            f.state
                .metrics
                .render(&Default::default())
                .contains("asc_daemon_reconcile_paths_total 7")
        );

        // A change in progress holds the workspace: background passes skip it.
        let held = f.state.mounted(ws.ws.id).unwrap();
        let _g = held.gate.lock().await;
        Reconcile(f.state.clone()).run().await.unwrap();
        assert_eq!(spy.reconciled.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn a_removed_workspace_is_unmounted() {
        let spy = Arc::new(Spy::default());
        let f = fixture(Some(Arc::new(SpyBackend(spy.clone())))).await;
        let ws = f.workspace(Mode::Virtual, None).await;
        f.state.mount(ws.ws.id).await.unwrap();
        assert_eq!(f.state.forget_unregistered().await.unwrap(), 0);
        workspaces::remove(&f.state.db, ws.ws.id).await.unwrap();
        assert_eq!(f.state.forget_unregistered().await.unwrap(), 1);
        assert_eq!(spy.unmounted.load(Ordering::SeqCst), 1);
        assert!(f.state.mounts.is_empty());
    }

    struct Recorder(Arc<std::sync::Mutex<Vec<WorkspaceIdText>>>);
    type WorkspaceIdText = String;

    #[async_trait]
    impl Prefetcher for Recorder {
        async fn prefetch(&self, ws: &Workspace) -> Result<String, String> {
            self.0.lock().unwrap().push(ws.ws.id.to_string());
            Ok(String::new())
        }
    }

    #[tokio::test]
    async fn a_new_mount_is_prefetched_once() {
        let f = fixture(None).await;
        let ws = f.workspace(Mode::Plain, None).await;
        f.state.mount(ws.ws.id).await.unwrap();
        f.state.mount(ws.ws.id).await.unwrap();
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let job = PostCloneFetch {
            state: f.state.clone(),
            prefetcher: Some(Arc::new(Recorder(seen.clone()))),
        };
        assert_eq!(job.run().await.unwrap(), "prefetched 1 workspaces");
        assert_eq!(job.run().await.unwrap(), "", "the queue is drained");
        assert_eq!(*seen.lock().unwrap(), vec![ws.ws.id.to_string()]);
        let none = PostCloneFetch {
            state: f.state.clone(),
            prefetcher: None,
        };
        none.run().await.unwrap();
    }

    struct Release(Result<String, String>, Arc<AtomicU32>);

    #[async_trait]
    impl UpdateSource for Release {
        async fn latest(&self) -> Result<String, String> {
            self.1.fetch_add(1, Ordering::SeqCst);
            self.0.clone()
        }
    }

    #[tokio::test]
    async fn the_update_check_writes_a_valid_answer_once_a_day() {
        let f = fixture(None).await;
        let asked = Arc::new(AtomicU32::new(0));
        let job = |r: Result<String, String>| UpdateCheck {
            state: f.state.clone(),
            source: Arc::new(Release(r, asked.clone())),
            period: Duration::from_secs(86_400),
        };
        let file = update_file(&f.state.home.run());
        assert!(
            job(Ok("1.2.3\n<script>".into())).run().await.is_err(),
            "a hostile answer is refused"
        );
        assert!(!file.exists());
        assert!(job(Err("offline".into())).run().await.is_err());
        assert!(
            job(Ok("1.2.3".into()))
                .run()
                .await
                .unwrap()
                .contains("1.2.3")
        );
        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
        assert_eq!(v["latest"], "1.2.3");
        assert_eq!(v["current"], super::super::VERSION);
        let before = asked.load(Ordering::SeqCst);
        job(Ok("9.9.9".into())).run().await.unwrap();
        assert_eq!(
            asked.load(Ordering::SeqCst),
            before,
            "a fresh answer is not asked for again"
        );
    }

    struct Seeds(Arc<AtomicU32>);

    #[async_trait]
    impl SeedSource for Seeds {
        async fn unpack(&self, cache: &Path) -> Result<String, String> {
            self.0.fetch_add(1, Ordering::SeqCst);
            std::fs::write(cache.join("seeded"), b"x").map_err(|e| e.to_string())?;
            Ok("seeded".into())
        }
    }

    #[tokio::test]
    async fn only_a_cold_cache_is_seeded() {
        let f = fixture(None).await;
        std::fs::create_dir_all(f.state.home.cache()).unwrap();
        let calls = Arc::new(AtomicU32::new(0));
        let job = SeedUnpack {
            state: f.state.clone(),
            source: Arc::new(Seeds(calls.clone())),
        };
        assert_eq!(job.run().await.unwrap(), "seeded");
        assert_eq!(
            job.run().await.unwrap(),
            "",
            "a cache with content is left alone"
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn expiry_prunes_beside_an_unexpired_worktree_and_skips_locked_ones() {
        let f = fixture(None).await;
        // The anchor and the expired worktree must share a repository.
        let anchor = f.workspace(Mode::Plain, None).await;
        let expired = asc_state::state::workspaces::NewWorkspace {
            id: asc_state::WorkspaceId::new(),
            path: f.dir.path().join("tmp-wt").to_string_lossy().into_owned(),
            mount: f.dir.path().join("tmp-wt").to_string_lossy().into_owned(),
            ephemeral_until: Some(unix_now() - 10),
            ..anchor.ws.clone()
        };
        std::fs::create_dir_all(&expired.mount).unwrap();
        workspaces::insert(&f.state.db, &expired).await.unwrap();
        // A stand-in for `asc`: records where it was run and what it was asked.
        let log = f.dir.path().join("ran.txt");
        let exe = fake_asc(f.dir.path(), &log);
        let job = EphemeralExpiry {
            state: f.state.clone(),
            asc_exe: exe.clone(),
        };
        let note = job.run().await.unwrap();
        assert!(note.contains("pruned in 1 repos"), "{note}");
        let ran = std::fs::read_to_string(&log).unwrap();
        assert!(ran.contains("worktree prune"), "{ran}");
        assert!(
            ran.contains(&anchor.ws.mount.replace('\\', "/")) || ran.to_lowercase().contains("wc-"),
            "{ran}"
        );

        // Locked: not asked.
        std::fs::remove_file(&log).unwrap();
        workspaces::set_locked(&f.state.db, expired.id, Some("keep"))
            .await
            .unwrap();
        job.run().await.unwrap();
        assert!(!log.exists(), "a locked worktree does not expire");

        // A failing prune is an error, not a hang.
        workspaces::set_locked(&f.state.db, expired.id, None)
            .await
            .unwrap();
        let bad = EphemeralExpiry {
            state: f.state.clone(),
            asc_exe: f.dir.path().join("missing-asc"),
        };
        assert!(bad.run().await.is_err());
    }

    /// A script that appends its arguments and directory to `log`.
    fn fake_asc(dir: &Path, log: &Path) -> PathBuf {
        if cfg!(windows) {
            let exe = dir.join("fake-asc.cmd");
            std::fs::write(
                &exe,
                format!("@echo %* in %CD%>> \"{}\"\r\n@exit /b 0\r\n", log.display()),
            )
            .unwrap();
            exe
        } else {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let exe = dir.join("fake-asc.sh");
                std::fs::write(
                    &exe,
                    format!(
                        "#!/bin/sh
echo \"$@ in $PWD\" >> '{}'
",
                        log.display()
                    ),
                )
                .unwrap();
                std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();
                return exe;
            }
            #[allow(unreachable_code)]
            dir.join("unsupported")
        }
    }
}
