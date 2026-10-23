//! Branch comparison (`a..b` and `a...b`): ahead and behind counts, one page of changesets per
//! side and the changed files (SPEC §20.6, PLAN C30b).
//!
//! `ahead` is what `b` has that `a` lacks and `behind` is what `a` has that `b` lacks, both
//! over all parents. The merge base comes from the skip-list graph, so the cost is
//! O(log n + result), not the size of the history. Every part is bounded: each side lists at
//! most [`MAX_LIMIT`] changesets per page and counts at most [`COUNT_CAP`] (a count at the cap
//! means "at least that many"), and the file list stops at [`MAX_FILES`] changed files.
//!
//! The page tokens are the log cursors of each side. They are untrusted input: they are size
//! checked by the caller, decoded strictly here, and every id in them is read through the
//! store, which fails for an unknown one.

use asc_core::id::ChangesetId;
use asc_store::objects::Objects;
use asc_store::refs::Refs;
use futures::TryStreamExt;

use crate::diff::TreeChange;
use crate::graph::{self, LogCursor, LogQuery};
use crate::history::MAX_RPC_DIFF_VISITS;
use crate::{Engine, commit, tree};

/// Changesets per side when the request names no limit.
pub const DEFAULT_LIMIT: usize = 50;
/// Hard cap on the changesets of one side of one page.
pub const MAX_LIMIT: usize = 500;
/// Largest ahead or behind count reported.
pub const COUNT_CAP: usize = 100_000;
/// Changed files returned.
pub const MAX_FILES: usize = 5_000;
/// Longest page token accepted.
pub const MAX_TOKEN_BYTES: usize = 64 * 1024;

/// Comparison failures.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Graph(#[from] graph::Error),
    #[error(transparent)]
    Commit(#[from] commit::Error),
    #[error(transparent)]
    Tree(#[from] tree::Error),
    #[error(transparent)]
    Diff(#[from] crate::diff::Error),
    #[error("malformed page token")]
    BadToken,
    #[error("the revisions have no merge base")]
    NoMergeBase,
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

/// A comparison request.
#[derive(Clone, Debug)]
pub struct Query {
    pub a: ChangesetId,
    pub b: ChangesetId,
    /// Files are `merge_base..b` (`a...b`) instead of `a..b`.
    pub three_dot: bool,
    /// Changesets per side; 0 for [`DEFAULT_LIMIT`].
    pub limit: usize,
    pub ahead_page: Option<Vec<u8>>,
    pub behind_page: Option<Vec<u8>>,
}

/// One side of a comparison.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Side {
    /// Changesets of this page, newest generation first.
    pub ids: Vec<ChangesetId>,
    /// Changesets on this side, at most [`COUNT_CAP`].
    pub total: u64,
    /// Token of the next page.
    pub next: Option<Vec<u8>>,
}

/// The answer to a [`Query`].
#[derive(Clone, Debug)]
pub struct Comparison {
    pub merge_base: Option<ChangesetId>,
    pub ahead: Side,
    pub behind: Side,
    pub files: Vec<TreeChange>,
    /// More than [`MAX_FILES`] files changed.
    pub files_truncated: bool,
}

impl<O: Objects, R: Refs> Engine<O, R> {
    /// Compares `q.a` with `q.b`.
    pub async fn compare(&self, q: &Query) -> Result<Comparison> {
        let limit = match q.limit {
            0 => DEFAULT_LIMIT,
            n => n.min(MAX_LIMIT),
        };
        let (g, _) = self.graph();
        let merge_base = match g.merge_base(q.a, &[q.b], false).await {
            Err(e) if e.is_missing() => None,
            other => other?.into_iter().next(),
        };
        let side = |from: ChangesetId, hide: ChangesetId, page: Option<&Vec<u8>>| {
            let g = &g;
            let cursor = page.map(|t| decode(t)).transpose();
            async move {
                let cursor = cursor?;
                let base = LogQuery {
                    include: vec![from],
                    exclude: vec![hide],
                    ..LogQuery::default()
                };
                let total = g
                    .log(LogQuery {
                        limit: Some(COUNT_CAP),
                        ..base.clone()
                    })
                    .try_fold(0u64, |n, _| async move { Ok(n + 1) })
                    .await?;
                let page = g
                    .log_page(LogQuery {
                        limit: Some(limit),
                        cursor,
                        ..base
                    })
                    .await?;
                Ok::<_, Error>(Side {
                    ids: page.ids,
                    total,
                    next: page.next.map(|c| c.to_bytes()),
                })
            }
        };
        let ahead = side(q.b, q.a, q.ahead_page.as_ref()).await?;
        let behind = side(q.a, q.b, q.behind_page.as_ref()).await?;
        let from = if q.three_dot {
            merge_base.ok_or(Error::NoMergeBase)?
        } else {
            q.a
        };
        let (old, new) = (
            self.get_changeset(from).await?.root,
            self.get_changeset(q.b).await?.root,
        );
        let (mut files, _) = self
            .tree_changes_limited(old, new, None, MAX_RPC_DIFF_VISITS, MAX_FILES)
            .await?;
        let files_truncated = files.len() > MAX_FILES;
        files.truncate(MAX_FILES);
        Ok(Comparison {
            merge_base,
            ahead,
            behind,
            files,
            files_truncated,
        })
    }
}

fn decode(token: &[u8]) -> Result<LogCursor> {
    if token.len() > MAX_TOKEN_BYTES {
        return Err(Error::BadToken);
    }
    LogCursor::from_bytes(token).ok_or(Error::BadToken)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::commit::PathPolicy;
    use crate::history::testkit::*;

    struct Fx {
        _dir: tempfile::TempDir,
        e: Arc<Eng>,
    }

    async fn fx() -> Fx {
        let dir = tempfile::tempdir().unwrap();
        let refs = sqlite(dir.path(), "a.db").await;
        let e = engine(&refs, 1);
        Fx { _dir: dir, e }
    }

    async fn add(e: &Eng, parents: &[ChangesetId], file: &str, text: &str) -> ChangesetId {
        commit_files(
            e,
            parents,
            &[(file, text.as_bytes())],
            PathPolicy::Native { windows_safe: true },
            Vec::new(),
        )
        .await
    }

    fn q(a: ChangesetId, b: ChangesetId) -> Query {
        Query {
            a,
            b,
            three_dot: false,
            limit: 0,
            ahead_page: None,
            behind_page: None,
        }
    }

    /// base - a1 - a2 (a) and base - b1 - b2 - b3 (b).
    async fn forked(e: &Eng) -> [ChangesetId; 6] {
        let base = add(e, &[], "base", "x").await;
        let a1 = add(e, &[base], "a1", "1").await;
        let a2 = add(e, &[a1], "a2", "2").await;
        let b1 = add(e, &[base], "b1", "1").await;
        let b2 = add(e, &[b1], "b2", "2").await;
        let b3 = add(e, &[b2], "b3", "3").await;
        [base, a1, a2, b1, b2, b3]
    }

    fn paths(c: &Comparison) -> Vec<String> {
        c.files
            .iter()
            .map(|f| String::from_utf8_lossy(f.path.as_bytes()).into_owned())
            .collect()
    }

    #[tokio::test]
    async fn counts_sides_and_merge_base() {
        let f = fx().await;
        let [base, _, a2, _, b2, b3] = forked(&f.e).await;
        let c = f.e.compare(&q(a2, b3)).await.unwrap();
        assert_eq!(c.merge_base, Some(base));
        assert_eq!((c.ahead.total, c.behind.total), (3, 2));
        assert_eq!(c.ahead.ids.len(), 3);
        assert_eq!(c.ahead.ids[0], b3);
        assert_eq!(c.ahead.ids[1], b2);
        assert_eq!(c.behind.ids.len(), 2);
        assert!(c.ahead.next.is_none() && c.behind.next.is_none());
        // Same changeset: nothing either way, no files.
        let same = f.e.compare(&q(b3, b3)).await.unwrap();
        assert_eq!((same.ahead.total, same.behind.total), (0, 0));
        assert!(same.files.is_empty());
    }

    #[tokio::test]
    async fn two_dot_and_three_dot_differ_in_files() {
        let f = fx().await;
        let [_, _, a2, _, _, b3] = forked(&f.e).await;
        let two = f.e.compare(&q(a2, b3)).await.unwrap();
        // a..b diff includes the a-side files as deletions.
        assert_eq!(
            paths(&two),
            ["a1", "a2", "b1", "b2", "b3"].map(String::from)
        );
        let mut t = q(a2, b3);
        t.three_dot = true;
        let three = f.e.compare(&t).await.unwrap();
        assert_eq!(paths(&three), ["b1", "b2", "b3"].map(String::from));
    }

    #[tokio::test]
    async fn pages_each_side_by_token() {
        let f = fx().await;
        let [_, _, a2, _, b2, b3] = forked(&f.e).await;
        let mut r = q(a2, b3);
        r.limit = 2;
        let first = f.e.compare(&r).await.unwrap();
        assert_eq!(first.ahead.ids, vec![b3, b2]);
        assert_eq!(first.ahead.total, 3);
        assert!(first.ahead.next.is_some());
        r.ahead_page = first.ahead.next.clone();
        let second = f.e.compare(&r).await.unwrap();
        assert_eq!(second.ahead.ids.len(), 1);
        assert!(second.ahead.next.is_none());
        assert!(!first.ahead.ids.contains(&second.ahead.ids[0]));
    }

    #[tokio::test]
    async fn unrelated_histories_have_no_merge_base() {
        let f = fx().await;
        let x = add(&f.e, &[], "x", "1").await;
        let y = add(&f.e, &[], "y", "1").await;
        let two = f.e.compare(&q(x, y)).await.unwrap();
        assert_eq!(two.merge_base, None);
        assert_eq!((two.ahead.total, two.behind.total), (1, 1));
        let mut t = q(x, y);
        t.three_dot = true;
        assert!(matches!(f.e.compare(&t).await, Err(Error::NoMergeBase)));
    }

    #[tokio::test]
    async fn rejects_bad_tokens() {
        let f = fx().await;
        let [_, _, a2, _, _, b3] = forked(&f.e).await;
        let mut r = q(a2, b3);
        r.ahead_page = Some(vec![1, 2, 3]);
        assert!(matches!(f.e.compare(&r).await, Err(Error::BadToken)));
        r.ahead_page = Some(vec![0; MAX_TOKEN_BYTES + 1]);
        assert!(matches!(f.e.compare(&r).await, Err(Error::BadToken)));
    }

    #[tokio::test]
    async fn limit_is_capped() {
        let f = fx().await;
        let (chain, first) = Chain::new(&f.e).await;
        let ids = chain.line(&f.e, first, MAX_LIMIT + 5).await;
        let root = ids[0];
        let mut r = q(root, *ids.last().unwrap());
        r.limit = usize::MAX;
        let c = f.e.compare(&r).await.unwrap();
        assert_eq!(c.ahead.ids.len(), MAX_LIMIT);
        assert_eq!(c.ahead.total, (MAX_LIMIT + 4) as u64);
        assert!(c.ahead.next.is_some());
    }
}
