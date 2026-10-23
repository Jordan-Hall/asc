//! Symbol queries (SPEC §23.3): `def`, `refs`, `callers`, `outline` and `find`.
//!
//! The queries are search-based, with no stack graphs. Candidate files come from the
//! `sym_def` and `sym_ref` terms of the `files` index, intersected with the scope; the tags
//! of each candidate are then loaded and filtered, so a stale or hostile index entry can
//! never produce a symbol that the file does not hold. Every query treats its input as
//! untrusted and bounds its work: name length, files loaded, bytes read for import
//! evidence, files and bytes scanned by the word-boundary fallback, and the result count.
//!
//! Content comes through [`FileSource`], whose implementor must verify the content hash
//! before it returns bytes. The project graph (G07) comes through [`ProjectRank`].

use std::cmp::Ordering;
use std::sync::Arc;

use roaring::RoaringBitmap;
use tantivy::query::{FuzzyTermQuery, Query, RegexQuery, TermQuery};
use tantivy::schema::{IndexRecordOption, Term};

use crate::index::IndexError;
use crate::index::schema::FilesSchema;
use crate::index::segments::Snapshot;
use crate::lang::Lang;
use crate::tags::{self, Kind, NO_CONTAINER, Role, Tag};

/// Longest accepted `name`.
pub const MAX_NAME_BYTES: usize = tags::MAX_NAME;
/// Files whose tags one query loads; more sets `truncated`.
pub const MAX_FILES: usize = 5_000;
/// Paths of one file reported per query.
pub const MAX_PATHS_PER_FILE: usize = 64;
/// Default and largest result count.
pub const DEFAULT_LIMIT: usize = 100;
pub const MAX_LIMIT: usize = 1_000;
/// Bytes of a file read for import evidence.
pub const MAX_IMPORT_BYTES: usize = 256 << 10;
/// Lines of a file examined for import evidence.
pub const MAX_IMPORT_LINES: usize = 4_000;
/// Files and total bytes the word-boundary fallback scans.
pub const MAX_FALLBACK_FILES: usize = 2_000;
pub const MAX_FALLBACK_BYTES: u64 = 64 << 20;
/// Fuzzy matching needs at least this many chars, so short names do not match everything.
pub const MIN_FUZZY_CHARS: usize = 3;
/// `Symbol::confidence` values.
pub const IMPORT_MATCH: &str = "import_match";
pub const NAME_MATCH: &str = "name_match";

#[derive(Debug, thiserror::Error)]
pub enum SymbolError {
    #[error("invalid name: {0}")]
    InvalidName(&'static str),
    #[error("index: {0}")]
    Index(#[from] IndexError),
    #[error("source: {0}")]
    Source(String),
    #[error("tags: {0}")]
    Tags(#[from] tags::TagsError),
}

pub type Result<T> = std::result::Result<T, SymbolError>;

/// One symbol of a response (the `Symbol` message).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Symbol {
    pub name: String,
    pub kind: String,
    pub is_def: bool,
    pub path: Vec<u8>,
    /// One-based.
    pub line: u32,
    pub end_line: u32,
    pub signature: Vec<u8>,
    pub container: String,
    pub doc: Vec<u8>,
    pub confidence: &'static str,
    /// The secret flag of the file the symbol sits in. `None` (not known) counts as flagged:
    /// the renderer redacts the symbol. There is no default, so a producer must decide.
    pub secret: Option<bool>,
}

/// The symbols of one query.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Symbols {
    pub symbols: Vec<Symbol>,
    /// A limit stopped the query: results or candidate files were dropped.
    pub truncated: bool,
    /// A rough token estimate of the response (bytes / 4).
    pub est_tokens: u32,
}

/// Content of the files of one namespace.
pub trait FileSource {
    /// Repo paths that hold the file (a `FileId` can sit at several paths).
    fn paths(&self, ord: u32) -> Vec<Vec<u8>>;
    /// Language of the file, if it has a grammar.
    fn lang(&self, ord: u32) -> Option<Lang>;
    /// Hash-verified content, or `None` when the file is unavailable (skipped).
    fn bytes(&self, ord: u32) -> Result<Option<Arc<[u8]>>>;
    /// Hash-verified content of a repo path in the query's revision.
    fn read_path(&self, path: &[u8]) -> Result<Option<Arc<[u8]>>>;
    /// Up to `limit` ords of `scope` that have no grammar, hence no refs in the index.
    fn untagged(&self, scope: &RoaringBitmap, limit: usize) -> Vec<u32>;
    /// The secret flag of the file: `Some(false)` only when it is known to be clean. `None`
    /// counts as flagged, so rendering fails closed.
    fn secret(&self, ord: u32) -> Option<bool>;
    /// Tags of the file, from a cache or by extracting them. The default extracts.
    fn tags(&self, ord: u32) -> Result<Option<Arc<Vec<Tag>>>> {
        let Some(lang) = self.lang(ord) else {
            return Ok(None);
        };
        let Some(b) = self.bytes(ord)? else {
            return Ok(None);
        };
        match tags::extract(lang, &b) {
            Ok(t) => Ok(Some(Arc::new(t))),
            // A file that cannot be tagged has no symbols; the query goes on.
            Err(_) => Ok(None),
        }
    }
}

/// Project-graph proximity (G07).
pub trait ProjectRank {
    /// `Some(0)` for the same project, `Some(n)` when `from`'s project reaches `to`'s through
    /// `n` dependency edges (either direction), `None` when unrelated or not in a project.
    fn distance(&self, from: &[u8], to: &[u8]) -> Option<u32>;
}

/// No project graph: every distance is unknown.
pub struct NoProjects;

impl ProjectRank for NoProjects {
    fn distance(&self, _: &[u8], _: &[u8]) -> Option<u32> {
        None
    }
}

/// Parameters shared by the queries.
#[derive(Debug, Clone, Default)]
pub struct SymbolQuery {
    pub name: String,
    pub kinds: Vec<Kind>,
    /// The caller's position (`--at path:line`): for `def` it ranks by proximity; for `refs`
    /// and `callers` it names the definition whose references are wanted.
    pub at: Option<(Vec<u8>, u32)>,
    /// 0 means [`DEFAULT_LIMIT`].
    pub limit: usize,
}

impl SymbolQuery {
    pub fn new(name: impl Into<String>) -> Self {
        SymbolQuery {
            name: name.into(),
            ..SymbolQuery::default()
        }
    }

    fn limit(&self) -> usize {
        match self.limit {
            0 => DEFAULT_LIMIT,
            n => n.min(MAX_LIMIT),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Pattern {
    Exact(String),
    Prefix(String),
}

fn parse_pattern(name: &str, allow_prefix: bool) -> Result<Pattern> {
    if name.is_empty() {
        return Err(SymbolError::InvalidName("empty"));
    }
    if name.len() > MAX_NAME_BYTES {
        return Err(SymbolError::InvalidName("too long"));
    }
    if name.contains('\0') {
        return Err(SymbolError::InvalidName("NUL byte"));
    }
    let ident = |s: &str| {
        s.chars()
            .all(|c| c.is_alphanumeric() || c == '_' || c == '$')
    };
    if let Some(p) = name.strip_suffix('*') {
        if !allow_prefix {
            return Err(SymbolError::InvalidName("a prefix is not allowed here"));
        }
        if p.is_empty() {
            return Err(SymbolError::InvalidName("empty prefix"));
        }
        if !ident(p) {
            return Err(SymbolError::InvalidName("prefix is not an identifier"));
        }
        return Ok(Pattern::Prefix(p.to_owned()));
    }
    if name.contains('*') {
        return Err(SymbolError::InvalidName("`*` is allowed only at the end"));
    }
    Ok(Pattern::Exact(name.to_owned()))
}

/// Edit distance of at most one (insert, delete or substitute) between two names.
pub fn within_one(a: &str, b: &str) -> bool {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    if a.len().abs_diff(b.len()) > 1 {
        return false;
    }
    let (mut i, mut j, mut edits) = (0, 0, 0);
    while i < a.len() && j < b.len() {
        if a[i] == b[j] {
            i += 1;
            j += 1;
            continue;
        }
        edits += 1;
        if edits > 1 {
            return false;
        }
        match a.len().cmp(&b.len()) {
            Ordering::Greater => i += 1,
            Ordering::Less => j += 1,
            Ordering::Equal => {
                i += 1;
                j += 1;
            }
        }
    }
    edits + (a.len() - i) + (b.len() - j) <= 1
}

/// Number of leading path segments two paths share.
fn common_segments(a: &[u8], b: &[u8]) -> usize {
    a.split(|&c| c == b'/')
        .zip(b.split(|&c| c == b'/'))
        .take_while(|(x, y)| x == y)
        .count()
}

/// Smaller is nearer: segments of `a` and `b` that are not shared.
fn path_distance(a: &[u8], b: &[u8]) -> usize {
    let n = |p: &[u8]| p.split(|&c| c == b'/').count();
    let c = common_segments(a, b);
    (n(a) - c) + (n(b) - c)
}

/// The word a path is imported by: its stem, or its directory for index-like files.
fn module_key(path: &[u8]) -> Option<String> {
    let s = String::from_utf8_lossy(path);
    let mut segs: Vec<&str> = s.split('/').filter(|x| !x.is_empty()).collect();
    let file = segs.pop()?;
    let stem = file.split('.').next().unwrap_or(file);
    let index_like = matches!(
        stem,
        "mod" | "lib" | "main" | "index" | "__init__" | "__main__"
    );
    let key = if index_like {
        segs.pop().unwrap_or(stem)
    } else {
        stem
    };
    (!key.is_empty()).then(|| key.to_owned())
}

fn is_word(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'$' || b >= 0x80
}

/// Offsets of `word` in `hay` with word boundaries on both sides.
fn find_word(hay: &[u8], word: &[u8]) -> Option<usize> {
    if word.is_empty() || hay.len() < word.len() {
        return None;
    }
    let mut from = 0;
    while from + word.len() <= hay.len() {
        let rel = hay[from..].windows(word.len()).position(|w| w == word)?;
        let at = from + rel;
        let end = at + word.len();
        let before = at == 0 || !is_word(hay[at - 1]);
        let after = end == hay.len() || !is_word(hay[end]);
        if before && after {
            return Some(at);
        }
        from = at + 1;
    }
    None
}

fn is_import_line(line: &[u8]) -> bool {
    let t = line.trim_ascii_start();
    const STARTS: [&[u8]; 9] = [
        b"use ",
        b"pub use ",
        b"import ",
        b"from ",
        b"using ",
        b"mod ",
        b"pub mod ",
        b"#include",
        b"extern crate ",
    ];
    STARTS.iter().any(|p| t.starts_with(p))
        || find_word(t, b"require").is_some()
        || t.starts_with(b"export ") && find_word(t, b"from").is_some()
        || t.starts_with(b"const ") && find_word(t, b"import").is_some()
}

/// Whether `text` has an import line that names the module of `def_path`. Bounded by
/// [`MAX_IMPORT_BYTES`] and [`MAX_IMPORT_LINES`].
pub fn imports_path(text: &[u8], def_path: &[u8]) -> bool {
    let Some(key) = module_key(def_path) else {
        return false;
    };
    let text = &text[..text.len().min(MAX_IMPORT_BYTES)];
    text.split(|&c| c == b'\n')
        .take(MAX_IMPORT_LINES)
        .any(|l| is_import_line(l) && find_word(l, key.as_bytes()).is_some())
}

// ----------------------------------------------------------------------------- engine

/// Runs symbol queries against one index snapshot.
pub struct SymbolEngine<'a, S: FileSource, P: ProjectRank> {
    snap: &'a Snapshot,
    files: &'a S,
    projects: &'a P,
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Class {
    Exact,
    Prefix,
    Fuzzy,
}

struct Hit {
    sym: Symbol,
    class: Class,
    key: Vec<u8>,
}

fn kind_ok(kinds: &[Kind], k: Kind) -> bool {
    kinds.is_empty() || kinds.contains(&k)
}

fn to_symbol(
    t: &Tag,
    all: &[Tag],
    path: &[u8],
    secret: Option<bool>,
    is_def: bool,
    conf: &'static str,
) -> Symbol {
    let container = all
        .get(t.container as usize)
        .filter(|_| t.container != NO_CONTAINER)
        .map(|c| c.name.clone())
        .unwrap_or_default();
    Symbol {
        name: t.name.clone(),
        kind: t.kind.name().to_owned(),
        is_def,
        path: path.to_vec(),
        line: t.range.start_row.saturating_add(1),
        end_line: t.range.end_row.saturating_add(1),
        signature: t.sig.clone(),
        container,
        doc: t.doc.clone(),
        confidence: conf,
        secret,
    }
}

fn finish(mut symbols: Vec<Symbol>, limit: usize, mut truncated: bool) -> Symbols {
    if symbols.len() > limit {
        symbols.truncate(limit);
        truncated = true;
    }
    let bytes: usize = symbols
        .iter()
        .map(|s| s.name.len() + s.kind.len() + s.path.len() + s.signature.len() + s.doc.len() + 48)
        .sum();
    Symbols {
        symbols,
        truncated,
        est_tokens: u32::try_from(bytes / 4).unwrap_or(u32::MAX),
    }
}

impl<'a, S: FileSource, P: ProjectRank> SymbolEngine<'a, S, P> {
    pub fn new(snap: &'a Snapshot, files: &'a S, projects: &'a P) -> Self {
        SymbolEngine {
            snap,
            files,
            projects,
        }
    }

    fn candidates(&self, q: &dyn Query, scope: &RoaringBitmap) -> Result<(Vec<u32>, bool)> {
        let mut ords = self.snap.ords_for_query(q)?;
        ords &= scope;
        let truncated = ords.len() > MAX_FILES as u64;
        Ok((ords.iter().take(MAX_FILES).collect(), truncated))
    }

    fn def_candidates(
        &self,
        p: &Pattern,
        fuzzy: bool,
        scope: &RoaringBitmap,
    ) -> Result<(Vec<u32>, bool)> {
        let f = FilesSchema::build();
        let q: Box<dyn Query> = match (p, fuzzy) {
            (Pattern::Exact(n), false) => Box::new(TermQuery::new(
                Term::from_field_text(f.sym_def, n),
                IndexRecordOption::Basic,
            )),
            (Pattern::Exact(n), true) => Box::new(FuzzyTermQuery::new(
                Term::from_field_text(f.sym_def, n),
                1,
                false,
            )),
            (Pattern::Prefix(pre), _) => Box::new(
                RegexQuery::from_pattern(&format!("{pre}.*"), f.sym_def)
                    .map_err(|e| IndexError::Tantivy(e.to_string()))?,
            ),
        };
        self.candidates(&*q, scope)
    }

    /// Definitions of `name` (exact, `prefix*`, or fuzzy with distance 1 when an exact name
    /// finds nothing). Ranked: exact, then the project of `at`, then files `at` imports,
    /// then path distance.
    pub fn def(&self, scope: &RoaringBitmap, q: &SymbolQuery) -> Result<Symbols> {
        let pat = parse_pattern(&q.name, true)?;
        let (mut ords, mut truncated) = self.def_candidates(&pat, false, scope)?;
        if let Pattern::Exact(n) = &pat {
            let mut hits = self.def_hits(&ords, &pat, false, q)?;
            if hits.is_empty() && n.chars().count() >= MIN_FUZZY_CHARS {
                (ords, truncated) = self.def_candidates(&pat, true, scope)?;
                hits = self.def_hits(&ords, &pat, true, q)?;
            }
            return Ok(self.rank_defs(hits, q, truncated));
        }
        let hits = self.def_hits(&ords, &pat, false, q)?;
        Ok(self.rank_defs(hits, q, truncated))
    }

    /// Definitions whose name starts with `prefix` (`asc sym find`).
    pub fn find(&self, scope: &RoaringBitmap, q: &SymbolQuery) -> Result<Symbols> {
        let name = q.name.trim_end_matches('*');
        let mut q2 = q.clone();
        q2.name = format!("{name}*");
        let pat = parse_pattern(&q2.name, true)?;
        let (ords, truncated) = self.def_candidates(&pat, false, scope)?;
        let hits = self.def_hits(&ords, &pat, false, &q2)?;
        Ok(self.rank_defs(hits, &q2, truncated))
    }

    fn def_hits(
        &self,
        ords: &[u32],
        pat: &Pattern,
        fuzzy: bool,
        q: &SymbolQuery,
    ) -> Result<Vec<Hit>> {
        let mut out = Vec::new();
        for &ord in ords {
            let Some(tags) = self.files.tags(ord)? else {
                continue;
            };
            let paths = self.files.paths(ord);
            let secret = self.files.secret(ord);
            for t in tags.iter().filter(|t| t.role == Role::Def) {
                if !kind_ok(&q.kinds, t.kind) {
                    continue;
                }
                let class = match pat {
                    Pattern::Exact(n) if &t.name == n => Class::Exact,
                    Pattern::Exact(n) if fuzzy && within_one(&t.name, n) => Class::Fuzzy,
                    Pattern::Prefix(p) if t.name.starts_with(p.as_str()) => Class::Prefix,
                    _ => continue,
                };
                for path in paths.iter().take(MAX_PATHS_PER_FILE) {
                    out.push(Hit {
                        sym: to_symbol(t, &tags, path, secret, true, NAME_MATCH),
                        class,
                        key: path.clone(),
                    });
                }
            }
        }
        Ok(out)
    }

    fn rank_defs(&self, hits: Vec<Hit>, q: &SymbolQuery, truncated: bool) -> Symbols {
        let ctx = q.at.as_ref().map(|(p, _)| p.as_slice());
        let ctx_text = ctx.and_then(|p| self.files.read_path(p).ok().flatten());
        let mut keyed: Vec<(_, Hit)> = hits
            .into_iter()
            .map(|h| {
                let proj = ctx
                    .and_then(|c| self.projects.distance(c, &h.key))
                    .unwrap_or(u32::MAX);
                let near = match (&ctx_text, ctx) {
                    (Some(t), Some(c)) if c == h.key.as_slice() || imports_path(t, &h.key) => 0u8,
                    _ => 1,
                };
                let pd = ctx.map_or(0, |c| path_distance(c, &h.key));
                ((h.class, proj, near, pd, h.key.clone(), h.sym.line), h)
            })
            .collect();
        keyed.sort_by(|a, b| a.0.cmp(&b.0));
        finish(
            keyed.into_iter().map(|(_, h)| h.sym).collect(),
            q.limit(),
            truncated,
        )
    }

    /// The definition position `at`, as its path.
    fn at_def(&self, q: &SymbolQuery) -> Option<Vec<u8>> {
        q.at.as_ref().map(|(p, _)| p.clone())
    }

    fn ref_candidates(&self, name: &str, scope: &RoaringBitmap) -> Result<(Vec<u32>, bool)> {
        let f = FilesSchema::build();
        let q = TermQuery::new(
            Term::from_field_text(f.sym_ref, name),
            IndexRecordOption::Basic,
        );
        self.candidates(&q, scope)
    }

    /// References to `name`. With `at` (the definition's path and line) they are ranked by
    /// import evidence, then project-graph reachability, then path distance.
    pub fn refs(&self, scope: &RoaringBitmap, q: &SymbolQuery) -> Result<Symbols> {
        self.refs_inner(scope, q, false)
    }

    fn refs_inner(
        &self,
        scope: &RoaringBitmap,
        q: &SymbolQuery,
        calls_only: bool,
    ) -> Result<Symbols> {
        let Pattern::Exact(name) = parse_pattern(&q.name, false)? else {
            return Err(SymbolError::InvalidName("a prefix is not allowed here"));
        };
        let (ords, mut truncated) = self.ref_candidates(&name, scope)?;
        let mut found: Vec<Symbol> = Vec::new();
        for ord in ords {
            let Some(tags) = self.files.tags(ord)? else {
                continue;
            };
            let paths = self.files.paths(ord);
            let secret = self.files.secret(ord);
            for t in tags
                .iter()
                .filter(|t| t.role == Role::Ref && t.name == name)
            {
                if !kind_ok(&q.kinds, t.kind) || calls_only && t.kind != Kind::Call {
                    continue;
                }
                for path in paths.iter().take(MAX_PATHS_PER_FILE) {
                    let mut s = to_symbol(t, &tags, path, secret, false, NAME_MATCH);
                    if calls_only {
                        s = caller_of(t, &tags, path, secret);
                    }
                    found.push(s);
                }
            }
        }
        // Languages without refs: word-boundary scan of the untagged files in scope.
        let mut scanned = 0u64;
        let untagged = self.files.untagged(scope, MAX_FALLBACK_FILES + 1);
        if untagged.len() > MAX_FALLBACK_FILES {
            truncated = true;
        }
        if q.kinds.is_empty() || q.kinds.contains(&Kind::Call) || !calls_only {
            for &ord in untagged.iter().take(MAX_FALLBACK_FILES) {
                let Some(b) = self.files.bytes(ord)? else {
                    continue;
                };
                scanned += b.len() as u64;
                if scanned > MAX_FALLBACK_BYTES {
                    truncated = true;
                    break;
                }
                let paths = self.files.paths(ord);
                for s in scan_word(&b, &name, calls_only, &paths, self.files.secret(ord)) {
                    found.push(s);
                }
            }
        }
        if calls_only {
            found = group_callers(found);
        }
        Ok(self.rank_refs(found, q, truncated))
    }

    fn rank_refs(&self, found: Vec<Symbol>, q: &SymbolQuery, truncated: bool) -> Symbols {
        let def_path = self.at_def(q);
        let mut keyed: Vec<_> = Vec::new();
        let mut text_cache: std::collections::HashMap<Vec<u8>, bool> = Default::default();
        for mut s in found {
            let (imp, proj, pd) = match def_path.as_deref() {
                Some(d) => {
                    let imp = if s.path == d {
                        true
                    } else {
                        *text_cache.entry(s.path.clone()).or_insert_with(|| {
                            self.files
                                .read_path(&s.path)
                                .ok()
                                .flatten()
                                .is_some_and(|t| imports_path(&t, d))
                        })
                    };
                    let proj = self.projects.distance(&s.path, d).unwrap_or(u32::MAX);
                    (imp, proj, path_distance(&s.path, d))
                }
                None => (false, u32::MAX, 0),
            };
            if imp {
                s.confidence = IMPORT_MATCH;
            }
            keyed.push(((u8::from(!imp), proj, pd, s.path.clone(), s.line), s));
        }
        keyed.sort_by(|a, b| a.0.cmp(&b.0));
        finish(
            keyed.into_iter().map(|(_, s)| s).collect(),
            q.limit(),
            truncated,
        )
    }

    /// Callers of `name`: its call sites grouped by the enclosing definition.
    pub fn callers(&self, scope: &RoaringBitmap, q: &SymbolQuery) -> Result<Symbols> {
        self.refs_inner(scope, q, true)
    }
}

/// The caller entry of a call tag: its enclosing definition, or the call site itself when
/// the call is at the top level of the file.
fn caller_of(call: &Tag, all: &[Tag], path: &[u8], secret: Option<bool>) -> Symbol {
    match all
        .get(call.container as usize)
        .filter(|_| call.container != NO_CONTAINER)
    {
        Some(c) => {
            let parent = all
                .get(c.container as usize)
                .filter(|_| c.container != NO_CONTAINER)
                .map(|p| p.name.clone())
                .unwrap_or_default();
            Symbol {
                container: parent,
                ..to_symbol(c, all, path, secret, true, NAME_MATCH)
            }
        }
        None => Symbol {
            name: String::new(),
            kind: Kind::Call.name().to_owned(),
            is_def: false,
            path: path.to_vec(),
            line: call.range.start_row.saturating_add(1),
            end_line: call.range.end_row.saturating_add(1),
            signature: Vec::new(),
            container: String::new(),
            doc: Vec::new(),
            confidence: NAME_MATCH,
            secret,
        },
    }
}

/// One entry per `(path, enclosing definition)`; top-level calls make one entry per file.
fn group_callers(found: Vec<Symbol>) -> Vec<Symbol> {
    let mut seen = std::collections::HashSet::new();
    found
        .into_iter()
        .filter(|s| {
            let key = if s.is_def {
                (s.path.clone(), s.line, s.name.clone())
            } else {
                (s.path.clone(), 0, String::new())
            };
            seen.insert(key)
        })
        .collect()
}

/// Word-boundary matches of `name` in an untagged file, one per line, with the line as the
/// signature. For callers only a match followed by `(` counts.
fn scan_word(
    bytes: &[u8],
    name: &str,
    calls_only: bool,
    paths: &[Vec<u8>],
    secret: Option<bool>,
) -> Vec<Symbol> {
    if bytes.contains(&0) {
        return Vec::new();
    }
    let mut out = Vec::new();
    for (i, line) in bytes.split(|&c| c == b'\n').enumerate() {
        let Some(at) = find_word(line, name.as_bytes()) else {
            continue;
        };
        if calls_only && !line[at + name.len()..].trim_ascii_start().starts_with(b"(") {
            continue;
        }
        let sig = line.trim_ascii();
        let sig = &sig[..sig.len().min(tags::MAX_SIG)];
        let n = u32::try_from(i).unwrap_or(u32::MAX).saturating_add(1);
        for p in paths.iter().take(MAX_PATHS_PER_FILE) {
            out.push(Symbol {
                name: if calls_only {
                    String::new()
                } else {
                    name.to_owned()
                },
                kind: if calls_only {
                    Kind::Call.name().to_owned()
                } else {
                    String::new()
                },
                is_def: false,
                path: p.clone(),
                line: n,
                end_line: n,
                signature: sig.to_vec(),
                container: String::new(),
                doc: Vec::new(),
                confidence: NAME_MATCH,
                secret,
            });
        }
        if out.len() >= 4 * MAX_LIMIT {
            break;
        }
    }
    out
}

/// The definitions of one file in source order (`outline`): no index, only the file's own
/// tags. `container` names the enclosing definition. `secret` is the file's secret flag.
pub fn outline(
    path: &[u8],
    secret: Option<bool>,
    lang: Lang,
    bytes: &[u8],
    kinds: &[Kind],
    limit: usize,
) -> Result<Symbols> {
    let tags = tags::extract(lang, bytes)?;
    let out: Vec<Symbol> = tags::outline(&tags)
        .into_iter()
        .map(|e| &tags[e.index as usize])
        .filter(|t| kind_ok(kinds, t.kind))
        .map(|t| to_symbol(t, &tags, path, secret, true, NAME_MATCH))
        .collect();
    let limit = match limit {
        0 => DEFAULT_LIMIT,
        n => n.min(MAX_LIMIT),
    };
    Ok(finish(out, limit, false))
}

#[cfg(test)]
mod tests;
