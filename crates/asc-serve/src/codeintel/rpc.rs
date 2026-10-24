//! The `CodeIntel` service: `Search`, `Symbols`, `Semantic` and `IndexStatus` (PLAN I16, SPEC
//! §23.4.1, §23.7).
//!
//! * **Authorisation first.** The service shell authorises `Read` on the scope's repository
//!   before anything else is looked at. A composed repository (a gitlink in `.gitmodules`,
//!   SPEC §10.6) joins a query only when the same credential can read it; a child the caller
//!   cannot read, or that does not open, or whose pin is unknown, is absent, not redacted. The
//!   authorisation of each namespace happens before its registry entry, its doc table or any of
//!   its indexes is opened. Results of the namespaces are merged only after each was filtered
//!   on its own (scope, path filters, rules, tombstones), and no namespace's data reaches
//!   another's query.
//! * **Scopes.** A scope resolves to one changeset per namespace: `rev` through the same
//!   viewer-checked resolver the read RPCs use, `change` and `stack_top` to the newest
//!   published version, `workspace` is refused (daemon IPC only).
//! * **Page tokens** are postcard of `(scope digest, stage, generation, cursor)` with a
//!   BLAKE3 keyed MAC. The digest binds the caller, the repository, every resolved changeset,
//!   the filters and the query, so a token cannot be replayed for another request or principal.
//!   A token resumes on the snapshot of its generation while that is still pinned (see
//!   `snaps`), else `FAILED_PRECONDITION: stale page token`.
//! * **Limits.** Namespaces per query, composition depth, `.gitmodules` size, scans without a
//!   trigram, concurrent semantic queries, symbol paging depth and the work of every executor
//!   are named constants. The admission layer applies the `codeintel` and `semantic` rate
//!   classes by method (`limits::class_of`); the scan slots here are the extra `expensive`
//!   charge for a pattern without a trigram.
//!
//! Choices where SPEC is silent:
//!
//! * `Search` answers with one `SearchResponse` per call, rendered under the budget; the rest
//!   of the ranked window is the next page.
//! * The page-token key is random per process unless [`Settings::page_key`] is set, so a token
//!   does not survive a restart and answers `stale page token`.
//! * Selectivity (`kept chunks / namespace chunks`) is estimated from the namespace's doc and
//!   chunk counts; the exact path and the HNSW path give the same answers.
//! * A change with several unpublished changesets and no published version has no head.
//! * No project graph is given to the symbol ranking yet.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::{Arc, Mutex, OnceLock};

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use bytes::Bytes;
use asc_codeintel::config::{CodeintelConfig, MAX_CONFIG_BYTES};
use asc_codeintel::hybrid::{self, HybridOptions};
use asc_codeintel::index::schema::{ChunksSchema, IndexKind};
use asc_codeintel::index::segments::hex as model_hex;
use asc_codeintel::lang::{self, Lang};
use asc_codeintel::plan;
use asc_codeintel::render::{self, RawChunk, RawFileHit, RawLine, RenderCtx};
use asc_codeintel::scope::{ScopeError, ScopeQuery};
use asc_codeintel::search::{self, Cursor, Mode, SearchError, Searcher};
use asc_codeintel::skip;
use asc_codeintel::symbols::{self, NoProjects, SymbolEngine, SymbolError, SymbolQuery};
use asc_codeintel::tags::Kind as TagKind;
use asc_codeintel::vectors::{self, Selectivity, chunk_key, chunk_of, doc_ord_of};
use asc_core::change::ChangeId;
use asc_core::git::ObjectKind;
use asc_core::id::{ChangesetId, FileId, GitOid, TreeId};
use asc_core::object::Kind;
use asc_core::path::RepoPath;
use asc_rpc::auth::{Access, AuthInterceptor, Authorizer, Credential, Principal};
use asc_rpc::codeintel as ci;
use asc_rpc::server::{ClientVersion, MAX_MESSAGE, MAX_STRING, RepoName, ServerInfo, wire};
use asc_rpc::services::code_intel_server::{CodeIntel, CodeIntelServer};
use asc_rpc::{IdMode, RpcError};
use roaring::RoaringBitmap;
use serde::{Deserialize, Serialize};
use sqlx::AnyPool;
use tokio::sync::Semaphore;
use tonic::metadata::MetadataMap;
use tonic::service::interceptor::InterceptedService;
use tonic::{Extensions as TonicExt, Request, Response, Status};

use super::files::{NsSearcher, ServerFiles, file_bytes, secret_of};
use super::modelgen::{ModelGens, QueryLru};
use super::ords::{DocInfo, NsDocs, can_block};
use super::snaps::{DEFAULT_VECTOR_BYTES, Snaps, VecCache};
use super::worker::{EmbedModel, SqlManifest, current_versions, ns_name, read_chunk_rows};
use crate::idmode::{Eng, IdCodec};
use crate::registry::{Extensions, Registry, RegistryError, RepoEntry};

/// Namespaces one query may search: the repository and its composed children.
pub const MAX_TARGETS: usize = 16;
/// Levels of composition followed below the repository (the sdk's own gitlinks, say).
pub const MAX_COMPOSE_DEPTH: usize = 2;
/// Largest `.gitmodules` read.
pub const MAX_GITMODULES_BYTES: u64 = 64 * 1024;
/// Most submodule sections read from one `.gitmodules`.
pub const MAX_MODULES: usize = 64;
/// Results of a `Semantic` call when `k` is zero.
pub const DEFAULT_K: u32 = 10;
/// Scans without a trigram running at once on this replica, and per principal.
pub const MAX_SCANS: usize = 8;
pub const MAX_SCANS_PER_PRINCIPAL: usize = 2;
/// Semantic queries running at once on this replica (SPEC §23.4.1).
pub const MAX_SEMANTIC: usize = 32;
/// Doc tables kept in memory.
const MAX_NS_DOCS: usize = 256;
/// Symbols paged through by token; deeper results need a narrower query.
const MAX_SYMBOL_DEPTH: usize = symbols::MAX_LIMIT;
/// Identifiers of a query that get an exact definition lookup in a hybrid search.
const MAX_SYM_IDENTS: usize = 8;
/// Definitions taken per identifier.
const MAX_SYM_HITS: usize = 20;
/// Candidates the BM25 and vector lists contribute to a hybrid search, at most.
const MAX_LIST: usize = 200;
const KIND_SEARCH: u8 = 1;
const KIND_SYMBOLS: u8 = 2;
const TOKEN_MAC: usize = 32;

// ------------------------------------------------------------------------------ errors

fn internal(context: &str, e: &dyn std::fmt::Display) -> Status {
    tracing::error!(%context, error = %e, "internal error");
    Status::internal("internal error")
}

fn bad(m: impl Into<String>) -> Status {
    Status::invalid_argument(m.into())
}

fn registry_status(e: RegistryError) -> Status {
    match e {
        RegistryError::NotFound(_) => Status::not_found("no such repository"),
        e => internal("registry", &e),
    }
}

fn graph_status(e: asc_engine::graph::Error) -> Status {
    use asc_engine::graph::Error;
    match e {
        Error::Missing(id) => Status::not_found(format!("changeset {id} is missing")),
        Error::Query(m) => bad(m),
        Error::Remote(m) => bad(m),
        Error::Unsupported(m) => Status::unimplemented(m),
        e => internal("history", &e),
    }
}

fn scope_status(e: ScopeError) -> Status {
    match e {
        ScopeError::NoMergeBase => {
            Status::failed_precondition("the change has no merge base with the trunk")
        }
        ScopeError::TooLarge(what) => {
            Status::resource_exhausted(format!("{what} exceeds its bound; narrow the scope"))
        }
        e => internal("scope", &e),
    }
}

fn search_status(e: SearchError) -> Status {
    match e {
        SearchError::InvalidArgument(m) => bad(m),
        SearchError::Scope(e) => scope_status(e),
        e => internal("search", &e),
    }
}

fn symbol_status(e: SymbolError) -> Status {
    match e {
        SymbolError::InvalidName(m) => bad(format!("invalid name: {m}")),
        e => internal("symbols", &e),
    }
}

fn sql_status(e: sqlx::Error) -> Status {
    internal("database", &e)
}

fn ci_status(e: super::modelgen::CiError) -> Status {
    internal("codeintel state", &e)
}

fn rpc_error(e: impl std::fmt::Display) -> Status {
    bad(e.to_string())
}

// ------------------------------------------------------------------------------ settings

/// What the service needs from the deployment.
#[derive(Clone, Debug)]
pub struct Settings {
    /// Where the replica keeps unpacked index segments (`<disk_dir>/ci`).
    pub replica_dir: PathBuf,
    /// Bytes of decoded vector segments kept in memory.
    pub vector_cache_bytes: u64,
    /// The page-token MAC key; random per process when `None`.
    pub page_key: Option<[u8; 32]>,
}

impl Settings {
    pub fn new(replica_dir: impl Into<PathBuf>) -> Self {
        Self {
            replica_dir: replica_dir.into(),
            vector_cache_bytes: DEFAULT_VECTOR_BYTES,
            page_key: None,
        }
    }
}

/// The place the server's single `CodeIntel` handler is registered, once (read by
/// `build_routes`).
#[derive(Default)]
pub struct CodeIntelSlot(OnceLock<Arc<CodeIntelRpc>>);

impl CodeIntelSlot {
    /// Registers `rpc`; false when one is already registered.
    pub fn set(&self, rpc: Arc<CodeIntelRpc>) -> bool {
        self.0.set(rpc).is_ok()
    }

    pub fn get(&self) -> Option<&Arc<CodeIntelRpc>> {
        self.0.get()
    }
}

/// The unit's one line in `register_all`: makes the handler available to `build_routes`.
pub fn register(
    ext: &Extensions,
    registry: &Arc<Registry>,
    settings: Settings,
    embed: Option<Arc<dyn EmbedModel>>,
) -> std::io::Result<()> {
    let rpc = CodeIntelRpc::new(registry.clone(), settings, embed)?;
    if !ext.codeintel.set(Arc::new(rpc)) {
        tracing::warn!("a CodeIntel handler was already registered; keeping it");
    }
    Ok(())
}

// ------------------------------------------------------------------------------ scan gate

/// The extra charge for a search without a trigram: a bounded number of them at once.
struct ScanGate {
    total: Arc<Semaphore>,
    per: Mutex<HashMap<String, usize>>,
}

struct ScanPermit {
    _slot: tokio::sync::OwnedSemaphorePermit,
    gate: Arc<ScanGate>,
    who: String,
}

impl Drop for ScanPermit {
    fn drop(&mut self) {
        let mut g = self.gate.per.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(n) = g.get_mut(&self.who) {
            *n = n.saturating_sub(1);
            if *n == 0 {
                g.remove(&self.who);
            }
        }
    }
}

impl ScanGate {
    fn acquire(self: &Arc<Self>, who: &str) -> Result<ScanPermit, Status> {
        let busy = || Status::resource_exhausted("too many unindexed scans in flight; retry later");
        {
            let mut g = self.per.lock().unwrap_or_else(|p| p.into_inner());
            let n = g.entry(who.to_owned()).or_insert(0);
            if *n >= MAX_SCANS_PER_PRINCIPAL {
                return Err(busy());
            }
            *n += 1;
        }
        match self.total.clone().try_acquire_owned() {
            Ok(slot) => Ok(ScanPermit {
                _slot: slot,
                gate: self.clone(),
                who: who.to_owned(),
            }),
            Err(_) => {
                let mut g = self.per.lock().unwrap_or_else(|p| p.into_inner());
                if let Some(n) = g.get_mut(who) {
                    *n = n.saturating_sub(1);
                    if *n == 0 {
                        g.remove(who);
                    }
                }
                Err(busy())
            }
        }
    }
}

// ------------------------------------------------------------------------------ page tokens

#[derive(Serialize, Deserialize)]
struct SearchPage {
    digest: [u8; 32],
    stage: u32,
    /// The snapshot generation the cursor belongs to; read only when `pinned`.
    generation: u64,
    pinned: bool,
    window: u32,
    ord: u32,
}

#[derive(Serialize, Deserialize)]
struct SymbolPage {
    digest: [u8; 32],
    /// The files-index generation of each namespace, in target order.
    generations: Vec<u64>,
    offset: u32,
}

// ------------------------------------------------------------------------------ the handler

/// Who is asking, and how to ask the authorizer about other namespaces on their behalf.
pub struct Caller {
    authorizer: Arc<dyn Authorizer>,
    credential: Option<Credential>,
    principal: Principal,
    repo: RepoName,
    id_mode: IdMode,
}

/// One namespace of a query.
struct Target {
    name: RepoName,
    entry: Arc<RepoEntry>,
    docs: Arc<NsDocs>,
    /// The namespace's index name (lower-case hex).
    nsn: String,
    cs: ChangesetId,
    root: TreeId,
    /// Where the namespace sits in the main tree; empty for the main repository.
    prefix: Vec<u8>,
    include: Vec<RepoPath>,
    exclude: Vec<RepoPath>,
    /// `None` is `include_excluded`.
    rules: Option<CodeintelConfig>,
    trunk: Option<ChangesetId>,
    depth: usize,
}

impl Target {
    /// `path` of this namespace as the main tree names it.
    fn out_path(&self, path: &[u8]) -> Vec<u8> {
        if self.prefix.is_empty() {
            return path.to_vec();
        }
        let mut v = Vec::with_capacity(self.prefix.len() + 1 + path.len());
        v.extend_from_slice(&self.prefix);
        v.push(b'/');
        v.extend_from_slice(path);
        v
    }

    fn label(&self, index: &str) -> String {
        if self.prefix.is_empty() {
            index.to_owned()
        } else {
            format!("{index}@{}", String::from_utf8_lossy(&self.prefix))
        }
    }
}

struct Plan {
    targets: Vec<Target>,
    langs: Vec<Lang>,
    stack_diff_only: bool,
    include_excluded: bool,
}

/// The handler behind [`CodeIntelService`].
pub struct CodeIntelRpc {
    registry: Arc<Registry>,
    snaps: Snaps,
    docs: quick_cache::sync::Cache<[u8; 16], Arc<NsDocs>>,
    vecs: VecCache,
    embed: Option<Arc<dyn EmbedModel>>,
    qlru: QueryLru,
    key: [u8; 32],
    semantic: Arc<Semaphore>,
    scans: Arc<ScanGate>,
}

fn under(path: &[u8], dir: &[u8]) -> bool {
    path == dir || (path.len() > dir.len() && path.starts_with(dir) && path[dir.len()] == b'/')
}

/// The filters of the main tree as they apply below the gitlink `mount`: `None` when the
/// filters leave nothing of that subtree.
fn child_filters(
    include: &[RepoPath],
    exclude: &[RepoPath],
    mount: &RepoPath,
) -> Option<(Vec<RepoPath>, Vec<RepoPath>)> {
    let m = mount.as_bytes();
    let mut inc = Vec::new();
    if !include.is_empty() {
        let mut whole = false;
        for i in include {
            let ib = i.as_bytes();
            if under(m, ib) {
                whole = true;
                break;
            }
            if under(ib, m)
                && let Ok(p) = RepoPath::new(ib[m.len() + 1..].to_vec())
            {
                inc.push(p);
            }
        }
        if whole {
            inc.clear();
        } else if inc.is_empty() {
            return None;
        }
    }
    let mut exc = Vec::new();
    for e in exclude {
        let eb = e.as_bytes();
        if under(m, eb) {
            return None;
        }
        if under(eb, m)
            && let Ok(p) = RepoPath::new(eb[m.len() + 1..].to_vec())
        {
            exc.push(p);
        }
    }
    Some((inc, exc))
}

/// Whether `path` passes the include and exclude prefixes.
fn path_allowed(path: &[u8], include: &[RepoPath], exclude: &[RepoPath]) -> bool {
    (include.is_empty() || include.iter().any(|i| under(path, i.as_bytes())))
        && !exclude.iter().any(|e| under(path, e.as_bytes()))
}

fn parse_paths(v: &[String]) -> Result<Vec<RepoPath>, Status> {
    let mut out = Vec::new();
    for p in v {
        let t = p.trim_matches('/');
        if t.is_empty() || t == "." {
            // The root: no restriction.
            return Ok(Vec::new());
        }
        out.push(RepoPath::new(t.as_bytes().to_vec()).map_err(|e| bad(format!("bad path: {e}")))?);
    }
    Ok(out)
}

fn parse_exclude(v: &[String]) -> Result<Vec<RepoPath>, Status> {
    let mut out = Vec::new();
    for p in v {
        let t = p.trim_matches('/');
        if t.is_empty() || t == "." {
            continue;
        }
        out.push(RepoPath::new(t.as_bytes().to_vec()).map_err(|e| bad(format!("bad path: {e}")))?);
    }
    Ok(out)
}

// ------------------------------------------------------------------------------ gitmodules

fn valid_segment(s: &str) -> bool {
    !s.is_empty()
        && !s.starts_with('.')
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

/// The nested repository for a `.gitmodules` url, relative to `parent` (`asc/dds`):
/// `../soma.git` gives `asc/soma`; an absolute url gives its last two path segments.
fn derive_repo(parent: &str, url: &str) -> Option<RepoName> {
    let url = url.trim();
    let strip = |s: &str| s.trim_end_matches('/').trim_end_matches(".git").to_owned();
    let derived = if url.starts_with("./") || url.starts_with("../") {
        let mut stack: Vec<String> = parent.split('/').map(str::to_owned).collect();
        for seg in strip(url).split('/') {
            match seg {
                "" | "." => {}
                ".." => {
                    stack.pop()?;
                }
                s => stack.push(s.to_owned()),
            }
        }
        stack.join("/")
    } else {
        let path = match url.split_once("://") {
            Some((_, rest)) => rest.split_once('/')?.1,
            None => url.split_once(':')?.1,
        };
        let path = strip(path);
        let segs: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
        let n = segs.len();
        if n < 2 {
            return None;
        }
        format!("{}/{}", segs[n - 2], segs[n - 1])
    };
    let mut parts = derived.split('/');
    let (Some(a), Some(b), None) = (parts.next(), parts.next(), parts.next()) else {
        return None;
    };
    if !valid_segment(a) || !valid_segment(b) {
        return None;
    }
    RepoName::parse(&derived).ok()
}

/// `(path, url)` of each `[submodule "<name>"]` section that has both.
fn parse_gitmodules(text: &str) -> Vec<(RepoPath, String)> {
    let mut out: Vec<(RepoPath, String)> = Vec::new();
    let mut cur: Option<(Option<String>, Option<String>)> = None;
    let flush = |cur: &mut Option<(Option<String>, Option<String>)>,
                 out: &mut Vec<(RepoPath, String)>| {
        if let Some((Some(path), Some(url))) = cur.take()
            && out.len() < MAX_MODULES
            && let Ok(p) = RepoPath::new(path.trim_matches('/').as_bytes().to_vec())
        {
            out.push((p, url));
        }
    };
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        if let Some(head) = line.strip_prefix('[') {
            flush(&mut cur, &mut out);
            let head = head.trim_end_matches(']').trim();
            cur = head
                .strip_prefix("submodule")
                .map(str::trim)
                .filter(|n| n.starts_with('"') && n.ends_with('"') && n.len() >= 2)
                .map(|_| (None, None));
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let value = value.trim().trim_matches('"').to_owned();
        if let Some((path, url)) = cur.as_mut() {
            match key.trim().to_ascii_lowercase().as_str() {
                "path" => *path = Some(value),
                "url" => *url = Some(value),
                _ => {}
            }
        }
    }
    flush(&mut cur, &mut out);
    out
}

/// The file at `path` below `root`, at most `max` bytes; `None` when it is absent, not a
/// regular file, or larger.
async fn read_blob(
    engine: &Eng,
    root: TreeId,
    path: &str,
    max: u64,
) -> Result<Option<Bytes>, Status> {
    let Ok(p) = RepoPath::new(path.as_bytes().to_vec()) else {
        return Ok(None);
    };
    let Some(e) = engine.lookup(root, &p).await.map_err(|e| internal("tree", &e))? else {
        return Ok(None);
    };
    if !matches!(e.kind, Kind::File | Kind::Exec) || e.size > max {
        return Ok(None);
    }
    engine
        .read_file(FileId::from_bytes(e.id), e.size)
        .await
        .map(Some)
        .map_err(|e| internal("file", &e))
}

// ------------------------------------------------------------------------------ construction

impl CodeIntelRpc {
    pub fn new(
        registry: Arc<Registry>,
        settings: Settings,
        embed: Option<Arc<dyn EmbedModel>>,
    ) -> std::io::Result<Self> {
        let key = match settings.page_key {
            Some(k) => k,
            None => {
                let mut k = [0u8; 32];
                getrandom::fill(&mut k).map_err(|e| std::io::Error::other(e.to_string()))?;
                k
            }
        };
        std::fs::create_dir_all(&settings.replica_dir)?;
        let manifests = Arc::new(SqlManifest::new(registry.pool().clone()));
        Ok(Self {
            snaps: Snaps::new(settings.replica_dir, manifests),
            registry,
            docs: quick_cache::sync::Cache::new(MAX_NS_DOCS),
            vecs: VecCache::new(settings.vector_cache_bytes),
            embed,
            qlru: QueryLru::new(super::modelgen::LRU_BYTES),
            key,
            semantic: Arc::new(Semaphore::new(MAX_SEMANTIC)),
            scans: Arc::new(ScanGate {
                total: Arc::new(Semaphore::new(MAX_SCANS)),
                per: Mutex::new(HashMap::new()),
            }),
        })
    }

    fn pool(&self) -> &AnyPool {
        self.registry.pool()
    }

    fn docs_of(&self, entry: &RepoEntry) -> Arc<NsDocs> {
        let ns = *entry.ns.as_bytes();
        if let Some(d) = self.docs.get(&ns) {
            return d;
        }
        let d = Arc::new(NsDocs::new(self.pool().clone(), &ns));
        self.docs.insert(ns, d.clone());
        d
    }

    fn searcher(&self, t: &Target) -> Arc<NsSearcher> {
        Arc::new(Searcher::new(
            t.entry.backend.engine().clone(),
            t.entry.objects.clone(),
            t.docs.clone(),
            t.docs.clone(),
        ))
    }

    // -------------------------------------------------------------------------- tokens

    fn seal<T: Serialize>(&self, kind: u8, v: &T) -> Result<String, Status> {
        let mut raw = postcard::to_stdvec(v).map_err(|e| internal("page token", &e))?;
        let mac = self.mac(kind, &raw);
        raw.extend_from_slice(mac.as_bytes());
        Ok(URL_SAFE_NO_PAD.encode(raw))
    }

    fn mac(&self, kind: u8, payload: &[u8]) -> blake3::Hash {
        let mut h = blake3::Hasher::new_keyed(&self.key);
        h.update(&[kind]);
        h.update(payload);
        h.finalize()
    }

    fn open<T: for<'de> Deserialize<'de>>(&self, kind: u8, token: &str) -> Result<T, Status> {
        let invalid = || bad("invalid page token");
        let raw = URL_SAFE_NO_PAD.decode(token).map_err(|_| invalid())?;
        if raw.len() <= TOKEN_MAC {
            return Err(invalid());
        }
        let (payload, mac) = raw.split_at(raw.len() - TOKEN_MAC);
        let want = self.mac(kind, payload);
        let got: [u8; TOKEN_MAC] = mac.try_into().map_err(|_| invalid())?;
        // `blake3::Hash` compares in constant time.
        if want != blake3::Hash::from(got) {
            return Err(invalid());
        }
        postcard::from_bytes(payload).map_err(|_| invalid())
    }

    fn digest(&self, caller: &Caller, plan: &Plan, op: &str, extra: &[&[u8]]) -> [u8; 32] {
        let mut h = blake3::Hasher::new_derive_key("asc v1 ci page scope");
        let mut put = |b: &[u8]| {
            h.update(&(b.len() as u64).to_le_bytes());
            h.update(b);
        };
        put(op.as_bytes());
        put(caller.principal.id.as_bytes());
        put(caller.repo.as_str().as_bytes());
        put(&caller.id_mode.to_wire().to_le_bytes());
        put(&[u8::from(plan.stack_diff_only), u8::from(plan.include_excluded)]);
        for l in &plan.langs {
            put(l.name().as_bytes());
        }
        for t in &plan.targets {
            put(t.name.as_str().as_bytes());
            put(t.cs.as_bytes());
            put(&t.prefix);
            for i in &t.include {
                put(i.as_bytes());
            }
            put(b"|");
            for e in &t.exclude {
                put(e.as_bytes());
            }
            put(b"||");
        }
        for e in extra {
            put(e);
        }
        *h.finalize().as_bytes()
    }

    // -------------------------------------------------------------------------- scope

    /// The changeset a revspec names for this caller. The codec refuses other users' refs by
    /// name and the resolver is told the same rule, so a ref reached some other way is held to
    /// it too.
    async fn resolve_rev(
        &self,
        caller: &Caller,
        entry: &RepoEntry,
        spec: &str,
    ) -> Result<ChangesetId, Status> {
        if spec.len() > MAX_STRING {
            return Err(bad("revision is too long"));
        }
        let spec = if spec.is_empty() { "HEAD" } else { spec };
        let ids = IdCodec::new(caller.id_mode, &self.registry, entry).for_viewer(&caller.principal);
        ids.resolve(spec, &entry.default_branch, graph_status)
            .await?
            .ok_or_else(|| Status::not_found(format!("unknown revision {spec:?}")))
    }

    /// The newest published version of `change`.
    async fn change_head(&self, entry: &RepoEntry, change: ChangeId) -> Result<ChangesetId, Status> {
        let mut conn = self.pool().acquire().await.map_err(sql_status)?;
        if let Some(v) = crate::changes::versions::latest(&mut conn, entry.ns, change)
            .await
            .map_err(|e| internal("versions", &e))?
        {
            return Ok(v.cs);
        }
        let css = crate::changes::index::changesets_of(
            &mut *conn,
            entry.ns,
            change,
            crate::changes::index::MAX_PREDS,
        )
        .await
        .map_err(|e| internal("change index", &e))?;
        match css.as_slice() {
            [one] => Ok(*one),
            _ => Err(Status::not_found("the change has no published version")),
        }
    }

    async fn rules_of(
        &self,
        engine: &Eng,
        root: TreeId,
        include_excluded: bool,
    ) -> Result<Option<CodeintelConfig>, Status> {
        if include_excluded {
            return Ok(None);
        }
        let blob = read_blob(engine, root, ".asc/codeintel.toml", MAX_CONFIG_BYTES as u64).await?;
        // A repository's own config can only exclude paths; a broken one excludes nothing.
        Ok(Some(
            blob.and_then(|b| CodeintelConfig::parse(&b).ok())
                .unwrap_or_default(),
        ))
    }

    async fn plan(&self, caller: &Caller, scope: &ci::Scope) -> Result<Plan, Status> {
        if !can_block() {
            return Err(Status::failed_precondition(
                "code intelligence needs the multi-threaded runtime",
            ));
        }
        if matches!(scope.at, ci::At::Workspace) {
            return Err(bad(
                "a workspace scope is accepted only over the daemon IPC",
            ));
        }
        if scope.stack_diff_only && !matches!(scope.at, ci::At::Change(_) | ci::At::StackTop(_)) {
            return Err(bad("stack_diff_only needs a change or a stack top"));
        }
        let include = parse_paths(&scope.paths)?;
        let exclude = parse_exclude(&scope.exclude_paths)?;
        let mut langs = Vec::new();
        for l in &scope.langs {
            langs.push(Lang::from_name(l).ok_or_else(|| bad(format!("unknown language {l:?}")))?);
        }
        let entry = self
            .registry
            .open(&caller.repo)
            .await
            .map_err(registry_status)?;
        let cs = match &scope.at {
            ci::At::Rev(spec) => self.resolve_rev(caller, &entry, spec).await?,
            ci::At::Change(c) | ci::At::StackTop(c) => self.change_head(&entry, *c).await?,
            ci::At::Workspace => return Err(bad("workspace scope")),
        };
        let trunk = if scope.stack_diff_only {
            Some(self.resolve_rev(caller, &entry, "HEAD").await?)
        } else {
            None
        };
        let engine = entry.backend.engine();
        let root = engine
            .get_changeset(cs)
            .await
            .map_err(|e| Status::not_found(format!("changeset {cs} is unavailable: {e}")))?
            .root;
        let rules = self.rules_of(engine, root, scope.include_excluded).await?;
        let main = Target {
            name: caller.repo.clone(),
            docs: self.docs_of(&entry),
            nsn: ns_name(entry.ns.as_bytes()).map_err(ci_status)?,
            entry,
            cs,
            root,
            prefix: Vec::new(),
            include,
            exclude,
            rules,
            trunk,
            depth: 0,
        };
        let mut plan = Plan {
            targets: vec![main],
            langs,
            stack_diff_only: scope.stack_diff_only,
            include_excluded: scope.include_excluded,
        };
        if !plan.stack_diff_only {
            self.compose(caller, &mut plan).await;
        }
        Ok(plan)
    }

    /// Adds the readable composed children, breadth first.
    async fn compose(&self, caller: &Caller, plan: &mut Plan) {
        let mut seen: HashSet<String> = plan.targets.iter().map(|t| t.name.to_string()).collect();
        let mut i = 0;
        while i < plan.targets.len() && plan.targets.len() < MAX_TARGETS {
            if plan.targets[i].depth < MAX_COMPOSE_DEPTH {
                let room = MAX_TARGETS - plan.targets.len();
                match self
                    .children(caller, &plan.targets[i], &mut seen, room, plan.include_excluded)
                    .await
                {
                    Ok(kids) => plan.targets.extend(kids),
                    Err(e) => tracing::debug!(error = %e, "composition skipped"),
                }
            }
            i += 1;
        }
    }

    async fn children(
        &self,
        caller: &Caller,
        parent: &Target,
        seen: &mut HashSet<String>,
        room: usize,
        include_excluded: bool,
    ) -> Result<Vec<Target>, Status> {
        let engine = parent.entry.backend.engine();
        let Some(text) =
            read_blob(engine, parent.root, ".gitmodules", MAX_GITMODULES_BYTES).await?
        else {
            return Ok(Vec::new());
        };
        let text = String::from_utf8_lossy(&text).into_owned();
        let mut out = Vec::new();
        for (mount, url) in parse_gitmodules(&text) {
            if out.len() >= room {
                break;
            }
            let Some((include, exclude)) =
                child_filters(&parent.include, &parent.exclude, &mount)
            else {
                continue;
            };
            // The pin: a gitlink entry at the mount path, and only that.
            let Some(entry) = engine
                .lookup(parent.root, &mount)
                .await
                .map_err(|e| internal("tree", &e))?
            else {
                continue;
            };
            if entry.kind != Kind::Gitlink {
                continue;
            }
            let Some(name) = derive_repo(parent.name.as_str(), &url) else {
                continue;
            };
            if !seen.insert(name.to_string()) {
                continue;
            }
            // Read access on the child's own namespace, before anything of it is opened. Any
            // failure of the decision is a refusal.
            if caller
                .authorizer
                .authorize_credential(caller.credential.as_ref(), name.as_str(), Access::Read)
                .await
                .is_err()
            {
                continue;
            }
            let Ok(child) = self.registry.open(&name).await else {
                continue;
            };
            let git = IdCodec::new(IdMode::Git, &self.registry, &child).for_viewer(&caller.principal);
            let Some(Some(cs)) = git
                .asc_of_git(ObjectKind::Commit, &[entry.git])
                .await?
                .into_iter()
                .next()
            else {
                continue;
            };
            let cs = ChangesetId::from_bytes(cs);
            let child_engine = child.backend.engine();
            let Ok(changeset) = child_engine.get_changeset(cs).await else {
                continue;
            };
            let root = changeset.root;
            let rules = self.rules_of(child_engine, root, include_excluded).await?;
            let mut prefix = parent.prefix.clone();
            if !prefix.is_empty() {
                prefix.push(b'/');
            }
            prefix.extend_from_slice(mount.as_bytes());
            out.push(Target {
                docs: self.docs_of(&child),
                nsn: ns_name(child.ns.as_bytes()).map_err(ci_status)?,
                name,
                entry: child,
                cs,
                root,
                prefix,
                include,
                exclude,
                rules,
                trunk: None,
                depth: parent.depth + 1,
            });
        }
        Ok(out)
    }

    async fn resolved(
        &self,
        caller: &Caller,
        plan: &Plan,
        generation: u64,
        coverage: f64,
    ) -> Result<wire::ResolvedScope, Status> {
        let main = &plan.targets[0];
        let ids =
            IdCodec::new(caller.id_mode, &self.registry, &main.entry).for_viewer(&caller.principal);
        let changeset = ids.changeset_out(main.cs).await?;
        let root = match caller.id_mode {
            IdMode::Asc => ids.tree_out(main.root, GitOid::from_bytes([0; 20])),
            IdMode::Git => ids.tree_out(main.root, ids.tree_git(main.root).await?),
        };
        let coverage = if coverage.is_finite() {
            coverage.clamp(0.0, 1.0)
        } else {
            0.0
        };
        Ok(wire::ResolvedScope {
            changeset,
            root,
            generation,
            coverage: coverage as f32,
        })
    }

    /// The kept ords of a target: scope, filters, rules and stack diff, without tombstones.
    async fn kept(&self, t: &Target, searcher: &NsSearcher) -> Result<RoaringBitmap, Status> {
        let mut q = ScopeQuery::new(t.cs);
        q.include = t.include.clone();
        q.exclude = t.exclude.clone();
        q.rules = t.rules.as_ref();
        q.stack_diff_trunk = t.trunk;
        let mut kept = searcher
            .scope_engine()
            .resolve(&q)
            .await
            .map_err(scope_status)?;
        let tomb = t.docs.tombstoned(&kept).await.map_err(sql_status)?;
        kept -= tomb;
        Ok(kept)
    }

    // -------------------------------------------------------------------------- Search

    pub async fn search(
        &self,
        caller: &Caller,
        req: ci::SearchRequest,
    ) -> Result<wire::SearchResponse, Status> {
        let mode = match req.mode {
            ci::SearchMode::Literal => Mode::Literal,
            ci::SearchMode::Regex => Mode::Regex,
            ci::SearchMode::Word => Mode::Word,
        };
        // A pattern without a trigram scans the scope: charged as `expensive`.
        let _scan = if is_scan(&req.pattern, mode, !req.case_sensitive) {
            Some(self.scans.acquire(&caller.principal.id)?)
        } else {
            None
        };
        let plan = self.plan(caller, &req.scope).await?;
        let digest = self.digest(
            caller,
            &plan,
            "search",
            &[
                req.pattern.as_bytes(),
                &[mode as u8, u8::from(req.case_sensitive)],
            ],
        );
        let (mut stage, mut cursor, mut pin) = (0usize, None, None);
        if !req.page_token.is_empty() {
            let p: SearchPage = self.open(KIND_SEARCH, &req.page_token)?;
            if p.digest != digest {
                return Err(bad("the page token is for another request"));
            }
            stage = p.stage as usize;
            if stage >= plan.targets.len() {
                return Err(bad("invalid page token"));
            }
            if p.pinned {
                cursor = Some(Cursor {
                    window: p.window,
                    ord: p.ord,
                });
                pin = Some(p.generation);
            }
        }
        let budget = render_budget(&req.budget);
        let results = budget_results(&req.budget);
        let mut raws: Vec<RawFileHit> = Vec::new();
        let mut truncated = false;
        let mut next: Option<SearchPage> = None;
        let mut coverage = 1.0f64;
        let mut generation: Option<u64> = None;
        while stage < plan.targets.len() {
            let t = &plan.targets[stage];
            let snap = match pin.take() {
                Some(g) => self
                    .snaps
                    .pinned(&t.nsn, IndexKind::Files, g)
                    .ok_or_else(|| Status::failed_precondition("stale page token"))?,
                None => {
                    self.snaps
                        .current(&*t.entry.objects, &t.nsn, IndexKind::Files)
                        .await?
                }
            };
            if stage == 0 {
                generation = Some(snap.generation);
            }
            let searcher = self.searcher(t);
            let sreq = search::SearchRequest {
                rev: t.cs,
                pattern: req.pattern.clone(),
                mode,
                case_sensitive: req.case_sensitive,
                paths: t.include.clone(),
                exclude_paths: t.exclude.clone(),
                langs: plan.langs.clone(),
                rules: t.rules.as_ref(),
                cursor: cursor.take(),
            };
            let res = searcher.search(&snap, &sreq).await.map_err(search_status)?;
            coverage = coverage.min(res.coverage);
            let room = results.saturating_sub(raws.len());
            if res.hits.len() > room {
                truncated = true;
            }
            let want_context = req.context_lines as usize;
            raws.extend(
                self.raw_hits(t, &snap.indexed, res.hits, room, want_context)
                    .await?,
            );
            if let Some(c) = res.next {
                next = Some(SearchPage {
                    digest,
                    stage: stage as u32,
                    generation: snap.generation,
                    pinned: true,
                    window: c.window,
                    ord: c.ord,
                });
                break;
            }
            stage += 1;
            if raws.len() >= results && stage < plan.targets.len() {
                next = Some(SearchPage {
                    digest,
                    stage: stage as u32,
                    generation: 0,
                    pinned: false,
                    window: 0,
                    ord: 0,
                });
                break;
            }
        }
        let generation = match generation {
            Some(g) => g,
            None => {
                let t = &plan.targets[0];
                self.snaps
                    .current(&*t.entry.objects, &t.nsn, IndexKind::Files)
                    .await?
                    .generation
            }
        };
        let ids = IdCodec::new(caller.id_mode, &self.registry, &plan.targets[0].entry)
            .for_viewer(&caller.principal);
        let ctx = RenderCtx {
            repo: caller.repo.to_string(),
            changeset: ids.changeset_out(plan.targets[0].cs).await?.to_vec(),
        };
        let rendered = render::render_search(&ctx, budget, &raws, truncated);
        let files = rendered
            .files
            .into_iter()
            .map(|f| wire::FileHit {
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
            })
            .collect();
        Ok(wire::SearchResponse {
            scope: Some(self.resolved(caller, &plan, generation, coverage).await?),
            files,
            truncated: rendered.truncated,
            next_page_token: match &next {
                Some(p) => self.seal(KIND_SEARCH, p)?,
                None => String::new(),
            },
            est_tokens: rendered.est_tokens,
        })
    }

    /// The first `room` hits as renderer input: language, secret flag and context lines.
    async fn raw_hits(
        &self,
        t: &Target,
        indexed: &RoaringBitmap,
        hits: Vec<search::FileHit>,
        room: usize,
        context: usize,
    ) -> Result<Vec<RawFileHit>, Status> {
        let hits: Vec<search::FileHit> = hits.into_iter().take(room).collect();
        let ords: Vec<u32> = hits.iter().map(|h| h.ord).collect();
        let infos = t.docs.info(&ords).await.map_err(sql_status)?;
        let mut out = Vec::with_capacity(hits.len());
        for hit in hits {
            let info: Option<&DocInfo> = infos.get(&hit.ord);
            // The row's flag counts only for a doc the worker analysed and the snapshot holds;
            // otherwise the bytes decide, and unreadable bytes mean flagged.
            let from_row = if indexed.contains(hit.ord) {
                info.and_then(DocInfo::secret)
            } else {
                None
            };
            let need_bytes = from_row.is_none() || context > 0;
            let bytes = if need_bytes {
                file_bytes(&*t.entry.objects, &hit.file).await.ok().flatten()
            } else {
                None
            };
            let secret = from_row.or_else(|| bytes.as_deref().map(secret_of));
            let paths: Vec<Vec<u8>> = hit
                .paths
                .iter()
                .map(|p| t.out_path(p.as_bytes()))
                .collect();
            let lang = info
                .and_then(|i| i.langs.first().map(|l| l.name().to_owned()))
                .or_else(|| {
                    paths.first().and_then(|p| {
                        lang::detect(&String::from_utf8_lossy(p), b"").map(|l| l.name().to_owned())
                    })
                })
                .unwrap_or_default();
            let lines = hit
                .lines
                .iter()
                .map(|l| {
                    let (before, after) = match (&bytes, context) {
                        (Some(b), n) if n > 0 => context_of(b, l.line, n),
                        _ => (Vec::new(), Vec::new()),
                    };
                    RawLine {
                        line: l.line,
                        text: l.text.clone(),
                        spans: l.spans.clone(),
                        before,
                        after,
                    }
                })
                .collect();
            out.push(RawFileHit {
                file: hit.file,
                lang,
                paths,
                lines,
                flagged: secret != Some(false),
            });
        }
        Ok(out)
    }

    // -------------------------------------------------------------------------- Symbols

    pub async fn symbols(
        &self,
        caller: &Caller,
        req: ci::SymbolsRequest,
    ) -> Result<wire::SymbolsResponse, Status> {
        let plan = self.plan(caller, &req.scope).await?;
        let mut kinds = Vec::new();
        for k in &req.kinds {
            kinds.push(TagKind::from_name(k).ok_or_else(|| bad(format!("unknown kind {k:?}")))?);
        }
        let want = {
            let n = match req.budget.max_results {
                0 => symbols::DEFAULT_LIMIT,
                n => n as usize,
            };
            n.clamp(1, MAX_SYMBOL_DEPTH)
        };
        let budget = render_budget(&req.budget);
        let ids = IdCodec::new(caller.id_mode, &self.registry, &plan.targets[0].entry)
            .for_viewer(&caller.principal);
        let ctx = RenderCtx {
            repo: caller.repo.to_string(),
            changeset: ids.changeset_out(plan.targets[0].cs).await?.to_vec(),
        };
        let op = match req.op {
            ci::SymbolOp::Def => 0u8,
            ci::SymbolOp::Refs => 1,
            ci::SymbolOp::Callers => 2,
            ci::SymbolOp::Outline => 3,
            ci::SymbolOp::Find => 4,
        };
        let at = req
            .def_path
            .as_ref()
            .map(|p| (p.as_bytes().to_vec(), req.def_line));
        let at_bytes: Vec<u8> = at
            .as_ref()
            .map(|(p, l)| [p.as_slice(), &l.to_le_bytes()].concat())
            .unwrap_or_default();
        let kind_bytes: Vec<u8> = req.kinds.join(",").into_bytes();
        let path_bytes = req.path.as_ref().map(|p| p.as_bytes().to_vec()).unwrap_or_default();
        let digest = self.digest(
            caller,
            &plan,
            "symbols",
            &[
                &[op],
                req.name.as_bytes(),
                &path_bytes,
                &kind_bytes,
                &at_bytes,
            ],
        );
        let (mut offset, mut pinned): (usize, Option<Vec<u64>>) = (0, None);
        if !req.page_token.is_empty() {
            let p: SymbolPage = self.open(KIND_SYMBOLS, &req.page_token)?;
            if p.digest != digest || p.generations.len() != plan.targets.len() {
                return Err(bad("the page token is for another request"));
            }
            offset = p.offset as usize;
            if offset >= MAX_SYMBOL_DEPTH {
                return Err(bad("invalid page token"));
            }
            pinned = Some(p.generations);
        }

        if req.op == ci::SymbolOp::Outline {
            let path = req.path.as_ref().ok_or_else(|| bad("outline needs a path"))?;
            let found = self.outline(&plan, path, &kinds, want).await?;
            let rendered = render::render_symbols(&ctx, budget, &found.0, found.1);
            return Ok(wire::SymbolsResponse {
                scope: Some(self.resolved(caller, &plan, 0, 1.0).await?),
                symbols: rendered.symbols.into_iter().map(symbol_out).collect(),
                truncated: rendered.truncated,
                next_page_token: String::new(),
                est_tokens: rendered.est_tokens,
            });
        }

        let limit = (offset + want + 1).min(MAX_SYMBOL_DEPTH);
        let mut all: Vec<symbols::Symbol> = Vec::new();
        let mut truncated = false;
        let mut gens = Vec::with_capacity(plan.targets.len());
        let mut covered = (0u64, 0u64);
        for (i, t) in plan.targets.iter().enumerate() {
            let snap = match pinned.as_ref().map(|g| g[i]) {
                Some(g) => self
                    .snaps
                    .pinned(&t.nsn, IndexKind::Files, g)
                    .ok_or_else(|| Status::failed_precondition("stale page token"))?,
                None => {
                    self.snaps
                        .current(&*t.entry.objects, &t.nsn, IndexKind::Files)
                        .await?
                }
            };
            gens.push(snap.generation);
            let searcher = self.searcher(t);
            let kept = self.kept(t, &searcher).await?;
            covered.0 += (&kept & &snap.indexed).len();
            covered.1 += kept.len();
            let files = ServerFiles {
                handle: tokio::runtime::Handle::current(),
                searcher: searcher.clone(),
                engine: t.entry.backend.engine().clone(),
                objects: t.entry.objects.clone(),
                docs: t.docs.clone(),
                root: t.root,
                indexed: snap.indexed.clone(),
                info: Mutex::new(HashMap::new()),
            };
            let mut q = SymbolQuery::new(req.name.clone());
            q.kinds = kinds.clone();
            q.limit = limit;
            // Positions in the main tree name a file below a mount only for that namespace.
            q.at = at.as_ref().and_then(|(p, l)| {
                if t.prefix.is_empty() {
                    Some((p.clone(), *l))
                } else if under(p, &t.prefix) && p.len() > t.prefix.len() {
                    Some((p[t.prefix.len() + 1..].to_vec(), *l))
                } else {
                    None
                }
            });
            let op = req.op;
            let got = tokio::task::spawn_blocking(move || {
                let engine = SymbolEngine::new(&snap, &files, &NoProjects);
                match op {
                    ci::SymbolOp::Def => engine.def(&kept, &q),
                    ci::SymbolOp::Refs => engine.refs(&kept, &q),
                    ci::SymbolOp::Callers => engine.callers(&kept, &q),
                    ci::SymbolOp::Find => engine.find(&kept, &q),
                    ci::SymbolOp::Outline => Ok(symbols::Symbols::default()),
                }
            })
            .await
            .map_err(|e| internal("symbols task", &e))?
            .map_err(symbol_status)?;
            truncated |= got.truncated;
            for mut s in got.symbols {
                if !plan.langs.is_empty()
                    && !lang::detect(&String::from_utf8_lossy(&s.path), b"")
                        .is_some_and(|l| plan.langs.contains(&l))
                {
                    continue;
                }
                s.path = t.out_path(&s.path);
                all.push(s);
            }
        }
        let more = all.len() > offset + want;
        let page: Vec<symbols::Symbol> = all.into_iter().skip(offset).take(want).collect();
        let next = if more && offset + want < MAX_SYMBOL_DEPTH {
            self.seal(
                KIND_SYMBOLS,
                &SymbolPage {
                    digest,
                    generations: gens.clone(),
                    offset: u32::try_from(offset + want).unwrap_or(u32::MAX),
                },
            )?
        } else {
            String::new()
        };
        let rendered = render::render_symbols(&ctx, budget, &page, truncated || (more && next.is_empty()));
        let coverage = if covered.1 == 0 {
            1.0
        } else {
            covered.0 as f64 / covered.1 as f64
        };
        Ok(wire::SymbolsResponse {
            scope: Some(self.resolved(caller, &plan, gens[0], coverage).await?),
            symbols: rendered.symbols.into_iter().map(symbol_out).collect(),
            truncated: rendered.truncated,
            next_page_token: next,
            est_tokens: rendered.est_tokens,
        })
    }

    /// `outline`: the tags of one file, no index. The file belongs to the deepest namespace
    /// whose mount holds the path.
    async fn outline(
        &self,
        plan: &Plan,
        path: &RepoPath,
        kinds: &[TagKind],
        limit: usize,
    ) -> Result<(Vec<symbols::Symbol>, bool), Status> {
        let p = path.as_bytes();
        let t = plan
            .targets
            .iter()
            .filter(|t| t.prefix.is_empty() || (under(p, &t.prefix) && p.len() > t.prefix.len()))
            .max_by_key(|t| t.prefix.len())
            .ok_or_else(|| Status::not_found("no such file"))?;
        let rel = if t.prefix.is_empty() {
            p.to_vec()
        } else {
            p[t.prefix.len() + 1..].to_vec()
        };
        if !path_allowed(&rel, &t.include, &t.exclude) {
            return Err(Status::not_found("no such file"));
        }
        let rel_str = String::from_utf8_lossy(&rel).into_owned();
        let Some(bytes) =
            read_blob(t.entry.backend.engine(), t.root, &rel_str, skip::MAX_BYTES as u64).await?
        else {
            return Err(Status::not_found("no such file"));
        };
        let Some(l) = lang::detect(&rel_str, &bytes) else {
            return Ok((Vec::new(), false));
        };
        let secret = Some(secret_of(&bytes) || asc_codeintel::secrets::path_is_secret(&rel_str));
        let out_path = path.as_bytes().to_vec();
        let kinds = kinds.to_vec();
        let got = tokio::task::spawn_blocking(move || {
            symbols::outline(&out_path, secret, l, &bytes, &kinds, limit)
        })
        .await
        .map_err(|e| internal("outline task", &e))?
        .map_err(symbol_status)?;
        Ok((got.symbols, got.truncated))
    }

    // -------------------------------------------------------------------------- Semantic

    pub async fn semantic(
        &self,
        caller: &Caller,
        req: ci::SemanticRequest,
    ) -> Result<wire::SemanticResponse, Status> {
        let _slot = self
            .semantic
            .clone()
            .try_acquire_owned()
            .map_err(|_| Status::resource_exhausted("too many semantic queries in flight"))?;
        let Some(embed) = self.embed.clone() else {
            return Err(Status::failed_precondition(
                "semantic search is not available on this server",
            ));
        };
        let plan = self.plan(caller, &req.scope).await?;
        let k = match req.k {
            0 => DEFAULT_K,
            k => k,
        }
        .clamp(1, ci::MAX_K) as usize;
        let hybrid_mode = req.mode == ci::SemanticMode::Hybrid;
        let spec = embed.spec().clone();
        let calib_hex = model_hex(&spec.calib);
        let mut found: Vec<(f64, usize, RawChunk)> = Vec::new();
        let mut ann_exhausted = false;
        let mut covered = (0u64, 0u64);
        let mut generation = 0u64;
        for (ti, t) in plan.targets.iter().enumerate() {
            let gens = ModelGens::new(self.pool().clone(), t.entry.objects.clone());
            let ns = t.docs.ns().to_vec();
            let row = gens.load(&ns).await.map_err(ci_status)?;
            let snap = self
                .snaps
                .current(&*t.entry.objects, &t.nsn, IndexKind::Files)
                .await?;
            if ti == 0 {
                generation = snap.generation;
            }
            let searcher = self.searcher(t);
            let kept = self.kept(t, &searcher).await?;
            covered.0 += (&kept & &snap.indexed).len();
            covered.1 += kept.len();
            if row.serving == 0 || kept.is_empty() {
                continue;
            }
            let Some(entry) = row
                .state
                .models
                .iter()
                .find(|m| m.model_gen == row.serving)
            else {
                continue;
            };
            if entry.model_id != spec.model_id || entry.calib != calib_hex {
                tracing::warn!(repo = %t.name, "the server's embedding model is not the serving one");
                continue;
            }
            let q = self.query_vector(&embed, row.serving, &req.query).await?;
            let mut segs = Vec::new();
            for s in row.state.segments.iter().filter(|s| s.model_gen == row.serving) {
                segs.push(self.vecs.get(&gens, &ns, s).await.map_err(ci_status)?);
            }
            let counts = t.docs.counts().await.map_err(sql_status)?;
            let est = if counts.docs == 0 {
                0
            } else {
                u64::try_from(
                    u128::from(counts.chunks) * u128::from(kept.len()) / u128::from(counts.docs),
                )
                .unwrap_or(u64::MAX)
            };
            let sel = Selectivity {
                kept_chunks: est,
                ns_chunks: counts.chunks,
            };
            let k_vec = if hybrid_mode { (k * 4).clamp(20, MAX_LIST) } else { k };
            let model_ref = spec.model_ref(row.serving);
            let outcome = {
                let kept = kept.clone();
                tokio::task::spawn_blocking(move || {
                    let refs: Vec<&vectors::VectorSegment> = segs.iter().map(|s| &**s).collect();
                    vectors::search(&refs, &model_ref, &q, k_vec.min(vectors::MAX_K), &kept, sel)
                })
                .await
                .map_err(|e| internal("vector task", &e))?
                .map_err(|e| internal("vector search", &e))?
            };
            ann_exhausted |= outcome.ann_exhausted;
            let vec_list: Vec<u64> = outcome.hits.iter().map(|h| h.key).collect();
            let scored: Vec<(u64, f64, Vec<&'static str>)> = if hybrid_mode {
                let bm = self.bm25_list(t, &req.query, &kept).await?;
                let sym = self.symbol_list(t, &searcher, &snap, &req.query, &kept).await?;
                let fused = hybrid::hybrid(
                    &req.query,
                    &bm,
                    &vec_list,
                    &sym,
                    Some(&kept),
                    HybridOptions { k, rerank: false },
                    None,
                )
                .map_err(|e| internal("hybrid", &e))?;
                fused
                    .into_iter()
                    .map(|f| {
                        let mut src = Vec::new();
                        if bm.contains(&f.key) {
                            src.push("bm25");
                        }
                        if vec_list.contains(&f.key) {
                            src.push("vector");
                        }
                        if sym.contains(&f.key) {
                            src.push("symbol");
                        }
                        (f.key, f.score, src)
                    })
                    .collect()
            } else {
                outcome
                    .hits
                    .iter()
                    .map(|h| (h.key, f64::from(h.score), vec!["vector"]))
                    .collect()
            };
            for (key, score, sources) in scored {
                if let Some(c) = self.raw_chunk(t, &searcher, &kept, key, score, sources).await? {
                    found.push((score, ti, c));
                }
            }
        }
        found.sort_by(|a, b| b.0.total_cmp(&a.0).then(a.1.cmp(&b.1)));
        let more = found.len() > k;
        found.truncate(k);
        let ids = IdCodec::new(caller.id_mode, &self.registry, &plan.targets[0].entry)
            .for_viewer(&caller.principal);
        let ctx = RenderCtx {
            repo: caller.repo.to_string(),
            changeset: ids.changeset_out(plan.targets[0].cs).await?.to_vec(),
        };
        let raws: Vec<RawChunk> = found.into_iter().map(|f| f.2).collect();
        let rendered = render::render_chunks(&ctx, render_budget(&req.budget), &raws, more);
        let coverage = if covered.1 == 0 {
            1.0
        } else {
            covered.0 as f64 / covered.1 as f64
        };
        Ok(wire::SemanticResponse {
            scope: Some(self.resolved(caller, &plan, generation, coverage).await?),
            chunks: rendered
                .chunks
                .into_iter()
                .map(|c| wire::Chunk {
                    path: Bytes::from(c.path),
                    file_id: Bytes::copy_from_slice(&c.file_id),
                    start_line: c.start_line,
                    end_line: c.end_line,
                    symbol: c.symbol,
                    score: c.score,
                    sources: c.sources,
                    text: Bytes::from(c.text),
                    redacted: c.redacted,
                })
                .collect(),
            model: spec.model_id,
            est_tokens: rendered.est_tokens,
            ann_exhausted,
        })
    }

    /// The query embedding for generation `model_gen` (LRU keyed by generation and calibration).
    async fn query_vector(
        &self,
        embed: &Arc<dyn EmbedModel>,
        model_gen: u32,
        text: &str,
    ) -> Result<Vec<i8>, Status> {
        let calib = embed.spec().calib;
        if let Some(v) = self.qlru.get(model_gen, &calib, text) {
            return Ok(v.iter().map(|x| *x as i8).collect());
        }
        let (e, t) = (embed.clone(), text.to_owned());
        let mut got = tokio::task::spawn_blocking(move || e.embed(&[t]))
            .await
            .map_err(|e| internal("embed task", &e))?
            .map_err(|e| internal("embedding", &e))?;
        let v = got
            .pop()
            .filter(|v| v.len() == embed.spec().dims)
            .ok_or_else(|| internal("embedding", &"wrong vector count or size"))?;
        self.qlru
            .put(model_gen, &calib, text, v.iter().map(|x| f32::from(*x)).collect());
        Ok(v)
    }

    /// BM25 over the chunks index in the kept scope, as chunk keys.
    async fn bm25_list(
        &self,
        t: &Target,
        query: &str,
        kept: &RoaringBitmap,
    ) -> Result<Vec<u64>, Status> {
        let snap = self
            .snaps
            .current(&*t.entry.objects, &t.nsn, IndexKind::Chunks)
            .await?;
        let (query, kept) = (query.to_owned(), kept.clone());
        tokio::task::spawn_blocking(move || {
            let searchers: Vec<_> = snap.segments.iter().map(|s| s.reader.searcher()).collect();
            hybrid::bm25(&searchers, &ChunksSchema::build(), &query, Some(&kept), MAX_LIST)
        })
        .await
        .map_err(|e| internal("bm25 task", &e))?
        .map_err(|e| internal("bm25", &e))
    }

    /// Chunk keys of the exact definitions of the query's identifiers in the kept scope.
    async fn symbol_list(
        &self,
        t: &Target,
        searcher: &Arc<NsSearcher>,
        snap: &Arc<asc_codeintel::index::segments::Snapshot>,
        query: &str,
        kept: &RoaringBitmap,
    ) -> Result<Vec<u64>, Status> {
        let idents: Vec<String> = hybrid::query_identifiers(query)
            .into_iter()
            .take(MAX_SYM_IDENTS)
            .collect();
        if idents.is_empty() {
            return Ok(Vec::new());
        }
        let handle = tokio::runtime::Handle::current();
        let files = ServerFiles {
            handle: handle.clone(),
            searcher: searcher.clone(),
            engine: t.entry.backend.engine().clone(),
            objects: t.entry.objects.clone(),
            docs: t.docs.clone(),
            root: t.root,
            indexed: snap.indexed.clone(),
            info: Mutex::new(HashMap::new()),
        };
        let (snap, kept) = (snap.clone(), kept.clone());
        let ns = t.docs.ns().to_vec();
        tokio::task::spawn_blocking(move || -> Result<Vec<u64>, Status> {
            let engine = SymbolEngine::new(&snap, &files, &NoProjects);
            let mut keys = Vec::new();
            for id in idents {
                let mut q = SymbolQuery::new(id.clone());
                q.limit = MAX_SYM_HITS;
                let Ok(found) = engine.def(&kept, &q) else {
                    continue;
                };
                for s in found.symbols.into_iter().filter(|s| s.name == id) {
                    let Ok(p) = RepoPath::new(s.path.clone()) else {
                        continue;
                    };
                    let Ok(Some(e)) = handle.block_on(files.engine.lookup(files.root, &p)) else {
                        continue;
                    };
                    let file = FileId::from_bytes(e.id);
                    let Some(ord) = asc_codeintel::scope::OrdSource::get(&*files.docs, &file) else {
                        continue;
                    };
                    let Ok(Some(rows)) =
                        handle.block_on(read_chunk_rows(&*files.objects, &ns, &file))
                    else {
                        continue;
                    };
                    if let Some(i) = rows
                        .iter()
                        .position(|r| r.start <= s.line && s.line <= r.end)
                    {
                        keys.push(chunk_key(ord, u32::try_from(i).unwrap_or(u32::MAX)));
                    }
                }
            }
            Ok(keys)
        })
        .await
        .map_err(|e| internal("symbol list task", &e))?
    }

    /// The chunk behind `key` with its text from the file's verified bytes. `None` drops it:
    /// the doc is not in the kept scope, or no kept path holds it, or its bytes are gone.
    async fn raw_chunk(
        &self,
        t: &Target,
        searcher: &NsSearcher,
        kept: &RoaringBitmap,
        key: u64,
        score: f64,
        sources: Vec<&'static str>,
    ) -> Result<Option<RawChunk>, Status> {
        let ord = doc_ord_of(key);
        // The last line of defence for the leakage rule: nothing outside the scope surfaces.
        if !kept.contains(ord) {
            return Ok(None);
        }
        let Some(file) = t.docs.file(ord).await.map_err(sql_status)? else {
            return Ok(None);
        };
        let info = t.docs.info(&[ord]).await.map_err(sql_status)?;
        let Some(info) = info.get(&ord) else {
            return Ok(None);
        };
        let paths = searcher
            .scope_engine()
            .locate(t.root, ord, 4 * search::MAX_PATHS)
            .await
            .map_err(scope_status)?;
        let Some(path) = paths
            .iter()
            .find(|p| path_allowed(p.as_bytes(), &t.include, &t.exclude))
        else {
            return Ok(None);
        };
        let Ok(Some(rows)) = read_chunk_rows(&*t.entry.objects, t.docs.ns(), &file).await else {
            return Ok(None);
        };
        let Some(row) = rows.get(chunk_of(key) as usize) else {
            return Ok(None);
        };
        let Ok(Some(bytes)) = file_bytes(&*t.entry.objects, &file).await else {
            return Ok(None);
        };
        let body = bytes.strip_suffix(b"\n").unwrap_or(&bytes);
        let lines: Vec<&[u8]> = body.split(|&b| b == b'\n').collect();
        let from = (row.start as usize).saturating_sub(1).min(lines.len());
        let to = (row.end as usize).clamp(from, lines.len());
        let text = lines[from..to].join(&b'\n');
        Ok(Some(RawChunk {
            path: t.out_path(path.as_bytes()),
            file,
            start_line: row.start,
            end_line: row.end,
            symbol: row.symbol.clone(),
            score,
            sources: sources.into_iter().map(str::to_owned).collect(),
            text,
            flagged: info.secret() != Some(false),
        }))
    }

    // -------------------------------------------------------------------------- IndexStatus

    pub async fn index_status(
        &self,
        caller: &Caller,
        req: ci::IndexStatusRequest,
    ) -> Result<wire::IndexStatusResponse, Status> {
        let plan = self.plan(caller, &req.scope).await?;
        let versions = current_versions();
        let version = format!(
            "grams{}.tags{}.chunker{}",
            versions.grams, versions.tags, versions.chunker
        );
        let mut indexes = Vec::new();
        let mut main = (0u64, 1.0f64);
        for (ti, t) in plan.targets.iter().enumerate() {
            let searcher = self.searcher(t);
            let kept = self.kept(t, &searcher).await?;
            let counts = t.docs.counts().await.map_err(sql_status)?;
            let files = self
                .snaps
                .current(&*t.entry.objects, &t.nsn, IndexKind::Files)
                .await?;
            let coverage = if kept.is_empty() {
                1.0
            } else {
                (&kept & &files.indexed).len() as f64 / kept.len() as f64
            };
            if ti == 0 {
                main = (files.generation, coverage);
            }
            indexes.push(wire::IndexStatus {
                index: t.label("files"),
                coverage: coverage as f32,
                lag_seconds: counts.lag_secs,
                generation: files.generation,
                version: version.clone(),
            });
            let chunks = self
                .snaps
                .current(&*t.entry.objects, &t.nsn, IndexKind::Chunks)
                .await?;
            // Files and chunks are published together, so the same ords are covered.
            indexes.push(wire::IndexStatus {
                index: t.label("chunks"),
                coverage: coverage as f32,
                lag_seconds: counts.lag_secs,
                generation: chunks.generation,
                version: version.clone(),
            });
            let gens = ModelGens::new(self.pool().clone(), t.entry.objects.clone());
            let row = gens.load(t.docs.ns()).await.map_err(ci_status)?;
            let vectors: u64 = row
                .state
                .segments
                .iter()
                .filter(|s| s.model_gen == row.serving)
                .map(|s| s.vectors)
                .sum();
            let vec_cov = if row.serving == 0 {
                0.0
            } else if counts.chunks == 0 {
                1.0
            } else {
                (vectors as f64 / counts.chunks as f64).min(1.0)
            };
            let model = row
                .state
                .models
                .iter()
                .find(|m| m.model_gen == row.serving)
                .map(|m| m.model_id.clone())
                .unwrap_or_default();
            indexes.push(wire::IndexStatus {
                index: t.label("vectors"),
                coverage: vec_cov as f32,
                lag_seconds: counts.lag_secs,
                generation: u64::try_from(row.generation).unwrap_or(0),
                version: model,
            });
        }
        Ok(wire::IndexStatusResponse {
            scope: Some(self.resolved(caller, &plan, main.0, main.1).await?),
            indexes,
        })
    }
}

// ------------------------------------------------------------------------------ helpers

/// True when the pattern has no trigram, so the search scans the scope.
fn is_scan(pattern: &str, mode: Mode, case_insensitive: bool) -> bool {
    let planned = match mode {
        Mode::Literal => plan::plan_literal(pattern, case_insensitive),
        Mode::Word => plan::plan_regex(
            &format!(r"\b(?:{})\b", regex::escape(pattern)),
            case_insensitive,
        ),
        Mode::Regex => plan::plan_regex(pattern, case_insensitive),
    };
    planned.is_ok_and(|p| p.is_all())
}

fn render_budget(b: &ci::Budget) -> render::Budget {
    render::Budget {
        max_results: b.max_results,
        max_bytes: b.max_bytes,
        max_tokens: b.max_tokens,
    }
}

/// The result count the renderer will use, so no more hits are prepared than can be shown.
fn budget_results(b: &ci::Budget) -> usize {
    (match b.max_results {
        0 => render::DEFAULT_RESULTS,
        n => n.min(render::MAX_RESULTS),
    }) as usize
}

/// Up to `n` lines before and after the 1-based `line`, each cut at the renderer's input cap.
fn context_of(bytes: &[u8], line: u32, n: usize) -> (Vec<Vec<u8>>, Vec<Vec<u8>>) {
    let body = bytes.strip_suffix(b"\n").unwrap_or(bytes);
    let lines: Vec<&[u8]> = body.split(|&b| b == b'\n').collect();
    let idx = (line as usize).saturating_sub(1);
    let cut = |l: &[u8]| l[..l.len().min(render::MAX_LINE_IN)].to_vec();
    let before = lines
        .get(idx.saturating_sub(n)..idx.min(lines.len()))
        .unwrap_or_default()
        .iter()
        .map(|l| cut(l))
        .collect();
    let after = lines
        .get((idx + 1).min(lines.len())..(idx + 1 + n).min(lines.len()))
        .unwrap_or_default()
        .iter()
        .map(|l| cut(l))
        .collect();
    (before, after)
}

fn symbol_out(s: render::SymbolOut) -> wire::Symbol {
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

// ------------------------------------------------------------------------------ service

/// The `asc.v1.CodeIntel` tonic service.
pub struct CodeIntelService {
    rpc: Arc<CodeIntelRpc>,
    authorizer: Arc<dyn Authorizer>,
    info: ServerInfo,
}

impl CodeIntelService {
    pub fn new(rpc: Arc<CodeIntelRpc>, authorizer: Arc<dyn Authorizer>, info: ServerInfo) -> Self {
        Self {
            rpc,
            authorizer,
            info,
        }
    }

    /// The tonic service with message limits, zstd request decoding and [`AuthInterceptor`].
    pub fn into_service(self) -> InterceptedService<CodeIntelServer<Self>, AuthInterceptor> {
        let server = CodeIntelServer::new(self)
            .max_decoding_message_size(MAX_MESSAGE)
            .max_encoding_message_size(MAX_MESSAGE)
            .accept_compressed(tonic::codec::CompressionEncoding::Zstd);
        InterceptedService::new(server, AuthInterceptor)
    }

    /// Validates the repository, checks the client version and authorises `Read` on it. This
    /// is the first thing every method does, before the request body is looked at further.
    async fn caller(
        &self,
        meta: &MetadataMap,
        ext: &TonicExt,
        scope: Option<&wire::Scope>,
    ) -> Result<Caller, Status> {
        let repo = scope
            .and_then(|s| s.repo.as_ref())
            .ok_or_else(|| bad("missing repo"))?;
        let name = RepoName::parse(&repo.name)?;
        let id_mode = IdMode::from_wire(repo.id_mode).map_err(rpc_error)?;
        let client = match meta.get("x-asc-client") {
            None => None,
            Some(v) => {
                let text = v.to_str().map_err(|_| bad("malformed x-asc-client"))?;
                ClientVersion::from_header(text)?
            }
        };
        if let (Some(min), Some(got)) = (self.info.min_client, client)
            && got < min
        {
            let mut info = asc_rpc::errors::ErrorInfo::default();
            info.paths.push(format!("minimum client version is {min}"));
            return Err(asc_rpc::server::rpc_status(&RpcError::ClientTooOld(info)));
        }
        let credential = ext.get::<Credential>().cloned();
        let principal = self
            .authorizer
            .authorize_credential(credential.as_ref(), name.as_str(), Access::Read)
            .await?;
        Ok(Caller {
            authorizer: self.authorizer.clone(),
            credential,
            principal,
            repo: name,
            id_mode,
        })
    }
}

type SearchStream =
    Pin<Box<dyn futures::Stream<Item = Result<wire::SearchResponse, Status>> + Send + 'static>>;

#[tonic::async_trait]
impl CodeIntel for CodeIntelService {
    type SearchStream = SearchStream;

    async fn search(
        &self,
        request: Request<wire::SearchRequest>,
    ) -> Result<Response<Self::SearchStream>, Status> {
        let (meta, ext, msg) = request.into_parts();
        let caller = self.caller(&meta, &ext, msg.scope.as_ref()).await?;
        let req = ci::SearchRequest::try_from(&msg).map_err(rpc_error)?;
        let resp = self.rpc.search(&caller, req).await?;
        Ok(Response::new(Box::pin(futures::stream::once(async move {
            Ok(resp)
        }))))
    }

    async fn symbols(
        &self,
        request: Request<wire::SymbolsRequest>,
    ) -> Result<Response<wire::SymbolsResponse>, Status> {
        let (meta, ext, msg) = request.into_parts();
        let caller = self.caller(&meta, &ext, msg.scope.as_ref()).await?;
        let req = ci::SymbolsRequest::try_from(&msg).map_err(rpc_error)?;
        self.rpc.symbols(&caller, req).await.map(Response::new)
    }

    async fn semantic(
        &self,
        request: Request<wire::SemanticRequest>,
    ) -> Result<Response<wire::SemanticResponse>, Status> {
        let (meta, ext, msg) = request.into_parts();
        let caller = self.caller(&meta, &ext, msg.scope.as_ref()).await?;
        let req = ci::SemanticRequest::try_from(&msg).map_err(rpc_error)?;
        self.rpc.semantic(&caller, req).await.map(Response::new)
    }

    async fn index_status(
        &self,
        request: Request<wire::IndexStatusRequest>,
    ) -> Result<Response<wire::IndexStatusResponse>, Status> {
        let (meta, ext, msg) = request.into_parts();
        let caller = self.caller(&meta, &ext, msg.scope.as_ref()).await?;
        let req = ci::IndexStatusRequest::try_from(&msg).map_err(rpc_error)?;
        self.rpc.index_status(&caller, req).await.map(Response::new)
    }
}
