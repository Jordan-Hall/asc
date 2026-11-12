//! `pathlog.v1`: the changesets on a first-parent chain that touch one path (SPEC §5, named
//! future type `pathlog`).
//!
//! On a land branch the server answers a path log from the `path_changes` table (§5.1). Any other
//! ref used to walk every changeset of its history and read each one's change list. A `pathlog`
//! value, memoised per `(changeset, path)`, holds the first [`MAX_HITS`] changesets on the
//! first-parent chain from that changeset (itself included) whose changes touch the path, and
//! where to resume. A later tip of the same ref scans only until it reaches a changeset that has a
//! memo for the path and splices that memo in, so a ref that moves forward pays for its new
//! changesets only.

use asc_core::id::ChangesetId;
use asc_core::path::RepoPath;
use asc_store::objects::Objects;
use asc_store::refs::Refs;
use async_trait::async_trait;

use super::keyed::{DerivedKeyed, memo_key};
use super::{Error, Result, read_memo};
use crate::Engine;
use crate::graph::touches;

/// Hits one value holds.
pub const MAX_HITS: usize = 64;
/// Changesets one derivation reads before it stops and names where to resume.
pub const MAX_SCAN: usize = 2048;

/// The changesets touching a path on the first-parent chain from a changeset, newest first.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PathLog {
    /// At most [`MAX_HITS`], newest first.
    pub hits: Vec<ChangesetId>,
    /// The first changeset not yet looked at (resume there, inclusive); `None` at the root.
    pub next: Option<ChangesetId>,
}

fn changes_touch(cs: &asc_core::object::Changeset, path: &RepoPath) -> bool {
    cs.changes.as_slice().iter().any(|(k, _)| touches(k, path))
}

#[async_trait]
impl DerivedKeyed for PathLog {
    const NAME: &'static str = "pathlog";
    const VERSION: u32 = 1;
    type Key = RepoPath;

    fn encode_key(key: &RepoPath) -> Vec<u8> {
        key.as_bytes().to_vec()
    }

    async fn derive<O: Objects, R: Refs>(
        e: &Engine<O, R>,
        cs: ChangesetId,
        path: &RepoPath,
    ) -> Result<Self> {
        let mut hits = Vec::new();
        let mut at = Some(cs);
        let mut scanned = 0usize;
        while let Some(id) = at {
            if id != cs
                && let Some(b) = read_memo(&*e.objects, &memo_key::<Self>(id, path)).await?
                && let Ok(older) = Self::decode(&b)
            {
                return Ok(splice(hits, older));
            }
            if scanned == MAX_SCAN || hits.len() == MAX_HITS {
                return Ok(Self {
                    hits,
                    next: Some(id),
                });
            }
            let c = e.get_changeset(id).await?;
            scanned += 1;
            if changes_touch(&c, path) {
                hits.push(id);
            }
            at = c.parents.first().copied();
        }
        Ok(Self { hits, next: None })
    }

    fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(5 + 32 * (self.hits.len() + 1));
        out.push(u8::from(self.next.is_some()));
        if let Some(n) = self.next {
            out.extend(n.as_bytes());
        }
        out.extend((self.hits.len() as u32).to_le_bytes());
        for h in &self.hits {
            out.extend(h.as_bytes());
        }
        out
    }

    fn decode(b: &[u8]) -> Result<Self> {
        let bad = || Error::BadValue { name: "pathlog" };
        let (&flag, mut rest) = b.split_first().ok_or_else(bad)?;
        let id = |r: &mut &[u8]| -> Result<ChangesetId> {
            let (h, t) = r.split_first_chunk::<32>().ok_or_else(bad)?;
            *r = t;
            Ok(ChangesetId::from_bytes(*h))
        };
        let next = match flag {
            0 => None,
            1 => Some(id(&mut rest)?),
            _ => return Err(bad()),
        };
        let (n, mut rest) = rest.split_first_chunk::<4>().ok_or_else(bad)?;
        let n = u32::from_le_bytes(*n) as usize;
        if n > MAX_HITS || rest.len() != n * 32 {
            return Err(bad());
        }
        let hits = (0..n).map(|_| id(&mut rest)).collect::<Result<_>>()?;
        Ok(Self { hits, next })
    }
}

/// `newer` (the hits above the changeset `older` was memoised at) followed by `older`, cut to
/// [`MAX_HITS`]. When cut, resuming at the first hit left out loses nothing: every changeset
/// between the last kept hit and it touches nothing.
fn splice(mut newer: Vec<ChangesetId>, older: PathLog) -> PathLog {
    let room = MAX_HITS.saturating_sub(newer.len());
    if older.hits.len() > room {
        let next = Some(older.hits[room]);
        newer.extend_from_slice(&older.hits[..room]);
        return PathLog { hits: newer, next };
    }
    newer.extend(older.hits);
    PathLog {
        hits: newer,
        next: older.next,
    }
}

impl<O: Objects, R: Refs> Engine<O, R> {
    /// Up to `limit` changesets on the first-parent chain from `start` (inclusive) whose changes
    /// touch `path`, newest first, and where the next page starts (`None` when the chain is done).
    pub async fn path_log_page(
        &self,
        start: ChangesetId,
        path: &RepoPath,
        limit: usize,
    ) -> Result<(Vec<ChangesetId>, Option<ChangesetId>)> {
        let mut out = Vec::new();
        let mut at = Some(start);
        while let Some(id) = at {
            let v = self.get_keyed::<PathLog>(id, path).await?;
            for (i, &h) in v.hits.iter().enumerate() {
                if out.len() == limit {
                    return Ok((out, Some(v.hits[i])));
                }
                out.push(h);
            }
            if out.len() == limit {
                return Ok((out, v.next));
            }
            at = v.next;
        }
        Ok((out, None))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::derive::testutil::*;

    fn p(s: &str) -> RepoPath {
        RepoPath::new(s).unwrap()
    }

    /// A chain of `n` changesets; every third one writes `a/x`, the others write `b`.
    async fn chain(e: &TestEngine, n: usize) -> (Vec<ChangesetId>, Vec<ChangesetId>) {
        let mut all = Vec::new();
        let mut touching = Vec::new();
        let mut parent: Vec<ChangesetId> = Vec::new();
        for i in 0..n {
            let body = format!("{i}");
            let path = if i % 3 == 0 { "a/x" } else { "b" };
            let id = commit_ops(e, &parent, &[(path, Some(body.as_bytes()))]).await;
            if i % 3 == 0 {
                touching.push(id);
            }
            all.push(id);
            parent = vec![id];
        }
        all.reverse();
        touching.reverse();
        (all, touching)
    }

    #[tokio::test]
    async fn a_path_log_lists_the_touching_changesets_newest_first() {
        let e = engine();
        let (all, touching) = chain(&e, 10).await;
        let (got, next) = e.path_log_page(all[0], &p("a/x"), 100).await.unwrap();
        assert_eq!(got, touching);
        assert_eq!(next, None);
        // A directory matches what is inside it.
        let (dir, _) = e.path_log_page(all[0], &p("a"), 100).await.unwrap();
        assert_eq!(dir, touching);
        let (none, _) = e.path_log_page(all[0], &p("c"), 100).await.unwrap();
        assert!(none.is_empty());
    }

    #[tokio::test]
    async fn pages_resume_where_the_last_one_stopped() {
        let e = engine();
        let (all, touching) = chain(&e, 30).await;
        let mut seen = Vec::new();
        let mut at = Some(all[0]);
        while let Some(s) = at {
            let (page, next) = e.path_log_page(s, &p("a/x"), 3).await.unwrap();
            assert!(page.len() <= 3);
            seen.extend(page);
            at = next;
        }
        assert_eq!(seen, touching);
    }

    #[tokio::test]
    async fn a_later_tip_reuses_the_memo_of_an_earlier_one() {
        let e = engine();
        let (all, touching) = chain(&e, 20).await;
        let old = all[0];
        e.path_log_page(old, &p("a/x"), 100).await.unwrap();
        let newer = commit_ops(&e, &[old], &[("a/x", Some(b"new"))]).await;
        let v = PathLog::derive(&e, newer, &p("a/x")).await.unwrap();
        let mut want = vec![newer];
        want.extend(&touching);
        assert_eq!(v.hits, want);
        assert_eq!(v.next, None);
    }

    #[test]
    fn splicing_past_the_cap_resumes_at_the_first_hit_left_out() {
        let ids: Vec<ChangesetId> = (0..(MAX_HITS as u8 + 5))
            .map(|i| ChangesetId::from_bytes([i; 32]))
            .collect();
        let older = PathLog {
            hits: ids[3..].to_vec(),
            next: None,
        };
        let v = splice(ids[..3].to_vec(), older);
        assert_eq!(v.hits, ids[..MAX_HITS].to_vec());
        assert_eq!(v.next, Some(ids[MAX_HITS]));
    }

    #[test]
    fn values_round_trip_and_bad_bytes_are_refused() {
        let v = PathLog {
            hits: vec![
                ChangesetId::from_bytes([1; 32]),
                ChangesetId::from_bytes([2; 32]),
            ],
            next: Some(ChangesetId::from_bytes([3; 32])),
        };
        assert_eq!(PathLog::decode(&v.encode()).unwrap(), v);
        let done = PathLog {
            hits: Vec::new(),
            next: None,
        };
        assert_eq!(PathLog::decode(&done.encode()).unwrap(), done);
        assert!(PathLog::decode(&[]).is_err());
        assert!(PathLog::decode(&[2]).is_err());
        let mut long = v.encode();
        long.push(0);
        assert!(PathLog::decode(&long).is_err());
    }
}
