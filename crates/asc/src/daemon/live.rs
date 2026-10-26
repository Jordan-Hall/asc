//! The daemon's live-update driver (SPEC §21.3.8; B30).
//!
//! One driver runs per repository. It consumes the server's ref-update stream (`WatchRefs` for
//! `master` and `release/*`, plus the daemon's `Watch KIND_CLOUD` for the user's own drafts, which
//! reaches the driver as a unit message on its `cloud` channel), debounces and coalesces what it
//! sees (`asc.live.debounce`), and then asks the [`Runner`] to run the B29 plan once for every
//! unpaused worktree: at most one run per worktree per window.
//!
//! [`prod`] is the production side: the supervisor the daemon starts, the [`Feed`] over the RPC
//! client and the [`Runner`] that starts `asc live run` ([`pass`]) in each worktree.
//!
//! When the stream is unavailable the driver polls `ListRefs` every `asc.live.pollInterval`, with
//! exponential backoff (x2, cap 1 h, reset on success), and tries to resubscribe with the cursor of
//! the last event, so nothing is skipped and nothing is replayed.
//!
//! Only `master`, `release/*` and the caller's own drafts wake the driver. Every other event,
//! including anything about another user's drafts, is dropped before it can start a window. A
//! stream or poll error is never fatal and never reaches the runner: the driver keeps retrying, so
//! the fetch-only mode (`asc.live.fetch`) is never blocked by a broken stream.
//!
//! The server side filters by ACL (U76); the client filters again because a server is not trusted
//! to send only what was asked. Fetched objects pass the sync verification in the runner (B29).
//! Credentials come from the daemon's credential path (`Prompt::Never`) inside the [`Feed`]; this
//! module never sees one. Every loop is bounded: [`MAX_SNAPSHOT`] refs per poll and
//! [`MAX_WORKTREES`] worktrees per window.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::{mpsc, watch};

pub mod pass;
pub mod prod;

/// Default `asc.live.debounce` (SPEC §21.6.1).
pub const DEFAULT_DEBOUNCE: Duration = Duration::from_secs(2);
/// Default `asc.live.pollInterval`.
pub const DEFAULT_POLL_INTERVAL: Duration = Duration::from_secs(300);
/// The longest wait between two polls.
pub const POLL_BACKOFF_CAP: Duration = Duration::from_secs(3600);
/// Refs kept from one poll.
pub const MAX_SNAPSHOT: usize = 1024;
/// Worktrees run per window.
pub const MAX_WORKTREES: usize = 256;
/// Longest ref name accepted in an event or poll.
const MAX_NAME: usize = 4096;

/// The timing of one driver.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Timing {
    pub debounce: Duration,
    pub poll_interval: Duration,
}

impl Default for Timing {
    fn default() -> Self {
        Self {
            debounce: DEFAULT_DEBOUNCE,
            poll_interval: DEFAULT_POLL_INTERVAL,
        }
    }
}

/// One event from the server, already decoded.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    /// A ref changed. `resume` is the stream cursor after this event.
    Ref { name: String, resume: u64 },
    /// A `Watch KIND_CLOUD` event; `own` is whether it concerns the caller's own workspace.
    Cloud { own: bool },
}

/// Whether `name` is a ref live updates follow: `master` and `release/*` only. A user ref (another
/// user's draft or branch) is never one.
pub fn is_live_ref(name: &str) -> bool {
    if name.is_empty() || name.len() > MAX_NAME || name.chars().any(char::is_control) {
        return false;
    }
    let short = name.strip_prefix("refs/heads/").unwrap_or(name);
    if short.contains("..") {
        return false;
    }
    short == "master"
        || short
            .strip_prefix("release/")
            .is_some_and(|r| !r.is_empty())
}

impl Event {
    /// Whether this event should start a window.
    pub fn is_relevant(&self) -> bool {
        match self {
            Event::Ref { name, .. } => is_live_ref(name),
            Event::Cloud { own } => *own,
        }
    }
}

/// An open event stream.
#[async_trait]
pub trait EventStream: Send {
    /// The next event; `Ok(None)` when the server closed the stream.
    async fn next(&mut self) -> Result<Option<Event>, String>;
}

/// The server side of one repository.
#[async_trait]
pub trait Feed: Send + Sync {
    /// Opens the stream after cursor `resume` (`None`: the newest update).
    async fn open(&self, resume: Option<u64>) -> Result<Box<dyn EventStream>, String>;
    /// `ListRefs`: the name and version of every ref (the driver keeps the live ones).
    async fn poll(&self) -> Result<Vec<(String, u64)>, String>;
}

/// The worktrees of one repository and what runs in them.
#[async_trait]
pub trait Runner: Send + Sync {
    /// The worktrees to consider for a window.
    async fn worktrees(&self) -> Vec<String>;
    /// Whether live mode is paused for `worktree` (a conflict, or `asc live pause`).
    async fn paused(&self, worktree: &str) -> bool;
    /// Runs the B29 plan for `worktree`: fetch, and per `asc.live.*` restack and move.
    async fn run(&self, worktree: &str) -> Result<(), String>;
}

/// The driver's time source, so tests need no real waiting.
#[async_trait]
pub trait Clock: Send + Sync {
    /// Time since an arbitrary fixed start.
    fn now(&self) -> Duration;
    async fn sleep(&self, d: Duration);
}

/// The wall clock.
pub struct TokioClock(std::time::Instant);

impl TokioClock {
    pub fn new() -> Self {
        Self(std::time::Instant::now())
    }
}

impl Default for TokioClock {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Clock for TokioClock {
    fn now(&self) -> Duration {
        self.0.elapsed()
    }
    async fn sleep(&self, d: Duration) {
        tokio::time::sleep(d).await;
    }
}

/// What the driver is doing, for `asc live status` (B31) and tests.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Starting,
    Stream,
    Poll,
}

/// The wait before the next poll after `fails` consecutive failures minus one: `base * 2^fails`, capped at [`POLL_BACKOFF_CAP`].
pub fn poll_wait(base: Duration, fails: u32) -> Duration {
    let mult = 1u32.checked_shl(fails.min(20)).unwrap_or(u32::MAX);
    base.saturating_mul(mult).min(POLL_BACKOFF_CAP.max(base))
}

type Snapshot = BTreeMap<String, u64>;

fn snapshot(refs: Vec<(String, u64)>) -> Snapshot {
    refs.into_iter()
        .filter(|(n, _)| is_live_ref(n))
        .take(MAX_SNAPSHOT)
        .collect()
}

enum Woke {
    Stop,
    Run,
    Poll,
    Event(Result<Option<Event>, String>),
    /// A message on the cloud channel; `false` when its sender is gone.
    Cloud(bool),
}

/// Runs one repository until `stop` turns true; `mode` carries what it is doing. A message on
/// `cloud` says the user's own drafts changed on another device (`Event::Cloud { own: true }`);
/// it is independent of the ref stream, so it counts while the driver polls too.
pub async fn drive(
    feed: Arc<dyn Feed>,
    runner: Arc<dyn Runner>,
    clock: Arc<dyn Clock>,
    timing: Timing,
    mut stop: watch::Receiver<bool>,
    mode: watch::Sender<Mode>,
    mut cloud: Option<mpsc::UnboundedReceiver<()>>,
) {
    let mut stream: Option<Box<dyn EventStream>> = None;
    let mut opened_at = clock.now();
    let mut resume: Option<u64> = None;
    let mut last: Option<Snapshot> = None;
    let mut pending: Option<Duration> = None;
    let mut fails: u32 = 0;
    let mut immediate = true;
    let _ = mode.send(Mode::Starting);

    loop {
        if *stop.borrow() {
            return;
        }
        if pending.is_some_and(|d| clock.now() >= d) {
            pending = None;
            run_window(runner.as_ref()).await;
            continue;
        }
        let until_run = pending.map(|d| d.saturating_sub(clock.now()));
        let until_poll = if stream.is_some() {
            None
        } else if immediate {
            Some(Duration::ZERO)
        } else {
            Some(poll_wait(timing.poll_interval, fails.saturating_sub(1)))
        };

        let woke = {
            let run_sleep = async {
                match until_run {
                    Some(d) => clock.sleep(d).await,
                    None => std::future::pending().await,
                }
            };
            let poll_sleep = async {
                match until_poll {
                    Some(d) => clock.sleep(d).await,
                    None => std::future::pending().await,
                }
            };
            let next = async {
                match stream.as_mut() {
                    Some(s) => s.next().await,
                    None => std::future::pending().await,
                }
            };
            let wake = async {
                match cloud.as_mut() {
                    Some(c) => c.recv().await,
                    None => std::future::pending().await,
                }
            };
            tokio::select! {
                _ = stop.changed() => Woke::Stop,
                _ = run_sleep => Woke::Run,
                _ = poll_sleep => Woke::Poll,
                ev = next => Woke::Event(ev),
                w = wake => Woke::Cloud(w.is_some()),
            }
        };

        match woke {
            Woke::Stop | Woke::Run => {}
            Woke::Cloud(true) => {
                if (Event::Cloud { own: true }).is_relevant() && pending.is_none() {
                    pending = Some(clock.now() + timing.debounce);
                }
            }
            Woke::Cloud(false) => cloud = None,
            Woke::Event(Ok(Some(ev))) => {
                if let Event::Ref { resume: r, .. } = &ev {
                    resume = Some(*r);
                }
                if ev.is_relevant() && pending.is_none() {
                    pending = Some(clock.now() + timing.debounce);
                }
            }
            Woke::Event(_) => {
                // The stream ended or failed. The error text is server text and is dropped.
                stream = None;
                let _ = mode.send(Mode::Poll);
                if clock.now().saturating_sub(opened_at) >= timing.poll_interval {
                    fails = 0;
                    immediate = true;
                } else {
                    // A stream that dies at once backs off like a failed poll.
                    fails = fails.saturating_add(1);
                    immediate = false;
                }
            }
            Woke::Poll => {
                immediate = false;
                let mut ok = false;
                if let Ok(s) = feed.open(resume).await {
                    stream = Some(s);
                    opened_at = clock.now();
                    ok = true;
                    let _ = mode.send(Mode::Stream);
                }
                if let Ok(refs) = feed.poll().await {
                    ok = true;
                    let now = snapshot(refs);
                    if last.as_ref() != Some(&now) && pending.is_none() {
                        pending = Some(clock.now() + timing.debounce);
                    }
                    last = Some(now);
                }
                if ok {
                    fails = 0;
                } else {
                    fails = fails.saturating_add(1);
                }
                if stream.is_none() {
                    let _ = mode.send(Mode::Poll);
                }
            }
        }
    }
}

/// One window: the plan runs at most once for each unpaused worktree.
async fn run_window(runner: &dyn Runner) {
    let mut seen = BTreeSet::new();
    for wt in runner.worktrees().await.into_iter().take(MAX_WORKTREES) {
        if !seen.insert(wt.clone()) || runner.paused(&wt).await {
            continue;
        }
        // A failed run is retried by the next window; it never stops the driver.
        let _ = runner.run(&wt).await;
    }
}

// ------------------------------------------------------------------------------ production feed

/// The feed over the repository's RPC client (credentials: the daemon's `Prompt::Never` path,
/// inside the client).
pub struct RpcFeed {
    client: Arc<asc_rpc::client::RpcClient>,
}

impl RpcFeed {
    pub fn new(client: Arc<asc_rpc::client::RpcClient>) -> Self {
        Self { client }
    }
}

struct RpcStream(asc_rpc::watch_refs::RefWatch);

#[async_trait]
impl EventStream for RpcStream {
    async fn next(&mut self) -> Result<Option<Event>, String> {
        match self.0.next().await {
            Ok(Some(u)) => Ok(Some(Event::Ref {
                name: u.name,
                resume: u.resume.0,
            })),
            Ok(None) => Ok(None),
            Err(e) => Err(e.to_string()),
        }
    }
}

#[async_trait]
impl Feed for RpcFeed {
    async fn open(&self, resume: Option<u64>) -> Result<Box<dyn EventStream>, String> {
        let w = self
            .client
            .watch_refs(&[], resume.map(asc_rpc::watch_refs::Cursor))
            .await
            .map_err(|e| e.to_string())?;
        Ok(Box::new(RpcStream(w)))
    }

    async fn poll(&self) -> Result<Vec<(String, u64)>, String> {
        let refs = self
            .client
            .list_refs_all()
            .await
            .map_err(|e| e.to_string())?;
        Ok(refs
            .into_iter()
            .map(|r| {
                (
                    String::from_utf8_lossy(r.name.as_bytes()).into_owned(),
                    r.version,
                )
            })
            .collect())
    }
}

#[cfg(test)]
mod tests;
