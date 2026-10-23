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
//! multi-writer database; waiting keeps the cursor from skipping a slow commit. Open streams are
//! capped per process. The service layer re-checks the caller's credential while the stream is
//! open and closes it when the token expires.

use std::collections::VecDeque;
use std::sync::Arc;
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
use tonic::Status;

use crate::refpolicy::{self, SHARED_ROOTS, USER_NS};
use crate::registry::{Extensions, Registry, RegistryError};
use crate::update::internal;

/// Open streams the process serves at once.
pub const MAX_WATCHERS: usize = 1024;
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
    _slot: OwnedSemaphorePermit,
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
}

/// The `WatchRefs` handler.
pub struct ServerWatch {
    registry: Arc<Registry>,
    slots: Arc<Semaphore>,
    poll: Duration,
    settle: i64,
}

impl ServerWatch {
    pub fn new(registry: Arc<Registry>) -> Self {
        Self {
            registry,
            slots: Arc::new(Semaphore::new(MAX_WATCHERS)),
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
            _slot: slot,
        };
        Ok(stream::unfold(reader, |mut r| async move {
            loop {
                if let Some(ev) = r.queue.pop_front() {
                    return Some((Ok(ev), r));
                }
                if r.failed {
                    return None;
                }
                match r.fill().await {
                    Ok(true) => {}
                    Ok(false) => {
                        if r.queue.is_empty() {
                            tokio::time::sleep(r.poll).await;
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
pub fn register(ext: &Extensions, registry: &Arc<Registry>) {
    ext.handlers
        .set_watch(Arc::new(ServerWatch::new(registry.clone())))
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
