//! The commit graph walk behind `CommitGraph` (SPEC §20.6): generation-ordered pages whose lane
//! state rides in the page token.
//!
//! [`Engine::graph_page`] walks from start points, hides what the excluded ones reach, and
//! returns at most one page of [`lanes::Row`]s with a token for the next. Earlier pages are never
//! read again: the token holds the [`lanes::Frontier`]. A frontier that fits in
//! [`INLINE_TOKEN`] bytes is the token itself; a larger one is stored as a content-addressed
//! memo and the token names it, so a token stays under 4 KiB however many lines are open.
//!
//! Tokens are client input. A token is decoded strictly, and each changeset taken from it is
//! checked against its real generation, so a forged one can at worst fail or show part of the
//! repository the caller can already read.

use asc_core::id::ChangesetId;
use asc_store::key::ObjKey;
use asc_store::objects::{self, Objects};
use asc_store::refs::Refs;

use crate::derive::{self, Gen, read_memo, write_memo};
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

const TAG_INLINE: u8 = 0;
const TAG_SPILLED: u8 = 1;
const SPILL_NAME: &str = "graphpage";
const SPILL_VERSION: u32 = 1;

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

fn spill_key(hash: &blake3::Hash) -> ObjKey {
    ObjKey::Derived {
        name: SPILL_NAME,
        version: SPILL_VERSION,
        id: *hash.as_bytes(),
    }
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
            Start::Token(t) => self.load_frontier(t).await?,
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
            Some(self.store_frontier(&frontier).await?)
        };
        Ok(GraphPage { rows, next })
    }

    async fn store_frontier(&self, f: &Frontier) -> Result<Vec<u8>> {
        let state = f.encode();
        let mut token = Vec::with_capacity(1 + state.len().min(32));
        if state.len() <= INLINE_TOKEN {
            token.push(TAG_INLINE);
            token.extend(state);
        } else {
            let hash = blake3::hash(&state);
            write_memo(&*self.objects, &spill_key(&hash), &state).await?;
            token.push(TAG_SPILLED);
            token.extend(hash.as_bytes());
        }
        Ok(token)
    }

    async fn load_frontier(&self, token: &[u8]) -> Result<Frontier> {
        let bad = || Error::Query("malformed page token");
        let (&tag, rest) = token.split_first().ok_or_else(bad)?;
        let decoded = match tag {
            TAG_INLINE if rest.len() <= INLINE_TOKEN => Frontier::decode(rest),
            TAG_SPILLED => {
                let hash = blake3::Hash::from_bytes(rest.try_into().map_err(|_| bad())?);
                let state = read_memo(&*self.objects, &spill_key(&hash))
                    .await?
                    .ok_or(Error::Expired)?;
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
