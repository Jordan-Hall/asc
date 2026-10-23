//! Answering code intelligence queries from the local index (SPEC §23.2.5).
//!
//! [`Open`] is one revision opened for reading: its segments are unpacked from hash-verified
//! packs and mmapped by tantivy, and closed again after [`READER_IDLE`] without a query.
//! [`LocalAnswer`] is what the daemon asks (`ServerAnswer`) when no asc server answers; the
//! answers have the server's shape, with `coverage` saying how much of the revision the local
//! index holds. [`LocalSource`] is the same index offered to `grep <rev>` as a candidate
//! prefilter.
//!
//! Exact search takes the trigram candidates of the index and verifies each one against the
//! file's bytes, which are re-read from `store/` and `cache/` and checked against their
//! `FileId`; the index only ever removes files, so an answer is never wrong, only incomplete.
//! No query loads a model: `semantic` has no vectors here and answers with `coverage = 0`.

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime};

use async_trait::async_trait;
use bytes::Bytes;
use asc_codeintel::index::schema::{FilesSchema, IndexKind};
use asc_codeintel::index::segments::{Snapshot, unpack_and_open};
use asc_codeintel::lang::Lang;
use asc_codeintel::plan::{self, Plan};
use asc_codeintel::render::{self, RawFileHit, RawLine, RenderCtx};
use asc_codeintel::search::{
    MAX_LINE_BYTES, MAX_LINES_PER_FILE, MAX_SPANS_PER_LINE, REGEX_SIZE_LIMIT, SCAN_SCOPE_FILES,
    STREAM_BYTES, WINDOW,
};
use asc_codeintel::skip::Flags;
use asc_codeintel::symbols::{self, FileSource, NoProjects, SymbolEngine, SymbolError};
use asc_codeintel::tags::Kind;
use asc_core::id::FileId;
use asc_core::path::RepoPath;
use asc_rpc::codeintel as ci;
use asc_rpc::server::wire;
use asc_store::key::ObjKey;
use asc_store::objects::Objects;
use regex::bytes::RegexBuilder;
use roaring::RoaringBitmap;
use tantivy::Term;
use tantivy::query::{BooleanQuery, EmptyQuery, Query, TermQuery};
use tantivy::schema::IndexRecordOption;
use tonic::Status;

use super::store::{CiRoot, RepoStore, RevRec, State};
use super::{READER_IDLE, hex, unhex32};

const MAX_PLAN_DEPTH: usize = 64;
/// Most bytes of the decoded pack.
const MAX_RAW_PACK: u64 = 1 << 28;

fn bad(m: impl Into<String>) -> Status {
    Status::invalid_argument(m.into())
}

// -------------------------------------------------------------------------------- Open

/// One revision of one repository, opened for reading.
pub struct Open {
    pub repo_id: String,
    pub changeset: [u8; 32],
    pub rev: RevRec,
    pub st: State,
    pub snap: Snapshot,
    /// The ords the revision's indexed segments hold.
    indexed: RoaringBitmap,
    /// Paths of each ord in this revision.
    paths: BTreeMap<u32, Vec<String>>,
    by_path: HashMap<Vec<u8>, u32>,
    ids: HashMap<u32, FileId>,
    recs: HashMap<u32, (u8, Option<Lang>)>,
    /// Files of the revision's tree the index cannot narrow (skip rules).
    unindexed: Vec<String>,
    stamp: (Option<SystemTime>, u64),
}

fn stamp_of(store: &RepoStore) -> (Option<SystemTime>, u64) {
    std::fs::symlink_metadata(store.path().join("state.json"))
        .map(|m| (m.modified().ok(), m.len()))
        .unwrap_or((None, 0))
}

impl Open {
    /// Opens `changeset` of `store`; `None` when the revision is absent or its record or one of
    /// its segments fails validation.
    pub fn load(store: &RepoStore, changeset: &[u8; 32]) -> Option<Open> {
        let cs = hex(changeset);
        let st = store.state().ok()?;
        let rev = store.rev(&cs, &st)?;
        let mut segments = Vec::new();
        let mut indexed = RoaringBitmap::new();
        for h in &rev.segs {
            let rec = st.segs.get(h)?;
            let hash = unhex32(h)?;
            let stored = Bytes::from(store.read_seg(h).ok()?);
            let raw = asc_store::codec::decode_stored_capped(&stored, MAX_RAW_PACK).ok()?;
            if *blake3::hash(&raw).as_bytes() != hash {
                return None;
            }
            let dest = store.open_path(h).ok()?;
            // A fresh unpack each time: the directory is a cache and is not trusted.
            let _ = std::fs::remove_dir_all(&dest);
            let seg = unpack_and_open(IndexKind::Files, hash, &raw, &dest).ok()?;
            segments.push(seg);
            indexed |= rec.bitmap();
        }
        let mut paths: BTreeMap<u32, Vec<String>> = BTreeMap::new();
        let mut by_path = HashMap::new();
        for (p, o) in &rev.files {
            paths.entry(*o).or_default().push(p.clone());
            by_path.insert(p.as_bytes().to_vec(), *o);
        }
        let mut ids = HashMap::new();
        let mut recs = HashMap::new();
        for (id, r) in &st.files {
            if paths.contains_key(&r.ord) {
                ids.insert(r.ord, FileId::from_bytes(unhex32(id)?));
                recs.insert(
                    r.ord,
                    (r.flags, r.lang.as_deref().and_then(Lang::from_name)),
                );
            }
        }
        let unindexed = rev
            .files
            .iter()
            .filter(|(_, o)| !indexed.contains(*o))
            .map(|(p, _)| p.clone())
            .collect();
        let snap = Snapshot {
            generation: 1,
            kind: IndexKind::Files,
            indexed: indexed.clone(),
            segments,
        };
        Some(Open {
            repo_id: store.id().to_owned(),
            changeset: *changeset,
            rev,
            st,
            snap,
            indexed,
            paths,
            by_path,
            ids,
            recs,
            unindexed,
            stamp: stamp_of(store),
        })
    }

    pub fn coverage(&self) -> f32 {
        self.rev.coverage()
    }

    fn flags(&self, ord: u32) -> Flags {
        self.recs
            .get(&ord)
            .and_then(|(f, _)| Flags::from_bits(*f))
            .unwrap_or_default()
    }

    fn lang(&self, ord: u32) -> Option<Lang> {
        self.recs.get(&ord).and_then(|(_, l)| *l)
    }

    /// The ords of the revision that `f` admits and the index holds.
    fn scope(&self, f: &Filter) -> RoaringBitmap {
        let mut b = RoaringBitmap::new();
        for (ord, ps) in &self.paths {
            if !self.indexed.contains(*ord) {
                continue;
            }
            if ps.iter().any(|p| f.admits(p.as_bytes(), self.lang(*ord))) {
                b.insert(*ord);
            }
        }
        b
    }

    /// Candidate paths of a plain-text pattern for `grep` (see [`LocalSource`]): every file of
    /// the revision that may contain it, including the files the index cannot rule out.
    pub fn candidates(&self, plan: &Plan) -> Option<Vec<String>> {
        let q = plan_query(plan, &FilesSchema::build(), 0)?;
        let hits = self.snap.ords_for_query(&*q).ok()?;
        let mut out: Vec<String> = self.unindexed.clone();
        for o in &hits {
            if let Some(ps) = self.paths.get(&o) {
                out.extend(ps.iter().cloned());
            }
        }
        Some(out)
    }
}

// --------------------------------------------------------------------------------- Filter

fn under(p: &[u8], dir: &[u8]) -> bool {
    p.starts_with(dir)
        && (p.len() == dir.len() || dir.last() == Some(&b'/') || p[dir.len()] == b'/')
}

/// The path and language filters of a scope.
#[derive(Default)]
pub struct Filter {
    paths: Vec<Vec<u8>>,
    exclude: Vec<Vec<u8>>,
    langs: Vec<Lang>,
}

impl Filter {
    pub fn of(scope: &ci::Scope) -> Result<Self, Status> {
        let paths = |v: &[String], what: &str| -> Result<Vec<Vec<u8>>, Status> {
            v.iter()
                .map(|s| {
                    RepoPath::new(s.as_bytes().to_vec())
                        .map(|p| p.as_bytes().to_vec())
                        .map_err(|_| bad(format!("{what}: not a repository path")))
                })
                .collect()
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

    fn admits(&self, p: &[u8], lang: Option<Lang>) -> bool {
        (self.paths.is_empty() || self.paths.iter().any(|d| under(p, d)))
            && !self.exclude.iter().any(|d| under(p, d))
            && (self.langs.is_empty() || lang.is_some_and(|l| self.langs.contains(&l)))
    }
}

// -------------------------------------------------------------------------------- plans

fn term(f: &FilesSchema, g: u32) -> Box<dyn Query> {
    Box::new(TermQuery::new(
        Term::from_field_u64(f.tri, u64::from(g)),
        IndexRecordOption::Basic,
    ))
}

/// The tantivy query of a plan; `None` means no restriction (a superset is always safe).
pub fn plan_query(p: &Plan, f: &FilesSchema, depth: usize) -> Option<Box<dyn Query>> {
    if depth > MAX_PLAN_DEPTH {
        return None;
    }
    match p {
        Plan::All => None,
        Plan::None => Some(Box::new(EmptyQuery)),
        Plan::Gram(g) => Some(term(f, *g)),
        Plan::And(v) => {
            let mut parts: Vec<_> = v
                .iter()
                .filter_map(|c| plan_query(c, f, depth + 1))
                .collect();
            match parts.len() {
                0 => None,
                1 => parts.pop(),
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
                1 => parts.pop(),
                _ => Some(Box::new(BooleanQuery::union(parts))),
            }
        }
    }
}

// ------------------------------------------------------------------------------ files

/// File bytes for the symbol engine and the verify step: local objects only, checked against
/// the `FileId`. Call from a blocking thread.
pub struct LocalFiles {
    open: Arc<Open>,
    objects: Arc<dyn Objects>,
    handle: tokio::runtime::Handle,
}

impl LocalFiles {
    fn read(&self, ord: u32) -> Option<Arc<[u8]>> {
        let id = *self.open.ids.get(&ord)?;
        let got = self
            .handle
            .block_on(self.objects.get(&ObjKey::File(id)))
            .ok()??;
        (FileId::of(&got) == id).then(|| Arc::from(&got[..]))
    }
}

impl FileSource for LocalFiles {
    fn paths(&self, ord: u32) -> Vec<Vec<u8>> {
        self.open
            .paths
            .get(&ord)
            .map(|v| v.iter().map(|p| p.as_bytes().to_vec()).collect())
            .unwrap_or_default()
    }

    fn lang(&self, ord: u32) -> Option<Lang> {
        self.open.lang(ord)
    }

    fn bytes(&self, ord: u32) -> symbols::Result<Option<Arc<[u8]>>> {
        Ok(self.read(ord))
    }

    fn read_path(&self, path: &[u8]) -> symbols::Result<Option<Arc<[u8]>>> {
        Ok(self.open.by_path.get(path).and_then(|o| self.read(*o)))
    }

    fn untagged(&self, scope: &RoaringBitmap, limit: usize) -> Vec<u32> {
        scope
            .iter()
            .filter(|o| self.open.lang(*o).is_none())
            .take(limit)
            .collect()
    }

    fn secret(&self, ord: u32) -> Option<bool> {
        self.open
            .recs
            .contains_key(&ord)
            .then(|| self.open.flags(ord).contains(Flags::SECRET))
    }
}

// ---------------------------------------------------------------------------- the scan

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

/// Exact search over an opened revision.
pub fn search(
    files: &LocalFiles,
    req: &ci::SearchRequest,
    f: &Filter,
) -> Result<(Vec<RawFileHit>, bool), Status> {
    let open = &files.open;
    if req.pattern.is_empty() || req.pattern.len() > plan::MAX_PATTERN {
        return Err(bad("INVALID_ARGUMENT: pattern is empty or too long"));
    }
    let escaped = regex::escape(&req.pattern);
    let source = match req.mode {
        ci::SearchMode::Literal => escaped,
        ci::SearchMode::Word => format!(r"\b(?:{escaped})\b"),
        ci::SearchMode::Regex => req.pattern.clone(),
    };
    let re = RegexBuilder::new(&source)
        .case_insensitive(!req.case_sensitive)
        .size_limit(REGEX_SIZE_LIMIT)
        .dfa_size_limit(REGEX_SIZE_LIMIT)
        .build()
        .map_err(|e| bad(format!("INVALID_ARGUMENT: bad pattern: {e}")))?;
    let plan = match req.mode {
        ci::SearchMode::Literal => plan::plan_literal(&req.pattern, !req.case_sensitive),
        _ => plan::plan_regex(&source, !req.case_sensitive),
    }
    .map_err(|e| bad(format!("INVALID_ARGUMENT: {e}")))?;
    let mut scope = open.scope(f);
    let restricted = plan_query(&plan, &FilesSchema::build(), 0);
    match restricted {
        Some(q) => {
            let hits = open
                .snap
                .ords_for_query(&*q)
                .map_err(|e| Status::internal(e.to_string()))?;
            scope &= &hits;
        }
        None if f.paths.is_empty() && scope.len() > SCAN_SCOPE_FILES => {
            return Err(bad("INVALID_ARGUMENT: pattern too broad"));
        }
        None => {}
    }
    // Path order, one hit per file with every path it sits at.
    let mut order: Vec<(&str, u32)> = Vec::new();
    for o in &scope {
        if let Some(ps) = open.paths.get(&o)
            && let Some(first) = ps
                .iter()
                .filter(|p| f.admits(p.as_bytes(), open.lang(o)))
                .min()
        {
            order.push((first.as_str(), o));
        }
    }
    order.sort();
    let mut hits = Vec::new();
    let mut truncated = false;
    let mut read = 0u64;
    for (_, ord) in order {
        if read > STREAM_BYTES {
            truncated = true;
            break;
        }
        let Some(bytes) = files.read(ord) else {
            continue;
        };
        read += bytes.len() as u64;
        if !re.is_match(&bytes) {
            continue;
        }
        let (lines, more) = scan(&re, &bytes, req.context_lines as usize);
        if lines.is_empty() {
            continue;
        }
        truncated |= more;
        if hits.len() >= WINDOW {
            truncated = true;
            break;
        }
        let mut ps: Vec<&String> = open.paths[&ord]
            .iter()
            .filter(|p| f.admits(p.as_bytes(), open.lang(ord)))
            .collect();
        ps.sort();
        hits.push(RawFileHit {
            file: open.ids[&ord],
            lang: open
                .lang(ord)
                .map(|l| l.name().to_owned())
                .unwrap_or_default(),
            paths: ps.into_iter().map(|p| p.as_bytes().to_vec()).collect(),
            lines,
            flagged: open.flags(ord).contains(Flags::SECRET),
        });
    }
    Ok((hits, truncated))
}

fn kinds_of(names: &[String]) -> Result<Vec<Kind>, Status> {
    names
        .iter()
        .map(|n| Kind::from_name(n).ok_or_else(|| bad(format!("unknown kind '{n}'"))))
        .collect()
}

fn sym_status(e: SymbolError) -> Status {
    match e {
        SymbolError::InvalidName(m) => bad(format!("INVALID_ARGUMENT: invalid name: {m}")),
        other => Status::internal(other.to_string()),
    }
}

/// Symbol queries over an opened revision.
pub fn symbols(
    files: &LocalFiles,
    req: &ci::SymbolsRequest,
    f: &Filter,
) -> Result<symbols::Symbols, Status> {
    let open = &files.open;
    let kinds = kinds_of(&req.kinds)?;
    if req.op == ci::SymbolOp::Outline {
        let path = req
            .path
            .as_ref()
            .ok_or_else(|| bad("outline needs a path"))?;
        let Some(&ord) = open.by_path.get(path.as_bytes()) else {
            return Ok(symbols::Symbols::default());
        };
        let (Some(lang), Some(bytes)) = (open.lang(ord), files.read(ord)) else {
            return Ok(symbols::Symbols::default());
        };
        let secret = Some(open.flags(ord).contains(Flags::SECRET));
        return Ok(
            symbols::outline(path.as_bytes(), secret, lang, &bytes, &kinds, 0).unwrap_or_default(),
        );
    }
    let scope = open.scope(f);
    let engine = SymbolEngine::new(&open.snap, files, &NoProjects);
    let q = symbols::SymbolQuery {
        name: req.name.clone(),
        kinds,
        at: req
            .def_path
            .as_ref()
            .map(|p| (p.as_bytes().to_vec(), req.def_line)),
        limit: 0,
    };
    match req.op {
        ci::SymbolOp::Def => engine.def(&scope, &q),
        ci::SymbolOp::Find => engine.find(&scope, &q),
        ci::SymbolOp::Refs => engine.refs(&scope, &q),
        ci::SymbolOp::Callers => engine.callers(&scope, &q),
        ci::SymbolOp::Outline => Ok(symbols::Symbols::default()),
    }
    .map_err(sym_status)
}

// -------------------------------------------------------------------------------- wire

fn resolved(cs: &[u8], generation: u64, coverage: f32) -> wire::ResolvedScope {
    wire::ResolvedScope {
        changeset: Bytes::copy_from_slice(cs),
        root: Bytes::new(),
        generation,
        coverage,
    }
}

fn budget(b: ci::Budget) -> render::Budget {
    render::Budget {
        max_results: b.max_results,
        max_bytes: b.max_bytes,
        max_tokens: b.max_tokens,
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

fn no_server() -> Status {
    Status::failed_precondition(
        "no asc server answers for this workspace; only workspace scope is served locally",
    )
}

/// The revision a scope names: the bytes to cite (a changeset id, or a git commit id the
/// workspace reports for `HEAD`) and the hex to look up. Only full hex ids resolve offline; a
/// branch name or an abbreviation needs the repository's refs, which this index does not hold.
fn target_of(scope: &ci::Scope) -> Result<(Vec<u8>, String), Status> {
    let ci::At::Rev(s) = &scope.at else {
        return Err(no_server());
    };
    let ok = (s.len() == 64 || s.len() == 40)
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
    if !ok {
        return Err(no_server());
    }
    let mut cited = Vec::with_capacity(s.len() / 2);
    for i in 0..s.len() / 2 {
        cited.push(u8::from_str_radix(&s[2 * i..2 * i + 2], 16).map_err(|_| no_server())?);
    }
    Ok((cited, s.clone()))
}

// ------------------------------------------------------------------------- LocalAnswer

struct Cached {
    open: Arc<Open>,
    used: Instant,
}

/// Answers from `$ASC_HOME/ci` for revisions the helper indexed.
pub struct LocalAnswer {
    ci: CiRoot,
    objects: Arc<dyn Objects>,
    cache: Mutex<HashMap<(String, [u8; 32]), Cached>>,
    idle: Duration,
}

impl LocalAnswer {
    pub fn new(ci: CiRoot, objects: Arc<dyn Objects>) -> Self {
        Self {
            ci,
            objects,
            cache: Mutex::new(HashMap::new()),
            idle: READER_IDLE,
        }
    }

    /// Readers close after `idle` without a query (tests).
    #[must_use]
    pub fn with_idle(mut self, idle: Duration) -> Self {
        self.idle = idle;
        self
    }

    /// Readers currently open.
    pub fn open_readers(&self) -> usize {
        self.cache.lock().map(|c| c.len()).unwrap_or(0)
    }

    /// Closes the readers idle for too long.
    pub fn sweep(&self) -> usize {
        let Ok(mut c) = self.cache.lock() else {
            return 0;
        };
        let before = c.len();
        c.retain(|_, v| v.used.elapsed() < self.idle);
        before - c.len()
    }

    /// The opened revision named by `name` (a changeset id or the git commit id of one), from
    /// whichever repository holds it.
    fn open(&self, name: &str) -> Option<Arc<Open>> {
        self.sweep();
        for store in self.ci.repos() {
            let key = if name.len() == 40 {
                store.resolve_alias(name)?
            } else if store.has_rev(name) {
                name.to_owned()
            } else {
                continue;
            };
            let cs = unhex32(&key)?;
            let ck = (store.id().to_owned(), cs);
            if let Ok(mut c) = self.cache.lock()
                && let Some(hit) = c.get_mut(&ck)
                && hit.open.stamp == stamp_of(&store)
            {
                hit.used = Instant::now();
                store.touch_rev(&key);
                return Some(Arc::clone(&hit.open));
            }
            let Some(open) = Open::load(&store, &cs) else {
                continue;
            };
            let open = Arc::new(open);
            store.touch_rev(&key);
            if let Ok(mut c) = self.cache.lock() {
                c.insert(
                    ck,
                    Cached {
                        open: Arc::clone(&open),
                        used: Instant::now(),
                    },
                );
            }
            return Some(open);
        }
        None
    }

    fn files(&self, open: Arc<Open>) -> LocalFiles {
        LocalFiles {
            open,
            objects: Arc::clone(&self.objects),
            handle: tokio::runtime::Handle::current(),
        }
    }
}

fn join_err(e: tokio::task::JoinError) -> Status {
    Status::internal(format!("local index task: {e}"))
}

#[async_trait]
impl crate::daemon::codeintel_overlay::ServerAnswer for LocalAnswer {
    async fn search(&self, req: wire::SearchRequest) -> Result<Vec<wire::SearchResponse>, Status> {
        let req = ci::SearchRequest::try_from(&req).map_err(|e| bad(e.to_string()))?;
        if !req.page_token.is_empty() {
            return Err(bad("INVALID_ARGUMENT: the page token is not valid here"));
        }
        let (cited, name) = target_of(&req.scope)?;
        let f = Filter::of(&req.scope)?;
        let Some(open) = self.open(&name) else {
            return Ok(vec![wire::SearchResponse {
                scope: Some(resolved(&cited, 0, 0.0)),
                ..wire::SearchResponse::default()
            }]);
        };
        let coverage = open.coverage();
        let files = self.files(open);
        let repo = req.scope.repo.clone();
        let b = req.budget;
        let (hits, cut) = tokio::task::spawn_blocking(move || search(&files, &req, &f))
            .await
            .map_err(join_err)??;
        let r = render::render_search(
            &RenderCtx {
                repo,
                changeset: cited.clone(),
            },
            budget(b),
            &hits,
            cut,
        );
        Ok(vec![wire::SearchResponse {
            scope: Some(resolved(&cited, 1, coverage)),
            files: r.files.into_iter().map(wire_file).collect(),
            truncated: r.truncated,
            next_page_token: String::new(),
            est_tokens: r.est_tokens,
        }])
    }

    async fn symbols(&self, req: wire::SymbolsRequest) -> Result<wire::SymbolsResponse, Status> {
        let req = ci::SymbolsRequest::try_from(&req).map_err(|e| bad(e.to_string()))?;
        if !req.page_token.is_empty() {
            return Err(bad("INVALID_ARGUMENT: the page token is not valid here"));
        }
        let (cited, name) = target_of(&req.scope)?;
        let f = Filter::of(&req.scope)?;
        let Some(open) = self.open(&name) else {
            return Ok(wire::SymbolsResponse {
                scope: Some(resolved(&cited, 0, 0.0)),
                ..wire::SymbolsResponse::default()
            });
        };
        let coverage = open.coverage();
        let files = self.files(open);
        let repo = req.scope.repo.clone();
        let b = req.budget;
        let found = tokio::task::spawn_blocking(move || symbols(&files, &req, &f))
            .await
            .map_err(join_err)??;
        let r = render::render_symbols(
            &RenderCtx {
                repo,
                changeset: cited.clone(),
            },
            budget(b),
            &found.symbols,
            found.truncated,
        );
        Ok(wire::SymbolsResponse {
            scope: Some(resolved(&cited, 1, coverage)),
            symbols: r.symbols.into_iter().map(wire_symbol).collect(),
            truncated: r.truncated,
            est_tokens: r.est_tokens,
            next_page_token: String::new(),
        })
    }

    async fn semantic(&self, req: wire::SemanticRequest) -> Result<wire::SemanticResponse, Status> {
        let req = ci::SemanticRequest::try_from(&req).map_err(|e| bad(e.to_string()))?;
        let (cited, _) = target_of(&req.scope)?;
        // No vectors, no model: the local answer is empty and says so.
        Ok(wire::SemanticResponse {
            scope: Some(resolved(&cited, 0, 0.0)),
            ..wire::SemanticResponse::default()
        })
    }

    async fn index_status(
        &self,
        req: wire::IndexStatusRequest,
    ) -> Result<wire::IndexStatusResponse, Status> {
        let req = ci::IndexStatusRequest::try_from(&req).map_err(|e| bad(e.to_string()))?;
        let (cited, name) = target_of(&req.scope)?;
        let Some(open) = self.open(&name) else {
            return Ok(wire::IndexStatusResponse {
                scope: Some(resolved(&cited, 0, 0.0)),
                indexes: Vec::new(),
            });
        };
        let coverage = open.coverage();
        Ok(wire::IndexStatusResponse {
            scope: Some(resolved(&cited, 1, coverage)),
            indexes: vec![wire::IndexStatus {
                index: "local".to_owned(),
                coverage,
                lag_seconds: 0,
                generation: 1,
                version: "1".to_owned(),
            }],
        })
    }
}

// --------------------------------------------------------------------------- LocalSource

/// Offers the local index to `asc grep <rev>` as a candidate prefilter. It answers only for a
/// plain fixed-string pattern (the same meaning in every regex dialect), and only from a revision
/// the helper indexed; `coverage` below 1 makes `grep` ignore the answer.
pub struct LocalSource {
    ci: CiRoot,
}

impl LocalSource {
    pub fn new(ci: CiRoot) -> Self {
        Self { ci }
    }
}

impl crate::cli::grep_index::IndexSource for LocalSource {
    fn candidates(
        &self,
        q: &crate::cli::grep_index::Query<'_>,
    ) -> Option<crate::cli::grep_index::Answer> {
        let literal = q.fixed
            || q.pattern
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b' ');
        if !literal || q.pattern.len() < 3 {
            return None;
        }
        let cs = *q.rev.as_bytes();
        let key = hex(&cs);
        let plan = plan::plan_literal(q.pattern, q.ignore_case).ok()?;
        for store in self.ci.repos() {
            if !store.has_rev(&key) {
                continue;
            }
            let open = Open::load(&store, &cs)?;
            let names = open.candidates(&plan)?;
            let mut out = Vec::with_capacity(names.len());
            for n in names {
                out.push(RepoPath::new(n.into_bytes()).ok()?);
            }
            store.touch_rev(&key);
            return Some(crate::cli::grep_index::Answer {
                coverage: f64::from(open.coverage()),
                candidates: out,
            });
        }
        None
    }
}

/// The index of this machine as a `grep` source; `None` when `$ASC_HOME/ci` does not exist.
pub fn grep_source() -> Option<Box<dyn crate::cli::grep_index::IndexSource>> {
    static HOME: OnceLock<Option<PathBuf>> = OnceLock::new();
    let home = HOME
        .get_or_init(|| {
            asc_state::AscHome::locate()
                .ok()
                .map(|h| h.root().to_path_buf())
        })
        .clone()?;
    let ci = CiRoot::new(&home);
    super::store::plain_dir(ci.path()).then(|| Box::new(LocalSource::new(ci)) as _)
}

/// The local answer of this machine: `$ASC_HOME/ci` read with the default local tiers. `None`
/// when `$ASC_HOME` cannot be located.
pub fn system_answer() -> Option<Arc<LocalAnswer>> {
    let home = asc_state::AscHome::locate().ok()?;
    let objects = crate::cli::common::ObjectStack::build(
        &home,
        &crate::cli::config::ConfigSnapshot::empty(),
        None,
    )
    .ok()?;
    Some(Arc::new(LocalAnswer::new(
        CiRoot::new(home.root()),
        Arc::new(objects),
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codeintel_local::build::{Entry, Job};
    use crate::daemon::codeintel_overlay::ServerAnswer;
    use asc_store::cachedir::CacheDir;
    use asc_store::codec::encode_stored;
    use asc_store::storeset::StoreSet;

    const REPO: &str = "00000000-0000-4000-8000-0000000000aa";
    const CS: [u8; 32] = [3; 32];

    struct Rig {
        _dir: tempfile::TempDir,
        ci: CiRoot,
        objects: Arc<StoreSet>,
    }

    fn rig() -> Rig {
        let dir = tempfile::tempdir().unwrap();
        let objects = Arc::new(StoreSet {
            store: CacheDir::open(dir.path().join("store"), dir.path().join("tmp")).unwrap(),
            cache: CacheDir::open(dir.path().join("cache"), dir.path().join("tmp")).unwrap(),
        });
        Rig {
            ci: CiRoot::new(dir.path()),
            _dir: dir,
            objects,
        }
    }

    async fn put(r: &Rig, bytes: &[u8]) -> FileId {
        let id = FileId::of(bytes);
        r.objects
            .put(&ObjKey::File(id), encode_stored(bytes))
            .await
            .unwrap();
        id
    }

    /// Indexes `files` as the revision `CS` of the repository `REPO`.
    async fn index(r: &Rig, files: &[(&str, &str)]) -> Vec<FileId> {
        let mut entries = Vec::new();
        let mut ids = Vec::new();
        for (p, c) in files {
            let id = put(r, c.as_bytes()).await;
            ids.push(id);
            entries.push(Entry {
                path: (*p).to_owned(),
                id,
                size: c.len() as u64,
            });
        }
        let store = r.ci.repo(REPO).unwrap();
        crate::codeintel_local::build::index_rev(Job {
            ci: &r.ci,
            store: &store,
            lock: None,
            cap: 64 << 20,
            changeset: CS,
            git: Some("ab".repeat(20)),
            entries,
            extra_missing: 0,
            objects: &*r.objects,
            threads: 2,
        })
        .await
        .unwrap();
        ids
    }

    fn answer(r: &Rig) -> LocalAnswer {
        LocalAnswer::new(r.ci.clone(), r.objects.clone())
    }

    fn scope(rev: &str) -> Option<wire::Scope> {
        Some(wire::Scope {
            repo: Some(wire::RepoRef {
                name: "r".into(),
                id_mode: 0,
            }),
            at: Some(wire::scope::At::Rev(rev.into())),
            ..wire::Scope::default()
        })
    }

    fn search_req(rev: &str, pattern: &str, mode: wire::SearchMode) -> wire::SearchRequest {
        wire::SearchRequest {
            scope: scope(rev),
            pattern: pattern.into(),
            mode: mode as i32,
            ..wire::SearchRequest::default()
        }
    }

    fn found(r: &[wire::SearchResponse]) -> Vec<String> {
        r.iter()
            .flat_map(|m| &m.files)
            .map(|f| String::from_utf8_lossy(&f.path).into_owned())
            .collect()
    }

    const FILES: &[(&str, &str)] = &[
        (
            "a/one.rs",
            "pub fn alpha_one() {}\nfn caller() { alpha_one(); }\n",
        ),
        ("b/two.rs", "pub fn beta_two() {}\n"),
        ("notes.txt", "alpha_one is described here\n"),
    ];

    #[test]
    fn only_full_hex_revisions_resolve_offline() {
        let at = |s: &str| ci::Scope {
            repo: "r".into(),
            id_mode: Default::default(),
            at: ci::At::Rev(s.into()),
            stack_diff_only: false,
            paths: vec![],
            exclude_paths: vec![],
            langs: vec![],
            include_excluded: false,
        };
        assert_eq!(target_of(&at(&"ab".repeat(32))).unwrap().0.len(), 32);
        assert_eq!(target_of(&at(&"ab".repeat(20))).unwrap().0.len(), 20);
        for bad in [
            "master",
            "HEAD",
            "abc",
            &"AB".repeat(32),
            &"zz".repeat(20),
            "",
        ] {
            assert!(target_of(&at(bad)).is_err(), "{bad}");
        }
    }

    #[tokio::test]
    async fn exact_search_and_status_come_from_the_index() {
        let r = rig();
        index(&r, FILES).await;
        let a = answer(&r);
        let cs = hex(&CS);
        let out = a
            .search(search_req(&cs, "alpha_one", wire::SearchMode::Literal))
            .await
            .unwrap();
        assert_eq!(found(&out), ["a/one.rs", "notes.txt"]);
        assert_eq!(out[0].scope.as_ref().unwrap().coverage, 1.0);
        // The git id of the commit finds the same revision.
        let out = a
            .search(search_req(
                &"ab".repeat(20),
                "beta_two",
                wire::SearchMode::Word,
            ))
            .await
            .unwrap();
        assert_eq!(found(&out), ["b/two.rs"]);
        // A regex: the trigram plan keeps candidates, the verify step decides.
        let out = a
            .search(search_req(
                &cs,
                "alpha_(one|two)\\(\\)",
                wire::SearchMode::Regex,
            ))
            .await
            .unwrap();
        assert_eq!(found(&out), ["a/one.rs"]);
        let none = a
            .search(search_req(&cs, "zzzzzz", wire::SearchMode::Literal))
            .await
            .unwrap();
        assert!(found(&none).is_empty());

        let st = a
            .index_status(wire::IndexStatusRequest { scope: scope(&cs) })
            .await
            .unwrap();
        assert_eq!(st.indexes[0].index, "local");
        assert_eq!(st.indexes[0].coverage, 1.0);
        // An unknown revision is an honest empty answer.
        let none = a
            .index_status(wire::IndexStatusRequest {
                scope: scope(&"cd".repeat(32)),
            })
            .await
            .unwrap();
        assert!(none.indexes.is_empty());
        assert_eq!(none.scope.unwrap().coverage, 0.0);
        // A branch name needs refs this index does not hold.
        let e = a
            .index_status(wire::IndexStatusRequest {
                scope: scope("master"),
            })
            .await
            .unwrap_err();
        assert_eq!(e.code(), tonic::Code::FailedPrecondition);
    }

    #[tokio::test]
    async fn symbols_come_from_the_tags_in_the_index() {
        let r = rig();
        index(&r, FILES).await;
        let a = answer(&r);
        let q = |op: wire::SymbolOp, name: &str| wire::SymbolsRequest {
            scope: scope(&hex(&CS)),
            op: op as i32,
            name: name.into(),
            ..wire::SymbolsRequest::default()
        };
        let defs = a
            .symbols(q(wire::SymbolOp::Def, "alpha_one"))
            .await
            .unwrap();
        assert_eq!(defs.symbols.len(), 1);
        assert_eq!(defs.symbols[0].path.as_ref(), b"a/one.rs");
        let refs = a
            .symbols(q(wire::SymbolOp::Refs, "alpha_one"))
            .await
            .unwrap();
        assert!(refs.symbols.iter().any(|s| !s.is_def));
        assert_eq!(defs.scope.unwrap().coverage, 1.0);
    }

    #[tokio::test]
    async fn a_page_token_and_a_bad_pattern_are_refused() {
        let r = rig();
        index(&r, FILES).await;
        let a = answer(&r);
        let cs = hex(&CS);
        let mut q = search_req(&cs, "alpha", wire::SearchMode::Literal);
        q.page_token = "x".into();
        assert_eq!(
            a.search(q).await.unwrap_err().code(),
            tonic::Code::InvalidArgument
        );
        let e = a
            .search(search_req(&cs, "(", wire::SearchMode::Regex))
            .await
            .unwrap_err();
        assert_eq!(e.code(), tonic::Code::InvalidArgument);
    }

    #[tokio::test]
    async fn bytes_that_do_not_match_their_file_id_are_never_served() {
        let r = rig();
        let ids = index(&r, FILES).await;
        // The object under the first file's id now holds other bytes.
        let key = ObjKey::File(ids[0]);
        r.objects.delete(&key).await.unwrap();
        let _ = r
            .objects
            .put(&key, encode_stored(b"alpha_one planted\n"))
            .await;
        let a = answer(&r);
        let out = a
            .search(search_req(
                &hex(&CS),
                "alpha_one",
                wire::SearchMode::Literal,
            ))
            .await
            .unwrap();
        assert_eq!(found(&out), ["notes.txt"], "the swapped file is skipped");
    }

    #[tokio::test]
    async fn a_tampered_segment_pack_is_refused() {
        let r = rig();
        index(&r, FILES).await;
        let store = r.ci.repo(REPO).unwrap();
        let st = store.state().unwrap();
        let (h, _) = st.segs.iter().next().unwrap();
        let mut pack = store.read_seg(h).unwrap();
        let mid = pack.len() / 2;
        pack[mid] ^= 0xff;
        std::fs::write(store.path().join("seg").join(format!("{h}.pack")), pack).unwrap();
        assert!(Open::load(&store, &CS).is_none());
        let a = answer(&r);
        let out = a
            .search(search_req(
                &hex(&CS),
                "alpha_one",
                wire::SearchMode::Literal,
            ))
            .await
            .unwrap();
        assert!(
            found(&out).is_empty(),
            "an index that fails its hash answers nothing"
        );
    }

    #[tokio::test]
    async fn readers_close_after_the_idle_time() {
        let r = rig();
        index(&r, FILES).await;
        let a = answer(&r).with_idle(Duration::from_millis(40));
        a.search(search_req(
            &hex(&CS),
            "alpha_one",
            wire::SearchMode::Literal,
        ))
        .await
        .unwrap();
        assert_eq!(a.open_readers(), 1);
        tokio::time::sleep(Duration::from_millis(80)).await;
        assert_eq!(a.sweep(), 1);
        assert_eq!(a.open_readers(), 0);
        // The next query opens it again.
        let out = a
            .search(search_req(
                &hex(&CS),
                "alpha_one",
                wire::SearchMode::Literal,
            ))
            .await
            .unwrap();
        assert_eq!(found(&out).len(), 2);
    }

    #[tokio::test]
    async fn grep_candidates_drop_files_the_index_rules_out() {
        use crate::cli::grep_index::{IndexSource, Query};
        let r = rig();
        index(&r, FILES).await;
        let src = LocalSource::new(r.ci.clone());
        let q = |p: &'static str, fixed: bool| Query {
            rev: asc_core::id::ChangesetId::from_bytes(CS),
            pattern: p,
            fixed,
            ignore_case: false,
            word: false,
        };
        let got = src.candidates(&q("alpha_one", true)).unwrap();
        assert_eq!(got.coverage, 1.0);
        let names: Vec<String> = got
            .candidates
            .iter()
            .map(|p| String::from_utf8_lossy(p.as_bytes()).into_owned())
            .collect();
        assert!(names.contains(&"a/one.rs".to_owned()));
        assert!(names.contains(&"notes.txt".to_owned()));
        assert!(!names.contains(&"b/two.rs".to_owned()));
        // A pattern with BRE/ERE meaning, or one under three bytes, is not prefiltered.
        assert!(src.candidates(&q("a\\|b", false)).is_none());
        assert!(src.candidates(&q("al", true)).is_none());
    }
}
