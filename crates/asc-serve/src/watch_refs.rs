//! The `WatchRefs` ref-update stream (PLAN U76, SPEC §21.3.8).
//!
//! The stream reads the `ref_log` that every `UpdateRefs` commit appends to (U43b), in sequence
//! order, so the cursor is the sequence number: a resume delivers exactly the rows after it, with
//! no gap and no duplicate. Every row is filtered before it leaves:
//!
//! * a shared ref (under `refs/heads/`, `refs/tags/`, `refs/pull/` or `refs/snapshots/`) goes to
//!   any caller with read access, which the service layer checked before the handler ran;
//! * a user ref (`refs/users/<owner>/heads/<n>`) goes to its owner only, renamed to
//!   `refs/heads/<n>` as `ListRefs` shows it; another user's draft ref is dropped before the
//!   patterns are looked at, so its name never reaches the wire;
//! * anything else is dropped, and so is a name no pattern matches.
//!
//! The reader polls. A row is delivered once its `at` second is `settle` seconds old, because
//! sequence numbers are taken when a row is inserted and commits may land out of order on a
//! multi-writer database; waiting keeps the cursor from skipping a slow commit.
//!
//! Open streams are capped per process and per principal (`[limits] watch_streams` and
//! `watch_streams_per_principal`), so one caller cannot take every slot. The per-principal table
//! is bounded and evicts idle entries. An anonymous caller is charged to its network peer instead
//! (the address admission reads, `limits::current_peer`): anonymous callers together may hold
//! `watch_streams_anonymous` of the slots and one peer `watch_streams_per_peer`, so one anonymous
//! client cannot lock out the others or any verified caller. A stream ends
//! cleanly (the client resumes from its last cursor) after `watch_max_lifetime`, or after
//! `watch_idle_timeout` with no event delivered. The service layer re-checks the caller's
//! credential while the stream is open and closes it when the token expires.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use asc_core::id::RefTarget;
use asc_rpc::IdMode;
use asc_rpc::server::{CallCtx, RpcStream, WatchHandler, wire};
use asc_rpc::watch_refs::{Cursor, Patterns, parse_request};
use futures::StreamExt;
use futures::stream;
use sqlx::{AnyPool, Row};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio::time::Instant;
use tonic::Status;

use crate::authz::UserIdentity;
use crate::config::Limits;
use crate::limits::current_peer;
use crate::refpolicy::{self, SHARED_ROOTS, USER_NS};
use crate::registry::{Extensions, Registry, RegistryError};
use crate::update::internal;

/// Open streams the process serves at once.
pub const MAX_WATCHERS: usize = 1024;
/// Open streams one principal holds at once.
pub const MAX_WATCHERS_PER_PRINCIPAL: usize = 8;
/// Longest a stream lives before the server ends it (the client resumes).
pub const MAX_LIFETIME: Duration = Duration::from_secs(60 * 60);
/// Longest a stream stays open with no event delivered.
pub const IDLE_TIMEOUT: Duration = Duration::from_secs(10 * 60);
/// Open streams all anonymous callers hold together.
pub const MAX_ANONYMOUS_WATCHERS: usize = 256;
/// Open streams one anonymous peer address holds.
pub const MAX_WATCHERS_PER_PEER: usize = 4;
/// Principals the per-principal table tracks; idle entries are evicted beyond it.
pub const MAX_TRACKED_PRINCIPALS: usize = 10_000;
/// Rows read per poll.
const BATCH: i64 = 256;
/// How often an idle stream looks for new rows.
const POLL: Duration = Duration::from_secs(1);
/// Seconds a row must age before it is delivered.
const SETTLE_SECS: i64 = 2;

/// The name a caller sees for the stored ref `stored`, or `None` when the caller may not see the
/// event or no pattern selects it.
fn visible(stored: &str, owner: Option<i64>, patterns: &Patterns) -> Option<String> {
    if stored.starts_with(USER_NS) {
        let (id, branch) = refpolicy::parse_user_ref(stored)?;
        if owner != Some(id) {
            return None;
        }
        let shown = format!("refs/heads/{branch}");
        return (patterns.matches(&shown) || patterns.matches(stored)).then_some(shown);
    }
    if !SHARED_ROOTS.iter().any(|r| stored.starts_with(r)) {
        return None;
    }
    patterns.matches(stored).then(|| stored.to_owned())
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

fn target(blob: Option<Vec<u8>>) -> Result<Option<wire::RefTarget>, ()> {
    blob.map(|b| {
        RefTarget::from_bytes(&b)
            .map(|t| wire::RefTarget::from(&t))
            .map_err(drop)
    })
    .transpose()
}

/// The stream limits (`[limits] watch_*`).
#[derive(Clone, Copy, Debug)]
pub struct WatchLimits {
    pub streams: usize,
    pub per_principal: usize,
    pub anonymous: usize,
    pub per_peer: usize,
    pub max_lifetime: Duration,
    pub idle_timeout: Duration,
}

impl Default for WatchLimits {
    fn default() -> Self {
        Self {
            streams: MAX_WATCHERS,
            per_principal: MAX_WATCHERS_PER_PRINCIPAL,
            anonymous: MAX_ANONYMOUS_WATCHERS,
            per_peer: MAX_WATCHERS_PER_PEER,
            max_lifetime: MAX_LIFETIME,
            idle_timeout: IDLE_TIMEOUT,
        }
    }
}

impl WatchLimits {
    /// The limits `asc-serve.toml` sets.
    pub fn from_config(l: &Limits) -> Self {
        let n = |v: u32| usize::try_from(v).unwrap_or(usize::MAX);
        Self {
            streams: n(l.watch_streams),
            per_principal: n(l.watch_streams_per_principal),
            anonymous: n(l.watch_streams_anonymous),
            per_peer: n(l.watch_streams_per_peer),
            max_lifetime: Duration::from_secs(l.watch_max_lifetime.0),
            idle_timeout: Duration::from_secs(l.watch_idle_timeout.0),
        }
    }
}

/// One semaphore per principal, in a table of bounded size.
struct PrincipalSlots {
    per: usize,
    max_keys: usize,
    table: Mutex<Table>,
}

#[derive(Default)]
struct Table {
    clock: u64,
    map: HashMap<String, (Arc<Semaphore>, u64)>,
}

impl PrincipalSlots {
    /// `max_keys` is raised above `streams` so a full table always holds an idle entry: every
    /// entry with an open stream needs one of the `streams` global slots.
    fn new(per: usize, streams: usize, max_keys: usize) -> Self {
        Self {
            per,
            max_keys: max_keys.max(streams.saturating_add(1)),
            table: Mutex::new(Table::default()),
        }
    }

    /// A slot for `who`, or `None` when `who` holds its share already.
    fn acquire(&self, who: &str) -> Option<OwnedSemaphorePermit> {
        let mut t = self.table.lock().unwrap_or_else(PoisonError::into_inner);
        t.clock += 1;
        let stamp = t.clock;
        if let Some((sem, last)) = t.map.get_mut(who) {
            *last = stamp;
            return sem.clone().try_acquire_owned().ok();
        }
        if t.map.len() >= self.max_keys {
            // Only an entry with no open stream goes; the oldest one first.
            let per = self.per;
            let oldest = t
                .map
                .iter()
                .filter(|(_, (s, _))| s.available_permits() >= per)
                .min_by_key(|(_, (_, last))| *last)
                .map(|(k, _)| k.clone())?;
            t.map.remove(&oldest);
        }
        let sem = Arc::new(Semaphore::new(self.per));
        let permit = sem.clone().try_acquire_owned().ok()?;
        t.map.insert(who.to_owned(), (sem, stamp));
        Some(permit)
    }

    #[cfg(test)]
    fn tracked(&self) -> usize {
        self.table
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .map
            .len()
    }
}

/// One open stream.
struct Reader {
    pool: AnyPool,
    repo_id: i64,
    owner: Option<i64>,
    patterns: Patterns,
    cursor: i64,
    queue: VecDeque<wire::RefEvent>,
    poll: Duration,
    settle: i64,
    failed: bool,
    /// When the stream ends however busy it is.
    expires: Instant,
    idle_timeout: Duration,
    /// When the last event went out (the open time before the first).
    last_event: Instant,
    _slot: OwnedSemaphorePermit,
    _key_slot: OwnedSemaphorePermit,
    _anonymous_slot: Option<OwnedSemaphorePermit>,
}

impl Reader {
    /// Reads one batch into the queue. `true` when the batch was full, so more may be waiting.
    async fn fill(&mut self) -> Result<bool, Status> {
        let rows = sqlx::query(
            "SELECT seq, name, old, new, at FROM ref_log WHERE repo_id = $1 AND seq > $2 \
             ORDER BY seq LIMIT $3",
        )
        .bind(self.repo_id)
        .bind(self.cursor)
        .bind(BATCH)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| internal("ref_log", &e))?;
        let full = i64::try_from(rows.len()).is_ok_and(|n| n == BATCH);
        let cutoff = now() - self.settle;
        for row in &rows {
            let at: i64 = row.try_get("at").map_err(|e| internal("ref_log", &e))?;
            if at > cutoff {
                // Younger rows wait, and so does everything after them: the cursor never skips.
                return Ok(false);
            }
            let seq: i64 = row.try_get("seq").map_err(|e| internal("ref_log", &e))?;
            self.cursor = seq;
            let name: String = row.try_get("name").map_err(|e| internal("ref_log", &e))?;
            let Some(shown) = visible(&name, self.owner, &self.patterns) else {
                continue;
            };
            let old: Option<Vec<u8>> = row.try_get("old").map_err(|e| internal("ref_log", &e))?;
            let new: Option<Vec<u8>> = row.try_get("new").map_err(|e| internal("ref_log", &e))?;
            let (Ok(old), Ok(new)) = (target(old), target(new)) else {
                tracing::error!(seq, "a ref_log row holds an undecodable target; skipped");
                continue;
            };
            let Ok(seq) = u64::try_from(seq) else {
                continue;
            };
            self.queue.push_back(wire::RefEvent {
                name: shown,
                old,
                new,
                resume: Cursor(seq).to_bytes().into(),
            });
        }
        Ok(full)
    }

    /// When the stream must end if nothing else happens: its lifetime, or the idle timeout.
    fn deadline(&self) -> Instant {
        self.expires.min(self.last_event + self.idle_timeout)
    }
}

/// The `WatchRefs` handler.
pub struct ServerWatch {
    registry: Arc<Registry>,
    slots: Arc<Semaphore>,
    principals: Arc<PrincipalSlots>,
    /// Anonymous callers: their share of the streams, and a table keyed by peer address.
    anonymous: Arc<Semaphore>,
    peers: Arc<PrincipalSlots>,
    limits: WatchLimits,
    poll: Duration,
    settle: i64,
}

impl ServerWatch {
    pub fn new(registry: Arc<Registry>) -> Self {
        Self::with_limits(registry, WatchLimits::default())
    }

    pub fn with_limits(registry: Arc<Registry>, limits: WatchLimits) -> Self {
        Self {
            registry,
            slots: Arc::new(Semaphore::new(limits.streams)),
            principals: Arc::new(PrincipalSlots::new(
                limits.per_principal,
                limits.streams,
                MAX_TRACKED_PRINCIPALS,
            )),
            anonymous: Arc::new(Semaphore::new(limits.anonymous.min(limits.streams))),
            peers: Arc::new(PrincipalSlots::new(
                limits.per_peer,
                limits.anonymous,
                MAX_TRACKED_PRINCIPALS,
            )),
            limits,
            poll: POLL,
            settle: SETTLE_SECS,
        }
    }

    /// Sets the idle poll interval and the age (seconds) a row must reach before it is sent.
    #[must_use]
    pub fn with_timing(mut self, poll: Duration, settle_secs: i64) -> Self {
        self.poll = poll.max(Duration::from_millis(1));
        self.settle = settle_secs.max(0);
        self
    }

    /// Caps the open streams (tests).
    #[must_use]
    pub fn with_capacity(mut self, streams: usize) -> Self {
        self.slots = Arc::new(Semaphore::new(streams));
        self
    }
}

/// Whether the authorizer admitted the caller without a credential of its own (a public
/// repository): user id 0.
fn is_anonymous(p: &asc_rpc::auth::Principal) -> bool {
    UserIdentity::of(p).is_some_and(|i| i.is_anonymous())
}

fn registry_status(e: RegistryError) -> Status {
    match e {
        RegistryError::NotFound(_) => Status::not_found("no such repository"),
        e => internal("registry", &e),
    }
}

#[async_trait]
impl WatchHandler for ServerWatch {
    async fn watch_refs(
        &self,
        ctx: &CallCtx,
        req: wire::WatchRefsRequest,
    ) -> Result<RpcStream<wire::RefEvent>, Status> {
        let watch = parse_request(&req)?;
        if ctx.id_mode != IdMode::Asc {
            return Err(Status::unimplemented(
                "WatchRefs needs the asc id mode; git ids are not carried on this stream",
            ));
        }
        let entry = self
            .registry
            .open(&ctx.repo)
            .await
            .map_err(registry_status)?;
        let pool = self.registry.pool().clone();
        // A verified caller is charged to its principal. An anonymous one has no identity of its
        // own, so it is charged to its peer address and to the anonymous share of the slots.
        let (key_slot, anonymous_slot) = if is_anonymous(&ctx.principal) {
            let key = format!("{:?}", current_peer());
            let by_peer = self.peers.acquire(&key).ok_or_else(|| {
                Status::resource_exhausted("too many open ref streams from this address")
            })?;
            let pool = self.anonymous.clone().try_acquire_owned().map_err(|_| {
                Status::resource_exhausted("too many open ref streams for anonymous callers")
            })?;
            (by_peer, Some(pool))
        } else {
            let by_principal = self.principals.acquire(&ctx.principal.id).ok_or_else(|| {
                Status::resource_exhausted("too many open ref streams for this principal")
            })?;
            (by_principal, None)
        };
        let slot = self
            .slots
            .clone()
            .try_acquire_owned()
            .map_err(|_| Status::resource_exhausted("too many open ref streams"))?;
        let newest: i64 =
            sqlx::query_scalar("SELECT COALESCE(MAX(seq), 0) FROM ref_log WHERE repo_id = $1")
                .bind(entry.key.0)
                .fetch_one(&pool)
                .await
                .map_err(|e| internal("ref_log", &e))?;
        let cursor = match watch.resume {
            None => newest,
            Some(Cursor(c)) => {
                let c =
                    i64::try_from(c).map_err(|_| Status::invalid_argument("bad resume cursor"))?;
                if c > newest {
                    return Err(Status::invalid_argument(
                        "resume cursor is ahead of the log",
                    ));
                }
                c
            }
        };
        let opened = Instant::now();
        let reader = Reader {
            pool,
            repo_id: entry.key.0,
            owner: refpolicy::owner_of(&ctx.principal),
            patterns: watch.patterns,
            cursor,
            queue: VecDeque::new(),
            poll: self.poll,
            settle: self.settle,
            failed: false,
            expires: opened + self.limits.max_lifetime,
            idle_timeout: self.limits.idle_timeout,
            last_event: opened,
            _slot: slot,
            _key_slot: key_slot,
            _anonymous_slot: anonymous_slot,
        };
        Ok(stream::unfold(reader, |mut r| async move {
            loop {
                if let Some(ev) = r.queue.pop_front() {
                    r.last_event = Instant::now();
                    return Some((Ok(ev), r));
                }
                if r.failed {
                    return None;
                }
                // A clean end: the client resumes from its last cursor.
                let left = r.deadline().saturating_duration_since(Instant::now());
                if left.is_zero() {
                    return None;
                }
                match r.fill().await {
                    Ok(true) => {}
                    Ok(false) => {
                        if r.queue.is_empty() {
                            tokio::time::sleep(r.poll.min(left)).await;
                        }
                    }
                    Err(e) => {
                        r.failed = true;
                        return Some((Err(e), r));
                    }
                }
            }
        })
        .boxed())
    }
}

/// The one registration line of this unit (called from `lib.rs`).
pub fn register(ext: &Extensions, registry: &Arc<Registry>, limits: &Limits) {
    let watch = ServerWatch::with_limits(registry.clone(), WatchLimits::from_config(limits));
    ext.handlers
        .set_watch(Arc::new(watch))
        .expect("the watch handler group is registered once");
}

#[cfg(test)]
mod tests {
    use asc_core::id::ChangesetId;
    use asc_core::path::RefName;
    use asc_rpc::auth::Principal;
    use asc_rpc::server::RepoName;
    use asc_store::refs::{Actor, Expect, RefTxn, RefUpdate, Refs};
    use uuid::Uuid;

    use super::*;
    use crate::ingest::testutil::{registry, repo};
    use crate::registry::RepoEntry;

    fn cs(n: u8) -> RefTarget {
        RefTarget::Changeset(ChangesetId::from_bytes([n; 32]))
    }

    fn ctx(who: &str) -> CallCtx {
        CallCtx {
            repo: RepoName::parse("o/r").unwrap(),
            id_mode: IdMode::Asc,
            principal: Principal { id: who.into() },
            op_id: None,
            client: None,
        }
    }

    struct Fx {
        r: Arc<Registry>,
        e: Arc<RepoEntry>,
        watch: ServerWatch,
    }

    async fn setup() -> Fx {
        let r = registry().await;
        let e = repo(&r).await;
        let watch = ServerWatch::new(r.clone()).with_timing(Duration::from_millis(5), 0);
        Fx { r, e, watch }
    }

    impl Fx {
        /// Moves `name` to `to` (`None` deletes), whatever it held.
        async fn set(&self, name: &str, to: Option<RefTarget>) {
            let txn = RefTxn {
                updates: vec![RefUpdate {
                    name: RefName::new(name.as_bytes()).unwrap(),
                    expect: Expect::Any,
                    new: to,
                    expires_at: None,
                }],
                actor: Actor {
                    id: "alice".into(),
                    via: "test".into(),
                },
                reason: "t".into(),
                op_id: Uuid::new_v4(),
            };
            self.r.refs().update(self.e.key, &txn).await.unwrap();
        }

        async fn open(
            &self,
            who: &str,
            patterns: &[&str],
            resume: Option<u64>,
        ) -> Result<Rx, Status> {
            let req = wire::WatchRefsRequest {
                repo: Some(wire::RepoRef {
                    name: "o/r".into(),
                    id_mode: 0,
                }),
                patterns: patterns.iter().map(|s| (*s).to_owned()).collect(),
                resume: resume.map_or_else(Default::default, |c| Cursor(c).to_bytes().into()),
            };
            Ok(pump(self.watch.watch_refs(&ctx(who), req).await?))
        }
    }

    type Rx = tokio::sync::mpsc::Receiver<wire::RefEvent>;

    /// A task drives the stream, so a timeout in `take` never cancels a query: the test database
    /// is in memory and dies with a dropped connection.
    fn pump(mut s: RpcStream<wire::RefEvent>) -> Rx {
        let (tx, rx) = tokio::sync::mpsc::channel(1024);
        tokio::spawn(async move {
            while let Some(Ok(e)) = s.next().await {
                if tx.send(e).await.is_err() {
                    break;
                }
            }
        });
        rx
    }

    /// The next `n` events, or fewer if the stream stays quiet for a moment.
    async fn take(s: &mut Rx, n: usize) -> Vec<wire::RefEvent> {
        let mut out = Vec::new();
        while out.len() < n {
            match tokio::time::timeout(Duration::from_millis(300), s.recv()).await {
                Ok(Some(e)) => out.push(e),
                _ => break,
            }
        }
        out
    }

    fn seq(e: &wire::RefEvent) -> u64 {
        Cursor::from_bytes(&e.resume).unwrap().0
    }

    #[tokio::test]
    async fn a_burst_arrives_in_order_and_resume_replays_without_gap_or_duplicate() {
        let f = setup().await;
        let mut s = f.open("alice", &[], None).await.unwrap();
        for n in 1..=20u8 {
            f.set("refs/heads/master", Some(cs(n))).await;
        }
        let got = take(&mut s, 20).await;
        assert_eq!(got.len(), 20);
        assert!(got.windows(2).all(|w| seq(&w[0]) < seq(&w[1])));
        assert!(got[0].old.is_none(), "the first update created the ref");
        for (i, e) in got.iter().enumerate().skip(1) {
            let prev = RefTarget::try_from(got[i - 1].new.as_ref().unwrap()).unwrap();
            assert_eq!(RefTarget::try_from(e.old.as_ref().unwrap()).unwrap(), prev);
        }
        // Resume after the 7th event: events 8 to 20, once each.
        let mut again = f.open("alice", &[], Some(seq(&got[6]))).await.unwrap();
        let rest = take(&mut again, 20).await;
        let want: Vec<u64> = got[7..].iter().map(seq).collect();
        assert_eq!(rest.iter().map(seq).collect::<Vec<_>>(), want);
        // A live update after the replay follows without a duplicate.
        f.set("refs/heads/master", Some(cs(99))).await;
        let live = take(&mut again, 2).await;
        assert_eq!(live.len(), 1);
        assert!(seq(&live[0]) > seq(&got[19]));
    }

    #[tokio::test]
    async fn the_default_patterns_pass_master_and_release_only() {
        let f = setup().await;
        let mut s = f.open("alice", &[], Some(0)).await.unwrap();
        f.set("refs/heads/feature", Some(cs(1))).await;
        f.set("refs/heads/release/1.0", Some(cs(2))).await;
        f.set("refs/heads/master", Some(cs(3))).await;
        f.set("refs/tags/v1", Some(cs(4))).await;
        let names: Vec<_> = take(&mut s, 5).await.into_iter().map(|e| e.name).collect();
        assert_eq!(names, ["refs/heads/release/1.0", "refs/heads/master"]);
        let mut t = f.open("alice", &["refs/tags/*"], Some(0)).await.unwrap();
        let names: Vec<_> = take(&mut t, 5).await.into_iter().map(|e| e.name).collect();
        assert_eq!(names, ["refs/tags/v1"]);
    }

    #[tokio::test]
    async fn another_users_draft_never_appears_and_the_owner_sees_it_renamed() {
        let f = setup().await;
        let mut alice = f.open("7:alice", &["*"], Some(0)).await.unwrap();
        let mut bob = f.open("8:bob", &["*"], Some(0)).await.unwrap();
        let mut anon = f
            .open("not-a-user", &["*", "refs/users/*"], Some(0))
            .await
            .unwrap();
        f.set("refs/users/7/heads/wip", Some(cs(1))).await;
        f.set("refs/users/8/heads/mine", Some(cs(2))).await;
        f.set("refs/heads/master", Some(cs(3))).await;
        let a: Vec<_> = take(&mut alice, 5)
            .await
            .into_iter()
            .map(|e| e.name)
            .collect();
        assert_eq!(a, ["refs/heads/wip", "refs/heads/master"]);
        let b: Vec<_> = take(&mut bob, 5)
            .await
            .into_iter()
            .map(|e| e.name)
            .collect();
        assert_eq!(b, ["refs/heads/mine", "refs/heads/master"]);
        let n: Vec<_> = take(&mut anon, 5)
            .await
            .into_iter()
            .map(|e| e.name)
            .collect();
        assert_eq!(
            n,
            ["refs/heads/master"],
            "a principal with no user id has no drafts"
        );
        // A user-namespace pattern matches the caller's own refs only.
        let mut own = f.open("7:alice", &["refs/users/*"], Some(0)).await.unwrap();
        let o = take(&mut own, 5).await;
        assert_eq!(o.len(), 1);
        assert_eq!(o[0].name, "refs/heads/wip");
        assert!(!format!("{o:?}").contains("refs/users/8"));
    }

    #[tokio::test]
    async fn unreadable_names_do_not_leak_through_the_wire_or_the_cursor_gap() {
        let f = setup().await;
        let mut s = f.open("8:bob", &["*"], Some(0)).await.unwrap();
        f.set("refs/users/7/heads/secret-name", Some(cs(1))).await;
        f.set("refs/internal/x", Some(cs(2))).await;
        f.set("refs/heads/master", Some(cs(3))).await;
        let got = take(&mut s, 3).await;
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].name, "refs/heads/master");
        assert!(!format!("{got:?}").contains("secret"));
    }

    #[tokio::test]
    async fn invalid_requests_are_refused() {
        let f = setup().await;
        let many: Vec<&str> = vec!["a"; 9];
        let e = f.open("alice", &many, None).await.err().unwrap();
        assert_eq!(e.code(), tonic::Code::InvalidArgument);
        let long = "a".repeat(129);
        let e = f.open("alice", &[long.as_str()], None).await.err().unwrap();
        assert_eq!(e.code(), tonic::Code::InvalidArgument);
        // A cursor past the end of the log is not ours.
        let e = f.open("alice", &[], Some(1000)).await.err().unwrap();
        assert_eq!(e.code(), tonic::Code::InvalidArgument);
        let mut git = ctx("alice");
        git.id_mode = IdMode::Git;
        let req = wire::WatchRefsRequest::default();
        let e = f.watch.watch_refs(&git, req).await.err().unwrap();
        assert_eq!(e.code(), tonic::Code::Unimplemented);
    }

    #[tokio::test]
    async fn open_streams_are_capped_and_a_closed_stream_frees_its_slot() {
        let f = setup().await;
        let watch = ServerWatch::new(f.r.clone()).with_capacity(1);
        let req = || wire::WatchRefsRequest {
            repo: Some(wire::RepoRef {
                name: "o/r".into(),
                id_mode: 0,
            }),
            ..Default::default()
        };
        let first = watch.watch_refs(&ctx("alice"), req()).await.unwrap();
        let e = watch.watch_refs(&ctx("alice"), req()).await.err().unwrap();
        assert_eq!(e.code(), tonic::Code::ResourceExhausted);
        drop(first);
        assert!(watch.watch_refs(&ctx("alice"), req()).await.is_ok());
    }

    fn limits(streams: usize, per_principal: usize, life_ms: u64, idle_ms: u64) -> WatchLimits {
        WatchLimits {
            streams,
            per_principal,
            anonymous: streams,
            per_peer: per_principal,
            max_lifetime: Duration::from_millis(life_ms),
            idle_timeout: Duration::from_millis(idle_ms),
        }
    }

    fn plain_request() -> wire::WatchRefsRequest {
        wire::WatchRefsRequest {
            repo: Some(wire::RepoRef {
                name: "o/r".into(),
                id_mode: 0,
            }),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn one_principal_at_its_cap_leaves_slots_for_another() {
        let f = setup().await;
        let watch = ServerWatch::with_limits(f.r.clone(), limits(3, 2, 60_000, 60_000));
        let a1 = watch.watch_refs(&ctx("alice"), plain_request()).await;
        let a2 = watch.watch_refs(&ctx("alice"), plain_request()).await;
        assert!(a1.is_ok() && a2.is_ok());
        let e = watch
            .watch_refs(&ctx("alice"), plain_request())
            .await
            .err()
            .unwrap();
        assert_eq!(e.code(), tonic::Code::ResourceExhausted);
        assert!(e.message().contains("principal"), "{}", e.message());
        // The refused attempt took no global slot: bob still gets the third one.
        let b = watch.watch_refs(&ctx("bob"), plain_request()).await;
        assert!(b.is_ok());
        // The global cap still holds for everyone.
        let e = watch
            .watch_refs(&ctx("carol"), plain_request())
            .await
            .err()
            .unwrap();
        assert_eq!(e.code(), tonic::Code::ResourceExhausted);
        // A closed stream frees alice's own slot.
        drop(a1);
        drop(b);
        assert!(
            watch
                .watch_refs(&ctx("alice"), plain_request())
                .await
                .is_ok()
        );
    }

    async fn open_from(
        watch: &ServerWatch,
        who: &CallCtx,
        peer: crate::limits::Peer,
    ) -> Result<RpcStream<wire::RefEvent>, Status> {
        crate::limits::with_peer(peer, watch.watch_refs(who, plain_request())).await
    }

    #[tokio::test]
    async fn anonymous_callers_are_charged_to_their_own_address_and_share() {
        use crate::limits::Peer;
        let f = setup().await;
        let mut l = limits(8, 8, 60_000, 60_000);
        l.anonymous = 3;
        l.per_peer = 2;
        let watch = ServerWatch::with_limits(f.r.clone(), l);
        let anon = ctx("0:anonymous");
        let at = |n: u8| Peer::from_ip(Some(std::net::IpAddr::from([10, 0, 0, n])));
        let a1 = open_from(&watch, &anon, at(1)).await;
        let a2 = open_from(&watch, &anon, at(1)).await;
        assert!(a1.is_ok() && a2.is_ok());
        // One address at its cap does not block another address.
        let e = open_from(&watch, &anon, at(1)).await.err().unwrap();
        assert_eq!(e.code(), tonic::Code::ResourceExhausted);
        assert!(e.message().contains("address"), "{}", e.message());
        let b1 = open_from(&watch, &anon, at(2)).await;
        assert!(b1.is_ok(), "a second address has slots of its own");
        // The anonymous share (3) is spent, but a verified caller is not locked out.
        let e = open_from(&watch, &anon, at(3)).await.err().unwrap();
        assert_eq!(e.code(), tonic::Code::ResourceExhausted);
        assert!(e.message().contains("anonymous"), "{}", e.message());
        let verified = ctx("9:dana");
        assert!(open_from(&watch, &verified, at(1)).await.is_ok());
        // Closing a stream frees its address and its share.
        drop(a1);
        assert!(open_from(&watch, &anon, at(3)).await.is_ok());
    }

    #[tokio::test]
    async fn an_anonymous_flood_cannot_take_every_global_slot() {
        use crate::limits::{Peer, with_peer};
        let f = setup().await;
        let mut l = limits(4, 4, 60_000, 60_000);
        l.anonymous = 2;
        l.per_peer = 2;
        let watch = ServerWatch::with_limits(f.r.clone(), l);
        let anon = ctx("0:anonymous");
        let mut held = Vec::new();
        for n in 1..=6u8 {
            let peer = Peer::from_ip(Some(std::net::IpAddr::from([10, 0, 0, n])));
            if let Ok(s) = with_peer(peer, watch.watch_refs(&anon, plain_request())).await {
                held.push(s);
            }
        }
        assert_eq!(held.len(), 2, "anonymous callers hold their share only");
        let verified = ctx("9:dana");
        assert!(watch.watch_refs(&verified, plain_request()).await.is_ok());
        assert!(watch.watch_refs(&verified, plain_request()).await.is_ok());
    }

    #[test]
    fn the_principal_table_stays_bounded_and_never_evicts_an_open_stream() {
        let slots = PrincipalSlots::new(1, 4, 6);
        let held: Vec<_> = (0..4)
            .map(|i| slots.acquire(&format!("busy{i}")).unwrap())
            .collect();
        for i in 0..1000 {
            drop(slots.acquire(&format!("idle{i}")).unwrap());
            assert!(slots.tracked() <= 6, "{} entries", slots.tracked());
        }
        // The busy principals kept their entries, so their caps still bind.
        for i in 0..4 {
            assert!(slots.acquire(&format!("busy{i}")).is_none());
        }
        drop(held);
        assert!(slots.acquire("busy0").is_some());
    }

    #[tokio::test]
    async fn a_stream_ends_at_its_max_lifetime_even_with_events_flowing() {
        let f = setup().await;
        let watch = ServerWatch::with_limits(f.r.clone(), limits(8, 8, 400, 60_000))
            .with_timing(Duration::from_millis(5), 0);
        let mut s = pump(
            watch
                .watch_refs(&ctx("alice"), plain_request())
                .await
                .unwrap(),
        );
        let started = std::time::Instant::now();
        let mut n = 0u8;
        let ended = loop {
            n = n.wrapping_add(1);
            f.set("refs/heads/master", Some(cs(n))).await;
            if let Ok(None) = tokio::time::timeout(Duration::from_millis(100), s.recv()).await {
                break true;
            }
            if started.elapsed() > Duration::from_secs(10) {
                break false;
            }
        };
        assert!(ended, "the stream outlived its max lifetime");
        assert!(started.elapsed() >= Duration::from_millis(400));
    }

    #[tokio::test]
    async fn an_idle_stream_ends_but_one_receiving_events_stays_open() {
        let f = setup().await;
        let watch = ServerWatch::with_limits(f.r.clone(), limits(8, 8, 60_000, 400))
            .with_timing(Duration::from_millis(5), 0);
        let mut busy = pump(
            watch
                .watch_refs(&ctx("alice"), plain_request())
                .await
                .unwrap(),
        );
        // Bob's pattern never matches, so no event is ever delivered to him.
        let never = wire::WatchRefsRequest {
            patterns: vec!["refs/tags/none".into()],
            ..plain_request()
        };
        let mut quiet = pump(watch.watch_refs(&ctx("bob"), never).await.unwrap());
        // Events every 100 ms for 800 ms span two idle windows.
        for n in 1..=8u8 {
            f.set("refs/heads/master", Some(cs(n))).await;
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        f.set("refs/heads/master", Some(cs(99))).await;
        let got = take(&mut busy, 20).await;
        assert!(
            got.len() >= 9,
            "the busy stream kept receiving: {}",
            got.len()
        );
        // Bob's stream delivered nothing for 800 ms, so it ended cleanly.
        let end = tokio::time::timeout(Duration::from_secs(5), quiet.recv()).await;
        assert!(matches!(end, Ok(None)), "the idle stream did not end");
    }

    #[tokio::test]
    async fn fresh_rows_wait_for_the_settle_window() {
        let f = setup().await;
        let watch = ServerWatch::new(f.r.clone()).with_timing(Duration::from_millis(5), 3600);
        let req = wire::WatchRefsRequest {
            repo: Some(wire::RepoRef {
                name: "o/r".into(),
                id_mode: 0,
            }),
            resume: Cursor(0).to_bytes().into(),
            ..Default::default()
        };
        let mut s = pump(watch.watch_refs(&ctx("alice"), req).await.unwrap());
        f.set("refs/heads/master", Some(cs(1))).await;
        assert!(take(&mut s, 1).await.is_empty());
    }
}
