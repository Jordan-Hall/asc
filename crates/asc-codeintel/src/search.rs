//! Exact search executor (SPEC §23.3, I07): plan, postings and scope, verify, locate.
//!
//! 1. The pattern becomes a [`Plan`] over folded trigrams ([`crate::plan`]) and a byte
//!    `regex` (`size_limit` 10 MiB).
//! 2. The scope ([`ScopeEngine::resolve`]) gives the kept ords. The plan becomes a tantivy
//!    `BooleanQuery` of `TermQuery` on `tri`; its ords are intersected with the scope.
//! 3. Every candidate is verified: its bytes come from `Objects` (capped at the skip rules'
//!    1 MiB) and are matched line by line.
//! 4. A verified file is located (ord to paths) and the window is ranked.
//!
//! Completeness under lag (CI-D7): the part of the scope the index has not reached is
//! verified by streaming, up to [`STREAM_FILES`] files or [`STREAM_BYTES`]. Beyond that the
//! result has `coverage < 1` and the missing ords are handed to
//! [`DocDirectory::enqueue_interactive`].
//!
//! A pattern without a trigram (`.*`, under three bytes) scans the scope; it is allowed only
//! with a path filter or a scope of at most [`SCAN_SCOPE_FILES`] files, else
//! `INVALID_ARGUMENT: pattern too broad`.
//!
//! One call returns one ranked window of up to [`WINDOW`] verified files, in `ord` order
//! before ranking; `next` resumes after the last examined ord. All input is untrusted: the
//! pattern, the regex, the bytes and the page work (files, bytes, lines, spans) are bounded.

use std::collections::HashMap;
use std::sync::Arc;

use asc_core::id::{ChangesetId, FileId, TreeId};
use asc_core::path::RepoPath;
use asc_engine::Engine;
use asc_store::key::ObjKey;
use asc_store::objects::{self, Objects};
use asc_store::refs::Refs;
use regex::bytes::{Regex, RegexBuilder};
use roaring::RoaringBitmap;
use tantivy::query::{BooleanQuery, EmptyQuery, Query, TermQuery};
use tantivy::schema::{IndexRecordOption, Term};

use crate::config::CodeintelConfig;
use crate::index::schema::FilesSchema;
use crate::index::segments::Snapshot;
use crate::lang::{self, Lang};
use crate::plan::{self, MAX_PATTERN, Plan};
use crate::scope::{OrdSource, ScopeEngine, ScopeError, ScopeQuery};
use crate::skip;

/// Verified files in one ranked window.
pub const WINDOW: usize = 2_000;
/// Unindexed files verified by streaming per call.
pub const STREAM_FILES: usize = 2_000;
/// Unindexed bytes verified by streaming per call.
pub const STREAM_BYTES: u64 = 64 << 20;
/// A trigram-free pattern needs a path filter unless the scope is at most this many files.
pub const SCAN_SCOPE_FILES: u64 = 2_000;
/// `size_limit` of the compiled regex.
pub const REGEX_SIZE_LIMIT: usize = 10 << 20;
/// Bytes read and matched by one call (indexed and streamed files together).
pub const VERIFY_BYTES: u64 = 512 << 20;
/// Longest line text kept in a hit.
pub const MAX_LINE_BYTES: usize = 2_000;
/// Matching lines kept per file.
pub const MAX_LINES_PER_FILE: usize = 200;
/// Match spans kept per line.
pub const MAX_SPANS_PER_LINE: usize = 32;
/// Paths kept per file.
pub const MAX_PATHS: usize = 16;
/// Files fetched at once.
const FETCH_BATCH: usize = 16;
/// Deepest plan the query builder follows.
const MAX_PLAN_DEPTH: usize = 64;

/// How `pattern` is read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Literal,
    Regex,
    /// A literal bounded by `\b` on both sides.
    Word,
}

/// A failed search. `Display` of `InvalidArgument` is the wire text.
#[derive(Debug, thiserror::Error)]
pub enum SearchError {
    #[error("INVALID_ARGUMENT: {0}")]
    InvalidArgument(String),
    #[error("scope: {0}")]
    Scope(#[from] ScopeError),
    #[error("index: {0}")]
    Index(String),
    #[error("store: {0}")]
    Store(String),
}

pub type Result<T, E = SearchError> = std::result::Result<T, E>;

fn bad(m: impl Into<String>) -> SearchError {
    SearchError::InvalidArgument(m.into())
}

/// Resume point of a paged search: the window number and the last examined ord. The MAC
/// and the binding to the manifest generation and scope digest belong to the RPC layer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cursor {
    pub window: u32,
    pub ord: u32,
}

impl Cursor {
    pub fn to_bytes(self) -> [u8; 8] {
        let mut b = [0u8; 8];
        b[..4].copy_from_slice(&self.window.to_le_bytes());
        b[4..].copy_from_slice(&self.ord.to_le_bytes());
        b
    }

    pub fn from_bytes(b: &[u8]) -> Option<Cursor> {
        let b: [u8; 8] = b.try_into().ok()?;
        Some(Cursor {
            window: u32::from_le_bytes(b[..4].try_into().ok()?),
            ord: u32::from_le_bytes(b[4..].try_into().ok()?),
        })
    }
}

/// The namespace's doc table as the executor needs it.
pub trait DocDirectory: Send + Sync {
    /// The content of `ord`; `None` when the ord is unknown.
    fn file_of(&self, ord: u32) -> Option<FileId>;
    /// Queues the files for indexing at interactive priority (SPEC §23.2.2).
    fn enqueue_interactive(&self, ords: &RoaringBitmap);
}

/// A search request. `paths` and `exclude_paths` are literal path prefixes.
#[derive(Clone, Debug)]
pub struct SearchRequest<'a> {
    pub rev: ChangesetId,
    pub pattern: String,
    pub mode: Mode,
    pub case_sensitive: bool,
    pub paths: Vec<RepoPath>,
    pub exclude_paths: Vec<RepoPath>,
    pub langs: Vec<Lang>,
    /// The `xdocs` rules to subtract; `None` is `include_excluded`.
    pub rules: Option<&'a CodeintelConfig>,
    pub cursor: Option<Cursor>,
}

impl<'a> SearchRequest<'a> {
    pub fn new(rev: ChangesetId, pattern: impl Into<String>, mode: Mode) -> Self {
        SearchRequest {
            rev,
            pattern: pattern.into(),
            mode,
            case_sensitive: true,
            paths: Vec::new(),
            exclude_paths: Vec::new(),
            langs: Vec::new(),
            rules: None,
            cursor: None,
        }
    }
}

/// One matching line. `line` is 1-based; `spans` are byte ranges inside `text`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LineMatch {
    pub line: u32,
    pub text: Vec<u8>,
    pub spans: Vec<(u32, u32)>,
}

/// One verified file. `paths` is sorted and holds at most [`MAX_PATHS`] of the paths that
/// pass the filters; a `FileId` at several paths is one hit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileHit {
    pub ord: u32,
    pub file: FileId,
    pub paths: Vec<RepoPath>,
    pub lines: Vec<LineMatch>,
    /// More matching lines than [`MAX_LINES_PER_FILE`].
    pub more_lines: bool,
}

/// The answer to one call.
#[derive(Debug)]
pub struct SearchResult {
    /// The resolved scope, echoed so a caller can cite it.
    pub rev: ChangesetId,
    pub root: TreeId,
    /// Manifest generation of the index snapshot that answered.
    pub generation: u64,
    /// Ranked window.
    pub hits: Vec<FileHit>,
    pub next: Option<Cursor>,
    /// Share of the scope that was searched; below 1 when streaming hit its bound.
    pub coverage: f64,
    /// Unindexed files left unsearched (already enqueued).
    pub missing: u64,
}

/// The executor of one namespace.
pub struct Searcher<O: Objects, R: Refs, S: OrdSource, D: DocDirectory> {
    engine: Arc<Engine<O, R>>,
    objects: Arc<O>,
    scope: ScopeEngine<O, R, S>,
    docs: Arc<D>,
}

/// A compiled pattern.
struct Compiled {
    re: Regex,
    plan: Plan,
}

fn compile(req: &SearchRequest<'_>) -> Result<Compiled> {
    if req.pattern.is_empty() {
        return Err(bad("empty pattern"));
    }
    if req.pattern.len() > MAX_PATTERN {
        return Err(bad("pattern too long"));
    }
    let pat = match req.mode {
        Mode::Literal => regex::escape(&req.pattern),
        Mode::Word => format!(r"\b(?:{})\b", regex::escape(&req.pattern)),
        Mode::Regex => req.pattern.clone(),
    };
    let re = RegexBuilder::new(&pat)
        .case_insensitive(!req.case_sensitive)
        .size_limit(REGEX_SIZE_LIMIT)
        .build()
        .map_err(|e| bad(format!("invalid regex: {e}")))?;
    let plan = match req.mode {
        Mode::Literal => plan::plan_literal(&req.pattern, !req.case_sensitive),
        _ => plan::plan_regex(&pat, !req.case_sensitive),
    }
    .map_err(|e| bad(e.to_string()))?;
    Ok(Compiled { re, plan })
}

fn term_query(f: &FilesSchema, g: u32) -> Box<dyn Query> {
    Box::new(TermQuery::new(
        Term::from_field_u64(f.tri, u64::from(g)),
        IndexRecordOption::Basic,
    ))
}

/// The tantivy query of a plan. `None` means no restriction (a superset is always safe for
/// the verify step).
fn plan_query(p: &Plan, f: &FilesSchema, depth: usize) -> Option<Box<dyn Query>> {
    if depth > MAX_PLAN_DEPTH {
        return None;
    }
    match p {
        Plan::All => None,
        Plan::None => Some(Box::new(EmptyQuery)),
        Plan::Gram(g) => Some(term_query(f, *g)),
        Plan::And(v) => {
            let parts: Vec<_> = v
                .iter()
                .filter_map(|c| plan_query(c, f, depth + 1))
                .collect();
            match parts.len() {
                0 => None,
                1 => parts.into_iter().next(),
                _ => Some(Box::new(BooleanQuery::intersection(parts))),
            }
        }
        Plan::Or(v) => {
            let mut parts = Vec::with_capacity(v.len());
            for c in v {
                parts.push(plan_query(c, f, depth + 1)?);
            }
            match parts.len() {
                0 => Some(Box::new(EmptyQuery)),
                1 => parts.into_iter().next(),
                _ => Some(Box::new(BooleanQuery::union(parts))),
            }
        }
    }
}

fn lang_query(f: &FilesSchema, langs: &[Lang]) -> Box<dyn Query> {
    let parts: Vec<Box<dyn Query>> = langs
        .iter()
        .map(|l| {
            Box::new(TermQuery::new(
                Term::from_field_text(f.lang, l.name()),
                IndexRecordOption::Basic,
            )) as Box<dyn Query>
        })
        .collect();
    Box::new(BooleanQuery::union(parts))
}

async fn index_ords(snap: &Arc<Snapshot>, q: Box<dyn Query>) -> Result<RoaringBitmap> {
    let snap = snap.clone();
    tokio::task::spawn_blocking(move || snap.ords_for_query(&*q))
        .await
        .map_err(|e| SearchError::Index(e.to_string()))?
        .map_err(|e| SearchError::Index(e.to_string()))
}

/// Matching lines of `bytes`, matched one line at a time like `git grep`.
fn scan_lines(re: &Regex, bytes: &[u8]) -> (Vec<LineMatch>, bool) {
    let mut out = Vec::new();
    if bytes.is_empty() {
        return (out, false);
    }
    let body = bytes.strip_suffix(b"\n").unwrap_or(bytes);
    for (i, line) in body.split(|&b| b == b'\n').enumerate() {
        let mut spans = Vec::new();
        let mut hit = false;
        for m in re.find_iter(line) {
            hit = true;
            if spans.len() < MAX_SPANS_PER_LINE && m.start() < MAX_LINE_BYTES {
                spans.push((m.start() as u32, m.end().min(MAX_LINE_BYTES) as u32));
            }
            if spans.len() >= MAX_SPANS_PER_LINE {
                break;
            }
        }
        if !hit {
            continue;
        }
        if out.len() >= MAX_LINES_PER_FILE {
            return (out, true);
        }
        out.push(LineMatch {
            line: u32::try_from(i + 1).unwrap_or(u32::MAX),
            text: line[..line.len().min(MAX_LINE_BYTES)].to_vec(),
            spans,
        });
    }
    (out, false)
}

fn under(p: &RepoPath, d: &RepoPath) -> bool {
    p == d || p.starts_with_dir(d)
}

fn path_ok(p: &RepoPath, req: &SearchRequest<'_>) -> bool {
    (req.paths.is_empty() || req.paths.iter().any(|d| under(p, d)))
        && !req.exclude_paths.iter().any(|d| under(p, d))
}

/// Ranking score of a hit: higher is better (SPEC §23.3).
fn score(paths: &[RepoPath], sym_def: bool, needle: Option<&str>) -> i32 {
    let Some(first) = paths.first() else {
        return i32::MIN;
    };
    let text = String::from_utf8_lossy(first.as_bytes()).to_ascii_lowercase();
    let base = text.rsplit('/').next().unwrap_or(&text);
    let depth = text.matches('/').count() as i32;
    let mut s = -2 * depth;
    if sym_def {
        s += 100;
    }
    if needle.is_some_and(|n| !n.is_empty() && base.contains(n)) {
        s += 50;
    }
    const DIRS: [&str; 8] = [
        "test",
        "tests",
        "__tests__",
        "spec",
        "testdata",
        "vendor",
        "generated",
        "node_modules",
    ];
    let low_value = text.split('/').any(|c| DIRS.contains(&c))
        || ["_test.", ".test.", ".spec.", ".min.", ".generated.", ".g."]
            .iter()
            .any(|m| base.contains(m));
    if low_value {
        s -= 40;
    }
    s
}

/// Where a candidate came from.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Src {
    Indexed,
    Streamed,
}

impl<O: Objects + 'static, R: Refs + 'static, S: OrdSource + 'static, D: DocDirectory + 'static>
    Searcher<O, R, S, D>
{
    /// `objects` must be the store `engine` was built on.
    pub fn new(engine: Arc<Engine<O, R>>, objects: Arc<O>, ords: Arc<S>, docs: Arc<D>) -> Self {
        let scope = ScopeEngine::new(engine.clone(), objects.clone(), ords);
        Searcher {
            engine,
            objects,
            scope,
            docs,
        }
    }

    /// The scope algebra this executor uses.
    pub fn scope_engine(&self) -> &ScopeEngine<O, R, S> {
        &self.scope
    }

    /// Runs one search against the pinned index `snap`.
    pub async fn search(
        &self,
        snap: &Arc<Snapshot>,
        req: &SearchRequest<'_>,
    ) -> Result<SearchResult> {
        let compiled = compile(req)?;
        let cs = self
            .engine
            .get_changeset(req.rev)
            .await
            .map_err(|e| ScopeError::Store(e.to_string()))?;
        let root = cs.root;
        let mut q = ScopeQuery::new(req.rev);
        q.include = req.paths.clone();
        q.exclude = req.exclude_paths.clone();
        q.rules = req.rules;
        let kept = self.scope.resolve(&q).await?;
        let mut result = SearchResult {
            rev: req.rev,
            root,
            generation: snap.generation,
            hits: Vec::new(),
            next: None,
            coverage: 1.0,
            missing: 0,
        };
        if matches!(compiled.plan, Plan::None) {
            return Ok(result);
        }
        let scan = plan::Plan::is_all(&compiled.plan);
        if scan && req.paths.is_empty() && kept.len() > SCAN_SCOPE_FILES {
            return Err(bad("pattern too broad"));
        }

        let after = req.cursor.map(|c| c.ord);
        let window = req.cursor.map_or(0, |c| c.window);
        let mut kept = kept;
        if let Some(a) = after {
            kept.remove_range(..=a);
        }
        let kept_total = kept.len();
        let files = FilesSchema::build();

        // Indexed candidates: postings, then the scope.
        let mut indexed = &kept & &snap.indexed;
        let restrict: Option<Box<dyn Query>> = {
            let plan_q = plan_query(&compiled.plan, &files, 0);
            let lang_q = (!req.langs.is_empty()).then(|| lang_query(&files, &req.langs));
            match (plan_q, lang_q) {
                (Some(a), Some(b)) => Some(Box::new(BooleanQuery::intersection(vec![a, b]))),
                (a, b) => a.or(b),
            }
        };
        if let Some(rq) = restrict {
            indexed &= index_ords(snap, rq).await?;
        }
        // The unindexed part of the scope is streamed, up to the bound.
        let unindexed = &kept - &snap.indexed;
        let mut missing = RoaringBitmap::new();
        let mut cands: Vec<(u32, Src)> = indexed.iter().map(|o| (o, Src::Indexed)).collect();
        for (n, o) in unindexed.iter().enumerate() {
            if n < STREAM_FILES {
                cands.push((o, Src::Streamed));
            } else {
                missing.insert(o);
            }
        }
        cands.sort_unstable_by_key(|c| c.0);

        // Names of the definition hits for the rank (identifier-like literals only).
        let sym_defs = if req.mode != Mode::Regex
            && !req.pattern.contains(char::is_whitespace)
            && req.pattern.len() <= crate::index::segments::MAX_TERM_LEN
        {
            let t = TermQuery::new(
                Term::from_field_text(files.sym_def, &req.pattern),
                IndexRecordOption::Basic,
            );
            index_ords(snap, Box::new(t)).await?
        } else {
            RoaringBitmap::new()
        };
        let needle = (req.mode != Mode::Regex).then(|| req.pattern.to_ascii_lowercase());

        let mut stream_bytes = 0u64;
        let mut verify_bytes = 0u64;
        let mut hits: Vec<(i32, FileHit)> = Vec::new();
        let mut last: Option<u32> = None;
        let mut stopped = false;
        let mut i = 0;
        'outer: while i < cands.len() {
            if hits.len() >= WINDOW || verify_bytes >= VERIFY_BYTES {
                stopped = true;
                break;
            }
            let end = (i + FETCH_BATCH).min(cands.len());
            let mut batch: Vec<(u32, Src, FileId)> = Vec::new();
            for &(o, src) in &cands[i..end] {
                if src == Src::Streamed && stream_bytes >= STREAM_BYTES {
                    missing.insert(o);
                } else if let Some(f) = self.docs.file_of(o) {
                    batch.push((o, src, f));
                }
            }
            let fetched = self.fetch(&batch).await?;
            let mut by_ord: HashMap<u32, Option<bytes::Bytes>> = batch
                .iter()
                .map(|b| b.0)
                .zip(fetched)
                .collect::<HashMap<_, _>>();
            for &(o, src) in &cands[i..end] {
                if hits.len() >= WINDOW {
                    stopped = true;
                    break 'outer;
                }
                i += 1;
                last = Some(o);
                let Some(Some(bytes)) = by_ord.remove(&o) else {
                    continue;
                };
                let n = bytes.len() as u64;
                verify_bytes += n;
                if src == Src::Streamed {
                    stream_bytes += n;
                }
                if !skip::classify(&bytes).indexable() {
                    continue;
                }
                let (lines, more_lines) = scan_lines(&compiled.re, &bytes);
                if lines.is_empty() {
                    continue;
                }
                let Some(file) = self.docs.file_of(o) else {
                    continue;
                };
                let located = self
                    .scope
                    .locate(root, o, 4 * MAX_PATHS)
                    .await
                    .map_err(SearchError::Scope)?;
                let mut paths: Vec<RepoPath> =
                    located.into_iter().filter(|p| path_ok(p, req)).collect();
                if paths.is_empty() {
                    continue;
                }
                if src == Src::Streamed && !req.langs.is_empty() {
                    let ok = paths.iter().any(|p| {
                        lang::detect(&String::from_utf8_lossy(p.as_bytes()), &bytes)
                            .is_some_and(|l| req.langs.contains(&l))
                    });
                    if !ok {
                        continue;
                    }
                }
                paths.truncate(MAX_PATHS);
                let s = score(&paths, sym_defs.contains(o), needle.as_deref());
                hits.push((
                    s,
                    FileHit {
                        ord: o,
                        file,
                        paths,
                        lines,
                        more_lines,
                    },
                ));
            }
        }
        hits.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.ord.cmp(&b.1.ord)));
        result.hits = hits.into_iter().map(|h| h.1).collect();
        if (stopped || i < cands.len())
            && let Some(o) = last
        {
            result.next = Some(Cursor {
                window: window.saturating_add(1),
                ord: o,
            });
        }
        result.missing = missing.len();
        if !missing.is_empty() {
            self.docs.enqueue_interactive(&missing);
            result.coverage = 1.0 - (missing.len() as f64 / kept_total.max(1) as f64);
        }
        Ok(result)
    }

    /// The first bytes of each file (at most one byte over the skip bound), concurrently.
    /// Unreadable or corrupt files give `None`; any other store failure fails the search.
    async fn fetch(&self, items: &[(u32, Src, FileId)]) -> Result<Vec<Option<bytes::Bytes>>> {
        let mut set = tokio::task::JoinSet::new();
        for (idx, (_, _, f)) in items.iter().enumerate() {
            let objs = self.objects.clone();
            let key = ObjKey::File(*f);
            set.spawn(async move {
                let r = objs.get_range(&key, 0..(skip::MAX_BYTES as u64 + 1)).await;
                (idx, r)
            });
        }
        let mut out: Vec<Option<bytes::Bytes>> = vec![None; items.len()];
        while let Some(j) = set.join_next().await {
            let (idx, r) = j.map_err(|e| SearchError::Store(e.to_string()))?;
            out[idx] = match r {
                Ok(b) => b,
                Err(objects::Error::Corrupt(_)) | Err(objects::Error::Codec(..)) => None,
                Err(e) => return Err(SearchError::Store(e.to_string())),
            };
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests;
