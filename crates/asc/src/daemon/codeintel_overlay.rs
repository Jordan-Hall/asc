//! Daemon code intelligence overlay (SPEC §23.2.5, I18).
//!
//! With workspace scope the server answers at the workspace's `HEAD` with `exclude_paths` set to
//! the dirty paths, and the daemon answers the same query over an in-memory overlay of the
//! dirty, staged and untracked files; the two answers are merged. The overlay is
//!
//! * built on the first query (the dirty set is what `Status` reports, so it is exact; the
//!   journal only tells the watchers when to look),
//! * bounded: [`MAX_FILES`] files and [`MAX_HEAP`] bytes of resident text, and at most
//!   [`MAX_READ`] bytes read per build; a dirty file that does not fit is counted and lowers
//!   `coverage`, never silently dropped,
//! * dropped after [`IDLE`] without a query (a sweeper task, started by the first build).
//!
//! The daemon never loads a model and never builds an index: exact search is a bounded scan,
//! symbols come from the grammar tags of the matching files, and semantic search has only a
//! lexical overlay answer (the vectors are the server's).
//!
//! Every byte is untrusted: files are reached through [`SafeRoot`] (a symlinked ancestor or a
//! linked leaf is a missing file), read with a hard cap, classified with the skip rules, and
//! rendered through `asc_codeintel::render` (redaction, control-character stripping, budgets).

use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::time::{Duration, Instant, SystemTime};

use asc_codeintel::chunk;
use asc_codeintel::lang::{self, Lang};
use asc_codeintel::render::{self, RawChunk, RawFileHit, RawLine, RenderCtx};
use asc_codeintel::search::{
    MAX_LINE_BYTES, MAX_LINES_PER_FILE, MAX_SPANS_PER_LINE, REGEX_SIZE_LIMIT, WINDOW,
};
use asc_codeintel::symbols::{self, IMPORT_MATCH, NAME_MATCH, Symbol};
use asc_codeintel::tags::{self, Kind, NO_CONTAINER, Role, Tag};
use asc_codeintel::{plan, secrets, skip};
use asc_core::id::FileId;
use asc_core::path::RepoPath;
use asc_rpc::codeintel as ci;
use asc_rpc::server::wire;
use asc_state::WorkspaceId;
use asc_wc::safe_path::{SafeRoot, is_link};
use async_trait::async_trait;
use bytes::Bytes;
use regex::bytes::RegexBuilder;
use tonic::Status;

use super::plugin_api::{self, PluginStream};
use super::service::{State, check_mount_root, parse_id};

/// Most files the overlay holds.
pub const MAX_FILES: usize = 4_096;
/// Most resident bytes (text plus a fixed cost per file).
pub const MAX_HEAP: usize = 16 << 20;
/// Most file bytes read by one build.
pub const MAX_READ: u64 = 32 << 20;
/// An overlay unused for this long is dropped.
pub const IDLE: Duration = Duration::from_secs(300);
/// Most entries sent to the server as `exclude_paths` (the wire limit); the daemon filters the
/// rest of the server's answer itself.
pub const MAX_EXCLUDE: usize = ci::MAX_LIST;
/// Fixed cost charged per held file.
const PER_FILE: usize = 256;
/// A file changed this recently is re-read even if its stamp is unchanged (timestamp
/// granularity).
const RACY: Duration = Duration::from_secs(2);
/// Time a symbol or semantic scan may take.
const SCAN_BUDGET: Duration = Duration::from_secs(3);
/// Most overlay chunks scored for a semantic query.
const MAX_CHUNK_FILES: usize = 512;
/// Most query terms of a semantic query.
const MAX_TERMS: usize = 16;
/// Most dirty paths the status answer may name before the rest is ignored.
const MAX_DIRTY: usize = 200_000;

fn bad(m: impl Into<String>) -> Status {
    Status::invalid_argument(m.into())
}

// ------------------------------------------------------------------------------ the overlay

/// One held file.
struct Entry {
    text: Arc<Vec<u8>>,
    lang: Option<Lang>,
    id: FileId,
    stamp: (u64, u128),
    /// The path or the content looks like a secret: results are redacted.
    flagged: bool,
    /// When the stamp was taken.
    read_at: SystemTime,
}

/// The dirty files of one workspace at one moment.
pub struct Overlay {
    files: BTreeMap<RepoPath, Entry>,
    /// Every dirty path (held or not): the base version of each is hidden from the server.
    hidden: Vec<RepoPath>,
    /// Dirty files that were readable but did not fit the bounds.
    unheld: u64,
    heap: usize,
    /// Build counter of the cache, so a client can see a rebuild.
    pub generation: u64,
}

impl Overlay {
    /// The paths whose base version the server must not answer.
    pub fn hidden(&self) -> &[RepoPath] {
        &self.hidden
    }

    /// Held files.
    pub fn len(&self) -> usize {
        self.files.len()
    }

    /// True when no file is held.
    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }

    /// Resident bytes.
    pub fn heap(&self) -> usize {
        self.heap
    }

    /// Dirty files that could not be held.
    pub fn unheld(&self) -> u64 {
        self.unheld
    }

    /// Share of the readable dirty files that the overlay holds.
    pub fn coverage(&self) -> f32 {
        let held = self.files.len() as f64;
        let all = held + self.unheld as f64;
        if all == 0.0 { 1.0 } else { (held / all) as f32 }
    }

    /// Builds the overlay of `dirty` below `root`. An entry of `prev` whose size and
    /// modification time are unchanged (and old enough not to be racy) is reused. Never
    /// follows a link, never reads more than the bounds allow.
    pub fn build(root: &Path, dirty: &[RepoPath], prev: Option<&Overlay>, generation: u64) -> Self {
        let safe = SafeRoot::new(root);
        let mut hidden: Vec<RepoPath> = dirty.to_vec();
        hidden.sort();
        hidden.dedup();
        let mut files = BTreeMap::new();
        let (mut heap, mut read, mut unheld) = (0usize, 0u64, 0u64);
        for path in &hidden {
            let Some(abs) = safe.resolve(path) else {
                continue;
            };
            let Ok(meta) = std::fs::symlink_metadata(&abs) else {
                continue;
            };
            if is_link(&meta) || !meta.is_file() || meta.len() > skip::MAX_BYTES as u64 {
                continue;
            }
            let stamp = (meta.len(), mtime_ns(&meta));
            let cost = meta.len() as usize + PER_FILE + path.as_bytes().len();
            if files.len() >= MAX_FILES || heap + cost > MAX_HEAP {
                unheld += 1;
                continue;
            }
            let kept = prev.and_then(|p| p.files.get(path)).filter(|e| {
                e.stamp == stamp
                    && e.read_at
                        .duration_since(SystemTime::UNIX_EPOCH)
                        .is_ok_and(|t| t.as_nanos() > stamp.1 + RACY.as_nanos())
            });
            let entry = if let Some(e) = kept {
                Entry {
                    text: Arc::clone(&e.text),
                    lang: e.lang,
                    id: e.id,
                    stamp,
                    flagged: e.flagged,
                    read_at: e.read_at,
                }
            } else {
                if read + meta.len() > MAX_READ {
                    unheld += 1;
                    continue;
                }
                read += meta.len();
                let read_at = SystemTime::now();
                let Some(text) = read_capped(&abs) else {
                    continue;
                };
                if !skip::classify(&text).indexable() {
                    continue;
                }
                let name = String::from_utf8_lossy(path.as_bytes()).into_owned();
                Entry {
                    lang: lang::detect(&name, &text[..text.len().min(256)]),
                    id: FileId::of(&text),
                    flagged: secrets::path_is_secret(&name)
                        || secrets::scan_content(&text).is_secret(),
                    stamp,
                    read_at,
                    text: Arc::new(text),
                }
            };
            heap += cost;
            files.insert(path.clone(), entry);
        }
        Self {
            files,
            hidden,
            unheld,
            heap,
            generation,
        }
    }
}

fn mtime_ns(meta: &std::fs::Metadata) -> u128 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_nanos())
}

/// The file's bytes, at most [`skip::MAX_BYTES`]; `None` when it cannot be read or grew.
fn read_capped(abs: &Path) -> Option<Vec<u8>> {
    let f = std::fs::File::open(abs).ok()?;
    let mut buf = Vec::new();
    f.take(skip::MAX_BYTES as u64 + 1)
        .read_to_end(&mut buf)
        .ok()?;
    (buf.len() <= skip::MAX_BYTES).then_some(buf)
}

// ---------------------------------------------------------------------------- query filters

fn under(p: &RepoPath, d: &RepoPath) -> bool {
    p == d || p.starts_with_dir(d)
}

fn repo_path(s: &str, what: &str) -> Result<RepoPath, Status> {
    RepoPath::new(s.trim_end_matches('/').as_bytes())
        .map_err(|_| bad(format!("{what}: not a repository path")))
}

/// The path and language filters of a scope.
#[derive(Default)]
pub struct Filter {
    paths: Vec<RepoPath>,
    exclude: Vec<RepoPath>,
    langs: Vec<Lang>,
}

impl Filter {
    /// Reads the filters of a checked scope.
    pub fn of(scope: &ci::Scope) -> Result<Self, Status> {
        let paths = |v: &[String], what| -> Result<Vec<RepoPath>, Status> {
            v.iter().map(|s| repo_path(s, what)).collect()
        };
        let langs = scope
            .langs
            .iter()
            .map(|l| Lang::from_name(l).ok_or_else(|| bad(format!("unknown language '{l}'"))))
            .collect::<Result<Vec<Lang>, Status>>()?;
        Ok(Self {
            paths: paths(&scope.paths, "paths")?,
            exclude: paths(&scope.exclude_paths, "exclude_paths")?,
            langs,
        })
    }

    fn admits(&self, p: &RepoPath, lang: Option<Lang>) -> bool {
        (self.paths.is_empty() || self.paths.iter().any(|d| under(p, d)))
            && !self.exclude.iter().any(|d| under(p, d))
            && (self.langs.is_empty() || lang.is_some_and(|l| self.langs.contains(&l)))
    }
}

// --------------------------------------------------------------------------------- search

/// An exact search over the overlay.
pub struct SearchQuery<'a> {
    pub pattern: &'a str,
    pub mode: ci::SearchMode,
    pub case_sensitive: bool,
    pub context: usize,
}

/// Matching lines of one file, bounded like the server's executor.
fn scan(re: &regex::bytes::Regex, text: &[u8], context: usize) -> (Vec<RawLine>, bool) {
    let body = text.strip_suffix(b"\n").unwrap_or(text);
    let lines: Vec<&[u8]> = body.split(|&b| b == b'\n').collect();
    let mut out = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        let mut spans = Vec::new();
        let mut hit = false;
        for m in re.find_iter(line) {
            hit = true;
            if m.start() < MAX_LINE_BYTES {
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
        let cut = |l: &[u8]| l[..l.len().min(MAX_LINE_BYTES)].to_vec();
        out.push(RawLine {
            line: u32::try_from(i + 1).unwrap_or(u32::MAX),
            text: cut(line),
            spans,
            before: lines[i.saturating_sub(context)..i]
                .iter()
                .map(|l| cut(l))
                .collect(),
            after: lines[(i + 1).min(lines.len())..(i + 1 + context).min(lines.len())]
                .iter()
                .map(|l| cut(l))
                .collect(),
        });
    }
    (out, false)
}

impl Overlay {
    /// Files of the overlay that contain a match, in path order.
    pub fn search(
        &self,
        q: &SearchQuery<'_>,
        f: &Filter,
    ) -> Result<(Vec<RawFileHit>, bool), Status> {
        if q.pattern.is_empty() || q.pattern.len() > plan::MAX_PATTERN {
            return Err(bad("INVALID_ARGUMENT: pattern is empty or too long"));
        }
        let escaped = regex::escape(q.pattern);
        let source = match q.mode {
            ci::SearchMode::Literal => escaped,
            ci::SearchMode::Word => format!(r"\b{escaped}\b"),
            ci::SearchMode::Regex => q.pattern.to_owned(),
        };
        let re = RegexBuilder::new(&source)
            .case_insensitive(!q.case_sensitive)
            .size_limit(REGEX_SIZE_LIMIT)
            .dfa_size_limit(REGEX_SIZE_LIMIT)
            .build()
            .map_err(|e| bad(format!("INVALID_ARGUMENT: bad pattern: {e}")))?;
        let mut hits = Vec::new();
        let mut truncated = false;
        for (path, e) in &self.files {
            if !f.admits(path, e.lang) {
                continue;
            }
            if !re.is_match(&e.text) {
                continue;
            }
            let (lines, more) = scan(&re, &e.text, q.context);
            if lines.is_empty() {
                continue;
            }
            truncated |= more;
            if hits.len() >= WINDOW {
                truncated = true;
                break;
            }
            hits.push(RawFileHit {
                file: e.id,
                lang: e.lang.map(|l| l.name().to_owned()).unwrap_or_default(),
                paths: vec![path.as_bytes().to_vec()],
                lines,
                flagged: e.flagged,
            });
        }
        Ok((hits, truncated))
    }
}

// -------------------------------------------------------------------------------- symbols

/// A name pattern: exact, or `prefix*`.
enum Pat {
    Exact(String),
    Prefix(String),
}

fn pattern(name: &str) -> Result<Pat, Status> {
    if name.is_empty() || name.len() > symbols::MAX_NAME_BYTES || name.contains('\0') {
        return Err(bad("invalid symbol name"));
    }
    match name.strip_suffix('*') {
        Some("") => Err(bad("empty prefix")),
        Some(p) if p.contains('*') => Err(bad("`*` is allowed only at the end")),
        Some(p) => Ok(Pat::Prefix(p.to_owned())),
        None if name.contains('*') => Err(bad("`*` is allowed only at the end")),
        None => Ok(Pat::Exact(name.to_owned())),
    }
}

impl Pat {
    fn needle(&self) -> &str {
        match self {
            Pat::Exact(s) | Pat::Prefix(s) => s,
        }
    }
    fn matches(&self, name: &str) -> bool {
        match self {
            Pat::Exact(s) => name == s,
            Pat::Prefix(p) => name.starts_with(p.as_str()),
        }
    }
}

fn contains(hay: &[u8], needle: &[u8]) -> bool {
    needle.is_empty() || hay.windows(needle.len()).any(|w| w == needle)
}

fn symbol_of(
    t: &Tag,
    all: &[Tag],
    path: &RepoPath,
    flagged: bool,
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
        path: path.as_bytes().to_vec(),
        line: t.range.start_row.saturating_add(1),
        end_line: t.range.end_row.saturating_add(1),
        signature: t.sig.clone(),
        container,
        doc: t.doc.clone(),
        confidence: conf,
        secret: Some(flagged),
    }
}

/// The caller of a call tag: its enclosing definition, or the call site at the top level.
fn caller_of(call: &Tag, all: &[Tag], path: &RepoPath, flagged: bool) -> Symbol {
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
                ..symbol_of(c, all, path, flagged, true, NAME_MATCH)
            }
        }
        None => Symbol {
            name: String::new(),
            kind: Kind::Call.name().to_owned(),
            is_def: false,
            path: path.as_bytes().to_vec(),
            line: call.range.start_row.saturating_add(1),
            end_line: call.range.end_row.saturating_add(1),
            signature: Vec::new(),
            container: String::new(),
            doc: Vec::new(),
            confidence: NAME_MATCH,
            secret: Some(flagged),
        },
    }
}

/// A symbol query over the overlay.
pub struct SymbolQuery<'a> {
    pub op: ci::SymbolOp,
    pub name: &'a str,
    pub path: Option<&'a RepoPath>,
    pub kinds: &'a [Kind],
    pub def_path: Option<&'a RepoPath>,
}

impl Overlay {
    /// Symbols of the overlay's files. `bool` is true when the time budget cut the scan.
    pub fn symbols(&self, q: &SymbolQuery<'_>, f: &Filter) -> Result<(Vec<Symbol>, bool), Status> {
        let kind_ok = |k: Kind| q.kinds.is_empty() || q.kinds.contains(&k);
        if q.op == ci::SymbolOp::Outline {
            let path = q.path.ok_or_else(|| bad("outline needs a path"))?;
            let Some(e) = self.files.get(path) else {
                return Ok((Vec::new(), false));
            };
            let Some(l) = e.lang else {
                return Ok((Vec::new(), false));
            };
            let out = symbols::outline(path.as_bytes(), Some(e.flagged), l, &e.text, q.kinds, 0)
                .map(|s| s.symbols)
                .unwrap_or_default();
            return Ok((out, false));
        }
        let pat = pattern(q.name)?;
        if q.op != ci::SymbolOp::Find && matches!(pat, Pat::Prefix(_)) && q.op != ci::SymbolOp::Def
        {
            return Err(bad("a prefix is allowed only for def and find"));
        }
        let started = Instant::now();
        let mut out: Vec<Symbol> = Vec::new();
        let mut cut = false;
        for (path, e) in &self.files {
            let Some(l) = e.lang else { continue };
            if !f.admits(path, e.lang) {
                continue;
            }
            // `find` is fuzzy, so the file text cannot be prefiltered by the name.
            if q.op != ci::SymbolOp::Find && !contains(&e.text, pat.needle().as_bytes()) {
                continue;
            }
            if started.elapsed() > SCAN_BUDGET {
                cut = true;
                break;
            }
            let Ok(all) = tags::extract(l, &e.text) else {
                continue;
            };
            let imports = q
                .def_path
                .is_some_and(|d| symbols::imports_path(&e.text, d.as_bytes()));
            for t in &all {
                if !kind_ok(t.kind) {
                    continue;
                }
                match q.op {
                    ci::SymbolOp::Def if t.role == Role::Def && pat.matches(&t.name) => {
                        out.push(symbol_of(t, &all, path, e.flagged, true, NAME_MATCH));
                    }
                    ci::SymbolOp::Find
                        if t.role == Role::Def
                            && (pat.matches(&t.name)
                                || symbols::within_one(pat.needle(), &t.name)) =>
                    {
                        out.push(symbol_of(t, &all, path, e.flagged, true, NAME_MATCH));
                    }
                    ci::SymbolOp::Refs if t.role == Role::Ref && pat.matches(&t.name) => {
                        let conf = if imports { IMPORT_MATCH } else { NAME_MATCH };
                        out.push(symbol_of(t, &all, path, e.flagged, false, conf));
                    }
                    ci::SymbolOp::Callers
                        if t.role == Role::Ref && t.kind == Kind::Call && pat.matches(&t.name) =>
                    {
                        out.push(caller_of(t, &all, path, e.flagged));
                    }
                    _ => {}
                }
            }
        }
        if q.op == ci::SymbolOp::Callers {
            let mut seen = HashSet::new();
            out.retain(|s| {
                let key = if s.is_def {
                    (s.path.clone(), s.line, s.name.clone())
                } else {
                    (s.path.clone(), 0, String::new())
                };
                seen.insert(key)
            });
        }
        // Import evidence first, then path order (the files were visited in path order).
        out.sort_by_key(|s| s.confidence != IMPORT_MATCH);
        Ok((out, cut))
    }
}

// ------------------------------------------------------------------------------- semantic

fn terms(query: &str) -> Vec<String> {
    let mut v: Vec<String> = query
        .split(|c: char| !(c.is_alphanumeric() || c == '_'))
        .filter(|t| t.chars().count() >= 3)
        .map(str::to_lowercase)
        .collect();
    v.sort();
    v.dedup();
    v.truncate(MAX_TERMS);
    v
}

/// The bytes of lines `start..=end` (one-based) of `text`, capped.
fn line_range(text: &[u8], start: u32, end: u32) -> Vec<u8> {
    let mut out = Vec::new();
    for (i, l) in text.split(|&b| b == b'\n').enumerate() {
        let n = u32::try_from(i + 1).unwrap_or(u32::MAX);
        if n < start {
            continue;
        }
        if n > end || out.len() > chunk::MAX_CHUNK_BYTES {
            break;
        }
        out.extend_from_slice(l);
        out.push(b'\n');
    }
    out
}

impl Overlay {
    /// A lexical answer for a semantic query: the overlay's chunks that contain query terms,
    /// scored by the share of terms they contain. The daemon has no vectors.
    pub fn lexical_chunks(&self, query: &str, f: &Filter, k: usize) -> (Vec<RawChunk>, bool) {
        let want = terms(query);
        if want.is_empty() {
            return (Vec::new(), false);
        }
        let started = Instant::now();
        let mut out: Vec<RawChunk> = Vec::new();
        let mut cut = false;
        let mut scanned = 0;
        for (path, e) in &self.files {
            if !f.admits(path, e.lang) {
                continue;
            }
            let lower = String::from_utf8_lossy(&e.text).to_lowercase();
            if !want.iter().any(|t| lower.contains(t.as_str())) {
                continue;
            }
            scanned += 1;
            if scanned > MAX_CHUNK_FILES || started.elapsed() > SCAN_BUDGET {
                cut = true;
                break;
            }
            let Ok(chunks) = chunk::chunk(e.lang, &e.text) else {
                continue;
            };
            for c in chunks {
                let text = line_range(&e.text, c.start_line, c.end_line);
                let low = String::from_utf8_lossy(&text).to_lowercase();
                let n = want.iter().filter(|t| low.contains(t.as_str())).count();
                if n == 0 {
                    continue;
                }
                out.push(RawChunk {
                    path: path.as_bytes().to_vec(),
                    file: e.id,
                    start_line: c.start_line,
                    end_line: c.end_line,
                    symbol: c.symbol,
                    score: n as f64 / want.len() as f64,
                    sources: vec!["overlay".to_owned()],
                    text,
                    flagged: e.flagged,
                });
            }
        }
        out.sort_by(|a, b| {
            b.score
                .total_cmp(&a.score)
                .then_with(|| a.path.cmp(&b.path))
                .then_with(|| a.start_line.cmp(&b.start_line))
        });
        if out.len() > k {
            out.truncate(k);
            cut = true;
        }
        (out, cut)
    }
}

// ------------------------------------------------------------------------------- the cache

struct Slot {
    overlay: Arc<Overlay>,
    last_used: Instant,
}

/// The overlays of the mounted workspaces, dropped after [`IDLE`].
pub struct OverlayCache {
    slots: Mutex<HashMap<WorkspaceId, Slot>>,
    idle: Duration,
    builds: AtomicU64,
    sweeping: AtomicBool,
    /// One build at a time, so concurrent queries share the work.
    gate: tokio::sync::Mutex<()>,
}

impl OverlayCache {
    /// A cache that drops overlays after `idle`.
    pub fn new(idle: Duration) -> Arc<Self> {
        Arc::new(Self {
            slots: Mutex::default(),
            idle,
            builds: AtomicU64::new(0),
            sweeping: AtomicBool::new(false),
            gate: tokio::sync::Mutex::new(()),
        })
    }

    /// The idle time from `ASC_OVERLAY_IDLE_SECS` (a test knob, 1 to 3600), else [`IDLE`].
    pub fn from_env() -> Arc<Self> {
        let idle = std::env::var("ASC_OVERLAY_IDLE_SECS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .filter(|s| (1..=3600).contains(s))
            .map_or(IDLE, Duration::from_secs);
        Self::new(idle)
    }

    fn slots(&self) -> MutexGuard<'_, HashMap<WorkspaceId, Slot>> {
        self.slots.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The held overlay of `id`, marked used.
    pub fn get(&self, id: WorkspaceId) -> Option<Arc<Overlay>> {
        let mut s = self.slots();
        let slot = s.get_mut(&id)?;
        slot.last_used = Instant::now();
        Some(Arc::clone(&slot.overlay))
    }

    /// Holds `overlay` for `id`.
    pub fn put(self: &Arc<Self>, id: WorkspaceId, overlay: Arc<Overlay>) {
        self.slots().insert(
            id,
            Slot {
                overlay,
                last_used: Instant::now(),
            },
        );
        self.start_sweeper();
    }

    /// Overlays held.
    pub fn len(&self) -> usize {
        self.slots().len()
    }

    /// True when none is held.
    pub fn is_empty(&self) -> bool {
        self.slots().is_empty()
    }

    /// Drops the overlay of `id` (the workspace was unmounted).
    pub fn forget(&self, id: WorkspaceId) {
        let gone = self.slots().remove(&id);
        if gone.is_some() {
            drop(gone);
            super::metrics::release_freed_memory();
        }
    }

    /// Drops every overlay unused for the idle time as of `now`; answers how many.
    pub fn sweep(&self, now: Instant) -> usize {
        let dropped = {
            let mut s = self.slots();
            let before = s.len();
            s.retain(|_, slot| now.saturating_duration_since(slot.last_used) < self.idle);
            before - s.len()
        };
        if dropped > 0 {
            // The overlay's text was many small allocations; the allocator keeps them mapped
            // until told to give them back, and the idle daemon must stay within its budget.
            super::metrics::release_freed_memory();
        }
        dropped
    }

    fn start_sweeper(self: &Arc<Self>) {
        if self.sweeping.swap(true, Ordering::SeqCst) {
            return;
        }
        let Ok(rt) = tokio::runtime::Handle::try_current() else {
            self.sweeping.store(false, Ordering::SeqCst);
            return;
        };
        let weak: Weak<Self> = Arc::downgrade(self);
        let tick = (self.idle / 4).clamp(Duration::from_millis(250), Duration::from_secs(30));
        rt.spawn(async move {
            loop {
                tokio::time::sleep(tick).await;
                let Some(cache) = weak.upgrade() else { return };
                cache.sweep(Instant::now());
            }
        });
    }
}

// ----------------------------------------------------------------------------- the server

/// What the daemon asks the asc server (the codeintel service of I16) for its part of an
/// answer. The daemon holds none when the workspace's remote is not an asc server or the daemon
/// is offline.
#[async_trait]
pub trait ServerAnswer: Send + Sync {
    async fn search(&self, req: wire::SearchRequest) -> Result<Vec<wire::SearchResponse>, Status>;
    async fn symbols(&self, req: wire::SymbolsRequest) -> Result<wire::SymbolsResponse, Status>;
    async fn semantic(&self, req: wire::SemanticRequest) -> Result<wire::SemanticResponse, Status>;
    async fn index_status(
        &self,
        req: wire::IndexStatusRequest,
    ) -> Result<wire::IndexStatusResponse, Status>;
}

fn no_server() -> Status {
    Status::failed_precondition(
        "no asc server answers for this workspace; only workspace scope is served locally",
    )
}

fn hex(b: &[u8]) -> String {
    use std::fmt::Write as _;
    b.iter()
        .fold(String::with_capacity(b.len() * 2), |mut s, x| {
            let _ = write!(s, "{x:02x}");
            s
        })
}

fn budget(b: ci::Budget) -> render::Budget {
    render::Budget {
        max_results: b.max_results,
        max_bytes: b.max_bytes,
        max_tokens: b.max_tokens,
    }
}

/// The scope of a request sent to the server: `HEAD` as a revision, the dirty paths excluded.
fn server_scope(scope: &ci::Scope, head: &[u8], hidden: &[RepoPath]) -> ci::Scope {
    let mut s = scope.clone();
    s.at = ci::At::Rev(hex(head));
    for p in hidden {
        if s.exclude_paths.len() >= MAX_EXCLUDE {
            break;
        }
        if let Ok(t) = std::str::from_utf8(p.as_bytes()) {
            s.exclude_paths.push(t.to_owned());
        }
    }
    s
}

fn resolved(
    head: &Bytes,
    ov: &Overlay,
    server: Option<&wire::ResolvedScope>,
) -> wire::ResolvedScope {
    match server {
        Some(s) => wire::ResolvedScope {
            coverage: s.coverage * ov.coverage(),
            ..s.clone()
        },
        // The base was not searched: the answer covers the dirty files only.
        None => wire::ResolvedScope {
            changeset: head.clone(),
            root: Bytes::new(),
            generation: ov.generation,
            coverage: 0.0,
        },
    }
}

/// What a request needs of the workspace: the overlay and the identity of `HEAD`.
pub struct Ctx {
    pub overlay: Arc<Overlay>,
    pub head: Bytes,
}

fn render_ctx(repo: &str, head: &Bytes) -> RenderCtx {
    RenderCtx {
        repo: repo.to_owned(),
        changeset: head.to_vec(),
    }
}

fn wire_file(f: render::FileOut) -> wire::FileHit {
    wire::FileHit {
        path: Bytes::from(f.path),
        file_id: Bytes::copy_from_slice(&f.file_id),
        lang: f.lang,
        also_at: f.also_at.into_iter().map(Bytes::from).collect(),
        matches: f
            .matches
            .into_iter()
            .map(|m| wire::LineMatch {
                line: m.line,
                cols: m.cols,
                text: Bytes::from(m.text),
                before: m.before.into_iter().map(Bytes::from).collect(),
                after: m.after.into_iter().map(Bytes::from).collect(),
            })
            .collect(),
        redacted: f.redacted,
    }
}

fn wire_symbol(s: render::SymbolOut) -> wire::Symbol {
    wire::Symbol {
        name: s.name,
        kind: s.kind,
        is_def: s.is_def,
        path: Bytes::from(s.path),
        line: s.line,
        end_line: s.end_line,
        signature: Bytes::from(s.signature),
        container: s.container,
        doc: Bytes::from(s.doc),
        confidence: s.confidence,
    }
}

fn wire_chunk(c: render::ChunkOut) -> wire::Chunk {
    wire::Chunk {
        path: Bytes::from(c.path),
        file_id: Bytes::copy_from_slice(&c.file_id),
        start_line: c.start_line,
        end_line: c.end_line,
        symbol: c.symbol,
        score: c.score,
        sources: c.sources,
        text: Bytes::from(c.text),
        redacted: c.redacted,
    }
}

fn limit(b: ci::Budget) -> usize {
    let n = match b.max_results {
        0 => render::DEFAULT_RESULTS,
        n => n.min(render::MAX_RESULTS),
    };
    n as usize
}

fn hidden_set(ov: &Overlay) -> HashSet<&[u8]> {
    ov.hidden.iter().map(RepoPath::as_bytes).collect()
}

/// Search: the overlay's hits first (the working copy is what the caller edits), then the
/// server's hits of the other files, up to the result limit.
pub async fn search(
    cx: &Ctx,
    server: Option<&dyn ServerAnswer>,
    req: &ci::SearchRequest,
) -> Result<Vec<wire::SearchResponse>, Status> {
    let ov = &cx.overlay;
    let page = !req.page_token.is_empty();
    // A page token belongs to the server's answer; the overlay answered on the first page.
    if page && server.is_none() {
        return Err(bad("INVALID_ARGUMENT: the page token is not valid here"));
    }
    let mut out = Vec::new();
    let max = limit(req.budget);
    let mut used = 0usize;
    let mut tokens = 0u32;
    if !page {
        let filter = Filter::of(&req.scope)?;
        let q = SearchQuery {
            pattern: &req.pattern,
            mode: req.mode,
            case_sensitive: req.case_sensitive,
            context: req.context_lines as usize,
        };
        let (hits, cut) = ov.search(&q, &filter)?;
        let r = render::render_search(
            &render_ctx(&req.scope.repo, &cx.head),
            budget(req.budget),
            &hits,
            cut,
        );
        used = r.files.len();
        tokens = r.est_tokens;
        out.push(wire::SearchResponse {
            scope: Some(resolved(&cx.head, ov, None)),
            files: r.files.into_iter().map(wire_file).collect(),
            truncated: r.truncated,
            next_page_token: String::new(),
            est_tokens: r.est_tokens,
        });
    }
    let Some(server) = server else {
        return Ok(out);
    };
    let mut sreq = req.clone();
    sreq.scope = server_scope(&req.scope, &cx.head, ov.hidden());
    let hidden = hidden_set(ov);
    for mut m in server.search(wire::SearchRequest::from(&sreq)).await? {
        // The server is not trusted to have honoured `exclude_paths`.
        let before = m.files.len();
        m.files.retain(|f| !hidden.contains(f.path.as_ref()));
        let mut cut = m.files.len() != before;
        if used + m.files.len() > max {
            m.files.truncate(max.saturating_sub(used));
            cut = true;
        }
        used += m.files.len();
        m.truncated |= cut;
        if cut {
            m.next_page_token.clear();
        }
        tokens = tokens.saturating_add(m.est_tokens);
        m.scope = Some(resolved(&cx.head, ov, m.scope.as_ref()));
        m.est_tokens = tokens;
        out.push(m);
        if used >= max {
            break;
        }
    }
    Ok(out)
}

fn kinds_of(names: &[String]) -> Result<Vec<Kind>, Status> {
    names
        .iter()
        .map(|n| Kind::from_name(n).ok_or_else(|| bad(format!("unknown kind '{n}'"))))
        .collect()
}

/// Symbols: the overlay's symbols, then the server's of the other files. `outline` of a dirty
/// path is the overlay's alone; of a clean path the server's alone.
pub async fn symbols(
    cx: &Ctx,
    server: Option<&dyn ServerAnswer>,
    req: &ci::SymbolsRequest,
) -> Result<wire::SymbolsResponse, Status> {
    let ov = &cx.overlay;
    let page = !req.page_token.is_empty();
    if page && server.is_none() {
        return Err(bad("INVALID_ARGUMENT: the page token is not valid here"));
    }
    let outline = req.op == ci::SymbolOp::Outline;
    let dirty_outline = outline && req.path.as_ref().is_some_and(|p| ov.hidden().contains(p));
    let mut resp = wire::SymbolsResponse::default();
    let max = limit(req.budget);
    if !page && (!outline || dirty_outline) {
        let filter = Filter::of(&req.scope)?;
        let kinds = kinds_of(&req.kinds)?;
        let q = SymbolQuery {
            op: req.op,
            name: &req.name,
            path: req.path.as_ref(),
            kinds: &kinds,
            def_path: req.def_path.as_ref(),
        };
        let (found, cut) = ov.symbols(&q, &filter)?;
        let r = render::render_symbols(
            &render_ctx(&req.scope.repo, &cx.head),
            budget(req.budget),
            &found,
            cut,
        );
        resp.symbols = r.symbols.into_iter().map(wire_symbol).collect();
        resp.truncated = r.truncated;
        resp.est_tokens = r.est_tokens;
    }
    resp.scope = Some(resolved(&cx.head, ov, None));
    if dirty_outline {
        return Ok(resp);
    }
    let Some(server) = server else {
        return Ok(resp);
    };
    let mut sreq = req.clone();
    sreq.scope = if outline {
        ci::Scope {
            at: ci::At::Rev(hex(&cx.head)),
            ..req.scope.clone()
        }
    } else {
        server_scope(&req.scope, &cx.head, ov.hidden())
    };
    let theirs = server.symbols(wire::SymbolsRequest::from(&sreq)).await?;
    let hidden = hidden_set(ov);
    resp.scope = Some(resolved(&cx.head, ov, theirs.scope.as_ref()));
    resp.est_tokens = resp.est_tokens.saturating_add(theirs.est_tokens);
    resp.truncated |= theirs.truncated;
    resp.next_page_token = theirs.next_page_token;
    for s in theirs.symbols {
        if !outline && hidden.contains(s.path.as_ref()) {
            resp.truncated = true;
            continue;
        }
        if resp.symbols.len() >= max {
            resp.truncated = true;
            resp.next_page_token.clear();
            break;
        }
        resp.symbols.push(s);
    }
    Ok(resp)
}

/// Semantic: the server's chunks with the dirty paths excluded, and, in hybrid mode, the
/// overlay's lexical chunks. The daemon never loads a model.
pub async fn semantic(
    cx: &Ctx,
    server: Option<&dyn ServerAnswer>,
    req: &ci::SemanticRequest,
) -> Result<wire::SemanticResponse, Status> {
    let ov = &cx.overlay;
    let k = match req.k {
        0 => 20,
        n => n.min(ci::MAX_K),
    };
    let k = k as usize;
    let mut resp = wire::SemanticResponse {
        scope: Some(resolved(&cx.head, ov, None)),
        ..wire::SemanticResponse::default()
    };
    if req.mode == ci::SemanticMode::Hybrid {
        let filter = Filter::of(&req.scope)?;
        let (chunks, cut) = ov.lexical_chunks(&req.query, &filter, k);
        let r = render::render_chunks(
            &render_ctx(&req.scope.repo, &cx.head),
            budget(req.budget),
            &chunks,
            cut,
        );
        resp.chunks = r.chunks.into_iter().map(wire_chunk).collect();
        resp.est_tokens = r.est_tokens;
    }
    let Some(server) = server else {
        return Ok(resp);
    };
    let mut sreq = req.clone();
    sreq.scope = server_scope(&req.scope, &cx.head, ov.hidden());
    let theirs = server.semantic(wire::SemanticRequest::from(&sreq)).await?;
    let hidden = hidden_set(ov);
    resp.scope = Some(resolved(&cx.head, ov, theirs.scope.as_ref()));
    resp.model = theirs.model;
    resp.ann_exhausted = theirs.ann_exhausted;
    resp.est_tokens = resp.est_tokens.saturating_add(theirs.est_tokens);
    resp.chunks.extend(
        theirs
            .chunks
            .into_iter()
            .filter(|c| !hidden.contains(c.path.as_ref())),
    );
    resp.chunks.truncate(k);
    Ok(resp)
}

/// Index status: the overlay's row, then the server's rows.
pub async fn index_status(
    cx: &Ctx,
    server: Option<&dyn ServerAnswer>,
    req: &ci::IndexStatusRequest,
) -> Result<wire::IndexStatusResponse, Status> {
    let ov = &cx.overlay;
    let mut resp = wire::IndexStatusResponse {
        scope: Some(resolved(&cx.head, ov, None)),
        indexes: vec![wire::IndexStatus {
            index: "overlay".to_owned(),
            coverage: ov.coverage(),
            lag_seconds: 0,
            generation: ov.generation,
            version: "1".to_owned(),
        }],
    };
    if let Some(server) = server {
        let sreq = ci::IndexStatusRequest {
            scope: server_scope(&req.scope, &cx.head, ov.hidden()),
        };
        let theirs = server
            .index_status(wire::IndexStatusRequest::from(&sreq))
            .await?;
        resp.scope = Some(resolved(&cx.head, ov, theirs.scope.as_ref()));
        resp.indexes.extend(theirs.indexes);
    }
    Ok(resp)
}

// ------------------------------------------------------------------------------- the service

/// The daemon's code intelligence: the overlay cache and the optional server.
pub struct CodeIntel {
    pub cache: Arc<OverlayCache>,
    pub server: Option<Arc<dyn ServerAnswer>>,
}

impl CodeIntel {
    /// A service with no server.
    pub fn new() -> Self {
        Self {
            cache: OverlayCache::from_env(),
            server: None,
        }
    }

    /// Brings the overlay of `ws_id` up to date. The dirty set is `Status` with untracked
    /// files, so staged, unstaged, untracked and renamed paths are all in it.
    async fn context(&self, state: &Arc<State>, ws_id: &str) -> Result<Ctx, Status> {
        let id = parse_id(ws_id)?;
        let m = state.mounted(id)?;
        let st = plugin_api::status(
            state,
            wire::WsStatusRequest {
                ws_id: ws_id.to_owned(),
                paths: Vec::new(),
                untracked: true,
                ignored: false,
            },
        )
        .await?;
        let mut dirty = Vec::new();
        for f in st.files.iter().take(MAX_DIRTY) {
            for raw in [&f.path, &f.orig_path] {
                if !raw.is_empty()
                    && let Ok(p) = RepoPath::new(raw.to_vec())
                {
                    dirty.push(p);
                }
            }
        }
        let root = PathBuf::from(&m.row.ws.mount);
        check_mount_root(&root).map_err(Status::permission_denied)?;
        let _one = self.cache.gate.lock().await;
        let prev = self.cache.get(id);
        let generation = self.cache.builds.fetch_add(1, Ordering::SeqCst) + 1;
        let overlay = tokio::task::spawn_blocking(move || {
            Overlay::build(&root, &dirty, prev.as_deref(), generation)
        })
        .await
        .map_err(|e| Status::internal(format!("overlay build: {e}")))?;
        let overlay = Arc::new(overlay);
        self.cache.put(id, Arc::clone(&overlay));
        Ok(Ctx {
            overlay,
            head: st.head,
        })
    }

    fn server(&self) -> Option<&dyn ServerAnswer> {
        self.server.as_deref()
    }

    pub async fn search(
        &self,
        state: &Arc<State>,
        r: wire::WsSearchRequest,
    ) -> Result<PluginStream<wire::SearchResponse>, Status> {
        let req = ci::SearchRequest::try_from(&r.query.unwrap_or_default())
            .map_err(|e| bad(e.to_string()))?;
        let msgs = if req.scope.at == ci::At::Workspace {
            let cx = self.context(state, &r.ws_id).await?;
            search(&cx, self.server(), &req).await?
        } else {
            self.server
                .as_ref()
                .ok_or_else(no_server)?
                .search(wire::SearchRequest::from(&req))
                .await?
        };
        Ok(Box::pin(futures::stream::iter(msgs.into_iter().map(Ok))))
    }

    pub async fn symbols(
        &self,
        state: &Arc<State>,
        r: wire::WsSymbolsRequest,
    ) -> Result<wire::SymbolsResponse, Status> {
        let req = ci::SymbolsRequest::try_from(&r.query.unwrap_or_default())
            .map_err(|e| bad(e.to_string()))?;
        if req.scope.at == ci::At::Workspace {
            let cx = self.context(state, &r.ws_id).await?;
            symbols(&cx, self.server(), &req).await
        } else {
            self.server
                .as_ref()
                .ok_or_else(no_server)?
                .symbols(wire::SymbolsRequest::from(&req))
                .await
        }
    }

    pub async fn semantic(
        &self,
        state: &Arc<State>,
        r: wire::WsSemanticRequest,
    ) -> Result<wire::SemanticResponse, Status> {
        let req = ci::SemanticRequest::try_from(&r.query.unwrap_or_default())
            .map_err(|e| bad(e.to_string()))?;
        if req.scope.at == ci::At::Workspace {
            let cx = self.context(state, &r.ws_id).await?;
            semantic(&cx, self.server(), &req).await
        } else {
            self.server
                .as_ref()
                .ok_or_else(no_server)?
                .semantic(wire::SemanticRequest::from(&req))
                .await
        }
    }

    pub async fn index_status(
        &self,
        state: &Arc<State>,
        r: wire::WsIndexStatusRequest,
    ) -> Result<wire::IndexStatusResponse, Status> {
        let req = ci::IndexStatusRequest::try_from(&r.query.unwrap_or_default())
            .map_err(|e| bad(e.to_string()))?;
        if req.scope.at == ci::At::Workspace {
            let cx = self.context(state, &r.ws_id).await?;
            index_status(&cx, self.server(), &req).await
        } else {
            self.server
                .as_ref()
                .ok_or_else(no_server)?
                .index_status(wire::IndexStatusRequest::from(&req))
                .await
        }
    }
}

impl Default for CodeIntel {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rp(s: &str) -> RepoPath {
        RepoPath::new(s.as_bytes()).unwrap()
    }

    fn write(root: &Path, rel: &str, body: &[u8]) {
        let p = root.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, body).unwrap();
    }

    fn overlay(files: &[(&str, &str)]) -> (tempfile::TempDir, Overlay) {
        let dir = tempfile::tempdir().unwrap();
        for (p, b) in files {
            write(dir.path(), p, b.as_bytes());
        }
        let paths: Vec<RepoPath> = files.iter().map(|(p, _)| rp(p)).collect();
        let ov = Overlay::build(dir.path(), &paths, None, 1);
        (dir, ov)
    }

    fn lit(p: &str) -> SearchQuery<'_> {
        SearchQuery {
            pattern: p,
            mode: ci::SearchMode::Literal,
            case_sensitive: true,
            context: 0,
        }
    }

    /// A directory link (a junction on Windows without the symlink privilege); false if refused.
    fn link_dir(target: &Path, link: &Path) -> bool {
        #[cfg(unix)]
        let r = std::os::unix::fs::symlink(target, link).is_ok();
        #[cfg(windows)]
        let r = std::os::windows::fs::symlink_dir(target, link).is_ok()
            || std::process::Command::new("cmd")
                .args(["/C", "mklink", "/J"])
                .arg(link)
                .arg(target)
                .output()
                .is_ok_and(|o| o.status.success());
        r
    }

    const RUST: &str = "use crate::util;\n\nfn helper() -> u32 {\n    1\n}\n\nfn caller() -> u32 {\n    helper() + 1\n}\n\nstruct Widget;\n";

    #[test]
    fn build_holds_dirty_files_and_hides_every_dirty_path() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "a.rs", RUST.as_bytes());
        write(dir.path(), "bin.dat", b"\0\x01\x02 binary");
        let dirty = [rp("a.rs"), rp("bin.dat"), rp("gone.rs"), rp("a.rs")];
        let ov = Overlay::build(dir.path(), &dirty, None, 7);
        assert_eq!(ov.len(), 1, "only the text file is searchable");
        assert_eq!(ov.hidden().len(), 3, "every dirty path is hidden, once");
        assert_eq!(ov.generation, 7);
        assert!(ov.coverage() >= 1.0);
    }

    #[test]
    fn file_count_and_heap_bounds_lower_coverage() {
        let dir = tempfile::tempdir().unwrap();
        let mut paths = Vec::new();
        for i in 0..MAX_FILES + 5 {
            let rel = format!("d/f{i}.txt");
            write(dir.path(), &rel, b"x\n");
            paths.push(rp(&rel));
        }
        let ov = Overlay::build(dir.path(), &paths, None, 1);
        assert_eq!(ov.len(), MAX_FILES);
        assert_eq!(ov.unheld(), 5);
        assert!(ov.coverage() < 1.0);

        let big = "word another line of text\n".repeat(36_000); // about 900 KiB
        let dir = tempfile::tempdir().unwrap();
        let mut paths = Vec::new();
        for i in 0..24 {
            let rel = format!("big{i}.txt");
            write(dir.path(), &rel, big.as_bytes());
            paths.push(rp(&rel));
        }
        let ov = Overlay::build(dir.path(), &paths, None, 1);
        assert!(ov.heap() <= MAX_HEAP, "heap {}", ov.heap());
        assert!(ov.len() < 24 && ov.unheld() > 0);
    }

    #[test]
    fn oversized_and_linked_files_are_not_read() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "big.txt", &vec![b'a'; skip::MAX_BYTES + 1]);
        write(dir.path(), "real/inner.txt", b"needle\n");
        let linked = link_dir(&dir.path().join("real"), &dir.path().join("lnk"));
        let dirty = [rp("big.txt"), rp("lnk/inner.txt")];
        let ov = Overlay::build(dir.path(), &dirty, None, 1);
        if linked {
            assert!(
                ov.is_empty(),
                "a big file and a file below a link are skipped"
            );
        } else {
            assert_eq!(
                ov.len(),
                1,
                "no link could be made here; only the size rule applies"
            );
        }
    }

    #[test]
    fn unchanged_files_are_reused_and_changed_ones_re_read() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "a.txt", b"first needle\n");
        let old = SystemTime::now() - Duration::from_secs(3600);
        std::fs::File::options()
            .write(true)
            .open(dir.path().join("a.txt"))
            .unwrap()
            .set_modified(old)
            .unwrap();
        let paths = [rp("a.txt")];
        let one = Overlay::build(dir.path(), &paths, None, 1);
        let two = Overlay::build(dir.path(), &paths, Some(&one), 2);
        assert!(Arc::ptr_eq(
            &one.files[&rp("a.txt")].text,
            &two.files[&rp("a.txt")].text
        ));
        write(dir.path(), "a.txt", b"second needle!\n");
        let three = Overlay::build(dir.path(), &paths, Some(&two), 3);
        assert_eq!(&**three.files[&rp("a.txt")].text, b"second needle!\n");
    }

    #[test]
    fn exact_search_modes_filters_and_context() {
        let (_d, ov) = overlay(&[
            (
                "src/a.rs",
                "let Needle = 1;\nlet needle = 2;\nlet needless = 3;\n",
            ),
            ("docs/b.md", "needle here\n"),
        ]);
        let all = Filter::default();
        let (hits, _) = ov.search(&lit("needle"), &all).unwrap();
        assert_eq!(hits.len(), 2);
        let a = hits.iter().find(|h| h.paths[0] == b"src/a.rs").unwrap();
        assert_eq!(a.lines.len(), 2, "case sensitive: needle and needless");
        assert_eq!(a.lang, "rust");

        let fold = SearchQuery {
            case_sensitive: false,
            ..lit("needle")
        };
        let (hits, _) = ov.search(&fold, &all).unwrap();
        let a = hits.iter().find(|h| h.paths[0] == b"src/a.rs").unwrap();
        assert_eq!(a.lines.len(), 3);

        let word = SearchQuery {
            mode: ci::SearchMode::Word,
            ..lit("needle")
        };
        let (hits, _) = ov.search(&word, &all).unwrap();
        let a = hits.iter().find(|h| h.paths[0] == b"src/a.rs").unwrap();
        assert_eq!(a.lines.len(), 1);

        let rx = SearchQuery {
            mode: ci::SearchMode::Regex,
            ..lit(r"need\w+ =")
        };
        let (hits, _) = ov.search(&rx, &all).unwrap();
        assert_eq!(hits.len(), 1);

        let only_docs = Filter {
            paths: vec![rp("docs")],
            ..Filter::default()
        };
        assert_eq!(ov.search(&lit("needle"), &only_docs).unwrap().0.len(), 1);
        let no_docs = Filter {
            exclude: vec![rp("docs")],
            ..Filter::default()
        };
        assert_eq!(ov.search(&lit("needle"), &no_docs).unwrap().0.len(), 1);
        let rust = Filter {
            langs: vec![Lang::Rust],
            ..Filter::default()
        };
        let (hits, _) = ov.search(&lit("needle"), &rust).unwrap();
        assert_eq!(hits[0].paths[0], b"src/a.rs");

        let ctx = SearchQuery {
            context: 1,
            ..lit("= 2")
        };
        let (hits, _) = ov.search(&ctx, &all).unwrap();
        assert_eq!(hits[0].lines[0].before.len(), 1);
        assert_eq!(hits[0].lines[0].after.len(), 1);

        assert!(ov.search(&lit(""), &all).is_err());
        let bad = SearchQuery {
            mode: ci::SearchMode::Regex,
            ..lit("(unclosed")
        };
        let e = ov.search(&bad, &all).unwrap_err();
        assert_eq!(e.code(), tonic::Code::InvalidArgument);
    }

    #[test]
    fn a_secret_file_is_flagged_and_its_lines_are_redacted_on_render() {
        let (_d, ov) = overlay(&[(".env", "API_KEY=abcdefghijklmnopqrstuvwxyz0123456789\n")]);
        let (hits, _) = ov.search(&lit("API_KEY"), &Filter::default()).unwrap();
        assert!(hits[0].flagged);
        let r = render::render_search(
            &RenderCtx {
                repo: "o/r".into(),
                changeset: vec![1],
            },
            render::Budget::default(),
            &hits,
            false,
        );
        assert!(r.files[0].redacted);
        assert!(r.files[0].matches.iter().all(|m| m.text.is_empty()));
    }

    #[test]
    fn symbols_def_refs_callers_outline_find() {
        let (_d, ov) = overlay(&[("lib.rs", RUST)]);
        let all = Filter::default();
        let q = |op, name| SymbolQuery {
            op,
            name,
            path: None,
            kinds: &[],
            def_path: None,
        };
        let (defs, _) = ov.symbols(&q(ci::SymbolOp::Def, "helper"), &all).unwrap();
        assert_eq!(defs.len(), 1);
        assert_eq!((defs[0].line, defs[0].is_def), (3, true));
        let (pre, _) = ov.symbols(&q(ci::SymbolOp::Def, "hel*"), &all).unwrap();
        assert_eq!(pre.len(), 1);
        let (refs, _) = ov.symbols(&q(ci::SymbolOp::Refs, "helper"), &all).unwrap();
        assert!(refs.iter().any(|s| !s.is_def && s.line == 8));
        let (callers, _) = ov
            .symbols(&q(ci::SymbolOp::Callers, "helper"), &all)
            .unwrap();
        assert_eq!(callers.len(), 1);
        assert_eq!(callers[0].name, "caller");
        let (fuzzy, _) = ov.symbols(&q(ci::SymbolOp::Find, "helpr"), &all).unwrap();
        assert_eq!(fuzzy.len(), 1);
        let path = rp("lib.rs");
        let outline = SymbolQuery {
            path: Some(&path),
            ..q(ci::SymbolOp::Outline, "")
        };
        let (o, _) = ov.symbols(&outline, &all).unwrap();
        assert!(o.iter().any(|s| s.name == "Widget"));
        assert!(ov.symbols(&q(ci::SymbolOp::Def, "a*b"), &all).is_err());
        assert!(ov.symbols(&q(ci::SymbolOp::Refs, "he*"), &all).is_err());
    }

    #[test]
    fn lexical_chunks_score_by_term_share() {
        let (_d, ov) = overlay(&[(
            "lib.rs",
            "fn parse_header() {\n    let header = 1;\n}\n\nfn unrelated() {\n    let x = 2;\n}\n",
        )]);
        let (c, _) = ov.lexical_chunks("parse header", &Filter::default(), 5);
        assert!(!c.is_empty());
        assert_eq!(c[0].sources, vec!["overlay".to_owned()]);
        assert!(c[0].text.windows(6).any(|w| w == b"header"));
        assert!(ov.lexical_chunks("zz", &Filter::default(), 5).0.is_empty());
    }

    #[test]
    fn idle_overlays_are_dropped() {
        let cache = OverlayCache::new(Duration::from_secs(10));
        let (_d, ov) = overlay(&[("a.txt", "x\n")]);
        let id: WorkspaceId = WorkspaceId::new();
        cache.slots().insert(
            id,
            Slot {
                overlay: Arc::new(ov),
                last_used: Instant::now(),
            },
        );
        assert_eq!(cache.sweep(Instant::now() + Duration::from_secs(5)), 0);
        assert!(cache.get(id).is_some());
        assert_eq!(cache.sweep(Instant::now() + Duration::from_secs(11)), 1);
        assert!(cache.is_empty() && cache.get(id).is_none());
    }

    // ------------------------------------------------------------------------ merging

    #[derive(Default)]
    struct Mock {
        seen: Mutex<Vec<wire::Scope>>,
        files: Vec<&'static str>,
    }

    #[async_trait]
    impl ServerAnswer for Mock {
        async fn search(
            &self,
            req: wire::SearchRequest,
        ) -> Result<Vec<wire::SearchResponse>, Status> {
            self.seen.lock().unwrap().push(req.scope.unwrap());
            Ok(vec![wire::SearchResponse {
                scope: Some(wire::ResolvedScope {
                    coverage: 1.0,
                    generation: 9,
                    ..Default::default()
                }),
                files: self
                    .files
                    .iter()
                    .map(|p| wire::FileHit {
                        path: Bytes::from(*p),
                        ..Default::default()
                    })
                    .collect(),
                next_page_token: "next".into(),
                est_tokens: 5,
                ..Default::default()
            }])
        }
        async fn symbols(
            &self,
            req: wire::SymbolsRequest,
        ) -> Result<wire::SymbolsResponse, Status> {
            self.seen.lock().unwrap().push(req.scope.unwrap());
            Ok(wire::SymbolsResponse {
                symbols: self
                    .files
                    .iter()
                    .map(|p| wire::Symbol {
                        name: "helper".into(),
                        path: Bytes::from(*p),
                        ..Default::default()
                    })
                    .collect(),
                ..Default::default()
            })
        }
        async fn semantic(
            &self,
            req: wire::SemanticRequest,
        ) -> Result<wire::SemanticResponse, Status> {
            self.seen.lock().unwrap().push(req.scope.unwrap());
            Ok(wire::SemanticResponse {
                chunks: self
                    .files
                    .iter()
                    .map(|p| wire::Chunk {
                        path: Bytes::from(*p),
                        ..Default::default()
                    })
                    .collect(),
                model: "m".into(),
                ..Default::default()
            })
        }
        async fn index_status(
            &self,
            req: wire::IndexStatusRequest,
        ) -> Result<wire::IndexStatusResponse, Status> {
            self.seen.lock().unwrap().push(req.scope.unwrap());
            Ok(wire::IndexStatusResponse {
                indexes: vec![wire::IndexStatus {
                    index: "files".into(),
                    coverage: 1.0,
                    ..Default::default()
                }],
                ..Default::default()
            })
        }
    }

    fn scope() -> ci::Scope {
        ci::Scope {
            repo: "o/r".into(),
            id_mode: asc_rpc::IdMode::Asc,
            at: ci::At::Workspace,
            stack_diff_only: false,
            paths: Vec::new(),
            exclude_paths: vec!["vendor".into()],
            langs: Vec::new(),
            include_excluded: false,
        }
    }

    fn search_req(pattern: &str) -> ci::SearchRequest {
        ci::SearchRequest {
            scope: scope(),
            pattern: pattern.into(),
            mode: ci::SearchMode::Literal,
            case_sensitive: true,
            context_lines: 0,
            budget: ci::Budget::default(),
            page_token: String::new(),
        }
    }

    fn cx(files: &[(&str, &str)]) -> (tempfile::TempDir, Ctx) {
        let (d, ov) = overlay(files);
        (
            d,
            Ctx {
                overlay: Arc::new(ov),
                head: Bytes::from_static(&[0xab, 0xcd]),
            },
        )
    }

    #[tokio::test]
    async fn search_merges_overlay_first_and_drops_stale_server_hits() {
        let (_d, cx) = cx(&[("src/dirty.rs", "fn needle() {}\n")]);
        let mock = Mock {
            files: vec!["src/dirty.rs", "src/clean.rs"],
            ..Mock::default()
        };
        let out = search(&cx, Some(&mock), &search_req("needle"))
            .await
            .unwrap();
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].files[0].path.as_ref(), b"src/dirty.rs");
        assert_eq!(out[0].files[0].matches[0].line, 1);
        // The server's copy of the dirty path is dropped even though it was returned.
        assert_eq!(out[1].files.len(), 1);
        assert_eq!(out[1].files[0].path.as_ref(), b"src/clean.rs");
        assert!(out[1].truncated);
        let sent = mock.seen.lock().unwrap()[0].clone();
        assert_eq!(sent.at, Some(wire::scope::At::Rev("abcd".into())));
        assert_eq!(
            sent.exclude_paths,
            vec!["vendor".to_owned(), "src/dirty.rs".to_owned()]
        );
        assert!((out[1].scope.as_ref().unwrap().coverage - 1.0).abs() < f32::EPSILON);
    }

    #[tokio::test]
    async fn search_without_a_server_answers_the_overlay_with_zero_coverage() {
        let (_d, cx) = cx(&[("a.txt", "needle\n")]);
        let out = search(&cx, None, &search_req("needle")).await.unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].files.len(), 1);
        let scope = out[0].scope.as_ref().unwrap();
        assert_eq!(scope.coverage, 0.0);
        assert_eq!(scope.changeset.as_ref(), &[0xab, 0xcd]);
    }

    #[tokio::test]
    async fn a_page_token_goes_to_the_server_alone() {
        let (_d, cx) = cx(&[("a.txt", "needle\n")]);
        let mock = Mock {
            files: vec!["b.txt"],
            ..Mock::default()
        };
        let mut req = search_req("needle");
        req.page_token = "tok".into();
        let out = search(&cx, Some(&mock), &req).await.unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].files[0].path.as_ref(), b"b.txt");
        let e = search(&cx, None, &req).await.unwrap_err();
        assert_eq!(e.code(), tonic::Code::InvalidArgument);
    }

    #[tokio::test]
    async fn result_limit_caps_the_merged_answer() {
        let (_d, cx) = cx(&[("a.txt", "needle\n")]);
        let mock = Mock {
            files: vec!["b.txt", "c.txt", "d.txt"],
            ..Mock::default()
        };
        let mut req = search_req("needle");
        req.budget.max_results = 2;
        let out = search(&cx, Some(&mock), &req).await.unwrap();
        let n: usize = out.iter().map(|m| m.files.len()).sum();
        assert_eq!(n, 2);
        assert!(out.last().unwrap().truncated);
        assert!(out.last().unwrap().next_page_token.is_empty());
    }

    #[tokio::test]
    async fn symbols_merge_and_outline_routing() {
        let (_d, cx) = cx(&[("lib.rs", RUST)]);
        let mock = Mock {
            files: vec!["lib.rs", "other.rs"],
            ..Mock::default()
        };
        let req = ci::SymbolsRequest {
            scope: scope(),
            op: ci::SymbolOp::Def,
            name: "helper".into(),
            path: None,
            kinds: Vec::new(),
            def_path: None,
            def_line: 0,
            budget: ci::Budget::default(),
            page_token: String::new(),
        };
        let r = symbols(&cx, Some(&mock), &req).await.unwrap();
        let paths: Vec<_> = r.symbols.iter().map(|s| s.path.to_vec()).collect();
        assert_eq!(paths, vec![b"lib.rs".to_vec(), b"other.rs".to_vec()]);
        assert!(r.truncated, "the stale server symbol was dropped");

        // Outline of a dirty path is the overlay's alone, of a clean path the server's alone.
        let dirty = ci::SymbolsRequest {
            op: ci::SymbolOp::Outline,
            name: String::new(),
            path: Some(rp("lib.rs")),
            ..req.clone()
        };
        let before = mock.seen.lock().unwrap().len();
        let r = symbols(&cx, Some(&mock), &dirty).await.unwrap();
        assert!(r.symbols.iter().any(|s| s.name == "Widget"));
        assert_eq!(
            mock.seen.lock().unwrap().len(),
            before,
            "the server was not asked"
        );
        let clean = ci::SymbolsRequest {
            path: Some(rp("clean.rs")),
            ..dirty
        };
        symbols(&cx, Some(&mock), &clean).await.unwrap();
        let sent = mock.seen.lock().unwrap().last().cloned().unwrap();
        assert_eq!(
            sent.exclude_paths,
            vec!["vendor".to_owned()],
            "outline excludes nothing"
        );
    }

    #[tokio::test]
    async fn symbols_of_a_flagged_overlay_file_show_no_secret_text() {
        // Flagged by content (a key literal in the doc comment) and by path (`.env.rs`).
        const BODY: &str = "/// Uses AKIAIOSFODNN7EXAMPLE as the key.
fn leakyname(leakyarg: u8) -> u8 {
    leakyname(1)
}

struct LeakyHolder;

impl LeakyHolder {
    fn leakyinner(&self) {
        leakyname(2);
    }
}
";
        let (_d, cx) = cx(&[("flagged.rs", BODY), (".env.rs", BODY), ("clean.rs", RUST)]);
        assert!(cx.overlay.files.values().filter(|e| e.flagged).count() == 2);
        let base = ci::SymbolsRequest {
            scope: scope(),
            op: ci::SymbolOp::Def,
            name: String::new(),
            path: None,
            kinds: Vec::new(),
            def_path: None,
            def_line: 0,
            budget: ci::Budget::default(),
            page_token: String::new(),
        };
        let mut shown = 0;
        for (op, name) in [
            (ci::SymbolOp::Def, "leakyname"),
            (ci::SymbolOp::Refs, "leakyname"),
            (ci::SymbolOp::Callers, "leakyname"),
            (ci::SymbolOp::Find, "leakynam"),
            (ci::SymbolOp::Def, "Leaky*"),
        ] {
            let req = ci::SymbolsRequest {
                op,
                name: name.into(),
                ..base.clone()
            };
            let r = symbols(&cx, None, &req).await.unwrap();
            for s in &r.symbols {
                shown += 1;
                assert!(
                    s.name.is_empty() && s.container.is_empty(),
                    "{op:?} {name}: {s:?}"
                );
                assert!(s.signature.is_empty() && s.doc.is_empty(), "{s:?}");
            }
            let dump = format!("{r:?}");
            for t in ["AKIA", "leaky", "Leaky"] {
                assert!(!dump.contains(t), "{op:?} {name}: {dump}");
            }
        }
        assert!(
            shown >= 10,
            "the queries found the flagged symbols: {shown}"
        );
        for file in ["flagged.rs", ".env.rs"] {
            let req = ci::SymbolsRequest {
                op: ci::SymbolOp::Outline,
                path: Some(rp(file)),
                ..base.clone()
            };
            let r = symbols(&cx, None, &req).await.unwrap();
            assert!(!r.symbols.is_empty());
            assert!(
                r.symbols
                    .iter()
                    .all(|s| { s.name.is_empty() && s.signature.is_empty() && s.doc.is_empty() })
            );
            let dump = format!("{r:?}");
            assert!(
                !["AKIA", "leaky", "Leaky"].iter().any(|t| dump.contains(t)),
                "{dump}"
            );
        }
        // A clean file still shows its symbols.
        let req = ci::SymbolsRequest {
            name: "helper".into(),
            ..base
        };
        let r = symbols(&cx, None, &req).await.unwrap();
        assert!(
            r.symbols
                .iter()
                .any(|s| s.name == "helper" && !s.signature.is_empty())
        );
    }

    #[tokio::test]
    async fn semantic_merges_lexical_overlay_chunks_with_the_servers() {
        let (_d, cx) = cx(&[("lib.rs", "fn parse_header() {\n    let header = 1;\n}\n")]);
        let mock = Mock {
            files: vec!["lib.rs", "far.rs"],
            ..Mock::default()
        };
        let req = ci::SemanticRequest {
            scope: scope(),
            query: "parse header".into(),
            mode: ci::SemanticMode::Hybrid,
            k: 10,
            budget: ci::Budget::default(),
            rerank: false,
        };
        let r = semantic(&cx, Some(&mock), &req).await.unwrap();
        assert_eq!(r.model, "m");
        let paths: Vec<_> = r.chunks.iter().map(|c| c.path.to_vec()).collect();
        let own = paths.iter().filter(|p| p.as_slice() == b"lib.rs").count();
        assert_eq!(own, 1, "{paths:?}");
        assert!(paths.contains(&b"far.rs".to_vec()));
        let vector = ci::SemanticRequest {
            mode: ci::SemanticMode::Vector,
            ..req
        };
        let r = semantic(&cx, None, &vector).await.unwrap();
        assert!(r.chunks.is_empty(), "the daemon has no vectors");
    }

    #[tokio::test]
    async fn index_status_lists_the_overlay_then_the_servers_indexes() {
        let (_d, cx) = cx(&[("a.txt", "x\n")]);
        let req = ci::IndexStatusRequest { scope: scope() };
        let r = index_status(&cx, None, &req).await.unwrap();
        assert_eq!(r.indexes[0].index, "overlay");
        assert_eq!(r.indexes[0].generation, 1);
        let r = index_status(&cx, Some(&Mock::default()), &req)
            .await
            .unwrap();
        let names: Vec<_> = r.indexes.iter().map(|i| i.index.as_str()).collect();
        assert_eq!(names, vec!["overlay", "files"]);
    }
}
