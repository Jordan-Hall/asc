//! The server side of `Watch KIND_STACK` (PLAN C49, SPEC §20.6): a bounded, per-repository feed of
//! "this change was rewritten, restacked or got a new review state".
//!
//! * [`emit`] is called by the writers: the change index (`write_rewrites`) and `SetReviewState`
//!   (after its commit). It never blocks and never fails; a full feed drops its oldest event.
//! * [`read`] hands a reader the events after a cursor. The service layer has authorised the
//!   caller for `Read` on the repository before the handler runs, and the feed is keyed by the
//!   repository's namespace, which the repository entry names, so a reader sees its own
//!   repository's events and no other. An event carries ids only (a change id and a kind), never
//!   content, so `Read` is enough.
//! * A cursor the feed cannot honour (older than the oldest kept event, or newer than the newest:
//!   the server restarted) answers `resync`, and the reader re-reads the stack.
//!
//! Bounds: [`RING`] events per repository, [`MAX_FEEDS`] repositories (the least recently written
//! is dropped), [`MAX_BATCH`] events per read.

use std::collections::{HashMap, VecDeque};
use std::sync::{LazyLock, Mutex, PoisonError};

use asc_core::change::ChangeId;
use asc_rpc::server::CallCtx;
use tonic::Status;

use crate::registry::{Ns, Registry, RegistryError};

/// Events kept per repository.
pub const RING: usize = 1024;
/// Repositories with a feed at once.
pub const MAX_FEEDS: usize = 4096;
/// Most events one [`read`] returns.
pub const MAX_BATCH: usize = 256;

/// What happened to a change.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StackEventKind {
    Rewritten,
    Restacked,
    ReviewState,
}

impl StackEventKind {
    /// The `WatchEvent.op` text.
    pub fn name(self) -> &'static str {
        match self {
            Self::Rewritten => "rewritten",
            Self::Restacked => "restacked",
            Self::ReviewState => "review",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StackEvent {
    pub seq: u64,
    pub change: ChangeId,
    pub kind: StackEventKind,
}

/// What [`read`] found.
#[derive(Debug, PartialEq, Eq)]
pub struct Batch {
    pub events: Vec<StackEvent>,
    /// The cursor to continue from.
    pub next: u64,
    /// The cursor could not be honoured: the reader re-reads the whole stack.
    pub resync: bool,
}

#[derive(Default)]
struct Ring {
    next_seq: u64,
    events: VecDeque<StackEvent>,
    touched: u64,
}

/// The feeds of all repositories.
#[derive(Default)]
pub struct StackFeed {
    inner: Mutex<(u64, HashMap<Vec<u8>, Ring>)>,
}

impl StackFeed {
    pub fn emit(&self, ns: &[u8], change: ChangeId, kind: StackEventKind) {
        let mut g = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        g.0 += 1;
        let tick = g.0;
        if !g.1.contains_key(ns)
            && g.1.len() >= MAX_FEEDS
            && let Some(oldest) =
                g.1.iter()
                    .min_by_key(|(_, r)| r.touched)
                    .map(|(k, _)| k.clone())
        {
            g.1.remove(&oldest);
        }
        let ring = g.1.entry(ns.to_vec()).or_default();
        ring.touched = tick;
        ring.next_seq += 1;
        if ring.events.len() >= RING {
            ring.events.pop_front();
        }
        let seq = ring.next_seq;
        ring.events.push_back(StackEvent { seq, change, kind });
    }

    pub fn read(&self, ns: &[u8], after: u64) -> Batch {
        let g = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        let Some(ring) = g.1.get(ns) else {
            // No event was ever kept: a reader that holds a cursor lost its place (a restart).
            return Batch {
                events: Vec::new(),
                next: 0,
                resync: after != 0,
            };
        };
        let oldest = ring.events.front().map_or(ring.next_seq + 1, |e| e.seq);
        if after > ring.next_seq || after + 1 < oldest {
            return Batch {
                events: Vec::new(),
                next: ring.next_seq,
                resync: true,
            };
        }
        let events: Vec<StackEvent> = ring
            .events
            .iter()
            .filter(|e| e.seq > after)
            .take(MAX_BATCH)
            .copied()
            .collect();
        let next = events.last().map_or(after, |e| e.seq);
        Batch {
            events,
            next,
            resync: false,
        }
    }
}

static FEED: LazyLock<StackFeed> = LazyLock::new(StackFeed::default);

/// Records that `change` of the repository in `ns` changed.
pub fn emit(ns: Ns, change: ChangeId, kind: StackEventKind) {
    FEED.emit(ns.as_bytes(), change, kind);
}

/// The events of the repository in `ns` after `after`, for a caller that already holds its `Ns`.
pub fn peek(ns: Ns, after: u64) -> Batch {
    FEED.read(ns.as_bytes(), after)
}

/// The events of the repository `ctx` names after `after`. The caller holds `Read` (checked by the
/// service layer); the repository comes from `ctx` alone.
pub async fn read(registry: &Registry, ctx: &CallCtx, after: u64) -> Result<Batch, Status> {
    let entry = registry.open(&ctx.repo).await.map_err(|e| match e {
        RegistryError::NotFound(_) => Status::not_found("no such repository"),
        other => Status::unavailable(other.to_string()),
    })?;
    Ok(FEED.read(entry.ns.as_bytes(), after))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn c(n: u8) -> ChangeId {
        ChangeId::from_bytes([n; 16])
    }

    #[test]
    fn events_arrive_in_order_after_a_cursor() {
        let f = StackFeed::default();
        f.emit(b"a", c(1), StackEventKind::Rewritten);
        f.emit(b"a", c(2), StackEventKind::Restacked);
        f.emit(b"a", c(1), StackEventKind::ReviewState);
        let all = f.read(b"a", 0);
        assert!(!all.resync);
        assert_eq!(
            all.events.iter().map(|e| e.seq).collect::<Vec<_>>(),
            [1, 2, 3]
        );
        let tail = f.read(b"a", 2);
        assert_eq!(tail.events.len(), 1);
        assert_eq!(
            (tail.events[0].change, tail.events[0].kind),
            (c(1), StackEventKind::ReviewState)
        );
        assert_eq!(f.read(b"a", tail.next).events.len(), 0);
    }

    #[test]
    fn a_repository_never_sees_another_repositorys_events() {
        let f = StackFeed::default();
        f.emit(b"a", c(1), StackEventKind::Rewritten);
        let other = f.read(b"b", 0);
        assert!(other.events.is_empty() && !other.resync);
        assert_eq!(f.read(b"a", 0).events.len(), 1);
    }

    #[test]
    fn a_cursor_the_feed_cannot_honour_resyncs() {
        let f = StackFeed::default();
        for i in 0..(RING + 10) {
            f.emit(b"a", c((i % 250) as u8), StackEventKind::Rewritten);
        }
        // Older than the oldest kept event.
        assert!(f.read(b"a", 1).resync);
        // Newer than anything emitted (the server restarted since).
        assert!(f.read(b"a", 1_000_000).resync);
        // A cursor for a repository with no feed.
        assert!(f.read(b"nobody", 5).resync);
        // The oldest kept event is still reachable.
        let first_kept = 10;
        let b = f.read(b"a", first_kept);
        assert!(!b.resync);
        assert_eq!(b.events.len(), MAX_BATCH);
    }

    #[tokio::test]
    async fn a_rewrite_reaches_the_feed_and_a_creation_does_not() {
        use crate::changes::index::ChangeIndex;
        use crate::ingest::testutil::{commit, registry, repo};
        use asc_core::change::{ChangeMeta, RewriteOp};

        let reg = registry().await;
        let e = repo(&reg).await;
        let idx = ChangeIndex::new(reg.clone());
        let base = commit(&e, &[], &[("base", b"0")]).await;
        let v1 = commit(&e, &[base], &[("a", b"1")]).await;
        let v2 = commit(&e, &[base], &[("a", b"2")]).await;
        let v3 = commit(&e, &[base], &[("a", b"3")]).await;
        let write = |meta: ChangeMeta| {
            let (reg, idx, key) = (reg.clone(), &idx, e.key);
            async move {
                let mut tx = reg.refs().begin().await.unwrap();
                idx.write_meta(&mut tx, key, &meta).await.unwrap();
                tx.commit().await.unwrap();
            }
        };
        // A creation: no event.
        write(ChangeMeta::new(v1, c(2), vec![], RewriteOp::New)).await;
        assert!(peek(e.ns, 0).events.is_empty());
        // An amend and a restack: one event each, with the right kind.
        write(ChangeMeta::new(v2, c(2), vec![v1], RewriteOp::Amend)).await;
        write(ChangeMeta::new(v3, c(2), vec![v2], RewriteOp::Restack)).await;
        let got = peek(e.ns, 0);
        let kinds: Vec<_> = got.events.iter().map(|x| (x.change, x.kind)).collect();
        assert_eq!(
            kinds,
            [
                (c(2), StackEventKind::Rewritten),
                (c(2), StackEventKind::Restacked)
            ]
        );
        // Another repository's feed is untouched.
        let other = repo_named(&reg, "o/other").await;
        assert!(peek(other.ns, 0).events.is_empty());
    }

    async fn repo_named(r: &Registry, name: &str) -> std::sync::Arc<crate::registry::RepoEntry> {
        r.create(
            &asc_rpc::server::RepoName::parse(name).unwrap(),
            &Default::default(),
        )
        .await
        .unwrap()
    }

    #[test]
    fn the_feed_table_is_bounded() {
        let f = StackFeed::default();
        for i in 0..(MAX_FEEDS + 5) {
            f.emit(&(i as u32).to_be_bytes(), c(1), StackEventKind::Rewritten);
        }
        assert!(f.inner.lock().unwrap().1.len() <= MAX_FEEDS);
        // The newest repository is kept.
        assert_eq!(
            f.read(&((MAX_FEEDS + 4) as u32).to_be_bytes(), 0)
                .events
                .len(),
            1
        );
    }
}
