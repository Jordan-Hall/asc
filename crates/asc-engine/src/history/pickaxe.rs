//! Pickaxe log (`git log -S`/`-G` over one path): the changesets that changed how often a
//! string, or a regular expression, occurs in one file (SPEC §20.5, PLAN C30c).
//!
//! The path is required: a pickaxe without one would read every file of every changeset, which
//! SPEC §22.4 forbids, so [`Error::PathRequired`] rejects it. The walk goes backwards along first
//! parents from a revision. A candidate is a hit when the number of matches in the file differs
//! from the number in its first parent's file (a missing file counts as zero). Every changeset
//! visited counts against `scan_budget`, hit or not; when the budget runs out the page ends with
//! [`Stop::Budget`] and a resume token. A page that fills its `limit` ends with [`Stop::Limit`]
//! and the same kind of token. The token is untrusted: it is size checked, decoded strictly and
//! its id is read through the store.
//!
//! Work and memory are bounded: the needle is at most [`MAX_NEEDLE_BYTES`], regular expressions
//! are compiled with size limits (the `regex` crate matches in linear time), and a file version
//! over [`MAX_FILE_BYTES`] is skipped as if it held no match.

use bytes::Bytes;
use asc_core::id::{ChangesetId, FileId};
use asc_core::object::{Entry, Kind};
use asc_core::path::RepoPath;
use asc_store::objects::Objects;
use asc_store::refs::Refs;

use crate::{Engine, commit, diff, tree};

pub use super::linehist::{
    DEFAULT_LIMIT, DEFAULT_SCAN_BUDGET, MAX_FILE_BYTES, MAX_LIMIT, MAX_SCAN_BUDGET,
};

/// Longest needle or pattern accepted.
pub const MAX_NEEDLE_BYTES: usize = 4096;
/// Compiled size limit of a pattern.
const REGEX_SIZE_LIMIT: usize = 1 << 20;

const TOKEN_VERSION: u8 = 1;

/// Pickaxe failures.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Commit(#[from] commit::Error),
    #[error(transparent)]
    Tree(#[from] tree::Error),
    #[error(transparent)]
    Diff(#[from] diff::Error),
    #[error("a pickaxe search needs a path; walking every file of the history is not allowed")]
    PathRequired,
    #[error("the pickaxe string is empty")]
    EmptyNeedle,
    #[error("the pickaxe is {0} bytes, over the limit of {MAX_NEEDLE_BYTES}")]
    NeedleTooLong(usize),
    #[error("bad pickaxe pattern: {0}")]
    BadPattern(String),
    #[error("malformed resume token")]
    BadToken,
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

/// A pickaxe request.
#[derive(Clone, Debug)]
pub struct Query {
    pub rev: ChangesetId,
    /// The one file searched; `None` is refused with [`Error::PathRequired`].
    pub path: Option<RepoPath>,
    pub needle: Vec<u8>,
    /// Treat `needle` as a regular expression (UTF-8) rather than literal bytes.
    pub regex: bool,
    /// Changesets per page; 0 for [`DEFAULT_LIMIT`].
    pub limit: usize,
    /// Candidates per page; 0 for [`DEFAULT_SCAN_BUDGET`].
    pub scan_budget: u32,
    /// Token of an earlier page. The revision comes from the token.
    pub resume: Option<Vec<u8>>,
}

/// Why a page ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Stop {
    /// History ended.
    Done,
    /// `limit` changesets were found; continue with the token.
    Limit(Vec<u8>),
    /// `scan_budget` candidates were scanned; continue with the token.
    Budget(Vec<u8>),
}

/// One page of pickaxe results, newest first.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Page {
    pub hits: Vec<ChangesetId>,
    pub scanned: u32,
    pub stop: Stop,
}

enum Matcher {
    Literal(Vec<u8>),
    Regex(regex::bytes::Regex),
}

impl Matcher {
    fn new(q: &Query) -> Result<Self> {
        if q.needle.is_empty() {
            return Err(Error::EmptyNeedle);
        }
        if q.needle.len() > MAX_NEEDLE_BYTES {
            return Err(Error::NeedleTooLong(q.needle.len()));
        }
        if !q.regex {
            return Ok(Self::Literal(q.needle.clone()));
        }
        let pat = std::str::from_utf8(&q.needle).map_err(|e| Error::BadPattern(e.to_string()))?;
        regex::bytes::RegexBuilder::new(pat)
            .size_limit(REGEX_SIZE_LIMIT)
            .dfa_size_limit(8 << 20)
            .nest_limit(64)
            .multi_line(true)
            .build()
            .map(Self::Regex)
            .map_err(|e| Error::BadPattern(e.to_string()))
    }

    /// Non-overlapping matches in `hay`.
    fn count(&self, hay: &[u8]) -> usize {
        match self {
            Self::Regex(r) => r.find_iter(hay).count(),
            Self::Literal(n) => {
                let (mut count, mut at) = (0, 0);
                while at + n.len() <= hay.len() {
                    match hay[at..].windows(n.len()).position(|w| w == n.as_slice()) {
                        Some(p) => {
                            count += 1;
                            at += p + n.len();
                        }
                        None => break,
                    }
                }
                count
            }
        }
    }
}

fn encode_token(cs: ChangesetId) -> Vec<u8> {
    let mut out = vec![TOKEN_VERSION];
    out.extend_from_slice(cs.as_bytes());
    out
}

fn decode_token(b: &[u8]) -> Result<ChangesetId> {
    if b.len() != 33 || b[0] != TOKEN_VERSION {
        return Err(Error::BadToken);
    }
    Ok(ChangesetId::from_bytes(
        b[1..33].try_into().map_err(|_| Error::BadToken)?,
    ))
}

impl<O: Objects, R: Refs> Engine<O, R> {
    /// The changesets whose first-parent diff changed the number of matches of the needle in
    /// one file, newest first.
    pub async fn pickaxe(&self, q: &Query) -> Result<Page> {
        let path = q.path.as_ref().ok_or(Error::PathRequired)?;
        let matcher = Matcher::new(q)?;
        let limit = match q.limit {
            0 => DEFAULT_LIMIT,
            n => n.min(MAX_LIMIT),
        };
        let budget = match q.scan_budget {
            0 => DEFAULT_SCAN_BUDGET,
            n => n.min(MAX_SCAN_BUDGET),
        };
        let mut cur = match &q.resume {
            Some(t) => decode_token(t)?,
            None => q.rev,
        };
        let mut hits = Vec::new();
        let mut scanned = 0u32;
        loop {
            if scanned >= budget {
                return Ok(Page {
                    hits,
                    scanned,
                    stop: Stop::Budget(encode_token(cur)),
                });
            }
            scanned += 1;
            let c = self.get_changeset(cur).await?;
            let parent = c.parents.first().copied();
            // A single-parent changeset that does not list the path holds the parent's file.
            let unchanged =
                parent.is_some() && c.parents.len() == 1 && c.changes.get(path).is_none();
            if !unchanged {
                let new = self.lookup(c.root, path).await?.filter(is_file);
                let old = match parent {
                    Some(p) => {
                        let pc = self.get_changeset(p).await?;
                        self.lookup(pc.root, path).await?.filter(is_file)
                    }
                    None => None,
                };
                if new.as_ref().map(|e| e.id) != old.as_ref().map(|e| e.id) {
                    let n = self.count_in(&matcher, new.as_ref()).await?;
                    let o = self.count_in(&matcher, old.as_ref()).await?;
                    if n != o {
                        hits.push(cur);
                    }
                }
            }
            let Some(p) = parent else {
                return Ok(Page {
                    hits,
                    scanned,
                    stop: Stop::Done,
                });
            };
            cur = p;
            if hits.len() >= limit {
                return Ok(Page {
                    hits,
                    scanned,
                    stop: Stop::Limit(encode_token(cur)),
                });
            }
        }
    }

    async fn count_in(&self, m: &Matcher, e: Option<&Entry>) -> Result<usize> {
        let Some(e) = e else { return Ok(0) };
        if e.size > MAX_FILE_BYTES {
            return Ok(0);
        }
        let bytes: Bytes = self.read_file(FileId::from_bytes(e.id), e.size).await?;
        Ok(m.count(&bytes))
    }
}

fn is_file(entry: &Entry) -> bool {
    matches!(entry.kind, Kind::File | Kind::Exec)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commit::PathPolicy;
    use crate::history::testkit::*;

    fn native() -> PathPolicy {
        PathPolicy::Native { windows_safe: true }
    }

    async fn commit_f(e: &Eng, parents: &[ChangesetId], text: &str) -> ChangesetId {
        commit_files(
            e,
            parents,
            &[("f.txt", text.as_bytes())],
            native(),
            Vec::new(),
        )
        .await
    }

    fn query(rev: ChangesetId, needle: &str) -> Query {
        Query {
            rev,
            path: Some(RepoPath::new("f.txt").unwrap()),
            needle: needle.as_bytes().to_vec(),
            regex: false,
            limit: 0,
            scan_budget: 0,
            resume: None,
        }
    }

    /// c1 adds `foo`; c2 changes another line; c3 adds a second `foo`; c4 removes both.
    async fn history(e: &Eng) -> Vec<ChangesetId> {
        let c1 = commit_f(e, &[], "foo\nbar\n").await;
        let c2 = commit_f(e, &[c1], "foo\nbaz\n").await;
        let c3 = commit_f(e, &[c2], "foo\nbaz\nfoo\n").await;
        let c4 = commit_f(e, &[c3], "baz\n").await;
        vec![c1, c2, c3, c4]
    }

    async fn fx() -> (tempfile::TempDir, std::sync::Arc<Eng>) {
        let dir = tempfile::tempdir().unwrap();
        let refs = sqlite(dir.path(), "a.db").await;
        let e = engine(&refs, 1);
        (dir, e)
    }

    #[tokio::test]
    async fn finds_changesets_that_change_the_match_count() {
        let (_d, e) = fx().await;
        let c = history(&e).await;
        let page = e.pickaxe(&query(c[3], "foo")).await.unwrap();
        assert_eq!(page.hits, vec![c[3], c[2], c[0]]);
        assert_eq!(page.stop, Stop::Done);
        let page = e.pickaxe(&query(c[3], "nothing")).await.unwrap();
        assert!(page.hits.is_empty());
    }

    #[tokio::test]
    async fn regex_pickaxe_counts_matches() {
        let (_d, e) = fx().await;
        let c = history(&e).await;
        let mut q = query(c[3], "^ba[rz]$");
        q.regex = true;
        let page = e.pickaxe(&q).await.unwrap();
        // bar -> baz keeps the count at one; the root adds the first.
        assert_eq!(page.hits, vec![c[0]]);
        q.needle = b"(".to_vec();
        assert!(matches!(e.pickaxe(&q).await, Err(Error::BadPattern(_))));
    }

    #[tokio::test]
    async fn pickaxe_without_a_path_is_rejected() {
        let (_d, e) = fx().await;
        let c = history(&e).await;
        let mut q = query(c[3], "foo");
        q.path = None;
        assert!(matches!(e.pickaxe(&q).await, Err(Error::PathRequired)));
        let mut q = query(c[3], "");
        assert!(matches!(e.pickaxe(&q).await, Err(Error::EmptyNeedle)));
        q.needle = vec![b'x'; MAX_NEEDLE_BYTES + 1];
        assert!(matches!(e.pickaxe(&q).await, Err(Error::NeedleTooLong(_))));
    }

    #[tokio::test]
    async fn budget_exhaustion_truncates_and_resumes() {
        let (_d, e) = fx().await;
        let c = history(&e).await;
        let mut q = query(c[3], "foo");
        q.scan_budget = 2;
        let first = e.pickaxe(&q).await.unwrap();
        assert_eq!(first.scanned, 2);
        assert_eq!(first.hits, vec![c[3], c[2]]);
        let Stop::Budget(tok) = first.stop else {
            panic!("expected a budget stop")
        };
        q.resume = Some(tok);
        let second = e.pickaxe(&q).await.unwrap();
        assert_eq!(second.hits, vec![c[0]]);
        assert_eq!(second.stop, Stop::Done);
    }

    #[tokio::test]
    async fn limit_pages_and_bad_tokens() {
        let (_d, e) = fx().await;
        let c = history(&e).await;
        let mut q = query(c[3], "foo");
        q.limit = 1;
        let page = e.pickaxe(&q).await.unwrap();
        assert_eq!(page.hits, vec![c[3]]);
        let Stop::Limit(tok) = page.stop else {
            panic!("expected a limit stop")
        };
        q.resume = Some(tok);
        let page = e.pickaxe(&q).await.unwrap();
        assert_eq!(page.hits, vec![c[2]]);
        q.resume = Some(vec![1; 5]);
        assert!(matches!(e.pickaxe(&q).await, Err(Error::BadToken)));
        q.resume = Some(vec![1; 33]);
        assert!(e.pickaxe(&q).await.is_err());
    }

    #[test]
    fn literal_count_is_non_overlapping() {
        let m = Matcher::Literal(b"aa".to_vec());
        assert_eq!(m.count(b"aaaaa"), 2);
        assert_eq!(m.count(b""), 0);
    }
}
