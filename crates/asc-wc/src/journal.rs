//! The change journal (SPEC §9.2, §10.2.6, §10.2.7): hints that say which paths may differ from
//! the base tree. Notifications push into an in-memory queue; a flusher writes the queue to
//! `state.db` in batches (at most every 100 ms). Hints can be lost without harm because reconcile
//! recovers them (SPEC §10.2.7), so every bound here degrades to `fresh` instead of growing.

use std::collections::HashSet;
use std::fmt;
use std::str::FromStr;
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::time::Duration;

use asc_core::path::RepoPath;
use asc_state::state::tables::journal;
use asc_state::{StateDb, WorkspaceId};

pub use journal::{Entry, Op};

use crate::plain::Result;

/// How often the flusher writes the queue.
pub const FLUSH_INTERVAL: Duration = Duration::from_millis(100);
/// Queued hints beyond this are dropped and the journal reports `fresh` (hints only).
pub const MAX_QUEUE: usize = 65_536;
/// [`Journal::since`] returns at most this many entries; more means `fresh`.
pub const MAX_SINCE: usize = 1_000_000;
/// Rows read per query in [`Journal::since`].
const PAGE: u32 = 10_000;
/// Longest accepted token text.
const MAX_TOKEN_LEN: usize = 41;

/// A position in the journal: `<epoch>:<seq>`. A token from another epoch (an earlier switch
/// generation) or one the journal cannot vouch for is stale.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Token {
    pub epoch: u64,
    pub seq: u64,
}

impl fmt::Display for Token {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.epoch, self.seq)
    }
}

/// A token that is not two decimal numbers joined by `:`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BadToken;

impl fmt::Display for BadToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("malformed journal token")
    }
}

impl std::error::Error for BadToken {}

impl FromStr for Token {
    type Err = BadToken;

    fn from_str(s: &str) -> Result<Self, BadToken> {
        let digits = |p: &str| {
            if !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()) {
                p.parse::<u64>().map_err(|_| BadToken)
            } else {
                Err(BadToken)
            }
        };
        if s.len() > MAX_TOKEN_LEN {
            return Err(BadToken);
        }
        let (e, q) = s.split_once(':').ok_or(BadToken)?;
        Ok(Self {
            epoch: digits(e)?,
            seq: digits(q)?,
        })
    }
}

/// The answer to [`Journal::since`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Since {
    /// Where the next call resumes.
    pub token: Token,
    /// Entries after the token, oldest first. Empty when `fresh`.
    pub paths: Vec<Entry>,
    /// The token could not be honoured (none given, other epoch, hints lost, too many): the caller
    /// must reconcile instead of trusting `paths`.
    pub fresh: bool,
}

#[derive(Default)]
struct Inner {
    epoch: u64,
    queue: Vec<Entry>,
    /// Last sequence number written to `state.db`.
    flushed: i64,
    /// Hints were dropped since the last `since`.
    lost: bool,
}

/// The journal of one workspace.
pub struct Journal {
    db: StateDb,
    ws: WorkspaceId,
    inner: Mutex<Inner>,
    /// Serialises flushes so sequence numbers follow queue order.
    flush_lock: tokio::sync::Mutex<()>,
}

impl Journal {
    /// Opens the journal of `ws`; `epoch` is the workspace's epoch (SPEC §10.1).
    pub async fn open(db: StateDb, ws: WorkspaceId, epoch: u64) -> Result<Self> {
        let flushed: i64 =
            sqlx::query_scalar("SELECT COALESCE(MAX(seq), 0) FROM journal WHERE ws_id = ?")
                .bind(ws.to_string())
                .fetch_one(db.pool())
                .await
                .map_err(asc_state::Error::from)?;
        Ok(Self {
            db,
            ws,
            inner: Mutex::new(Inner {
                epoch,
                flushed,
                ..Inner::default()
            }),
            flush_lock: tokio::sync::Mutex::new(()),
        })
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Queues a hint. Never blocks and never fails: an invalid path or a full queue drops the hint
    /// and makes the next [`since`](Self::since) report `fresh`.
    pub fn record(&self, op: Op, path: &[u8]) {
        let ok = RepoPath::new(path).is_ok();
        let mut g = self.lock();
        if !ok || g.queue.len() >= MAX_QUEUE {
            g.lost = true;
            return;
        }
        g.queue.push(Entry {
            path: path.to_vec(),
            op,
        });
    }

    /// Starts a new epoch (after a switch bumped the workspace epoch); older tokens become stale.
    pub fn set_epoch(&self, epoch: u64) {
        self.lock().epoch = epoch;
    }

    /// Writes the queue to `state.db` in one transaction.
    pub async fn flush(&self) -> Result<()> {
        let _serial = self.flush_lock.lock().await;
        let batch = std::mem::take(&mut self.lock().queue);
        if batch.is_empty() {
            return Ok(());
        }
        match journal::append(&self.db, self.ws, &batch).await {
            Ok(last) => {
                self.lock().flushed = last;
                Ok(())
            }
            Err(e) => {
                // Keep the hints for the next attempt, within the bound.
                let mut g = self.lock();
                let newer = std::mem::take(&mut g.queue);
                let mut back = batch;
                back.extend(newer);
                if back.len() > MAX_QUEUE {
                    back.truncate(MAX_QUEUE);
                    g.lost = true;
                }
                g.queue = back;
                Err(e.into())
            }
        }
    }

    /// The current position, after flushing.
    pub async fn token(&self) -> Result<Token> {
        self.flush().await?;
        let g = self.lock();
        Ok(Token {
            epoch: g.epoch,
            seq: g.flushed.max(0) as u64,
        })
    }

    /// Entries recorded after `token`. The queue is flushed first, so the answer includes hints
    /// that are still in memory.
    pub async fn since(&self, token: Option<&Token>) -> Result<Since> {
        self.flush().await?;
        let (epoch, flushed, lost) = {
            let mut g = self.lock();
            (g.epoch, g.flushed, std::mem::take(&mut g.lost))
        };
        let now = Token {
            epoch,
            seq: flushed.max(0) as u64,
        };
        let fresh = |t| Since {
            token: t,
            paths: Vec::new(),
            fresh: true,
        };
        let Some(t) = token else {
            return Ok(fresh(now));
        };
        if lost || t.epoch != epoch || t.seq > now.seq {
            return Ok(fresh(now));
        }
        let mut out = Vec::new();
        let mut after = i64::try_from(t.seq).unwrap_or(i64::MAX);
        loop {
            let page = journal::since(&self.db, self.ws, after, PAGE).await?;
            let Some(last) = page.last().map(|r| r.seq) else {
                break;
            };
            if out.len() + page.len() > MAX_SINCE {
                return Ok(fresh(now));
            }
            out.extend(page.into_iter().map(|r| r.entry));
            after = last;
        }
        Ok(Since {
            token: now,
            paths: out,
            fresh: false,
        })
    }

    /// Forgets the hints for `paths` recorded up to `upto` (the token the caller's status ran
    /// at); hints recorded after it stay, so a write during the status is not lost.
    pub async fn clear(&self, paths: &[Vec<u8>], upto: &Token) -> Result<()> {
        self.flush().await?;
        if upto.epoch != self.lock().epoch || paths.is_empty() {
            return Ok(());
        }
        let bound = i64::try_from(upto.seq).unwrap_or(i64::MAX);
        let wanted: HashSet<&[u8]> = paths.iter().map(Vec::as_slice).collect();
        let mut tx = self.db.begin().await?;
        for p in wanted {
            sqlx::query("DELETE FROM journal WHERE ws_id = ? AND path = ? AND seq <= ?")
                .bind(self.ws.to_string())
                .bind(p)
                .bind(bound)
                .execute(&mut *tx)
                .await
                .map_err(asc_state::Error::from)?;
        }
        tx.commit().await.map_err(asc_state::Error::from)?;
        Ok(())
    }

    /// Drops every hint (after a reconcile that covered the whole workspace).
    pub async fn clear_all(&self) -> Result<()> {
        let _serial = self.flush_lock.lock().await;
        self.lock().queue.clear();
        journal::clear(&self.db, self.ws).await?;
        Ok(())
    }

    /// Runs the 100 ms flusher until the last strong reference to the journal is dropped.
    pub fn spawn_flusher(this: &Arc<Self>) -> tokio::task::JoinHandle<()> {
        let weak: Weak<Self> = Arc::downgrade(this);
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(FLUSH_INTERVAL);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tick.tick().await;
                let Some(j) = weak.upgrade() else { return };
                // A failed flush keeps the hints queued; the next tick retries.
                let _ = j.flush().await;
            }
        })
    }
}

#[cfg(test)]
pub(crate) mod fixtures {
    use asc_state::state::repos::{NewRepo, RemoteKind};
    use asc_state::state::workspaces::{self, Mode, NewWorkspace};
    use asc_state::{AscHome, RepoId, StateDb, WorkspaceId};

    /// A temporary `state.db` with one registered workspace.
    pub(crate) async fn db() -> (tempfile::TempDir, StateDb, WorkspaceId) {
        let dir = tempfile::tempdir().unwrap();
        let home = AscHome::open(dir.path().join("home")).unwrap();
        let db = StateDb::open(&home).await.unwrap();
        let repo = asc_state::state::repos::insert(
            &db,
            &NewRepo {
                id: RepoId::new(),
                name: "r".into(),
                ns: vec![1, 2, 3],
                remote_url: None,
                remote_kind: RemoteKind::Server,
                default_branch: "master".into(),
                windows_safe: true,
            },
        )
        .await
        .unwrap();
        let ws = WorkspaceId::new();
        workspaces::insert(
            &db,
            &NewWorkspace {
                id: ws,
                repo,
                path: "/w".into(),
                mount: "/w".into(),
                mode: Mode::Virtual,
                head: "refs/heads/master".into(),
                base_cs: None,
                sparse: None,
                ephemeral_until: None,
            },
        )
        .await
        .unwrap();
        (dir, db, ws)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths(s: &Since) -> Vec<&[u8]> {
        s.paths.iter().map(|e| e.path.as_slice()).collect()
    }

    #[test]
    fn token_round_trip_and_rejects_garbage() {
        let t: Token = "3:42".parse().unwrap();
        assert_eq!(t, Token { epoch: 3, seq: 42 });
        assert_eq!(t.to_string(), "3:42");
        for bad in ["", "3", ":4", "3:", "a:1", "1:-2", "1:2:3", "+1:2", "1: 2"] {
            assert!(bad.parse::<Token>().is_err(), "{bad:?}");
        }
        assert!("99999999999999999999:1".parse::<Token>().is_err());
        assert!(format!("1:{}", "9".repeat(64)).parse::<Token>().is_err());
    }

    #[tokio::test]
    async fn since_includes_queued_hints_and_resumes() {
        let (_d, db, ws) = fixtures::db().await;
        let j = Journal::open(db, ws, 1).await.unwrap();
        let start = j.token().await.unwrap();
        j.record(Op::Modified, b"a");
        j.record(Op::Created, b"b/c");
        let s = j.since(Some(&start)).await.unwrap();
        assert!(!s.fresh);
        assert_eq!(paths(&s), [b"a".as_slice(), b"b/c"]);
        j.record(Op::Deleted, b"d");
        let s2 = j.since(Some(&s.token)).await.unwrap();
        assert_eq!(paths(&s2), [b"d".as_slice()]);
        assert!(j.since(Some(&s2.token)).await.unwrap().paths.is_empty());
    }

    #[tokio::test]
    async fn stale_or_missing_token_is_fresh() {
        let (_d, db, ws) = fixtures::db().await;
        let j = Journal::open(db, ws, 1).await.unwrap();
        j.record(Op::Modified, b"a");
        assert!(j.since(None).await.unwrap().fresh);
        let t = j.token().await.unwrap();
        j.set_epoch(2);
        let s = j.since(Some(&t)).await.unwrap();
        assert!(s.fresh && s.paths.is_empty());
        assert_eq!(s.token.epoch, 2);
        // A token from the future cannot be honoured either.
        let future = Token { epoch: 2, seq: 99 };
        assert!(j.since(Some(&future)).await.unwrap().fresh);
    }

    #[tokio::test]
    async fn overflow_and_invalid_paths_report_fresh_once() {
        let (_d, db, ws) = fixtures::db().await;
        let j = Journal::open(db, ws, 1).await.unwrap();
        let t = j.token().await.unwrap();
        j.record(Op::Modified, b"../escape");
        j.record(Op::Modified, b"ok");
        let s = j.since(Some(&t)).await.unwrap();
        assert!(s.fresh);
        let again = j.since(Some(&s.token)).await.unwrap();
        assert!(!again.fresh);
        assert_eq!(paths(&j.since(Some(&t)).await.unwrap()), [b"ok".as_slice()]);

        for _ in 0..MAX_QUEUE + 5 {
            j.record(Op::Modified, b"x");
        }
        assert_eq!(j.lock().queue.len(), MAX_QUEUE);
        assert!(j.since(Some(&again.token)).await.unwrap().fresh);
    }

    #[tokio::test]
    async fn clear_removes_only_hints_up_to_the_token() {
        let (_d, db, ws) = fixtures::db().await;
        let j = Journal::open(db, ws, 1).await.unwrap();
        let start = j.token().await.unwrap();
        j.record(Op::Modified, b"a");
        j.record(Op::Modified, b"b");
        let seen = j.token().await.unwrap();
        j.record(Op::Modified, b"a");
        j.clear(&[b"a".to_vec()], &seen).await.unwrap();
        let s = j.since(Some(&start)).await.unwrap();
        assert_eq!(paths(&s), [b"b".as_slice(), b"a"]);
        // A token of another epoch clears nothing.
        j.set_epoch(2);
        j.clear(&[b"b".to_vec()], &seen).await.unwrap();
        j.set_epoch(1);
        assert_eq!(j.since(Some(&start)).await.unwrap().paths.len(), 2);
    }

    #[tokio::test]
    async fn flusher_writes_within_a_few_intervals_and_reopen_keeps_sequence() {
        let (_d, db, ws) = fixtures::db().await;
        let j = Arc::new(Journal::open(db.clone(), ws, 1).await.unwrap());
        let handle = Journal::spawn_flusher(&j);
        j.record(Op::Created, b"f");
        tokio::time::sleep(FLUSH_INTERVAL * 5).await;
        assert!(j.lock().queue.is_empty());
        assert_eq!(journal::since(&db, ws, 0, 10).await.unwrap().len(), 1);
        drop(j);
        tokio::time::timeout(Duration::from_secs(2), handle)
            .await
            .expect("flusher exits once the journal is dropped")
            .unwrap();
        let j2 = Journal::open(db, ws, 1).await.unwrap();
        assert_eq!(j2.token().await.unwrap().seq, 1);
    }
}
