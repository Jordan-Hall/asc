//! `pathsearch.v1`: every path in a changeset's tree, for "go to file" queries (SPEC §5, named
//! future type `pathsearch`).
//!
//! Finding a file by a few letters of its name used to mean walking the whole tree of the
//! revision. A `pathsearch` value lists the tree's non-directory paths in order, split into
//! chunks whose boundaries depend only on the paths themselves (a path ends a chunk when its hash
//! says so, or when the chunk is full). Chunks are stored once by content, so consecutive
//! changesets share every chunk their commit did not touch. A changeset whose first parent has a
//! value is derived from it and the tree changes between the two (only changed subtrees are
//! read); otherwise the tree is listed once.
//!
//! [`Engine::find_paths`] ranks the paths against a query by fuzzy subsequence match: every query
//! character in order, with bonuses for matches at word starts, in the file name and in runs, and
//! a penalty for gaps and long paths. A query with an upper-case letter matches case; otherwise
//! case is ignored.

use std::collections::BTreeMap;

use asc_core::id::ChangesetId;
use asc_core::object::Kind;
use asc_core::path::RepoPath;
use asc_store::key::ObjKey;
use asc_store::objects::Objects;
use asc_store::refs::Refs;
use async_trait::async_trait;

use super::keyed::{DerivedKeyed, memo_key};
use super::{Error, Result, read_memo, write_memo};
use crate::Engine;
use crate::diff::Status;

/// Most paths in one chunk.
pub const CHUNK_MAX: usize = 4096;
/// Name of the shared chunk objects (`d/pathchunk.v1/…`).
pub const CHUNK_NAME: &str = "pathchunk";
/// Longest query [`Engine::find_paths`] takes, in bytes.
pub const MAX_QUERY: usize = 256;

/// The chunk ids of a tree's sorted path list, and how many paths it holds.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PathSearch {
    pub count: u64,
    pub chunks: Vec<[u8; 32]>,
}

/// One path of the list.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PathEntry {
    pub path: RepoPath,
    pub kind: Kind,
}

/// One ranked match.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PathHit {
    pub path: RepoPath,
    pub kind: Kind,
    pub score: i64,
}

fn kind_code(k: Kind) -> u8 {
    k as u8
}

fn kind_of(c: u8) -> Option<Kind> {
    Some(match c {
        0 => Kind::File,
        1 => Kind::Exec,
        2 => Kind::Symlink,
        4 => Kind::Gitlink,
        _ => return None,
    })
}

fn chunk_key(id: [u8; 32]) -> ObjKey {
    ObjKey::Derived {
        name: CHUNK_NAME,
        version: 1,
        id,
    }
}

/// Whether `path` ends a chunk: about one path in 512, chosen by the path alone.
fn is_boundary(path: &RepoPath) -> bool {
    let h = blake3::hash(path.as_bytes());
    let b = h.as_bytes();
    b[0] == 0 && b[1] & 1 == 0
}

fn encode_chunk(entries: &[PathEntry]) -> Vec<u8> {
    let mut out = Vec::with_capacity(4 + entries.len() * 48);
    out.extend((entries.len() as u32).to_le_bytes());
    for e in entries {
        out.push(kind_code(e.kind));
        out.extend((e.path.as_bytes().len() as u32).to_le_bytes());
        out.extend(e.path.as_bytes());
    }
    out
}

fn decode_chunk(b: &[u8]) -> Result<Vec<PathEntry>> {
    let bad = || Error::BadValue { name: CHUNK_NAME };
    let take = |r: &mut &[u8], n: usize| -> Result<Vec<u8>> {
        if r.len() < n {
            return Err(bad());
        }
        let (h, t) = r.split_at(n);
        *r = t;
        Ok(h.to_vec())
    };
    let u32_at = |r: &mut &[u8]| -> Result<usize> {
        let (h, t) = r.split_first_chunk::<4>().ok_or_else(bad)?;
        *r = t;
        Ok(u32::from_le_bytes(*h) as usize)
    };
    let mut rest = b;
    let n = u32_at(&mut rest)?;
    if n == 0 || n > CHUNK_MAX {
        return Err(bad());
    }
    let mut out: Vec<PathEntry> = Vec::with_capacity(n);
    for _ in 0..n {
        let (&k, t) = rest.split_first().ok_or_else(bad)?;
        rest = t;
        let kind = kind_of(k).ok_or_else(bad)?;
        let len = u32_at(&mut rest)?;
        let path = RepoPath::new(take(&mut rest, len)?).map_err(|_| bad())?;
        if out.last().is_some_and(|p| p.path >= path) {
            return Err(bad());
        }
        out.push(PathEntry { path, kind });
    }
    if !rest.is_empty() {
        return Err(bad());
    }
    Ok(out)
}

impl<O: Objects, R: Refs> Engine<O, R> {
    /// Every non-directory path of the value's tree, in order. `None` when a chunk is missing or
    /// damaged (the value is then derived again from the tree).
    async fn load_paths(&self, v: &PathSearch) -> Result<Option<Vec<PathEntry>>> {
        let mut out: Vec<PathEntry> = Vec::with_capacity(v.count.min(1 << 24) as usize);
        for &id in &v.chunks {
            let Some(b) = read_memo(&*self.objects, &chunk_key(id)).await? else {
                return Ok(None);
            };
            if *blake3::hash(&b).as_bytes() != id {
                return Ok(None);
            }
            let Ok(part) = decode_chunk(&b) else {
                return Ok(None);
            };
            if let (Some(a), Some(b)) = (out.last(), part.first())
                && a.path >= b.path
            {
                return Ok(None);
            }
            out.extend(part);
        }
        Ok((out.len() as u64 == v.count).then_some(out))
    }

    /// Splits `paths` into chunks, stores those `known` does not list, and returns the value.
    async fn store_paths(&self, paths: &[PathEntry], known: &[[u8; 32]]) -> Result<PathSearch> {
        let mut chunks = Vec::new();
        let mut start = 0;
        for (i, p) in paths.iter().enumerate() {
            let end = i + 1;
            if end - start == CHUNK_MAX || is_boundary(&p.path) || end == paths.len() {
                let bytes = encode_chunk(&paths[start..end]);
                let id = *blake3::hash(&bytes).as_bytes();
                if !known.contains(&id) {
                    write_memo(&*self.objects, &chunk_key(id), &bytes).await?;
                }
                chunks.push(id);
                start = end;
            }
        }
        Ok(PathSearch {
            count: paths.len() as u64,
            chunks,
        })
    }

    /// Every non-directory path of `cs`'s tree, in order, from the `pathsearch` memo.
    pub async fn list_paths(&self, cs: ChangesetId) -> Result<Vec<PathEntry>> {
        let v = self.get_keyed::<PathSearch>(cs, &()).await?;
        if let Some(p) = self.load_paths(&v).await? {
            return Ok(p);
        }
        // A chunk went missing after the value was stored: derive and store it again.
        let v = PathSearch::derive(self, cs, &()).await?;
        write_memo(
            &*self.objects,
            &memo_key::<PathSearch>(cs, &()),
            &v.encode(),
        )
        .await?;
        self.load_paths(&v)
            .await?
            .ok_or(Error::BadValue { name: CHUNK_NAME })
    }

    /// The paths of `cs`'s tree that match `query`, best first, at most `limit`. An empty query
    /// lists the first `limit` paths in order.
    pub async fn find_paths(
        &self,
        cs: ChangesetId,
        query: &str,
        limit: usize,
    ) -> Result<Vec<PathHit>> {
        let paths = self.list_paths(cs).await?;
        Ok(rank(paths, query, limit))
    }
}

/// `paths` that match `query`, best first, at most `limit`.
pub fn rank(paths: Vec<PathEntry>, query: &str, limit: usize) -> Vec<PathHit> {
    let q = query.as_bytes();
    let q = &q[..q.len().min(MAX_QUERY)];
    if q.is_empty() {
        return paths
            .into_iter()
            .take(limit)
            .map(|p| PathHit {
                path: p.path,
                kind: p.kind,
                score: 0,
            })
            .collect();
    }
    let fold = !q.iter().any(u8::is_ascii_uppercase);
    let mut hits: Vec<PathHit> = paths
        .into_iter()
        .filter_map(|p| {
            score(q, p.path.as_bytes(), fold).map(|score| PathHit {
                path: p.path,
                kind: p.kind,
                score,
            })
        })
        .collect();
    hits.sort_by(|a, b| {
        b.score
            .cmp(&a.score)
            .then(a.path.as_bytes().len().cmp(&b.path.as_bytes().len()))
            .then(a.path.cmp(&b.path))
    });
    hits.truncate(limit);
    hits
}

const MATCH: i64 = 16;
const BOUNDARY: i64 = 10;
const BASENAME: i64 = 6;
const CONSECUTIVE: i64 = 8;
const GAP_START: i64 = 3;
const GAP: i64 = 1;
const NEG: i64 = i64::MIN / 4;

fn same(a: u8, b: u8, fold: bool) -> bool {
    if fold {
        a.eq_ignore_ascii_case(&b)
    } else {
        a == b
    }
}

/// The best score of `q` as a subsequence of `path`; `None` when it is not one.
pub fn score(q: &[u8], path: &[u8], fold: bool) -> Option<i64> {
    // Cheap subsequence test first; most paths fail it.
    let mut it = path.iter();
    if !q.iter().all(|&c| it.any(|&p| same(c, p, fold))) {
        return None;
    }
    let n = path.len();
    let base = path.iter().rposition(|&c| c == b'/').map_or(0, |i| i + 1);
    let bonus = |j: usize| -> i64 {
        let prev = j.checked_sub(1).map(|k| path[k]);
        let word = match prev {
            None => true,
            Some(p) => {
                matches!(p, b'/' | b'_' | b'-' | b'.' | b' ')
                    || (p.is_ascii_lowercase() && path[j].is_ascii_uppercase())
            }
        };
        MATCH + if word { BOUNDARY } else { 0 } + if j >= base { BASENAME } else { 0 }
    };
    let mut prev_row = vec![NEG; n];
    for (j, &c) in path.iter().enumerate() {
        if same(q[0], c, fold) {
            prev_row[j] = bonus(j);
        }
    }
    let mut row = vec![NEG; n];
    for &qc in &q[1..] {
        // run: the best previous-row score that leaves a gap before j, less the gap's cost.
        let mut run = NEG;
        for j in 0..n {
            if j >= 2 {
                run = (run - GAP).max(prev_row[j - 2] - GAP_START);
            }
            row[j] = if same(qc, path[j], fold) {
                let consec = if j >= 1 && prev_row[j - 1] > NEG {
                    prev_row[j - 1] + CONSECUTIVE
                } else {
                    NEG
                };
                let best = consec.max(run);
                if best > NEG { best + bonus(j) } else { NEG }
            } else {
                NEG
            };
        }
        std::mem::swap(&mut prev_row, &mut row);
    }
    let best = prev_row.into_iter().max().filter(|&s| s > NEG)?;
    Some(best - (n as i64) / 8)
}

#[async_trait]
impl DerivedKeyed for PathSearch {
    const NAME: &'static str = "pathsearch";
    const VERSION: u32 = 1;
    type Key = ();

    fn encode_key(_: &()) -> Vec<u8> {
        Vec::new()
    }

    async fn derive<O: Objects, R: Refs>(
        e: &Engine<O, R>,
        cs: ChangesetId,
        _: &(),
    ) -> Result<Self> {
        let c = e.get_changeset(cs).await?;
        if let Some(&p) = c.parents.first()
            && let Some(b) = read_memo(&*e.objects, &memo_key::<Self>(p, &())).await?
            && let Ok(older) = Self::decode(&b)
            && let Some(paths) = e.load_paths(&older).await?
        {
            let base = e.get_changeset(p).await?.root;
            let mut edits: BTreeMap<RepoPath, Option<Kind>> = BTreeMap::new();
            for ch in e.tree_changes(base, c.root, None).await? {
                let kind = match ch.status {
                    Status::Deleted => None,
                    _ => ch.new.as_ref().map(|n| n.kind),
                };
                edits.insert(ch.path, kind);
            }
            let merged = merge(paths, edits);
            return e.store_paths(&merged, &older.chunks).await;
        }
        let empty = Engine::<O, R>::empty_tree_id();
        let paths: Vec<PathEntry> = e
            .tree_changes(empty, c.root, None)
            .await?
            .into_iter()
            .filter_map(|ch| {
                let kind = ch.new.as_ref()?.kind;
                (kind != Kind::Dir).then_some(PathEntry {
                    path: ch.path,
                    kind,
                })
            })
            .collect();
        e.store_paths(&paths, &[]).await
    }

    fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(12 + 32 * self.chunks.len());
        out.extend(self.count.to_le_bytes());
        out.extend((self.chunks.len() as u32).to_le_bytes());
        for c in &self.chunks {
            out.extend(c);
        }
        out
    }

    fn decode(b: &[u8]) -> Result<Self> {
        let bad = || Error::BadValue { name: "pathsearch" };
        let (count, rest) = b.split_first_chunk::<8>().ok_or_else(bad)?;
        let (n, mut rest) = rest.split_first_chunk::<4>().ok_or_else(bad)?;
        let n = u32::from_le_bytes(*n) as usize;
        if rest.len() != n * 32 {
            return Err(bad());
        }
        let mut chunks = Vec::with_capacity(n);
        while let Some((c, t)) = rest.split_first_chunk::<32>() {
            chunks.push(*c);
            rest = t;
        }
        Ok(Self {
            count: u64::from_le_bytes(*count),
            chunks,
        })
    }
}

/// `paths` with `edits` applied: `None` removes a path, `Some(kind)` adds or updates it.
fn merge(paths: Vec<PathEntry>, edits: BTreeMap<RepoPath, Option<Kind>>) -> Vec<PathEntry> {
    let mut out = Vec::with_capacity(paths.len() + edits.len());
    let mut edits = edits.into_iter().peekable();
    for p in paths {
        while let Some((path, _)) = edits.peek()
            && *path < p.path
        {
            let (path, kind) = edits.next().expect("peeked");
            if let Some(kind) = kind.filter(|k| *k != Kind::Dir) {
                out.push(PathEntry { path, kind });
            }
        }
        if edits.peek().is_some_and(|(path, _)| *path == p.path) {
            let (path, kind) = edits.next().expect("peeked");
            if let Some(kind) = kind.filter(|k| *k != Kind::Dir) {
                out.push(PathEntry { path, kind });
            }
            continue;
        }
        out.push(p);
    }
    for (path, kind) in edits {
        if let Some(kind) = kind.filter(|k| *k != Kind::Dir) {
            out.push(PathEntry { path, kind });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::derive::testutil::*;

    fn paths_of(v: &[PathEntry]) -> Vec<String> {
        v.iter()
            .map(|p| String::from_utf8(p.path.as_bytes().to_vec()).unwrap())
            .collect()
    }

    async fn full_list(e: &TestEngine, cs: ChangesetId) -> Vec<String> {
        let root = e.get_changeset(cs).await.unwrap().root;
        e.tree_changes(TestEngine::empty_tree_id(), root, None)
            .await
            .unwrap()
            .into_iter()
            .map(|c| String::from_utf8(c.path.as_bytes().to_vec()).unwrap())
            .collect()
    }

    #[tokio::test]
    async fn the_list_follows_the_tree_through_adds_edits_and_deletes() {
        let e = engine();
        let names: Vec<String> = (0..3000).map(|i| format!("src/m{i:04}/lib.rs")).collect();
        let ops: Vec<(&str, Option<&[u8]>)> = names
            .iter()
            .map(|n| (n.as_str(), Some(b"x".as_slice())))
            .collect();
        let a = commit_ops(&e, &[], &ops).await;
        let la = e.list_paths(a).await.unwrap();
        assert_eq!(paths_of(&la), full_list(&e, a).await);
        let va = e.get_keyed::<PathSearch>(a, &()).await.unwrap();
        assert!(va.chunks.len() > 1, "3000 paths make several chunks");

        let b = commit_ops(
            &e,
            &[a],
            &[
                ("src/m0007/lib.rs", None),
                ("src/m0100/lib.rs", Some(b"y")),
                ("src/new.rs", Some(b"n")),
                ("zz", Some(b"z")),
            ],
        )
        .await;
        let lb = e.list_paths(b).await.unwrap();
        assert_eq!(paths_of(&lb), full_list(&e, b).await);
        let vb = e.get_keyed::<PathSearch>(b, &()).await.unwrap();
        let shared = vb.chunks.iter().filter(|c| va.chunks.contains(c)).count();
        assert!(
            shared + 3 >= vb.chunks.len(),
            "untouched chunks are shared: {shared} of {}",
            vb.chunks.len()
        );
    }

    #[tokio::test]
    async fn a_missing_chunk_is_rebuilt_from_the_tree() {
        let e = engine();
        let a = commit_ops(&e, &[], &[("a/b.rs", Some(b"1")), ("c.rs", Some(b"2"))]).await;
        let v = e.get_keyed::<PathSearch>(a, &()).await.unwrap();
        write_memo(&*e.objects, &chunk_key(v.chunks[0]), b"junk")
            .await
            .unwrap();
        assert_eq!(
            paths_of(&e.list_paths(a).await.unwrap()),
            vec!["a/b.rs", "c.rs"]
        );
    }

    #[tokio::test]
    async fn queries_rank_file_names_and_word_starts_first() {
        let e = engine();
        let a = commit_ops(
            &e,
            &[],
            &[
                ("src/main.rs", Some(b"1")),
                ("src/mod/admin.rs", Some(b"1")),
                ("docs/MAINTAINERS", Some(b"1")),
                ("tools/xmainx.txt", Some(b"1")),
                ("README.md", Some(b"1")),
            ],
        )
        .await;
        let hits = e.find_paths(a, "main", 10).await.unwrap();
        let got: Vec<String> = hits
            .iter()
            .map(|h| String::from_utf8(h.path.as_bytes().to_vec()).unwrap())
            .collect();
        assert_eq!(got[0], "src/main.rs");
        assert!(got.contains(&"docs/MAINTAINERS".to_owned()));
        assert!(!got.contains(&"README.md".to_owned()));
        // Upper case matches case.
        let exact = e.find_paths(a, "MAIN", 10).await.unwrap();
        assert_eq!(exact.len(), 1);
        // Subsequence across directories.
        let sub = e.find_paths(a, "smr", 10).await.unwrap();
        assert_eq!(sub[0].path.as_bytes(), b"src/main.rs");
        // An empty query lists in order; the limit holds.
        let all = e.find_paths(a, "", 2).await.unwrap();
        assert_eq!(paths_of_hits(&all), vec!["README.md", "docs/MAINTAINERS"]);
    }

    fn paths_of_hits(v: &[PathHit]) -> Vec<String> {
        v.iter()
            .map(|p| String::from_utf8(p.path.as_bytes().to_vec()).unwrap())
            .collect()
    }

    #[test]
    fn scores_prefer_runs_boundaries_and_short_paths() {
        assert!(score(b"abc", b"xaxbxc", true).is_some());
        assert!(score(b"abd", b"abc", true).is_none());
        let run = score(b"lib", b"src/lib.rs", true).unwrap();
        let spread = score(b"lib", b"src/l_i_b.rs", true).unwrap();
        assert!(run > spread);
        let short = score(b"lib", b"lib.rs", true).unwrap();
        let long = score(b"lib", b"a/very/long/path/to/lib.rs", true).unwrap();
        assert!(short > long);
        let camel = score(b"fb", b"FooBar.ts", true).unwrap();
        let inner = score(b"fb", b"xfxb.ts", true).unwrap();
        assert!(camel > inner);
    }

    #[test]
    fn chunks_and_values_round_trip_and_bad_bytes_are_refused() {
        let entries = vec![
            PathEntry {
                path: RepoPath::new("a").unwrap(),
                kind: Kind::File,
            },
            PathEntry {
                path: RepoPath::new("b/c").unwrap(),
                kind: Kind::Symlink,
            },
        ];
        let b = encode_chunk(&entries);
        assert_eq!(decode_chunk(&b).unwrap(), entries);
        let mut rev = entries.clone();
        rev.reverse();
        assert!(decode_chunk(&encode_chunk(&rev)).is_err(), "out of order");
        assert!(decode_chunk(&encode_chunk(&[])).is_err(), "empty");
        let mut long = b.clone();
        long.push(0);
        assert!(decode_chunk(&long).is_err());
        let v = PathSearch {
            count: 2,
            chunks: vec![[7; 32]],
        };
        assert_eq!(PathSearch::decode(&v.encode()).unwrap(), v);
        assert!(PathSearch::decode(&v.encode()[..20]).is_err());
    }

    #[test]
    fn merging_applies_removals_updates_and_insertions() {
        let p = |s: &str, k: Kind| PathEntry {
            path: RepoPath::new(s).unwrap(),
            kind: k,
        };
        let base = vec![p("a", Kind::File), p("c", Kind::File), p("e", Kind::File)];
        let mut edits = BTreeMap::new();
        edits.insert(RepoPath::new("a").unwrap(), None);
        edits.insert(RepoPath::new("b").unwrap(), Some(Kind::File));
        edits.insert(RepoPath::new("c").unwrap(), Some(Kind::Exec));
        edits.insert(RepoPath::new("f").unwrap(), Some(Kind::File));
        let got = merge(base, edits);
        assert_eq!(
            got,
            vec![
                p("b", Kind::File),
                p("c", Kind::Exec),
                p("e", Kind::File),
                p("f", Kind::File)
            ]
        );
    }
}
