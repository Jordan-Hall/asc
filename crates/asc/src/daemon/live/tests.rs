//! Tests of the live driver on a virtual clock (SPEC §21.3.8; B30).

use std::collections::VecDeque;
use std::sync::Mutex;

use tokio::sync::{mpsc, watch};

use super::*;

const DEBOUNCE: Duration = Duration::from_secs(2);
const POLL: Duration = Duration::from_secs(300);

struct FakeClock {
    now: Mutex<Duration>,
    wake: tokio::sync::Notify,
}

impl FakeClock {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            now: Mutex::new(Duration::ZERO),
            wake: tokio::sync::Notify::new(),
        })
    }

    /// Moves time forward in one-second steps, letting the driver react after each.
    async fn advance(&self, d: Duration) {
        let mut left = d;
        settle().await;
        while !left.is_zero() {
            let step = left.min(Duration::from_secs(1));
            *self.now.lock().unwrap() += step;
            left -= step;
            self.wake.notify_waiters();
            settle().await;
        }
    }
}

#[async_trait]
impl Clock for FakeClock {
    fn now(&self) -> Duration {
        *self.now.lock().unwrap()
    }
    async fn sleep(&self, d: Duration) {
        let deadline = self.now() + d;
        loop {
            let notified = self.wake.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.now() >= deadline {
                return;
            }
            notified.await;
        }
    }
}

async fn settle() {
    for _ in 0..40 {
        tokio::task::yield_now().await;
    }
}

type Rx = mpsc::UnboundedReceiver<Result<Option<Event>, String>>;
type Tx = mpsc::UnboundedSender<Result<Option<Event>, String>>;

struct Chan(Rx);

#[async_trait]
impl EventStream for Chan {
    async fn next(&mut self) -> Result<Option<Event>, String> {
        match self.0.recv().await {
            Some(r) => r,
            None => Ok(None),
        }
    }
}

#[derive(Default)]
struct FakeFeed {
    opens: Mutex<VecDeque<Result<Rx, String>>>,
    open_resumes: Mutex<Vec<Option<u64>>>,
    poll_times: Mutex<Vec<Duration>>,
    poll_result: Mutex<Option<Vec<(String, u64)>>>,
    clock: Mutex<Option<Arc<FakeClock>>>,
}

impl FakeFeed {
    fn queue_stream(&self) -> Tx {
        let (tx, rx) = mpsc::unbounded_channel();
        self.opens.lock().unwrap().push_back(Ok(rx));
        tx
    }
    fn queue_open_error(&self) {
        self.opens.lock().unwrap().push_back(Err("down".into()));
    }
    fn set_poll(&self, r: Option<Vec<(String, u64)>>) {
        *self.poll_result.lock().unwrap() = r;
    }
    fn polls(&self) -> usize {
        self.poll_times.lock().unwrap().len()
    }
}

#[async_trait]
impl Feed for FakeFeed {
    async fn open(&self, resume: Option<u64>) -> Result<Box<dyn EventStream>, String> {
        self.open_resumes.lock().unwrap().push(resume);
        match self.opens.lock().unwrap().pop_front() {
            Some(Ok(rx)) => Ok(Box::new(Chan(rx))),
            Some(Err(e)) => Err(e),
            None => Err("down".into()),
        }
    }
    async fn poll(&self) -> Result<Vec<(String, u64)>, String> {
        let now = self.clock.lock().unwrap().as_ref().unwrap().now();
        self.poll_times.lock().unwrap().push(now);
        self.poll_result
            .lock()
            .unwrap()
            .clone()
            .ok_or("down".into())
    }
}

#[derive(Default)]
struct FakeRunner {
    paused: Mutex<Vec<String>>,
    runs: Mutex<Vec<String>>,
    fail: Mutex<bool>,
}

impl FakeRunner {
    fn runs(&self) -> Vec<String> {
        self.runs.lock().unwrap().clone()
    }
}

#[async_trait]
impl Runner for FakeRunner {
    async fn worktrees(&self) -> Vec<String> {
        vec!["a".into(), "b".into(), "a".into()]
    }
    async fn paused(&self, wt: &str) -> bool {
        self.paused.lock().unwrap().iter().any(|p| p == wt)
    }
    async fn run(&self, wt: &str) -> Result<(), String> {
        self.runs.lock().unwrap().push(wt.to_owned());
        if *self.fail.lock().unwrap() {
            Err("boom".into())
        } else {
            Ok(())
        }
    }
}

struct Rig {
    clock: Arc<FakeClock>,
    feed: Arc<FakeFeed>,
    runner: Arc<FakeRunner>,
    stop: watch::Sender<bool>,
    mode: watch::Receiver<Mode>,
    cloud: mpsc::UnboundedSender<()>,
    task: tokio::task::JoinHandle<()>,
}

impl Rig {
    /// Starts a driver. The caller queues streams and sets the poll result first.
    async fn start(feed: Arc<FakeFeed>) -> Self {
        let clock = FakeClock::new();
        *feed.clock.lock().unwrap() = Some(clock.clone());
        let runner = Arc::new(FakeRunner::default());
        let (stop, stop_rx) = watch::channel(false);
        let (mode_tx, mode) = watch::channel(Mode::Starting);
        let (cloud, cloud_rx) = mpsc::unbounded_channel();
        let task = tokio::spawn(drive(
            feed.clone(),
            runner.clone(),
            clock.clone(),
            Timing {
                debounce: DEBOUNCE,
                poll_interval: POLL,
            },
            stop_rx,
            mode_tx,
            Some(cloud_rx),
        ));
        let rig = Self {
            clock,
            feed,
            runner,
            stop,
            mode,
            cloud,
            task,
        };
        // The start-up baseline poll opens one window; let it run and forget it.
        rig.clock.advance(Duration::from_secs(3)).await;
        rig.runner.runs.lock().unwrap().clear();
        rig
    }
}

fn master(n: u64) -> Event {
    Event::Ref {
        name: "refs/heads/master".into(),
        resume: n,
    }
}

fn base_poll() -> Option<Vec<(String, u64)>> {
    Some(vec![("refs/heads/master".into(), 1)])
}

#[tokio::test]
async fn a_burst_of_events_inside_the_window_runs_one_update() {
    let feed = Arc::new(FakeFeed::default());
    let tx = feed.queue_stream();
    feed.set_poll(base_poll());
    let rig = Rig::start(feed).await;
    assert_eq!(*rig.mode.borrow(), Mode::Stream);

    for i in 1..=5 {
        tx.send(Ok(Some(master(i)))).unwrap();
    }
    rig.clock.advance(Duration::from_secs(1)).await;
    assert!(rig.runner.runs().is_empty(), "still inside the window");
    rig.clock.advance(Duration::from_secs(2)).await;
    // One run per unpaused worktree; the duplicate worktree name is run once.
    assert_eq!(rig.runner.runs(), ["a", "b"]);
    rig.clock.advance(Duration::from_secs(60)).await;
    assert_eq!(rig.runner.runs().len(), 2, "no further run without events");

    tx.send(Ok(Some(master(6)))).unwrap();
    rig.clock.advance(Duration::from_secs(3)).await;
    assert_eq!(
        rig.runner.runs().len(),
        4,
        "a later event opens a new window"
    );
}

#[tokio::test]
async fn events_for_other_refs_or_other_users_do_nothing() {
    let feed = Arc::new(FakeFeed::default());
    let tx = feed.queue_stream();
    feed.set_poll(base_poll());
    let rig = Rig::start(feed).await;

    for name in [
        "refs/heads/feature",
        "refs/heads/masterful",
        "refs/users/bob/drafts/x",
        "refs/users/bob/heads/master",
        "refs/heads/release/",
        "refs/heads/release/../x",
        "refs/tags/v1",
    ] {
        tx.send(Ok(Some(Event::Ref {
            name: name.into(),
            resume: 1,
        })))
        .unwrap();
    }
    tx.send(Ok(Some(Event::Cloud { own: false }))).unwrap();
    rig.clock.advance(Duration::from_secs(30)).await;
    assert!(rig.runner.runs().is_empty());

    tx.send(Ok(Some(Event::Ref {
        name: "refs/heads/release/1.0".into(),
        resume: 2,
    })))
    .unwrap();
    rig.clock.advance(Duration::from_secs(3)).await;
    assert_eq!(rig.runner.runs().len(), 2);

    tx.send(Ok(Some(Event::Cloud { own: true }))).unwrap();
    rig.clock.advance(Duration::from_secs(3)).await;
    assert_eq!(rig.runner.runs().len(), 4, "own drafts count");
}

#[tokio::test]
async fn a_cloud_wake_opens_a_window_even_while_the_driver_polls() {
    let feed = Arc::new(FakeFeed::default());
    feed.queue_open_error();
    feed.set_poll(base_poll());
    let rig = Rig::start(feed).await;
    assert_eq!(*rig.mode.borrow(), Mode::Poll, "no ref stream is open");

    // A burst of wake-ups inside one window runs the update once.
    for _ in 0..4 {
        rig.cloud.send(()).unwrap();
    }
    rig.clock.advance(Duration::from_secs(1)).await;
    assert!(rig.runner.runs().is_empty(), "still inside the window");
    rig.clock.advance(Duration::from_secs(2)).await;
    assert_eq!(rig.runner.runs(), ["a", "b"]);

    // Once its sender is gone the channel is dropped without ending or spinning the driver.
    let Rig {
        cloud,
        clock,
        runner,
        task,
        stop,
        ..
    } = rig;
    drop(cloud);
    clock.advance(Duration::from_secs(60)).await;
    assert_eq!(runner.runs().len(), 2);
    stop.send(true).unwrap();
    settle().await;
    assert!(task.is_finished());
}

#[tokio::test]
async fn poll_fallback_starts_on_stream_failure_and_the_stream_resumes_with_the_cursor() {
    let feed = Arc::new(FakeFeed::default());
    let tx = feed.queue_stream();
    feed.set_poll(base_poll());
    let rig = Rig::start(feed.clone()).await;
    let polls_before = feed.polls();

    tx.send(Ok(Some(master(7)))).unwrap();
    rig.clock.advance(Duration::from_secs(3)).await;
    rig.runner.runs.lock().unwrap().clear();

    // The stream dies after a long life; the next stream is available and trunk moved meanwhile.
    rig.clock.advance(POLL).await;
    let tx2 = feed.queue_stream();
    tx.send(Err("reset".into())).unwrap();
    feed.set_poll(Some(vec![("refs/heads/master".into(), 2)]));
    rig.clock.advance(Duration::from_secs(1)).await;
    assert!(feed.polls() > polls_before, "polling starts at once");
    assert_eq!(
        *feed.open_resumes.lock().unwrap().last().unwrap(),
        Some(7),
        "the resubscribe carries the cursor"
    );
    rig.clock.advance(Duration::from_secs(3)).await;
    assert_eq!(rig.runner.runs().len(), 2, "the poll saw trunk move");
    assert_eq!(*rig.mode.borrow(), Mode::Stream);

    // The new stream is live.
    tx2.send(Ok(Some(master(8)))).unwrap();
    rig.clock.advance(Duration::from_secs(3)).await;
    assert_eq!(rig.runner.runs().len(), 4);
}

#[tokio::test]
async fn polling_backs_off_by_doubling_and_resets_on_success() {
    let feed = Arc::new(FakeFeed::default());
    feed.queue_open_error();
    feed.set_poll(None);
    let rig = Rig::start(feed.clone()).await;
    assert_eq!(*rig.mode.borrow(), Mode::Poll);

    rig.clock.advance(POLL * 9).await;
    let times: Vec<u64> = feed
        .poll_times
        .lock()
        .unwrap()
        .iter()
        .map(|d| d.as_secs())
        .collect();
    // Immediately, then 300 s, 600 s and 1200 s apart.
    assert_eq!(times[0], 0);
    assert_eq!(times[1] - times[0], 300);
    assert_eq!(times[2] - times[1], 600);
    assert_eq!(times[3] - times[2], 1200);

    // A success runs the update (fetch-only mode is not blocked) and resets the wait.
    feed.set_poll(base_poll());
    rig.clock.advance(Duration::from_secs(2400)).await;
    assert!(!rig.runner.runs().is_empty());
    let at = feed.poll_times.lock().unwrap().clone();
    let n = at.len();
    assert_eq!(at[n - 1] - at[n - 2], POLL);
}

#[tokio::test]
async fn a_stream_that_dies_at_once_backs_off_instead_of_spinning() {
    let feed = Arc::new(FakeFeed::default());
    for _ in 0..20 {
        let tx = feed.queue_stream();
        tx.send(Ok(None)).unwrap();
    }
    feed.set_poll(base_poll());
    let rig = Rig::start(feed.clone()).await;
    rig.clock.advance(Duration::from_secs(60)).await;
    assert!(feed.polls() <= 3, "polls: {}", feed.polls());
    drop(rig);
}

#[tokio::test]
async fn a_paused_worktree_is_skipped_and_a_failing_run_does_not_stop_the_driver() {
    let feed = Arc::new(FakeFeed::default());
    let tx = feed.queue_stream();
    feed.set_poll(base_poll());
    let rig = Rig::start(feed).await;
    rig.runner.paused.lock().unwrap().push("a".into());
    *rig.runner.fail.lock().unwrap() = true;

    tx.send(Ok(Some(master(1)))).unwrap();
    rig.clock.advance(Duration::from_secs(3)).await;
    assert_eq!(rig.runner.runs(), ["b"]);

    rig.runner.paused.lock().unwrap().clear();
    tx.send(Ok(Some(master(2)))).unwrap();
    rig.clock.advance(Duration::from_secs(3)).await;
    assert_eq!(rig.runner.runs(), ["b", "a", "b"]);
}

#[tokio::test]
async fn stop_ends_the_driver() {
    let feed = Arc::new(FakeFeed::default());
    feed.queue_stream();
    feed.set_poll(base_poll());
    let rig = Rig::start(feed).await;
    rig.stop.send(true).unwrap();
    settle().await;
    assert!(rig.task.is_finished());
}

#[test]
fn live_ref_names_are_master_and_release_only() {
    for ok in [
        "master",
        "refs/heads/master",
        "release/1.0",
        "refs/heads/release/a/b",
    ] {
        assert!(is_live_ref(ok), "{ok}");
    }
    for bad in [
        "",
        "main",
        "refs/heads/feature",
        "refs/users/me/drafts/x",
        "release/",
        "release/../master",
        "refs/heads/master\n",
    ] {
        assert!(!is_live_ref(bad), "{bad:?}");
    }
}

#[test]
fn poll_wait_doubles_and_is_capped_at_one_hour() {
    assert_eq!(poll_wait(POLL, 0), POLL);
    assert_eq!(poll_wait(POLL, 1), POLL * 2);
    assert_eq!(poll_wait(POLL, 3), POLL * 8);
    assert_eq!(poll_wait(POLL, 4), POLL_BACKOFF_CAP);
    assert_eq!(poll_wait(POLL, u32::MAX), POLL_BACKOFF_CAP);
}
