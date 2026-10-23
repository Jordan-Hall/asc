//! The commit graph walk behind `CommitGraph` (SPEC §20.6): generation-ordered pages whose lane
//! state rides in the page token.
//!
//! [`Engine::graph_page`] walks from start points, hides what the excluded ones reach, and
//! returns at most one page of [`lanes::Row`]s with a token for the next. Earlier pages are never
//! read again: the token holds the [`lanes::Frontier`]. A frontier that fits in
//! [`INLINE_TOKEN`] bytes is the token itself; a larger one is held in a [`SpillCache`] and the
//! token names it by hash, so a token stays under 4 KiB however many lines are open.
//!
//! A read never writes to the permanent object store: the spill cache lives in memory, is capped
//! at [`SPILL_CACHE_BYTES`] and drops an entry after [`SPILL_TTL`]. A token whose state was
//! evicted, has expired or was made by another server is [`Error::Expired`], and the caller
//! restarts from the first page.
//!
//! Tokens are client input. A token is decoded strictly, and each changeset taken from it is
//! checked against its real generation, so a forged one can at worst fail or show part of the
//! repository the caller can already read. A spilled token is a blake3 hash of its state, so
//! it cannot name state the server did not make.

use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant};

use asc_core::id::ChangesetId;
use asc_store::objects::{self, Objects};
use asc_store::refs::{Refs, RepoKey};

use crate::derive::{self, Gen};
use crate::{Engine, commit};

pub mod lanes;

pub use lanes::{Edge, Frontier, MAX_LANES, OVERFLOW_COLUMN, Row};

/// Rows of a page when the caller asks for none.
pub const DEFAULT_PAGE: usize = 200;
/// Most rows of one page.
pub const MAX_PAGE: usize = 2000;
/// Changesets one page may take from the frontier, shown or hidden. A page that reaches it
/// returns what it has with a token, so a long hidden stretch cannot hold a call.
pub const SCAN_BUDGET: usize = 100_000;
/// Largest frontier carried inside the token itself.
pub const INLINE_TOKEN: usize = 3072;

/// Memory the spill cache may hold in all: about fifty of the widest frontiers.
pub const SPILL_CACHE_BYTES: u64 = 32 << 20;
/// How long a spilled frontier stays valid.
pub const SPILL_TTL: Duration = Duration::from_secs(15 * 60);
/// Bytes charged per entry on top of its state.
const SPILL_OVERHEAD: u64 = 128;

const TAG_INLINE: u8 = 0;
const TAG_SPILLED: u8 = 1;

type SpillKey = (RepoKey, [u8; 32]);

#[derive(Clone)]
struct StateWeight;

impl quick_cache::Weighter<SpillKey, (Instant, Arc<[u8]>)> for StateWeight {
    fn weight(&self, _: &SpillKey, v: &(Instant, Arc<[u8]>)) -> u64 {
        v.1.len() as u64 + SPILL_OVERHEAD
    }
}

/// Frontiers too wide for a token, by repository and hash. Bounded in bytes and in age, and
/// never persisted: it is the only place a read RPC keeps state.
pub struct SpillCache {
    cache: quick_cache::sync::Cache<SpillKey, (Instant, Arc<[u8]>), StateWeight>,
    ttl: Duration,
}

impl SpillCache {
    pub fn new(bytes: u64, ttl: Duration) -> Self {
        Self {
            cache: quick_cache::sync::Cache::with_weighter(1024, bytes, StateWeight),
            ttl,
        }
    }

    fn put(&self, repo: RepoKey, hash: &blake3::Hash, state: Arc<[u8]>) {
        self.cache
            .insert((repo, *hash.as_bytes()), (Instant::now(), state));
    }

    fn get(&self, repo: RepoKey, hash: &blake3::Hash) -> Option<Arc<[u8]>> {
        let key = (repo, *hash.as_bytes());
        let (at, state) = self.cache.get(&key)?;
        if at.elapsed() > self.ttl {
            self.cache.remove(&key);
            return None;
        }
        Some(state)
    }
}

static SPILLS: LazyLock<SpillCache> =
    LazyLock::new(|| SpillCache::new(SPILL_CACHE_BYTES, SPILL_TTL));

/// Commit graph failures.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// A malformed or forged page token.
    #[error("{0}")]
    Query(&'static str),
    /// A token that names state the server no longer holds.
    #[error("the page token has expired; restart from the first page")]
    Expired,
    #[error(transparent)]
    TooWide(#[from] lanes::TooWide),
    #[error(transparent)]
    Commit(#[from] commit::Error),
    #[error(transparent)]
    Derive(#[from] derive::Error),
    #[error(transparent)]
    Store(#[from] objects::Error),
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Where a walk begins.
pub enum Start<'a> {
    /// The first page: everything reachable from `include` and not from `exclude`.
    New {
        include: &'a [ChangesetId],
        exclude: &'a [ChangesetId],
    },
    /// A later page, from a token an earlier page returned.
    Token(&'a [u8]),
}

/// One page of the graph.
#[derive(Clone, Debug)]
pub struct GraphPage {
    pub rows: Vec<Row>,
    /// `None` once the walk is complete.
    pub next: Option<Vec<u8>>,
}

impl<O: Objects, R: Refs> Engine<O, R> {
    async fn generation(&self, id: ChangesetId) -> Result<u32> {
        Ok(self.get_derived::<Gen>(id).await?.generation)
    }

    /// One page of at most `limit` rows (clamped to `1..=`[`MAX_PAGE`]).
    pub async fn graph_page(&self, start: Start<'_>, limit: usize) -> Result<GraphPage> {
        let limit = limit.clamp(1, MAX_PAGE);
        let mut frontier = match start {
            Start::New { include, exclude } => {
                let mut f = Frontier::default();
                for (ids, excluded) in [(exclude, true), (include, false)] {
                    for &id in ids {
                        f.push_tip(id, self.generation(id).await?, excluded)?;
                    }
                }
                f
            }
            Start::Token(t) => self.load_frontier(t)?,
        };
        let mut rows = Vec::with_capacity(limit.min(DEFAULT_PAGE));
        let mut steps = 0;
        while rows.len() < limit && steps < SCAN_BUDGET {
            let Some(at) = frontier.pop() else { break };
            steps += 1;
            let cs = self.get_changeset(at.id).await?;
            if self.generation(at.id).await? != at.generation {
                return Err(Error::Query("malformed page token"));
            }
            let mut parents = Vec::with_capacity(cs.parents.len());
            for &p in &cs.parents {
                parents.push((p, self.generation(p).await?));
            }
            if at.excluded {
                frontier.skip(&at, &parents)?;
            } else {
                rows.push(frontier.visit(&at, &parents)?);
            }
        }
        let next = if frontier.is_done() {
            None
        } else {
            Some(self.store_frontier(&frontier))
        };
        Ok(GraphPage { rows, next })
    }

    fn store_frontier(&self, f: &Frontier) -> Vec<u8> {
        let state = f.encode();
        let mut token = Vec::with_capacity(1 + state.len().min(32));
        if state.len() <= INLINE_TOKEN {
            token.push(TAG_INLINE);
            token.extend(state);
        } else {
            let hash = blake3::hash(&state);
            SPILLS.put(self.repo, &hash, state.into());
            token.push(TAG_SPILLED);
            token.extend(hash.as_bytes());
        }
        token
    }

    fn load_frontier(&self, token: &[u8]) -> Result<Frontier> {
        let bad = || Error::Query("malformed page token");
        let (&tag, rest) = token.split_first().ok_or_else(bad)?;
        let decoded = match tag {
            TAG_INLINE if rest.len() <= INLINE_TOKEN => Frontier::decode(rest),
            TAG_SPILLED => {
                let hash = blake3::Hash::from_bytes(rest.try_into().map_err(|_| bad())?);
                let state = SPILLS.get(self.repo, &hash).ok_or(Error::Expired)?;
                if blake3::hash(&state) != hash {
                    return Err(Error::Expired);
                }
                Frontier::decode(&state)
            }
            _ => None,
        };
        decoded.ok_or_else(bad)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use asc_store::key::{KeyPrefix, ObjKey};
    use futures::TryStreamExt;

    use super::*;
    use crate::derive::testutil::*;

    async fn stored(e: &TestEngine) -> HashSet<ObjKey> {
        let keys: Vec<_> = e.objects.list(KeyPrefix::all()).try_collect().await.unwrap();
        keys.into_iter().map(|(k, _)| k).collect()
    }

    #[tokio::test]
    async fn a_wide_frontier_writes_nothing_to_the_object_store() {
        let e = engine();
        let mut tips = Vec::new();
        for i in 0..120 {
            let root = commit_on(&e, &[], format!("r{i}").as_bytes()).await;
            tips.push(commit_on(&e, &[root], format!("t{i}").as_bytes()).await);
        }
        let start = || Start::New {
            include: &tips,
            exclude: &[],
        };
        // Walk it whole once so every generation memo exists; only the page's own writes
        // show in the comparison below.
        let all = e.graph_page(start(), MAX_PAGE).await.unwrap();
        assert_eq!(all.rows.len(), 240);
        let before = stored(&e).await;
        let page = e.graph_page(start(), 1).await.unwrap();
        let token = page.next.expect("119 tips and a root are pending");
        assert!(
            token.len() - 1 > INLINE_TOKEN || token[0] == TAG_SPILLED,
            "the frontier is wider than the inline token"
        );
        assert!(token.len() < 4096);
        let rest = e.graph_page(Start::Token(&token), MAX_PAGE).await.unwrap();
        assert_eq!(rest.rows.len(), 239, "the token resumes the walk");
        assert_eq!(
            stored(&e).await,
            before,
            "a read must not add objects to the store"
        );
    }

    #[test]
    fn spilled_state_expires_and_is_bounded() {
        let state: Arc<[u8]> = vec![7u8; 1000].into();
        let hash = blake3::hash(&state);
        let live = SpillCache::new(1 << 20, Duration::from_secs(3600));
        live.put(RepoKey(1), &hash, state.clone());
        assert_eq!(live.get(RepoKey(1), &hash).as_deref(), Some(&*state));
        assert!(live.get(RepoKey(2), &hash).is_none(), "scoped to a repository");
        let stale = SpillCache::new(1 << 20, Duration::ZERO);
        stale.put(RepoKey(1), &hash, state.clone());
        std::thread::sleep(Duration::from_millis(2));
        assert!(stale.get(RepoKey(1), &hash).is_none());
        // Far more than the byte cap goes in; the cache keeps to its bound.
        let small = SpillCache::new(16 << 10, Duration::from_secs(3600));
        for i in 0..200u32 {
            let s: Arc<[u8]> = vec![i as u8; 1000].into();
            small.put(RepoKey(1), &blake3::hash(&[&s[..], &i.to_le_bytes()].concat()), s);
        }
        assert!(small.cache.weight() <= 16 << 10, "{}", small.cache.weight());
    }
}
