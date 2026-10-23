//! Line history (`git log -L`): the changesets that changed a line range of one file
//! (SPEC §20.6, PLAN C30a).
//!
//! The walk starts at a revision and goes backwards along first parents. At every candidate the
//! single-file line diff against the first parent says whether the range was touched and where
//! the range sits in the parent; the range is carried to the parent through that diff. A
//! candidate costs one changeset read plus, when the file differs from the parent's, two file
//! reads and one line diff, so a page costs O(min(candidates, scan budget) x file size).
//!
//! Every changeset visited counts against `scan_budget`, touched or not, so a long run of
//! untouched changesets cannot stretch a request. When the budget runs out the page ends
//! with [`Stop::Budget`] and a resume token. A page that fills its `limit` ends with
//! [`Stop::Limit`] and the same kind of token. The token holds the next changeset, the path and
//! the range in that changeset's coordinates; it is untrusted input, so it is decoded
//! strictly and every id in it is read through the store, which fails for an unknown one.
//!
//! Merges are followed through their first parent only (like `git log --first-parent -L`): a
//! line a merge brings in from another parent is attributed to the merge.

use std::ops::Range;

use bytes::Bytes;
use asc_core::id::{ChangesetId, FileId};
use asc_core::object::{Change, Entry, Kind};
use asc_core::path::RepoPath;
use asc_store::objects::Objects;
use asc_store::refs::Refs;

use crate::blame::{NoIndex, PathIndex};
use crate::diff::line::{self, Algorithm, Hunk};
use crate::{Engine, commit, diff, tree};

/// Candidates scanned per page when the request names none.
pub const DEFAULT_SCAN_BUDGET: u32 = 10_000;
/// Hard cap on `scan_budget` (SPEC §22.4: 10,000 by default, 50,000 at most).
pub const MAX_SCAN_BUDGET: u32 = 50_000;
/// Changesets per page when the request names no limit.
pub const DEFAULT_LIMIT: usize = 100;
/// Hard cap on the changesets of one page.
pub const MAX_LIMIT: usize = 1_000;
/// Largest file version the walk reads. Each step holds one version and diffs it against the
/// next, so this bounds memory.
pub const MAX_FILE_BYTES: u64 = 16 << 20;
/// Longest resume token accepted.
pub const MAX_TOKEN_BYTES: usize = 4096 + 64;

const TOKEN_VERSION: u8 = 1;

/// Line history failures.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Commit(#[from] commit::Error),
    #[error(transparent)]
    Tree(#[from] tree::Error),
    #[error(transparent)]
    Diff(#[from] diff::Error),
    #[error(transparent)]
    Blame(#[from] crate::blame::Error),
    #[error("{} is not a file at that revision", String::from_utf8_lossy(.0.as_bytes()))]
    NotFound(RepoPath),
    #[error("line range {}..{} is outside a file of {lines} lines", .range.start, .range.end)]
    BadRange { range: Range<u32>, lines: u32 },
    #[error("file is binary")]
    Binary,
    #[error("file is {0} bytes, over the line history limit of {MAX_FILE_BYTES}")]
    TooLarge(u64),
    #[error("malformed resume token")]
    BadToken,
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

/// A line history request. Lines are zero-based and the range is half open; `end == 0` means
/// the end of the file (the convention of `Blame`).
#[derive(Clone, Debug)]
pub struct Query {
    pub rev: ChangesetId,
    pub path: RepoPath,
    pub start: u32,
    pub end: u32,
    /// Changesets per page; 0 for [`DEFAULT_LIMIT`].
    pub limit: usize,
    /// Candidates per page; 0 for [`DEFAULT_SCAN_BUDGET`].
    pub scan_budget: u32,
    /// Token of an earlier page. The path and range come from the token.
    pub resume: Option<Vec<u8>>,
}

/// Why a page ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Stop {
    /// The range was traced to where its lines were introduced, or history ended.
    Done,
    /// `limit` changesets were found; continue with the token.
    Limit(Vec<u8>),
    /// `scan_budget` candidates were scanned; continue with the token.
    Budget(Vec<u8>),
}

/// One page of line history, newest first.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Page {
    pub hits: Vec<ChangesetId>,
    /// Candidates scanned for this page.
    pub scanned: u32,
    pub stop: Stop,
}

/// Where a walk stands: the next changeset to examine and the range in its coordinates.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Cursor {
    cs: ChangesetId,
    path: RepoPath,
    start: u32,
    end: u32,
}

impl Cursor {
    fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(45 + self.path.as_bytes().len());
        out.push(TOKEN_VERSION);
        out.extend_from_slice(self.cs.as_bytes());
        out.extend_from_slice(&self.start.to_le_bytes());
        out.extend_from_slice(&self.end.to_le_bytes());
        out.extend_from_slice(self.path.as_bytes());
        out
    }

    fn decode(b: &[u8]) -> Result<Self> {
        if b.len() > MAX_TOKEN_BYTES || b.len() < 42 || b[0] != TOKEN_VERSION {
            return Err(Error::BadToken);
        }
        let cs = ChangesetId::from_bytes(b[1..33].try_into().map_err(|_| Error::BadToken)?);
        let word = |at: usize| u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]]);
        let (start, end) = (word(33), word(37));
        let path = RepoPath::new(&b[41..]).map_err(|_| Error::BadToken)?;
        if start >= end {
            return Err(Error::BadToken);
        }
        Ok(Self {
            cs,
            path,
            start,
            end,
        })
    }
}

fn is_file(entry: &Entry) -> bool {
    matches!(entry.kind, Kind::File | Kind::Exec)
}

/// Maps a boundary between lines of the new file to the old file through `hunks`. A boundary
/// inside a changed region goes to that region's old start (`is_end` false) or old end. A
/// deletion exactly at the boundary stays outside a range that ends there and outside one
/// that starts there.
fn old_boundary(hunks: &[Hunk], p: u32, is_end: bool) -> u32 {
    let mut shift = 0i64;
    for h in hunks {
        let deletion_here = h.after.start == h.after.end && h.after.start == p;
        if h.after.end <= p && !(is_end && deletion_here) {
            shift +=
                i64::from(h.before.end - h.before.start) - i64::from(h.after.end - h.after.start);
        } else if h.after.start < p && p < h.after.end {
            return if is_end { h.before.end } else { h.before.start };
        } else if h.after.start >= p {
            break;
        }
    }
    (i64::from(p) + shift) as u32
}

/// Carries the new-file range `[s, e)` back through a line diff: whether the diff touches the
/// range, and the range in the old file (possibly empty).
fn carry(hunks: &[Hunk], s: u32, e: u32) -> (bool, Range<u32>) {
    let touched = hunks.iter().any(|h| {
        if h.after.start == h.after.end {
            s < h.after.start && h.after.start < e
        } else {
            h.after.start < e && s < h.after.end
        }
    });
    let (a, b) = (old_boundary(hunks, s, false), old_boundary(hunks, e, true));
    (touched, a..b.max(a))
}

/// A file version read for the walk.
struct Version {
    id: [u8; 32],
    bytes: Bytes,
}

impl<O: Objects, R: Refs> Engine<O, R> {
    /// [`Engine::line_history_indexed`] without a path index.
    pub async fn line_history(&self, q: &Query) -> Result<Page> {
        self.line_history_indexed(q, &NoIndex).await
    }

    /// The changesets that touched a line range of a file, walking first parents from the
    /// revision (or from the resume token). `index` may name the nearest changeset that
    /// changed the path, which skips the changesets in between at the cost of one candidate.
    pub async fn line_history_indexed(&self, q: &Query, index: &dyn PathIndex) -> Result<Page> {
        let limit = match q.limit {
            0 => DEFAULT_LIMIT,
            n => n.min(MAX_LIMIT),
        };
        let budget = match q.scan_budget {
            0 => DEFAULT_SCAN_BUDGET,
            n => n.min(MAX_SCAN_BUDGET),
        };
        let mut cur = match &q.resume {
            Some(t) => Cursor::decode(t)?,
            None => self.start_cursor(q).await?,
        };
        let mut hits = Vec::new();
        let mut scanned = 0u32;
        // The version of the file at `cur`, read once and reused as the next step's "new".
        let mut loaded: Option<Version> = None;
        loop {
            if scanned >= budget {
                let stop = Stop::Budget(cur.encode());
                return Ok(Page {
                    hits,
                    scanned,
                    stop,
                });
            }
            scanned += 1;
            if let Some(t) = index.last_touching(cur.cs, &cur.path).await?
                && t != cur.cs
            {
                cur.cs = t;
                loaded = None;
            }
            let c = self.get_changeset(cur.cs).await?;
            let Some(&parent) = c.parents.first() else {
                // The root changeset introduced every line it still holds.
                hits.push(cur.cs);
                return Ok(done(hits, scanned));
            };
            if c.parents.len() == 1 && c.changes.get(&cur.path).is_none() {
                // The file is the parent's, byte for byte.
                cur.cs = parent;
                continue;
            }
            let Some(new_entry) = self.file_at(&c, &cur.path).await? else {
                return Ok(done(hits, scanned));
            };
            // A rename: the destination records its source, which the same changeset deletes.
            let renamed_from = match c.changes.get(&cur.path) {
                Some(Change::Write {
                    copy_from: Some(src),
                    ..
                }) if matches!(c.changes.get(src), Some(Change::Delete)) => Some(src.clone()),
                _ => None,
            };
            let pc = self.get_changeset(parent).await?;
            let mut old_path = cur.path.clone();
            let mut old_entry = self.file_at(&pc, &old_path).await?;
            if old_entry.is_none()
                && let Some(src) = renamed_from
            {
                old_entry = self.file_at(&pc, &src).await?;
                old_path = src;
            }
            let Some(old_entry) = old_entry else {
                // The file is created here.
                hits.push(cur.cs);
                return Ok(done(hits, scanned));
            };
            if old_entry.id == new_entry.id {
                cur.cs = parent;
                cur.path = old_path;
                loaded = None;
                continue;
            }
            let new = match loaded.take() {
                Some(v) if v.id == new_entry.id => v.bytes,
                _ => self.read_version(&new_entry).await?,
            };
            let old = self.read_version(&old_entry).await?;
            let d = line::diff(&old, &new, Algorithm::Myers, false);
            let (touched, range) = carry(&d.hunks, cur.start, cur.end);
            if touched {
                hits.push(cur.cs);
            }
            cur = Cursor {
                cs: parent,
                path: old_path,
                start: range.start,
                end: range.end,
            };
            loaded = Some(Version {
                id: old_entry.id,
                bytes: old,
            });
            if touched && range.is_empty() {
                // Every line of the range was introduced by this changeset.
                return Ok(done(hits, scanned));
            }
            if hits.len() >= limit {
                let stop = Stop::Limit(cur.encode());
                return Ok(Page {
                    hits,
                    scanned,
                    stop,
                });
            }
        }
    }

    /// The first cursor of a request: validates the file and the range at the revision.
    async fn start_cursor(&self, q: &Query) -> Result<Cursor> {
        let c = self.get_changeset(q.rev).await?;
        let entry = self
            .file_at(&c, &q.path)
            .await?
            .ok_or_else(|| Error::NotFound(q.path.clone()))?;
        let bytes = self.read_version(&entry).await?;
        let lines = line::split_lines(&bytes).len() as u32;
        let end = if q.end == 0 { lines } else { q.end };
        if q.start >= end || end > lines {
            return Err(Error::BadRange {
                range: q.start..end,
                lines,
            });
        }
        Ok(Cursor {
            cs: q.rev,
            path: q.path.clone(),
            start: q.start,
            end,
        })
    }

    async fn file_at(
        &self,
        c: &asc_core::object::Changeset,
        path: &RepoPath,
    ) -> Result<Option<Entry>> {
        Ok(self.lookup(c.root, path).await?.filter(is_file))
    }

    /// The text of a file version; binary and oversized files are refused.
    async fn read_version(&self, e: &Entry) -> Result<Bytes> {
        if e.size > MAX_FILE_BYTES {
            return Err(Error::TooLarge(e.size));
        }
        let bytes = self.read_file(FileId::from_bytes(e.id), e.size).await?;
        if line::is_binary(&bytes) {
            return Err(Error::Binary);
        }
        Ok(bytes)
    }
}

fn done(hits: Vec<ChangesetId>, scanned: u32) -> Page {
    Page {
        hits,
        scanned,
        stop: Stop::Done,
    }
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

    fn lines(n: usize, tag: &str) -> String {
        (0..n).map(|i| format!("{tag}{i}\n")).collect()
    }

    fn query(rev: ChangesetId, start: u32, end: u32) -> Query {
        Query {
            rev,
            path: RepoPath::new("f.txt").unwrap(),
            start,
            end,
            limit: 0,
            scan_budget: 0,
            resume: None,
        }
    }

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

    use std::sync::Arc;

    #[test]
    fn boundary_mapping() {
        // old: a b c d  ->  new: a X Y c d   (b replaced by X Y)
        let hunks = [Hunk {
            before: 1..2,
            after: 1..3,
        }];
        assert_eq!(carry(&hunks, 3, 5), (false, 2..4));
        assert_eq!(carry(&hunks, 0, 2), (true, 0..2));
        assert_eq!(carry(&hunks, 1, 3), (true, 1..2));
        assert_eq!(carry(&hunks, 0, 1), (false, 0..1));
        // A deletion between lines 1 and 2 touches only a range that spans it.
        let del = [Hunk {
            before: 1..3,
            after: 1..1,
        }];
        assert!(!carry(&del, 1, 3).0);
        assert!(!carry(&del, 0, 1).0);
        assert_eq!(carry(&del, 0, 2), (true, 0..4));
    }

    #[tokio::test]
    async fn follows_a_range_through_shifts() {
        let f = fx().await;
        let e = &f.e;
        let base = lines(10, "l");
        let c1 = commit_f(e, &[], &base).await;
        // c2 inserts two lines at the top: the range moves down.
        let c2 = commit_f(e, &[c1], &format!("new0\nnew1\n{base}")).await;
        // c3 edits old line 5 (now line 7).
        let c3 = commit_f(
            e,
            &[c2],
            &format!("new0\nnew1\n{}", base.replace("l5\n", "L5\n")),
        )
        .await;
        // c4 touches an unrelated line (line 0).
        let c4 = commit_f(
            e,
            &[c3],
            &format!("top\nnew1\n{}", base.replace("l5\n", "L5\n")),
        )
        .await;
        let page = e.line_history(&query(c4, 7, 8)).await.unwrap();
        assert_eq!(page.hits, vec![c3, c1]);
        assert_eq!(page.stop, Stop::Done);
        // The unrelated line 0 is touched by c4, c2 (inserted), and the first commit.
        let page = e.line_history(&query(c4, 0, 1)).await.unwrap();
        assert_eq!(page.hits, vec![c4, c2]);
    }

    #[tokio::test]
    async fn exhausting_the_scan_budget_truncates_with_a_resume_token() {
        let f = fx().await;
        let e = &f.e;
        let mut parent = commit_f(e, &[], "a\nb\nc\n").await;
        let mut all = vec![parent];
        for i in 0..6 {
            parent = commit_f(e, &[parent], &format!("a{i}\nb\nc\n")).await;
            all.push(parent);
        }
        let tip = parent;
        let mut q = query(tip, 0, 1);
        q.scan_budget = 3;
        let mut found = Vec::new();
        let mut pages = 0;
        loop {
            let page = e.line_history(&q).await.unwrap();
            assert!(page.scanned <= 3);
            found.extend(page.hits);
            pages += 1;
            match page.stop {
                Stop::Budget(t) => q.resume = Some(t),
                Stop::Done => break,
                Stop::Limit(_) => panic!("no limit was set"),
            }
            assert!(pages < 20);
        }
        assert!(pages >= 2);
        let mut want = all.clone();
        want.reverse();
        assert_eq!(found, want);
        // The first page alone is truncated.
        q.resume = None;
        let first = e.line_history(&q).await.unwrap();
        assert!(matches!(first.stop, Stop::Budget(_)));
        assert_eq!(first.scanned, 3);
    }

    #[tokio::test]
    async fn the_limit_pages_the_result() {
        let f = fx().await;
        let e = &f.e;
        let mut parent = commit_f(e, &[], "a\n").await;
        for i in 0..5 {
            parent = commit_f(e, &[parent], &format!("a{i}\n")).await;
        }
        let mut q = query(parent, 0, 0);
        q.limit = 2;
        let first = e.line_history(&q).await.unwrap();
        assert_eq!(first.hits.len(), 2);
        let Stop::Limit(t) = first.stop else {
            panic!("expected a limit stop")
        };
        q.resume = Some(t);
        let second = e.line_history(&q).await.unwrap();
        assert_eq!(second.hits.len(), 2);
        assert!(!first.hits.iter().any(|h| second.hits.contains(h)));
    }

    #[tokio::test]
    async fn follows_renames() {
        let f = fx().await;
        let e = &f.e;
        let c1 = commit_f(e, &[], "a\nb\n").await;
        let c2 = commit_f(e, &[c1], "a\nB\n").await;
        let root = e.get_changeset(c2).await.unwrap().root;
        let renamed = put_content(e, "a\nB\n".as_bytes()).await;
        let new_root = e
            .apply(
                root,
                &[
                    (RepoPath::new("f.txt").unwrap(), Change::Delete),
                    (RepoPath::new("g.txt").unwrap(), renamed),
                ],
            )
            .await
            .unwrap();
        let c3 = e
            .create_changeset(
                crate::commit::NewChangeset {
                    parents: vec![c2],
                    author: asc_core::sig::Signature::new(
                        b"A".to_vec(),
                        b"a@x".to_vec(),
                        1_700_000_001,
                        0,
                    )
                    .unwrap(),
                    committer: asc_core::sig::Signature::new(
                        b"A".to_vec(),
                        b"a@x".to_vec(),
                        1_700_000_001,
                        0,
                    )
                    .unwrap(),
                    message: b"rename\n".to_vec(),
                    extra: Vec::new(),
                    root: new_root,
                    copies: vec![(
                        RepoPath::new("g.txt").unwrap(),
                        RepoPath::new("f.txt").unwrap(),
                    )],
                },
                native(),
            )
            .await
            .unwrap();
        let mut q = query(c3, 1, 2);
        q.path = RepoPath::new("g.txt").unwrap();
        let page = e.line_history(&q).await.unwrap();
        assert_eq!(page.hits, vec![c2, c1]);
    }

    #[tokio::test]
    async fn rejects_bad_input() {
        let f = fx().await;
        let e = &f.e;
        let c1 = commit_f(e, &[], "a\nb\n").await;
        assert!(matches!(
            e.line_history(&query(c1, 0, 3)).await,
            Err(Error::BadRange { .. })
        ));
        assert!(matches!(
            e.line_history(&query(c1, 2, 2)).await,
            Err(Error::BadRange { .. })
        ));
        let mut q = query(c1, 0, 0);
        q.path = RepoPath::new("missing").unwrap();
        assert!(matches!(e.line_history(&q).await, Err(Error::NotFound(_))));
        let bin = commit_files(e, &[], &[("f.txt", b"a\0b\n")], native(), Vec::new()).await;
        assert!(matches!(
            e.line_history(&query(bin, 0, 0)).await,
            Err(Error::Binary)
        ));
        for bad in [vec![], vec![9; 10], vec![TOKEN_VERSION; 50_000]] {
            let mut q = query(c1, 0, 0);
            q.resume = Some(bad);
            assert!(matches!(e.line_history(&q).await, Err(Error::BadToken)));
        }
        // A token naming a changeset that does not exist fails through the store.
        let ghost = Cursor {
            cs: ChangesetId::from_bytes([7; 32]),
            path: RepoPath::new("f.txt").unwrap(),
            start: 0,
            end: 1,
        };
        let mut q = query(c1, 0, 0);
        q.resume = Some(ghost.encode());
        assert!(matches!(
            e.line_history(&q).await,
            Err(Error::Commit(commit::Error::MissingChangeset(_)))
        ));
    }

    #[test]
    fn tokens_round_trip() {
        let c = Cursor {
            cs: ChangesetId::from_bytes([3; 32]),
            path: RepoPath::new("a/b.txt").unwrap(),
            start: 4,
            end: 9,
        };
        assert_eq!(Cursor::decode(&c.encode()).unwrap(), c);
    }
}
