//! Decoded read RPCs (PLAN U47, SPEC §7, §16.4).
//!
//! [`ServerReads`] answers `ReadChangeset`, `ReadTree`, `ReadFile`, `Stat`, `LastCommits`,
//! `CheckAttr`, `Grep`, `Archive` and `Blame`; [`ServerHistory`] answers `Resolve`, `GitMap`,
//! `MergeBase`, `IsAncestor`, `Log` and `Diff`. Every response streams, whole-file buffers go
//! through the [`TextBudget`](crate::TextBudget), and ids are translated by [`IdCodec`] when the
//! caller speaks `id_mode = GIT`.

use std::collections::HashMap;
use std::future::Future;
use std::io::{self, Write};
use std::ops::Range;
use std::sync::{Arc, RwLock};

use async_trait::async_trait;
use bytes::Bytes;
use asc_core::codec::decode_envelope;
use asc_core::content::{CHUNK_THRESHOLD, ChunkAssembler};
use asc_core::id::{ChangesetId, ChunkId, FileId, GitOid, TreeId};
use asc_core::object::{Change, ChunkList, Entry, Kind, Tree};
use asc_core::path::{MAX_TREE_DEPTH, Name, RepoPath};
use asc_engine::blame::{self, BlameApi};
use asc_engine::commit;
use asc_engine::derive::Gen;
use asc_engine::diff::{
    self, DiffCtx, DiffOptions, NoAttributes, RenameOptions, TreeChange, format as diff_format,
};
use asc_engine::graph::{self, LogQuery};
use asc_engine::history::{HistoryApi, LOG_PAGE, PageToken};
use asc_engine::tree::{self, MAX_WALK_PATH_BYTES, Pathspec};
use asc_rpc::IdMode;
use asc_rpc::server::{
    CallCtx, HistoryHandler, MAX_LIST, MAX_STRING, ReadsHandler, RpcStream, wire,
};
use asc_store::key::ObjKey;
use asc_store::objects::{self, Objects};
use futures::stream::{self, StreamExt, TryStreamExt};
use tokio::runtime::Handle;
use tokio::sync::mpsc;
use tonic::Status;

use crate::idmode::{Eng, IdCodec};
use crate::mainline::{MainlineIndex, SqlMainline};
use crate::registry::{Extensions, Registry, RegistryError, RepoEntry};
use crate::{TextBudgetSlot, TextGuard};

/// Entries in one `ReadTree` message.
const TREE_CHUNK: usize = 512;
/// Most entries one `ReadTree` call streams when `limit` is 0.
const MAX_TREE_ENTRIES: u64 = 1_000_000;
/// Bytes of file content in one `ReadFile` or `Archive` message.
const DATA_MSG: usize = 1 << 20;
/// Grep skips files above this size.
const GREP_MAX_FILE: u64 = 1 << 20;
/// Longest matching line text a `GrepMatch` carries.
const GREP_MAX_LINE: usize = 4096;
/// Grep matches in one message.
const GREP_CHUNK: usize = 256;
/// Matches returned when the request sets no limit, and the most it may ask for.
const GREP_DEFAULT: u32 = 1_000;
const GREP_MAX: u32 = 100_000;
/// Changed files a `Diff` reports when the request sets no limit, and the most it may ask for.
const DIFF_DEFAULT_FILES: usize = 10_000;
const DIFF_MAX_FILES: usize = 100_000;
/// `PathChange`s in one `DiffResponse`, and text bytes in one.
const DIFF_CHANGES_CHUNK: usize = 1_000;
const DIFF_TEXT_CHUNK: usize = 256 * 1024;
/// Names one `LastCommits` or paths one `Stat` or `CheckAttr` call may carry.
const MAX_NAMES: usize = MAX_LIST;
/// Attribute names one `CheckAttr` call may ask for.
const MAX_ATTR_NAMES: usize = 64;
/// Largest `.gitattributes` the server reads.
const ATTR_MAX_FILE: u64 = 1 << 20;
/// Largest file an `export-subst` expansion buffers.
const SUBST_MAX_FILE: u64 = 1 << 20;
/// Largest expanded `export-subst` file. A placeholder such as `%B` can expand to far more than
/// it spells, so the output is capped; past the cap the archive fails with `resource_exhausted`.
const SUBST_MAX_OUT: u64 = 4 * SUBST_MAX_FILE;
/// Largest symlink target an archive reads.
const SYMLINK_MAX: u64 = 4096;
/// Directory entries a `LastCommits` walk tracks, and commits it visits.
const LASTS_MAX_ENTRIES: usize = 20_000;
const LASTS_MAX_STEPS: usize = 20_000;
/// `LastCommits` walks kept (per tip and directory).
const LASTS_CACHE: usize = 256;
/// Backlog of response messages between a producer task and the client.
const STREAM_DEPTH: usize = 4;
/// Changed paths `ReadChangeset` lists.
const MAX_CHANGES: usize = 10_000;
/// Concurrent object reads in one batch.
const FANOUT: usize = 16;
/// Files larger than this are binary (`core.bigFileThreshold`, SPEC §16.4).
const BIG_FILE: u64 = 512 * 1024 * 1024;

// ------------------------------------------------------------------------------ statuses

fn internal(context: &str, e: &dyn std::fmt::Display) -> Status {
    tracing::error!(%context, error = %e, "internal error");
    Status::internal("internal error")
}

fn data_loss(context: &str, e: &dyn std::fmt::Display) -> Status {
    tracing::error!(%context, error = %e, "stored data is damaged or missing");
    Status::data_loss("stored object is corrupt or missing")
}

fn registry_status(e: RegistryError) -> Status {
    match e {
        RegistryError::NotFound(_) => Status::not_found("no such repository"),
        e => internal("registry", &e),
    }
}

fn commit_status(e: commit::Error) -> Status {
    match e {
        commit::Error::MissingChangeset(id) => {
            Status::not_found(format!("changeset {id} is missing"))
        }
        commit::Error::Store(e) => store_status(e),
        e => internal("changeset", &e),
    }
}

fn store_status(e: objects::Error) -> Status {
    match e {
        objects::Error::Corrupt(_) | objects::Error::Codec(..) => data_loss("object", &e),
        e => internal("object store", &e),
    }
}

fn tree_status(e: tree::Error) -> Status {
    match e {
        tree::Error::MissingTree(_) | tree::Error::Codec(_) => data_loss("tree", &e),
        tree::Error::TooLarge(_) | tree::Error::OutputTooLarge(_) => {
            Status::resource_exhausted(e.to_string())
        }
        tree::Error::PathTooLong(_) | tree::Error::TooDeep => Status::out_of_range(e.to_string()),
        tree::Error::Store(e) => store_status(e),
        e => internal("tree", &e),
    }
}

fn diff_status(e: diff::Error) -> Status {
    match e {
        diff::Error::Tree(e) => tree_status(e),
        diff::Error::Store(e) => store_status(e),
        diff::Error::MissingFile(_) | diff::Error::Codec(_) => data_loss("file", &e),
        diff::Error::FileTooLarge(..) => Status::resource_exhausted(e.to_string()),
        e => internal("diff", &e),
    }
}

fn graph_status(e: graph::Error) -> Status {
    match e {
        graph::Error::Missing(id) => Status::not_found(format!("changeset {id} is missing")),
        graph::Error::Query(m) => Status::invalid_argument(m),
        graph::Error::Remote(m) => Status::invalid_argument(m),
        graph::Error::Grep(e) => Status::invalid_argument(e.to_string()),
        graph::Error::Unsupported(m) => Status::unimplemented(m),
        graph::Error::Engine(e) => commit_status(e),
        e => internal("history", &e),
    }
}

fn exhausted(_: crate::TextBudgetExhausted) -> Status {
    Status::resource_exhausted("text memory budget exhausted; retry shortly")
}

// ------------------------------------------------------------------------------ hooks

/// Fills in `LogEntry` fields other units own (`change`, `meta`, `stats`; SPEC §20.4).
///
/// Applied to every `LogEntry` the read RPCs build and to `BlameResponse.commits`. `ids` are the
/// entries' changeset ids in asc form, in order, whatever the caller's id mode.
#[async_trait]
pub trait LogEntryEnricher: Send + Sync {
    async fn enrich(
        &self,
        repo: &RepoEntry,
        ids: &[ChangesetId],
        entries: &mut [wire::LogEntry],
    ) -> Result<(), Status>;
}

/// What later units plug into the read RPCs.
#[derive(Default)]
pub struct ReadHooks {
    enrichers: RwLock<Vec<Arc<dyn LogEntryEnricher>>>,
    /// The budget for whole-file buffers made by the read RPCs. Until it is set the budget is
    /// unlimited; U69 installs the real one here and in `Extensions::text_budget`.
    pub text_budget: TextBudgetSlot,
}

impl ReadHooks {
    /// Adds an enricher; they run in registration order.
    pub fn add_enricher(&self, e: Arc<dyn LogEntryEnricher>) {
        self.enrichers
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(e);
    }

    fn enrichers(&self) -> Vec<Arc<dyn LogEntryEnricher>> {
        self.enrichers
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

// ------------------------------------------------------------------------------ streams

/// The sending half of a producer task's stream.
struct Out<T>(mpsc::Sender<Result<T, Status>>);

impl<T> Out<T> {
    async fn send(&self, v: T) -> Result<(), Status> {
        self.0
            .send(Ok(v))
            .await
            .map_err(|_| Status::cancelled("the client went away"))
    }
}

/// Runs `f` on its own task and streams what it sends. A bounded backlog keeps a slow client from
/// making the producer buffer; dropping the stream stops the producer at its next send.
fn spawn_stream<T, F, Fut>(f: F) -> RpcStream<T>
where
    T: Send + 'static,
    F: FnOnce(Out<T>) -> Fut + Send + 'static,
    Fut: Future<Output = Result<(), Status>> + Send + 'static,
{
    let (tx, rx) = mpsc::channel(STREAM_DEPTH);
    let err = tx.clone();
    tokio::spawn(async move {
        if let Err(s) = f(Out(tx)).await {
            let _ = err.send(Err(s)).await;
        }
    });
    stream::unfold(rx, |mut rx| async move { rx.recv().await.map(|v| (v, rx)) }).boxed()
}

fn pathspec(items: &[String]) -> Result<Option<Pathspec>, Status> {
    if items.len() > MAX_LIST || items.iter().any(|p| p.len() > MAX_STRING) {
        return Err(Status::invalid_argument("too many or too long paths"));
    }
    if items.is_empty() {
        return Ok(None);
    }
    Pathspec::parse(items)
        .map(Some)
        .map_err(|e| Status::invalid_argument(e.to_string()))
}

fn repo_path(bytes: &[u8]) -> Result<RepoPath, Status> {
    if bytes.len() > MAX_WALK_PATH_BYTES {
        return Err(Status::invalid_argument("path is too long"));
    }
    RepoPath::new(bytes).map_err(|e| Status::invalid_argument(e.to_string()))
}

/// `path` without a trailing slash, or `None` for the root.
fn dir_path(bytes: &[u8]) -> Result<Option<RepoPath>, Status> {
    let trimmed = bytes.strip_suffix(b"/").unwrap_or(bytes);
    if trimmed.is_empty() {
        Ok(None)
    } else {
        repo_path(trimmed).map(Some)
    }
}

fn child(prefix: Option<&RepoPath>, name: &Name) -> Result<RepoPath, Status> {
    let path = match prefix {
        Some(p) => p.join(name),
        None => RepoPath::from(name.clone()),
    };
    if path.as_bytes().len() > MAX_WALK_PATH_BYTES {
        return Err(Status::out_of_range(
            "a path is longer than the server allows",
        ));
    }
    Ok(path)
}

fn looks_binary(data: &[u8]) -> bool {
    data[..data.len().min(8000)].contains(&0)
}

fn sig(s: &asc_core::sig::Signature) -> wire::Signature {
    wire::Signature::from(s)
}

// ------------------------------------------------------------------------------ per call

/// A known generation and git commit oid for a changeset.
type Hint = Option<(u32, Option<GitOid>)>;
/// A changeset to build a `LogEntry` for, with what is already known of it.
type LogItem = (ChangesetId, Hint);

/// One repository opened for one call.
struct Call {
    entry: Arc<RepoEntry>,
    ids: IdCodec,
    engine: Arc<Eng>,
    hooks: Arc<ReadHooks>,
}

struct Core {
    registry: Arc<Registry>,
    hooks: Arc<ReadHooks>,
}

impl Core {
    async fn call(&self, ctx: &CallCtx) -> Result<Arc<Call>, Status> {
        let entry = self
            .registry
            .open(&ctx.repo)
            .await
            .map_err(registry_status)?;
        Ok(Arc::new(Call {
            ids: IdCodec::new(ctx.id_mode, &self.registry, &entry),
            engine: entry.backend.engine().clone(),
            entry,
            hooks: self.hooks.clone(),
        }))
    }
}

impl Call {
    fn mode(&self) -> IdMode {
        self.ids.mode()
    }

    /// The changeset a revision names.
    async fn rev(&self, spec: &str) -> Result<ChangesetId, Status> {
        if spec.len() > MAX_STRING {
            return Err(Status::invalid_argument("revision is too long"));
        }
        let spec = if spec.is_empty() { "HEAD" } else { spec };
        let spec = self.ids.rev_in(spec, &self.entry.default_branch).await?;
        let got = self
            .engine
            .resolve(std::slice::from_ref(&spec))
            .await
            .map_err(graph_status)?;
        got.into_iter()
            .next()
            .flatten()
            .ok_or_else(|| Status::not_found(format!("unknown revision {spec:?}")))
    }

    async fn changeset(&self, id: ChangesetId) -> Result<Arc<asc_core::object::Changeset>, Status> {
        self.engine.get_changeset(id).await.map_err(commit_status)
    }

    async fn root(&self, id: ChangesetId) -> Result<TreeId, Status> {
        Ok(self.changeset(id).await?.root)
    }

    async fn tree(&self, id: TreeId) -> Result<Arc<Tree>, Status> {
        self.engine.get_tree(id).await.map_err(tree_status)
    }

    async fn lookup(&self, root: TreeId, path: &RepoPath) -> Result<Option<Entry>, Status> {
        self.engine.lookup(root, path).await.map_err(tree_status)
    }

    async fn budget(&self, bytes: u64) -> Result<TextGuard, Status> {
        self.hooks
            .text_budget
            .acquire(bytes)
            .await
            .map_err(exhausted)
    }

    /// The `LogEntry` of each id. `hints` carry a known generation and git oid.
    async fn log_entries(&self, items: &[LogItem]) -> Result<Vec<wire::LogEntry>, Status> {
        let mut out: Vec<wire::LogEntry> = stream::iter(
            items
                .iter()
                .map(|&(id, hint)| self.log_entry(id, hint))
                .collect::<Vec<_>>(),
        )
        .buffered(FANOUT)
        .try_collect()
        .await?;
        let ids: Vec<ChangesetId> = items.iter().map(|(id, _)| *id).collect();
        for e in self.hooks.enrichers() {
            e.enrich(&self.entry, &ids, &mut out).await?;
        }
        Ok(out)
    }

    async fn log_entry(&self, id: ChangesetId, hint: Hint) -> Result<wire::LogEntry, Status> {
        let cs = self.changeset(id).await?;
        let generation = match hint {
            Some((g, _)) => g,
            None => {
                self.engine
                    .get_derived::<Gen>(id)
                    .await
                    .map_err(|e| internal("generation", &e))?
                    .generation
            }
        };
        let git = match hint {
            Some((_, Some(g))) => g,
            _ => self.ids.git_of(id).await?,
        };
        Ok(wire::LogEntry {
            id: self.ids.changeset_out(id).await?,
            git: Bytes::copy_from_slice(git.as_bytes()),
            parents: self.ids.changesets_out(&cs.parents).await?,
            author: Some(sig(&cs.author)),
            committer: Some(sig(&cs.committer)),
            message: Bytes::copy_from_slice(&cs.message),
            r#gen: generation,
            ..wire::LogEntry::default()
        })
    }

    /// The wire form of a file id with its git oid.
    fn file_out(&self, file: &FileId, git: &GitOid) -> Bytes {
        match self.mode() {
            IdMode::Asc => Bytes::copy_from_slice(file.as_bytes()),
            IdMode::Git => Bytes::copy_from_slice(git.as_bytes()),
        }
    }

    fn entry_info(&self, path: &RepoPath, e: &Entry) -> wire::TreeEntryInfo {
        wire::TreeEntryInfo {
            name: Bytes::copy_from_slice(e.name.as_bytes()),
            path: Bytes::copy_from_slice(path.as_bytes()),
            mode: diff::mode(e.kind),
            id: self.ids.entry_out(e),
            git: Bytes::copy_from_slice(e.git.as_bytes()),
            size: if e.kind == Kind::Dir || e.kind == Kind::Gitlink {
                0
            } else {
                e.size
            },
        }
    }

    fn tree_change_wire(&self, c: &TreeChange) -> wire::PathChange {
        let side = |e: &Option<Entry>| match e {
            Some(e) if e.kind != Kind::Gitlink => {
                (self.file_out(&FileId::from_bytes(e.id), &e.git), e.size)
            }
            _ => (Bytes::new(), 0),
        };
        let git = |e: &Option<Entry>| {
            e.as_ref()
                .map_or_else(Bytes::new, |e| Bytes::copy_from_slice(e.git.as_bytes()))
        };
        let (old_id, old_size) = side(&c.old);
        let (new_id, new_size) = side(&c.new);
        wire::PathChange {
            path: Bytes::copy_from_slice(c.path.as_bytes()),
            status: c.status.letter().to_string(),
            old_path: if c.old_path == c.path {
                Bytes::new()
            } else {
                Bytes::copy_from_slice(c.old_path.as_bytes())
            },
            old_mode: c.old_mode(),
            new_mode: c.new_mode(),
            old_git: git(&c.old),
            new_git: git(&c.new),
            old_id,
            new_id,
            old_size,
            new_size,
            similarity: c.status.score().map_or(0, u32::from),
        }
    }
}

// ------------------------------------------------------------------------------ file streaming

/// Reads a file's bytes in order, a chunk at a time, verifying what it can.
///
/// A chunk is verified against its id as it is fetched. The chunk list is not identity-bearing,
/// so a read of the whole file also feeds an assembler that checks the file id; a ranged read of
/// a chunked file trusts the list the namespace stored at ingest.
struct FileStream {
    objects: Arc<dyn Objects>,
    plan: Plan,
    /// The byte range wanted, within the file.
    start: u64,
    end: u64,
    /// Offset of the next chunk's first byte.
    pos: u64,
    asm: Option<ChunkAssembler>,
}

enum Plan {
    Inline(Option<Bytes>),
    Chunked {
        chunks: Vec<(ChunkId, u32)>,
        next: usize,
    },
}

fn clamp(r: &Range<u64>, total: u64) -> (u64, u64) {
    let a = r.start.min(total);
    (a, r.end.clamp(a, total))
}

impl FileStream {
    /// Opens `file`. `size` is its tree entry's size when known; `range` is clamped to the file.
    /// Returns the stream and the file's total size.
    async fn open(
        objects: Arc<dyn Objects>,
        file: FileId,
        size: Option<u64>,
        range: Range<u64>,
    ) -> Result<(Self, u64), Status> {
        let chunked = match size {
            Some(s) => s > CHUNK_THRESHOLD,
            None => objects
                .has_many(&[ObjKey::ChunkList(file)])
                .await
                .map_err(store_status)?[0],
        };
        if !chunked {
            let data = objects
                .get(&ObjKey::File(file))
                .await
                .map_err(store_status)?
                .ok_or_else(|| Status::not_found(format!("file {file} is missing")))?;
            let total = data.len() as u64;
            let (a, b) = clamp(&range, total);
            let piece = data.slice(a as usize..b as usize);
            let stream = Self {
                objects,
                plan: Plan::Inline(Some(piece)),
                start: a,
                end: b,
                pos: a,
                asm: None,
            };
            return Ok((stream, total));
        }
        let raw = objects
            .get(&ObjKey::ChunkList(file))
            .await
            .map_err(store_status)?
            .ok_or_else(|| Status::not_found(format!("file {file} is missing")))?;
        let list = decode_envelope::<ChunkList>(&raw).map_err(|e| data_loss("chunk list", &e))?;
        if size.is_some_and(|s| s != list.size) {
            return Err(data_loss("chunk list", &"size differs from the tree entry"));
        }
        let total = list.size;
        let (a, b) = clamp(&range, total);
        let chunks = list.chunks.clone();
        let asm = if a == 0 && b == total {
            Some(ChunkAssembler::new(list, file).map_err(|e| data_loss("chunk list", &e))?)
        } else {
            None
        };
        // Chunks that end before the range starts are skipped without a fetch.
        let (mut skipped, mut next) = (0u64, 0usize);
        while next < chunks.len() && skipped + u64::from(chunks[next].1) <= a {
            skipped += u64::from(chunks[next].1);
            next += 1;
        }
        let stream = Self {
            objects,
            plan: Plan::Chunked { chunks, next },
            start: a,
            end: b,
            pos: skipped,
            asm,
        };
        Ok((stream, total))
    }

    /// The next piece of the range, or `None` once it is complete (and verified, for a whole file).
    async fn next(&mut self) -> Result<Option<Bytes>, Status> {
        match &mut self.plan {
            Plan::Inline(piece) => Ok(piece.take().filter(|p| !p.is_empty())),
            Plan::Chunked { chunks, next } => loop {
                if *next >= chunks.len() || (self.asm.is_none() && self.pos >= self.end) {
                    if let Some(asm) = self.asm.take() {
                        asm.finish().map_err(|e| data_loss("file", &e))?;
                    }
                    return Ok(None);
                }
                let (id, len) = chunks[*next];
                *next += 1;
                let chunk = self
                    .objects
                    .get(&ObjKey::Chunk(id))
                    .await
                    .map_err(store_status)?
                    .ok_or_else(|| data_loss("chunk", &format!("chunk {id} is missing")))?;
                if chunk.len() != len as usize {
                    return Err(data_loss("chunk", &"chunk length differs from the list"));
                }
                if let Some(asm) = &mut self.asm {
                    asm.push(&chunk).map_err(|e| data_loss("file", &e))?;
                }
                let (from, to) = (self.pos, self.pos + u64::from(len));
                self.pos = to;
                let (a, b) = (from.max(self.start), to.min(self.end));
                if a < b {
                    return Ok(Some(chunk.slice((a - from) as usize..(b - from) as usize)));
                }
            },
        }
    }
}

/// Blocking `Read` over a [`FileStream`], for the archive writers.
struct SyncFile {
    handle: Handle,
    stream: FileStream,
    cur: Bytes,
}

impl io::Read for SyncFile {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        while self.cur.is_empty() {
            match self.handle.block_on(self.stream.next()) {
                Ok(Some(b)) => self.cur = b,
                Ok(None) => return Ok(0),
                Err(s) => return Err(io::Error::other(s.message().to_owned())),
            }
        }
        let n = buf.len().min(self.cur.len());
        buf[..n].copy_from_slice(&self.cur[..n]);
        self.cur = self.cur.slice(n..);
        Ok(n)
    }
}

// ------------------------------------------------------------------------------ attributes

/// Attribute lookup at one revision: per directory `.ascattributes` else `.gitattributes`, root to
/// leaf, never merged (SPEC §2.4.1). An unreadable, linked or oversized file reads as absent.
struct TreeAttrs {
    engine: Arc<Eng>,
    objects: Arc<dyn Objects>,
    root: TreeId,
    collection: gix_attributes::search::MetadataCollection,
    outcome: gix_attributes::search::Outcome,
    base: gix_attributes::Search,
    dirs: HashMap<Vec<u8>, Arc<gix_attributes::Search>>,
    /// Whether any attributes file was found; false makes every lookup answer "nothing".
    any: bool,
    ignored: HashMap<Vec<u8>, bool>,
}

/// One attribute's state for a path.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Attr {
    Set,
    Unset,
    Value(String),
    Unspecified,
}

impl Attr {
    fn text(&self) -> String {
        match self {
            Self::Set => "set".into(),
            Self::Unset => "unset".into(),
            Self::Value(v) => v.clone(),
            Self::Unspecified => "unspecified".into(),
        }
    }
}

fn valid_attr_name(n: &str) -> bool {
    !n.is_empty()
        && n.len() <= 255
        && !n.starts_with('-')
        && n.bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_' | b'.'))
}

impl TreeAttrs {
    fn new(engine: Arc<Eng>, objects: Arc<dyn Objects>, root: TreeId) -> Result<Self, Status> {
        let mut collection = gix_attributes::search::MetadataCollection::default();
        let mut buf = Vec::new();
        let base = gix_attributes::Search::new_globals(
            None::<std::path::PathBuf>,
            &mut buf,
            &mut collection,
        )
        .map_err(|e| internal("attributes", &e))?;
        Ok(Self {
            engine,
            objects,
            root,
            collection,
            outcome: gix_attributes::search::Outcome::default(),
            base,
            dirs: HashMap::new(),
            any: false,
            ignored: HashMap::new(),
        })
    }

    /// The text of `dir/name` at the revision, when it is a regular file within the size cap.
    async fn read_attr_file(&self, dir: &[u8], name: &str) -> Result<Option<Bytes>, Status> {
        let path = if dir.is_empty() {
            name.as_bytes().to_vec()
        } else {
            [dir, b"/", name.as_bytes()].concat()
        };
        let Ok(path) = RepoPath::new(path) else {
            return Ok(None);
        };
        let Some(e) = self
            .engine
            .lookup(self.root, &path)
            .await
            .map_err(tree_status)?
        else {
            return Ok(None);
        };
        if !matches!(e.kind, Kind::File | Kind::Exec) || e.size > ATTR_MAX_FILE {
            return Ok(None);
        }
        let (mut s, _) = FileStream::open(
            self.objects.clone(),
            FileId::from_bytes(e.id),
            Some(e.size),
            0..e.size,
        )
        .await?;
        let mut out = Vec::new();
        while let Some(b) = s.next().await? {
            out.extend_from_slice(&b);
        }
        Ok(Some(Bytes::from(out)))
    }

    /// The search that applies to paths directly inside `dir` (`""` is the root).
    async fn search(&mut self, dir: &[u8]) -> Result<Arc<gix_attributes::Search>, Status> {
        if let Some(s) = self.dirs.get(dir) {
            return Ok(s.clone());
        }
        let mut prefixes: Vec<&[u8]> = vec![&dir[..0]];
        prefixes.extend(
            dir.iter()
                .enumerate()
                .filter(|(_, c)| **c == b'/')
                .map(|(i, _)| &dir[..i]),
        );
        if !dir.is_empty() {
            prefixes.push(dir);
        }
        let mut cur: Option<Arc<gix_attributes::Search>> = None;
        for d in prefixes {
            if let Some(s) = self.dirs.get(d) {
                cur = Some(s.clone());
                continue;
            }
            let mut search = match &cur {
                Some(p) => (**p).clone(),
                None => self.base.clone(),
            };
            let file = match self.read_attr_file(d, ".ascattributes").await? {
                Some(f) => Some(f),
                None => self.read_attr_file(d, ".gitattributes").await?,
            };
            if let Some(f) = file {
                self.any = true;
                let source = if d.is_empty() {
                    std::path::PathBuf::from(".gitattributes")
                } else {
                    std::path::PathBuf::from(format!("{}/.gitattributes", lossy(d)))
                };
                search
                    .add_patterns_buffer(
                        &f,
                        source,
                        Some(std::path::Path::new("")),
                        &mut self.collection,
                        true,
                    )
                    .map_err(|e| internal("attributes", &e))?;
            }
            let search = Arc::new(search);
            if self.dirs.len() >= 4096 {
                self.dirs.clear();
            }
            self.dirs.insert(d.to_vec(), search.clone());
            cur = Some(search);
        }
        cur.ok_or_else(|| internal("attributes", &"no search built"))
    }

    /// The states of `names` for `path`.
    async fn lookup(
        &mut self,
        path: &[u8],
        is_dir: bool,
        names: &[&str],
    ) -> Result<Vec<Attr>, Status> {
        let dir = path
            .iter()
            .rposition(|&c| c == b'/')
            .map_or(&[][..], |i| &path[..i]);
        let search = self.search(dir).await?;
        if !self.any {
            return Ok(vec![Attr::Unspecified; names.len()]);
        }
        use bstr::ByteSlice;
        use gix_attributes::StateRef;
        self.outcome
            .initialize_with_selection(&self.collection, names.iter().copied());
        search.pattern_matching_relative_path(
            path.as_bstr(),
            gix_attributes::glob::pattern::Case::Sensitive,
            Some(is_dir),
            &mut self.outcome,
        );
        let mut found: HashMap<String, Attr> = HashMap::new();
        for m in self.outcome.iter_selected() {
            let state = match m.assignment.state {
                StateRef::Set => Attr::Set,
                StateRef::Unset => Attr::Unset,
                StateRef::Value(v) => Attr::Value(v.as_bstr().to_str_lossy().into_owned()),
                StateRef::Unspecified => Attr::Unspecified,
            };
            found.insert(m.assignment.name.as_str().to_owned(), state);
        }
        Ok(names
            .iter()
            .map(|n| found.remove(*n).unwrap_or(Attr::Unspecified))
            .collect())
    }

    async fn is_set(&mut self, path: &[u8], is_dir: bool, name: &str) -> Result<bool, Status> {
        Ok(self.lookup(path, is_dir, &[name]).await?[0] == Attr::Set)
    }

    async fn dir_ignored(&mut self, dir: &[u8], parent: bool) -> Result<bool, Status> {
        if let Some(&h) = self.ignored.get(dir) {
            return Ok(h);
        }
        let hit = parent || self.is_set(dir, true, "export-ignore").await?;
        if self.ignored.len() >= 8192 {
            self.ignored.clear();
        }
        self.ignored.insert(dir.to_vec(), hit);
        Ok(hit)
    }

    /// Whether `path` or one of its directories is `export-ignore`.
    async fn export_ignored(&mut self, path: &[u8]) -> Result<bool, Status> {
        let mut parent = false;
        for (i, _) in path.iter().enumerate().filter(|(_, c)| **c == b'/') {
            parent = self.dir_ignored(&path[..i], parent).await?;
            if parent {
                return Ok(true);
            }
        }
        self.is_set(path, false, "export-ignore").await
    }
}

// ------------------------------------------------------------------------------ archive

/// What `$Format:…$` placeholders expand from.
struct Subst {
    git: GitOid,
    tree: GitOid,
    parents: Vec<GitOid>,
    author: asc_core::sig::Signature,
    committer: asc_core::sig::Signature,
    /// The commit message, decoded once.
    message: String,
    subject: String,
    body: String,
}

fn lossy(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

/// A capped output buffer: a push that would pass `limit` fails and leaves the buffer as it was.
struct CappedOut {
    buf: Vec<u8>,
    limit: usize,
}

/// The expansion passed [`SUBST_MAX_OUT`].
struct SubstOverflow;

impl CappedOut {
    fn push(&mut self, b: &[u8]) -> Result<(), SubstOverflow> {
        if b.len() > self.limit - self.buf.len() {
            return Err(SubstOverflow);
        }
        self.buf.extend_from_slice(b);
        Ok(())
    }
}

impl Subst {
    fn new(
        git: GitOid,
        tree: GitOid,
        parents: Vec<GitOid>,
        author: asc_core::sig::Signature,
        committer: asc_core::sig::Signature,
        message: &[u8],
    ) -> Self {
        let message = lossy(message);
        let (subject, body) = {
            let trimmed = message.trim_start_matches('\n');
            match trimmed.split_once("\n\n") {
                Some((s, b)) => (s.replace('\n', " "), b.to_owned()),
                None => (trimmed.trim_end().replace('\n', " "), String::new()),
            }
        };
        Self {
            git,
            tree,
            parents,
            author,
            committer,
            message,
            subject,
            body,
        }
    }

    fn expand(&self, fmt: &str, out: &mut CappedOut) -> Result<(), SubstOverflow> {
        let hex = |g: &GitOid| g.to_string();
        let mut chars = fmt.chars().peekable();
        while let Some(c) = chars.next() {
            if c != '%' {
                out.push(c.encode_utf8(&mut [0; 4]).as_bytes())?;
                continue;
            }
            let Some(&n) = chars.peek() else {
                out.push(b"%")?;
                break;
            };
            let two: String = chars.clone().take(2).collect();
            let (text, used): (std::borrow::Cow<'_, str>, usize) = match (n, two.as_str()) {
                ('%', _) => ("%".into(), 1),
                ('n', _) => ("\n".into(), 1),
                ('H', _) => (hex(&self.git).into(), 1),
                ('h', _) => (hex(&self.git)[..7].to_owned().into(), 1),
                ('T', _) => (hex(&self.tree).into(), 1),
                ('t', _) => (hex(&self.tree)[..7].to_owned().into(), 1),
                ('P', _) => (
                    self.parents
                        .iter()
                        .map(hex)
                        .collect::<Vec<_>>()
                        .join(" ")
                        .into(),
                    1,
                ),
                ('p', _) => (
                    self.parents
                        .iter()
                        .map(|p| hex(p)[..7].to_owned())
                        .collect::<Vec<_>>()
                        .join(" ")
                        .into(),
                    1,
                ),
                ('s', _) => (self.subject.as_str().into(), 1),
                ('b', _) => (self.body.as_str().into(), 1),
                ('B', _) => (self.message.as_str().into(), 1),
                (_, "an") => (lossy(self.author.name()).into(), 2),
                (_, "ae") => (lossy(self.author.email()).into(), 2),
                (_, "at") => (self.author.time().to_string().into(), 2),
                (_, "cn") => (lossy(self.committer.name()).into(), 2),
                (_, "ce") => (lossy(self.committer.email()).into(), 2),
                (_, "ct") => (self.committer.time().to_string().into(), 2),
                _ => {
                    out.push(b"%")?;
                    continue;
                }
            };
            out.push(text.as_bytes())?;
            for _ in 0..used {
                chars.next();
            }
        }
        Ok(())
    }

    /// Replaces each `$Format:<fmt>$` on one line, as `git archive` does with `export-subst`.
    ///
    /// Fails with `resource_exhausted` once the output would pass [`SUBST_MAX_OUT`].
    fn apply(&self, data: &[u8]) -> Result<Vec<u8>, Status> {
        const OPEN: &[u8] = b"$Format:";
        let overflow =
            |_: SubstOverflow| Status::resource_exhausted("export-subst expansion is too large");
        let mut out = CappedOut {
            buf: Vec::with_capacity(data.len()),
            limit: usize::try_from(SUBST_MAX_OUT).unwrap_or(usize::MAX),
        };
        let mut rest = data;
        while let Some(i) = rest.windows(OPEN.len()).position(|w| w == OPEN) {
            let after = &rest[i + OPEN.len()..];
            // One scan finds the closing `$` or the end of the line, whichever comes first, and
            // everything it passes is consumed, so the whole pass is linear.
            let stop = after.iter().position(|&c| c == b'$' || c == b'\n');
            match stop {
                Some(j) if after[j] == b'$' => {
                    out.push(&rest[..i]).map_err(overflow)?;
                    self.expand(&lossy(&after[..j]), &mut out)
                        .map_err(overflow)?;
                    rest = &after[j + 1..];
                }
                _ => {
                    // No closing `$` on this line (so no later opener on it): copy through its end.
                    let upto = i + OPEN.len() + stop.map_or(after.len(), |j| j + 1);
                    out.push(&rest[..upto]).map_err(overflow)?;
                    rest = &rest[upto..];
                }
            }
        }
        out.push(rest).map_err(overflow)?;
        Ok(out.buf)
    }
}

type ArchiveTx = mpsc::Sender<Result<wire::ArchiveResponse, Status>>;

/// Chunks written bytes into `ArchiveResponse`s from a blocking thread.
struct ChanWriter {
    tx: ArchiveTx,
    buf: Vec<u8>,
}

impl ChanWriter {
    fn send(&mut self) -> io::Result<()> {
        if self.buf.is_empty() {
            return Ok(());
        }
        let data = Bytes::from(std::mem::take(&mut self.buf));
        self.tx
            .blocking_send(Ok(wire::ArchiveResponse { data }))
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "the client went away"))
    }
}

impl Write for ChanWriter {
    fn write(&mut self, b: &[u8]) -> io::Result<usize> {
        let mut rest = b;
        while !rest.is_empty() {
            let (part, tail) = rest.split_at(rest.len().min(DATA_MSG - self.buf.len()));
            self.buf.extend_from_slice(part);
            rest = tail;
            if self.buf.len() >= DATA_MSG {
                self.send()?;
            }
        }
        Ok(b.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.send()
    }
}

/// An archive format writer.
trait Sink {
    fn file(
        &mut self,
        name: &[u8],
        exec: bool,
        size: u64,
        data: &mut dyn io::Read,
    ) -> io::Result<()>;
    fn symlink(&mut self, name: &[u8], target: &[u8]) -> io::Result<()>;
    fn finish(self: Box<Self>) -> io::Result<()>;
}

fn bytes_to_path(b: &[u8]) -> Option<std::path::PathBuf> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        Some(std::path::PathBuf::from(std::ffi::OsStr::from_bytes(b)))
    }
    #[cfg(not(unix))]
    {
        std::str::from_utf8(b).ok().map(std::path::PathBuf::from)
    }
}

struct TarSink {
    builder: tar::Builder<ChanWriter>,
    mtime: u64,
}

impl Sink for TarSink {
    fn file(
        &mut self,
        name: &[u8],
        exec: bool,
        size: u64,
        data: &mut dyn io::Read,
    ) -> io::Result<()> {
        let Some(path) = bytes_to_path(name) else {
            return Ok(());
        };
        let mut h = tar::Header::new_gnu();
        h.set_size(size);
        h.set_mode(if exec { 0o755 } else { 0o644 });
        h.set_mtime(self.mtime);
        h.set_entry_type(tar::EntryType::Regular);
        self.builder.append_data(&mut h, path, data)
    }

    fn symlink(&mut self, name: &[u8], target: &[u8]) -> io::Result<()> {
        let (Some(path), Some(target)) = (bytes_to_path(name), bytes_to_path(target)) else {
            return Ok(());
        };
        let mut h = tar::Header::new_gnu();
        h.set_size(0);
        h.set_mode(0o777);
        h.set_mtime(self.mtime);
        h.set_entry_type(tar::EntryType::Symlink);
        self.builder.append_link(&mut h, path, target)
    }

    fn finish(self: Box<Self>) -> io::Result<()> {
        let mut w = self.builder.into_inner()?;
        w.flush()
    }
}

struct ZipSink {
    zip: zip::ZipWriter<zip::write::StreamWriter<ChanWriter>>,
    time: zip::DateTime,
}

impl ZipSink {
    fn options(&self, mode: u32, size: u64) -> zip::write::SimpleFileOptions {
        zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated)
            .last_modified_time(self.time)
            .unix_permissions(mode)
            .large_file(size >= u64::from(u32::MAX))
    }
}

fn zip_err(e: zip::result::ZipError) -> io::Error {
    match e {
        zip::result::ZipError::Io(e) => e,
        e => io::Error::other(e.to_string()),
    }
}

impl Sink for ZipSink {
    fn file(
        &mut self,
        name: &[u8],
        exec: bool,
        size: u64,
        data: &mut dyn io::Read,
    ) -> io::Result<()> {
        let Ok(name) = std::str::from_utf8(name) else {
            return Ok(());
        };
        let opts = self.options(if exec { 0o100_755 } else { 0o100_644 }, size);
        self.zip.start_file(name, opts).map_err(zip_err)?;
        io::copy(data, &mut self.zip)?;
        Ok(())
    }

    fn symlink(&mut self, name: &[u8], target: &[u8]) -> io::Result<()> {
        let (Ok(name), Ok(target)) = (std::str::from_utf8(name), std::str::from_utf8(target))
        else {
            return Ok(());
        };
        let opts = self.options(0o120_777, 0);
        self.zip.add_symlink(name, target, opts).map_err(zip_err)
    }

    fn finish(self: Box<Self>) -> io::Result<()> {
        let mut w = self.zip.finish().map_err(zip_err)?.into_inner();
        w.flush()
    }
}

/// The calendar date of `secs` since the Unix epoch (UTC).
fn civil(secs: u64) -> (u16, u8, u8, u8, u8, u8) {
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (
        u16::try_from(y).unwrap_or(1980),
        m as u8,
        d as u8,
        (rem / 3600) as u8,
        (rem % 3600 / 60) as u8,
        (rem % 60) as u8,
    )
}

/// A validated archive prefix: empty, or `a/b/` with every component a legal repository name.
fn archive_prefix(p: &str) -> Result<Vec<u8>, Status> {
    if p.is_empty() {
        return Ok(Vec::new());
    }
    let trimmed = p.strip_suffix('/').unwrap_or(p);
    if trimmed.len() > MAX_STRING || RepoPath::new(trimmed.as_bytes()).is_err() {
        return Err(Status::invalid_argument("invalid archive prefix"));
    }
    Ok(format!("{trimmed}/").into_bytes())
}

fn invalid(e: impl std::fmt::Display) -> Status {
    Status::invalid_argument(e.to_string())
}

type Lasts = Arc<LastsMemo>;

/// Last-touching changesets per directory entry at one tip, from a first-parent walk.
struct LastsMemo {
    map: HashMap<Vec<u8>, ChangesetId>,
    /// The walk tracked every entry of the directory (not only the requested names).
    all: bool,
    /// The walk gave up at its budget; names missing from `map` stay unresolved.
    exhausted: bool,
}

// ------------------------------------------------------------------------------ reads handler

/// The `reads` handler group of the full server.
pub struct ServerReads {
    core: Arc<Core>,
    lasts: quick_cache::sync::Cache<(ChangesetId, Vec<u8>), Lasts>,
    mainline: SqlMainline,
}

impl ServerReads {
    fn new(core: Arc<Core>) -> Self {
        let mainline = SqlMainline::new(core.registry.pool().clone());
        Self {
            core,
            lasts: quick_cache::sync::Cache::new(LASTS_CACHE),
            mainline,
        }
    }

    /// The land branch `tip` is the mainline tip of, if any (SPEC §5.1).
    async fn land_ref(&self, call: &Call, rev: &str, tip: ChangesetId) -> Option<String> {
        let mut cands = Vec::new();
        if rev.starts_with("refs/") {
            cands.push(rev.to_owned());
        } else if !rev.is_empty() && rev != "HEAD" && rev != "@" {
            cands.push(format!("refs/heads/{rev}"));
        }
        cands.push(format!("refs/heads/{}", call.entry.default_branch));
        for c in cands {
            match self.mainline.tip(call.entry.ns, &c).await {
                Ok(Some((_, cs))) if cs == tip => return Some(c),
                Ok(_) => {}
                Err(e) => tracing::warn!(error = %e, "mainline read failed; walking instead"),
            }
        }
        None
    }

    /// The tree of `dir` in changeset `root`, or `None` when it is not a directory there.
    async fn dir_tree(
        call: &Call,
        root: TreeId,
        dir: Option<&RepoPath>,
    ) -> Result<Option<TreeId>, Status> {
        match dir {
            None => Ok(Some(root)),
            Some(p) => Ok(call
                .lookup(root, p)
                .await?
                .filter(|e| e.kind == Kind::Dir)
                .map(|e| TreeId::from_bytes(e.id))),
        }
    }

    /// First-parent walk from `tip`: the newest changeset whose first-parent diff changed each
    /// entry of `dir`. Memoised per (tip, dir) while the directory is small enough to track whole.
    async fn lasts(
        &self,
        call: &Call,
        tip: ChangesetId,
        dir: Option<&RepoPath>,
        want: &[Name],
    ) -> Result<Lasts, Status> {
        let key = (tip, dir.map_or_else(Vec::new, |d| d.as_bytes().to_vec()));
        if let Some(m) = self.lasts.get(&key)
            && (m.exhausted || m.all || want.iter().all(|n| m.map.contains_key(n.as_bytes())))
        {
            return Ok(m);
        }
        let tip_root = call.root(tip).await?;
        let Some(tip_dir) = Self::dir_tree(call, tip_root, dir).await? else {
            return Ok(Arc::new(LastsMemo {
                map: HashMap::new(),
                all: true,
                exhausted: false,
            }));
        };
        let tree = call.engine.get_tree(tip_dir).await.map_err(tree_status)?;
        let all = tree.entries().len() <= LASTS_MAX_ENTRIES;
        // Unresolved names and the entry each had at the tip.
        let mut open: HashMap<Vec<u8>, Entry> = tree
            .entries()
            .iter()
            .filter(|e| all || want.iter().any(|n| n == &e.name))
            .map(|e| (e.name.as_bytes().to_vec(), e.clone()))
            .collect();
        let mut map = HashMap::new();
        let (mut cur, mut cur_tree) = (tip, tip_dir);
        let mut steps = 0usize;
        let mut exhausted = false;
        while !open.is_empty() {
            let cs = call.changeset(cur).await?;
            let Some(&parent) = cs.parents.first() else {
                // The root commit introduced everything still open.
                map.extend(open.drain().map(|(n, _)| (n, cur)));
                break;
            };
            let parent_root = call.root(parent).await?;
            let parent_dir = Self::dir_tree(call, parent_root, dir).await?;
            if parent_dir != Some(cur_tree) {
                let theirs = match parent_dir {
                    Some(t) => Some(call.engine.get_tree(t).await.map_err(tree_status)?),
                    None => None,
                };
                open.retain(|name, mine| {
                    let same = theirs.as_ref().is_some_and(|t| {
                        Name::new(name.clone())
                            .ok()
                            .and_then(|n| t.get(&n).cloned())
                            .is_some_and(|o| {
                                o.id == mine.id && o.kind == mine.kind && o.git == mine.git
                            })
                    });
                    if !same {
                        map.insert(name.clone(), cur);
                    }
                    same
                });
                match parent_dir {
                    Some(t) => cur_tree = t,
                    None => {
                        // The directory did not exist before: the rest came with this commit.
                        map.extend(open.drain().map(|(n, _)| (n, cur)));
                        break;
                    }
                }
            }
            cur = parent;
            steps += 1;
            if steps >= LASTS_MAX_STEPS {
                exhausted = true;
                break;
            }
        }
        let memo = Arc::new(LastsMemo {
            map,
            all,
            exhausted,
        });
        if all {
            self.lasts.insert(key, memo.clone());
        }
        Ok(memo)
    }

    async fn change_wire(
        call: &Call,
        parent_root: TreeId,
        path: &RepoPath,
        change: &Change,
    ) -> Result<wire::PathChange, Status> {
        let class = |k: Kind| match k {
            Kind::Symlink => 1,
            Kind::Gitlink => 2,
            _ => 0,
        };
        let old = call
            .lookup(parent_root, path)
            .await?
            .filter(|e| e.kind != Kind::Dir);
        let mut pc = wire::PathChange {
            path: Bytes::copy_from_slice(path.as_bytes()),
            ..wire::PathChange::default()
        };
        if let Some(o) = &old {
            pc.old_mode = diff::mode(o.kind);
            pc.old_git = Bytes::copy_from_slice(o.git.as_bytes());
            if o.kind != Kind::Gitlink {
                pc.old_id = call.file_out(&FileId::from_bytes(o.id), &o.git);
                pc.old_size = o.size;
            }
        }
        match change {
            Change::Write {
                kind,
                file,
                size,
                git,
                copy_from,
            } => {
                pc.status = match (&old, copy_from) {
                    (Some(o), _) if class(o.kind) != class(*kind) => "T",
                    (Some(_), _) => "M",
                    (None, Some(_)) => "C",
                    (None, None) => "A",
                }
                .into();
                if let Some(from) = copy_from {
                    pc.old_path = Bytes::copy_from_slice(from.as_bytes());
                }
                pc.new_mode = diff::mode(*kind);
                pc.new_git = Bytes::copy_from_slice(git.as_bytes());
                pc.new_id = call.file_out(file, git);
                pc.new_size = *size;
            }
            Change::Gitlink { commit } => {
                pc.status = if old.is_some() { "M" } else { "A" }.into();
                pc.new_mode = diff::mode(Kind::Gitlink);
                pc.new_git = Bytes::copy_from_slice(commit.as_bytes());
            }
            Change::Delete => pc.status = "D".into(),
        }
        Ok(pc)
    }

    async fn stat_key(call: &Call, k: &wire::ObjKey) -> Result<ObjKey, Status> {
        use wire::ObjKind as K;
        let kind =
            K::try_from(k.kind).map_err(|_| Status::invalid_argument("unknown object kind"))?;
        Ok(match kind {
            K::File => ObjKey::File(call.ids.file_in(&k.id).await?),
            K::Tree => ObjKey::Tree(call.ids.tree_in(&k.id).await?),
            K::Changeset => ObjKey::Changeset(call.ids.changeset_in(&k.id).await?),
            _ => ObjKey::try_from(k).map_err(invalid)?,
        })
    }
}

#[async_trait]
impl ReadsHandler for ServerReads {
    async fn read_changeset(
        &self,
        ctx: &CallCtx,
        req: wire::ReadChangesetRequest,
    ) -> Result<wire::ReadChangesetResponse, Status> {
        let call = self.core.call(ctx).await?;
        let id = call.rev(&req.rev).await?;
        let cs = call.changeset(id).await?;
        let header = call
            .log_entries(&[(id, None)])
            .await?
            .pop()
            .ok_or_else(|| internal("log entry", &"no entry built"))?;
        let root_git = call.ids.tree_git(cs.root).await?;
        let parent_root = match cs.parents.first() {
            Some(&p) => call.root(p).await?,
            None => Eng::empty_tree_id(),
        };
        let changes: Vec<wire::PathChange> = stream::iter(
            cs.changes
                .as_slice()
                .iter()
                .take(MAX_CHANGES)
                .map(|(path, change)| Self::change_wire(&call, parent_root, path, change))
                .collect::<Vec<_>>(),
        )
        .buffered(FANOUT)
        .try_collect()
        .await?;
        let mut extra = HashMap::new();
        let mut raw_git_commit = Bytes::new();
        for (k, v) in cs.extra.as_slice() {
            if k.as_slice() == b"git.commit" {
                raw_git_commit = Bytes::copy_from_slice(v);
            } else if let Ok(k) = std::str::from_utf8(k) {
                extra.insert(k.to_owned(), Bytes::copy_from_slice(v));
            }
        }
        Ok(wire::ReadChangesetResponse {
            header: Some(header),
            root_tree: call.ids.tree_out(cs.root, root_git),
            root_git: Bytes::copy_from_slice(root_git.as_bytes()),
            changes,
            extra,
            raw_git_commit,
        })
    }

    async fn stat(
        &self,
        ctx: &CallCtx,
        req: wire::StatRequest,
    ) -> Result<wire::StatResponse, Status> {
        let call = self.core.call(ctx).await?;
        if req.paths.len() + req.objects.len() > MAX_NAMES {
            return Err(Status::invalid_argument("too many paths and objects"));
        }
        let mut entries = Vec::with_capacity(req.paths.len() + req.objects.len());
        let mut exists = Vec::with_capacity(entries.capacity());
        if !req.paths.is_empty() {
            let id = call.rev(&req.rev).await?;
            let root = call.root(id).await?;
            let found: Vec<(wire::TreeEntryInfo, bool)> = stream::iter(
                req.paths
                    .iter()
                    .map(|p| {
                        let call = &call;
                        async move {
                            match dir_path(p)? {
                                None => {
                                    let git = call.ids.tree_git(root).await?;
                                    let info = wire::TreeEntryInfo {
                                        mode: diff::mode(Kind::Dir),
                                        id: call.ids.tree_out(root, git),
                                        git: Bytes::copy_from_slice(git.as_bytes()),
                                        ..wire::TreeEntryInfo::default()
                                    };
                                    Ok::<_, Status>((info, true))
                                }
                                Some(path) => Ok(match call.lookup(root, &path).await? {
                                    Some(e) => (call.entry_info(&path, &e), true),
                                    None => (wire::TreeEntryInfo::default(), false),
                                }),
                            }
                        }
                    })
                    .collect::<Vec<_>>(),
            )
            .buffered(FANOUT)
            .try_collect()
            .await?;
            for (e, x) in found {
                entries.push(e);
                exists.push(x);
            }
        }
        if !req.objects.is_empty() {
            let mut keys = Vec::with_capacity(req.objects.len());
            for k in &req.objects {
                keys.push(Self::stat_key(&call, k).await?);
            }
            // A file is present as a single object or as a chunk list.
            let mut probe = Vec::with_capacity(keys.len() * 2);
            for k in &keys {
                probe.push(*k);
                if let ObjKey::File(f) = k {
                    probe.push(ObjKey::ChunkList(*f));
                }
            }
            let have = call
                .entry
                .objects
                .has_many(&probe)
                .await
                .map_err(store_status)?;
            let mut at = 0;
            for (k, wk) in keys.iter().zip(&req.objects) {
                let mut present = have[at];
                at += 1;
                if matches!(k, ObjKey::File(_)) {
                    present |= have[at];
                    at += 1;
                }
                entries.push(wire::TreeEntryInfo {
                    id: wk.id.clone(),
                    ..wire::TreeEntryInfo::default()
                });
                exists.push(present);
            }
        }
        Ok(wire::StatResponse { entries, exists })
    }

    async fn check_attr(
        &self,
        ctx: &CallCtx,
        req: wire::CheckAttrRequest,
    ) -> Result<wire::CheckAttrResponse, Status> {
        let call = self.core.call(ctx).await?;
        if req.paths.len() > MAX_NAMES || req.names.is_empty() || req.names.len() > MAX_ATTR_NAMES {
            return Err(Status::invalid_argument(
                "bad number of paths or attribute names",
            ));
        }
        if !req.names.iter().all(|n| valid_attr_name(n)) {
            return Err(Status::invalid_argument("invalid attribute name"));
        }
        let id = call.rev(&req.rev).await?;
        let root = call.root(id).await?;
        let mut attrs = TreeAttrs::new(call.engine.clone(), call.entry.objects.clone(), root)?;
        let names: Vec<&str> = req.names.iter().map(String::as_str).collect();
        let mut per_path = Vec::with_capacity(req.paths.len());
        for p in &req.paths {
            let path = repo_path(p)?;
            let got = attrs.lookup(path.as_bytes(), false, &names).await?;
            per_path.push(wire::AttrValues {
                values: req
                    .names
                    .iter()
                    .cloned()
                    .zip(got.iter().map(Attr::text))
                    .collect(),
            });
        }
        Ok(wire::CheckAttrResponse { per_path })
    }

    async fn last_commits(
        &self,
        ctx: &CallCtx,
        req: wire::LastCommitsRequest,
    ) -> Result<wire::LastCommitsResponse, Status> {
        let call = self.core.call(ctx).await?;
        if req.names.len() > MAX_NAMES {
            return Err(Status::invalid_argument("too many names"));
        }
        let tip = call.rev(&req.rev).await?;
        let dir = dir_path(&req.dir)?;
        let mut names = Vec::with_capacity(req.names.len());
        for n in &req.names {
            if std::str::from_utf8(n).is_err() {
                return Err(Status::invalid_argument("names must be UTF-8"));
            }
            names.push(Name::new(n.to_vec()).map_err(invalid)?);
        }
        // Only names that exist at the tip can have a last commit.
        let root = call.root(tip).await?;
        let Some(dir_id) = Self::dir_tree(&call, root, dir.as_ref()).await? else {
            return Err(Status::not_found("no such directory"));
        };
        let tree = call.engine.get_tree(dir_id).await.map_err(tree_status)?;
        names.retain(|n| tree.get(n).is_some());
        names.sort();
        names.dedup();

        let mut found: HashMap<Vec<u8>, ChangesetId> = HashMap::new();
        if let Some(refname) = self.land_ref(&call, &req.rev, tip).await {
            match self
                .mainline
                .last_commits(call.entry.ns, &refname, dir.as_ref(), &names)
                .await
            {
                Ok(rows) => {
                    for (n, row) in names.iter().zip(rows) {
                        if let Some((_, cs)) = row {
                            found.insert(n.as_bytes().to_vec(), cs);
                        }
                    }
                }
                Err(e) => tracing::warn!(error = %e, "mainline read failed; walking instead"),
            }
        }
        let missing: Vec<Name> = names
            .iter()
            .filter(|n| !found.contains_key(n.as_bytes()))
            .cloned()
            .collect();
        if !missing.is_empty() {
            let walked = self.lasts(&call, tip, dir.as_ref(), &missing).await?;
            for n in &missing {
                if let Some(cs) = walked.map.get(n.as_bytes()) {
                    found.insert(n.as_bytes().to_vec(), *cs);
                }
            }
        }
        let mut unique: Vec<ChangesetId> = found.values().copied().collect();
        unique.sort();
        unique.dedup();
        let items: Vec<_> = unique.iter().map(|id| (*id, None)).collect();
        let built = call.log_entries(&items).await?;
        let by_id: HashMap<ChangesetId, wire::LogEntry> = unique.into_iter().zip(built).collect();
        let by_name = found
            .into_iter()
            .filter_map(|(n, cs)| Some((String::from_utf8(n).ok()?, by_id.get(&cs)?.clone())))
            .collect();
        Ok(wire::LastCommitsResponse { by_name })
    }

    async fn read_tree(
        &self,
        ctx: &CallCtx,
        req: wire::ReadTreeRequest,
    ) -> Result<RpcStream<wire::ReadTreeResponse>, Status> {
        let call = self.core.call(ctx).await?;
        let id = call.rev(&req.rev).await?;
        let root = call.root(id).await?;
        let dir = dir_path(&req.path)?;
        let Some(dir_id) = Self::dir_tree(&call, root, dir.as_ref()).await? else {
            return Err(match dir.as_ref() {
                Some(p) if call.lookup(root, p).await?.is_some() => {
                    Status::failed_precondition("not a directory")
                }
                _ => Status::not_found("no such directory"),
            });
        };
        let dir_git = call.ids.tree_git(dir_id).await?;
        // The cursor is the `path` of the last entry received, below the directory.
        let after: Vec<Vec<u8>> = if req.after.is_empty() {
            Vec::new()
        } else {
            let a = repo_path(&req.after)?;
            let rel = match &dir {
                None => Some(a.as_bytes()),
                Some(d) => a
                    .as_bytes()
                    .strip_prefix(d.as_bytes())
                    .and_then(|r| r.strip_prefix(b"/")),
            }
            .ok_or_else(|| Status::invalid_argument("`after` is not below the directory"))?;
            rel.split(|&c| c == b'/').map(<[u8]>::to_vec).collect()
        };
        let (recursive, limit) = (req.recursive, u64::from(req.limit));
        Ok(spawn_stream(move |out| async move {
            let head = |entries| wire::ReadTreeResponse {
                entries,
                tree_id: call.ids.tree_out(dir_id, dir_git),
                tree_git: Bytes::copy_from_slice(dir_git.as_bytes()),
            };
            struct Frame {
                prefix: Option<RepoPath>,
                tree: Arc<Tree>,
                next: usize,
            }
            let mut stack: Vec<Frame> = Vec::new();
            // Position the stack just past the cursor, reading one tree per component.
            let (mut tree, mut prefix) = (call.tree(dir_id).await?, dir.clone());
            if after.is_empty() {
                stack.push(Frame {
                    prefix: prefix.clone(),
                    tree: tree.clone(),
                    next: 0,
                });
            }
            for (i, comp) in after.iter().enumerate() {
                let entries = tree.entries();
                let pos = entries.partition_point(|e| e.name.as_bytes() < comp.as_slice());
                let hit = entries
                    .get(pos)
                    .filter(|e| e.name.as_bytes() == comp.as_slice())
                    .cloned();
                match hit {
                    Some(e) if recursive && e.kind == Kind::Dir && i + 1 < after.len() => {
                        stack.push(Frame {
                            prefix: prefix.clone(),
                            tree: tree.clone(),
                            next: pos + 1,
                        });
                        prefix = Some(child(prefix.as_ref(), &e.name)?);
                        tree = call.tree(TreeId::from_bytes(e.id)).await?;
                        if stack.len() > MAX_TREE_DEPTH {
                            return Err(Status::out_of_range("tree is nested too deeply"));
                        }
                    }
                    other => {
                        let next = pos + usize::from(other.is_some());
                        stack.push(Frame {
                            prefix: prefix.clone(),
                            tree: tree.clone(),
                            next,
                        });
                        break;
                    }
                }
            }
            let (mut batch, mut sent) = (Vec::new(), 0u64);
            while let Some(top) = stack.last_mut() {
                if limit > 0 && sent >= limit {
                    break;
                }
                let Some(e) = top.tree.entries().get(top.next).cloned() else {
                    stack.pop();
                    continue;
                };
                top.next += 1;
                let path = child(top.prefix.as_ref(), &e.name)?;
                if recursive && e.kind == Kind::Dir {
                    if stack.len() > MAX_TREE_DEPTH {
                        return Err(Status::out_of_range("tree is nested too deeply"));
                    }
                    let tree = call.tree(TreeId::from_bytes(e.id)).await?;
                    stack.push(Frame {
                        prefix: Some(path),
                        tree,
                        next: 0,
                    });
                    continue;
                }
                batch.push(call.entry_info(&path, &e));
                sent += 1;
                if sent > MAX_TREE_ENTRIES {
                    return Err(Status::resource_exhausted(
                        "directory listing is too large; use `limit` and `after`",
                    ));
                }
                if batch.len() >= TREE_CHUNK {
                    out.send(head(std::mem::take(&mut batch))).await?;
                }
            }
            if !batch.is_empty() || sent == 0 {
                out.send(head(batch)).await?;
            }
            Ok(())
        }))
    }

    async fn read_file(
        &self,
        ctx: &CallCtx,
        req: wire::ReadFileRequest,
    ) -> Result<RpcStream<wire::ReadFileResponse>, Status> {
        use wire::read_file_request::At;
        let call = self.core.call(ctx).await?;
        let (file, size) = match req.at {
            Some(At::RevPath(rp)) => {
                let (rev, path) = rp
                    .split_once(':')
                    .ok_or_else(|| Status::invalid_argument("rev_path must be `rev:path`"))?;
                let id = call.rev(rev).await?;
                let root = call.root(id).await?;
                let path = repo_path(path.as_bytes())?;
                let e = call
                    .lookup(root, &path)
                    .await?
                    .ok_or_else(|| Status::not_found(format!("{path} does not exist at {rev}")))?;
                match e.kind {
                    Kind::File | Kind::Exec | Kind::Symlink => {
                        (FileId::from_bytes(e.id), Some(e.size))
                    }
                    Kind::Dir => return Err(Status::failed_precondition("path is a directory")),
                    Kind::Gitlink => {
                        return Err(Status::failed_precondition("path is a submodule"));
                    }
                }
            }
            Some(At::FileId(b)) => (call.ids.file_in(&b).await?, None),
            None => return Err(Status::invalid_argument("missing rev_path or file_id")),
        };
        let end = if req.length == 0 {
            u64::MAX
        } else {
            req.offset.saturating_add(req.length)
        };
        let objects: Arc<dyn Objects> = call.entry.objects.clone();
        let (mut file_stream, total) =
            FileStream::open(objects, file, size, req.offset..end).await?;
        Ok(spawn_stream(move |out| async move {
            let mut any = false;
            while let Some(mut piece) = file_stream.next().await? {
                while !piece.is_empty() {
                    let part = piece.split_to(piece.len().min(DATA_MSG));
                    out.send(wire::ReadFileResponse {
                        data: part,
                        total_size: total,
                    })
                    .await?;
                    any = true;
                }
            }
            if !any {
                out.send(wire::ReadFileResponse {
                    data: Bytes::new(),
                    total_size: total,
                })
                .await?;
            }
            Ok(())
        }))
    }

    async fn grep(
        &self,
        ctx: &CallCtx,
        req: wire::GrepRequest,
    ) -> Result<RpcStream<wire::GrepResponse>, Status> {
        let call = self.core.call(ctx).await?;
        if req.pattern.is_empty() || req.pattern.len() > MAX_STRING {
            return Err(Status::invalid_argument("empty or too long pattern"));
        }
        let pattern = if req.fixed {
            regex::escape(&req.pattern)
        } else {
            req.pattern.clone()
        };
        let re = regex::bytes::RegexBuilder::new(&pattern)
            .case_insensitive(req.ignore_case)
            .size_limit(1 << 20)
            .dfa_size_limit(8 << 20)
            .build()
            .map_err(invalid)?;
        let spec = pathspec(&req.paths)?;
        let max = match req.max_results {
            0 => GREP_DEFAULT,
            n => n.min(GREP_MAX),
        } as usize;
        let id = call.rev(&req.rev).await?;
        let root = call.root(id).await?;
        Ok(spawn_stream(move |out| async move {
            let mut walk = call.engine.walk(root, spec.as_ref());
            let (mut batch, mut found) = (Vec::new(), 0usize);
            'files: while let Some((path, e)) = walk.try_next().await.map_err(tree_status)? {
                if !matches!(e.kind, Kind::File | Kind::Exec)
                    || e.size == 0
                    || e.size > GREP_MAX_FILE
                {
                    continue;
                }
                let _guard = call.budget(e.size).await?;
                let data = call
                    .engine
                    .read_file(FileId::from_bytes(e.id), e.size)
                    .await
                    .map_err(diff_status)?;
                if looks_binary(&data) {
                    continue;
                }
                for (i, line) in data.split(|&c| c == b'\n').enumerate() {
                    if !re.is_match(line) {
                        continue;
                    }
                    let text = &line[..line.len().min(GREP_MAX_LINE)];
                    batch.push(wire::GrepMatch {
                        path: Bytes::copy_from_slice(path.as_bytes()),
                        line: u32::try_from(i + 1).unwrap_or(u32::MAX),
                        text: Bytes::copy_from_slice(text),
                    });
                    found += 1;
                    if batch.len() >= GREP_CHUNK {
                        out.send(wire::GrepResponse {
                            matches: std::mem::take(&mut batch),
                        })
                        .await?;
                    }
                    if found >= max {
                        break 'files;
                    }
                }
            }
            if !batch.is_empty() {
                out.send(wire::GrepResponse { matches: batch }).await?;
            }
            Ok(())
        }))
    }

    async fn archive(
        &self,
        ctx: &CallCtx,
        req: wire::ArchiveRequest,
    ) -> Result<RpcStream<wire::ArchiveResponse>, Status> {
        #[derive(Clone, Copy)]
        enum Fmt {
            Tar,
            Zip,
        }
        let call = self.core.call(ctx).await?;
        let fmt = match req.format.as_str() {
            "" | "tar" => Fmt::Tar,
            "zip" => Fmt::Zip,
            _ => return Err(Status::invalid_argument("format must be `tar` or `zip`")),
        };
        let prefix = archive_prefix(&req.prefix)?;
        let spec = pathspec(&req.paths)?;
        let id = call.rev(&req.rev).await?;
        let cs = call.changeset(id).await?;
        let mut parents = Vec::with_capacity(cs.parents.len());
        for p in &cs.parents {
            parents.push(call.ids.git_of(*p).await?);
        }
        let subst = Subst::new(
            call.ids.git_of(id).await?,
            call.ids.tree_git(cs.root).await?,
            parents,
            cs.author.clone(),
            cs.committer.clone(),
            &cs.message,
        );
        let mtime = u64::try_from(cs.committer.time()).unwrap_or(0);
        let root = cs.root;
        let (tx, rx) = mpsc::channel(STREAM_DEPTH);
        let err = tx.clone();
        let handle = Handle::current();
        tokio::task::spawn_blocking(move || {
            let writer = ChanWriter {
                tx,
                buf: Vec::new(),
            };
            let run = || -> Result<(), Status> {
                let objects: Arc<dyn Objects> = call.entry.objects.clone();
                let mut attrs = TreeAttrs::new(call.engine.clone(), objects.clone(), root)?;
                let mut sink: Box<dyn Sink> = match fmt {
                    Fmt::Tar => Box::new(TarSink {
                        builder: tar::Builder::new(writer),
                        mtime,
                    }),
                    Fmt::Zip => {
                        let (y, mo, d, h, mi, s) = civil(mtime);
                        let time = zip::DateTime::from_date_and_time(y, mo, d, h, mi, s)
                            .unwrap_or_default();
                        Box::new(ZipSink {
                            zip: zip::ZipWriter::new_stream(writer),
                            time,
                        })
                    }
                };
                let io = |e: io::Error| {
                    if e.kind() == io::ErrorKind::BrokenPipe {
                        Status::cancelled("the client went away")
                    } else {
                        internal("archive", &e)
                    }
                };
                let mut walk = call.engine.walk(root, spec.as_ref());
                while let Some((path, e)) = handle.block_on(walk.try_next()).map_err(tree_status)? {
                    if e.kind == Kind::Gitlink
                        || handle.block_on(attrs.export_ignored(path.as_bytes()))?
                    {
                        continue;
                    }
                    let name = [prefix.as_slice(), path.as_bytes()].concat();
                    let open = || {
                        FileStream::open(
                            objects.clone(),
                            FileId::from_bytes(e.id),
                            Some(e.size),
                            0..e.size,
                        )
                    };
                    let read_all = || -> Result<Vec<u8>, Status> {
                        let (mut s, _) = handle.block_on(open())?;
                        let mut data = Vec::new();
                        while let Some(b) = handle.block_on(s.next())? {
                            data.extend_from_slice(&b);
                        }
                        Ok(data)
                    };
                    if e.kind == Kind::Symlink {
                        if e.size <= SYMLINK_MAX {
                            sink.symlink(&name, &read_all()?).map_err(io)?;
                        }
                        continue;
                    }
                    let exec = e.kind == Kind::Exec;
                    if e.size <= SUBST_MAX_FILE
                        && handle.block_on(attrs.is_set(path.as_bytes(), false, "export-subst"))?
                    {
                        // Both the file and its expansion are held at once.
                        let _guard = handle.block_on(call.budget(e.size + SUBST_MAX_OUT))?;
                        let data = subst.apply(&read_all()?)?;
                        sink.file(&name, exec, data.len() as u64, &mut data.as_slice())
                            .map_err(io)?;
                    } else {
                        let (stream, _) = handle.block_on(open())?;
                        let mut reader = SyncFile {
                            handle: handle.clone(),
                            stream,
                            cur: Bytes::new(),
                        };
                        sink.file(&name, exec, e.size, &mut reader).map_err(io)?;
                    }
                }
                sink.finish().map_err(io)
            };
            if let Err(s) = run() {
                let _ = err.blocking_send(Err(s));
            }
        });
        Ok(stream::unfold(rx, |mut rx| async move { rx.recv().await.map(|v| (v, rx)) }).boxed())
    }

    async fn blame(
        &self,
        ctx: &CallCtx,
        req: wire::BlameRequest,
    ) -> Result<RpcStream<wire::BlameResponse>, Status> {
        const BLAME_CHUNK: usize = 1_000;
        let call = self.core.call(ctx).await?;
        if req.reverse {
            return Err(Status::unimplemented(
                "reverse blame is not available on this server yet",
            ));
        }
        let rev = call.rev(&req.rev).await?;
        let path = repo_path(&req.path)?;
        let root = call.root(rev).await?;
        let entry = call
            .lookup(root, &path)
            .await?
            .filter(|e| matches!(e.kind, Kind::File | Kind::Exec))
            .ok_or_else(|| Status::not_found(format!("{path} is not a file at that revision")))?;
        if entry.size > BIG_FILE {
            return Err(Status::failed_precondition(
                "file is binary (over core.bigFileThreshold)",
            ));
        }
        let _guard = call.budget(entry.size).await?;
        let blame_err = |e: blame::Error| match e {
            blame::Error::NotFound(p) => {
                Status::not_found(format!("{p} is not a file at that revision"))
            }
            blame::Error::BadRange { .. } => Status::out_of_range(e.to_string()),
            e => internal("blame", &e),
        };
        let whole = BlameApi::blame(&*call.engine, rev, &path, None)
            .await
            .map_err(blame_err)?;
        let view = if req.start_line == 0 && req.end_line == 0 {
            whole
        } else {
            let end = if req.end_line == 0 {
                whole.lines()
            } else {
                req.end_line
            };
            whole.slice(req.start_line..end).map_err(blame_err)?
        };
        let mut responses = Vec::new();
        for runs in view.runs().chunks(BLAME_CHUNK) {
            let mut seen = std::collections::HashSet::new();
            let origins: Vec<ChangesetId> = runs
                .iter()
                .map(|r| r.origin)
                .filter(|o| seen.insert(*o))
                .collect();
            let items: Vec<_> = origins.iter().map(|o| (*o, None)).collect();
            let commits = call.log_entries(&items).await?;
            let mut hunks = Vec::with_capacity(runs.len());
            for r in runs {
                hunks.push(wire::BlameHunk {
                    origin: call.ids.changeset_out(r.origin).await?,
                    orig_path: Bytes::copy_from_slice(r.orig_path.as_bytes()),
                    orig_start: r.orig_line,
                    start: r.start,
                    len: r.len,
                });
            }
            responses.push(wire::BlameResponse {
                hunks,
                commits,
                reviews: Vec::new(),
            });
        }
        if responses.is_empty() {
            responses.push(wire::BlameResponse::default());
        }
        Ok(stream::iter(responses.into_iter().map(Ok)).boxed())
    }
}

// ------------------------------------------------------------------------------ history handler

/// The `history` handler group of the full server.
pub struct ServerHistory {
    core: Arc<Core>,
}

impl ServerHistory {
    fn query(req: &wire::LogRequest) -> Result<LogQuery, Status> {
        if req.count_only || !req.pickaxe.is_empty() || !req.resume_token.is_empty() {
            return Err(Status::unimplemented(
                "count_only, pickaxe and resume_token are not available on this server yet",
            ));
        }
        if req.paths.len() > MAX_LIST || req.paths.iter().any(|p| p.len() > MAX_STRING) {
            return Err(Status::invalid_argument("too many or too long paths"));
        }
        if req.author.len() > MAX_STRING || req.grep.len() > MAX_STRING {
            return Err(Status::invalid_argument("author or grep is too long"));
        }
        let paths = req
            .paths
            .iter()
            .map(|p| repo_path(p.as_bytes()))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(LogQuery {
            first_parent: req.first_parent,
            paths,
            follow: req.follow,
            author: (!req.author.is_empty()).then(|| req.author.clone().into_bytes()),
            grep: (!req.grep.is_empty()).then(|| req.grep.clone()),
            since: (req.since != 0).then_some(req.since),
            until: (req.until != 0).then_some(req.until),
            skip: req.skip as usize,
            limit: (req.limit != 0).then_some(req.limit as usize),
            ..LogQuery::default()
        })
    }

    async fn revs(call: &Call, specs: &[String]) -> Result<Vec<ChangesetId>, Status> {
        if specs.len() > MAX_LIST {
            return Err(Status::invalid_argument("too many revisions"));
        }
        let mut out = Vec::with_capacity(specs.len());
        for s in specs {
            out.push(call.rev(s).await?);
        }
        Ok(out)
    }
}

#[async_trait]
impl HistoryHandler for ServerHistory {
    async fn resolve(
        &self,
        ctx: &CallCtx,
        req: wire::ResolveRequest,
    ) -> Result<wire::ResolveResponse, Status> {
        let call = self.core.call(ctx).await?;
        if req.revspecs.len() > MAX_LIST {
            return Err(Status::invalid_argument("too many revisions"));
        }
        let mut ids = Vec::with_capacity(req.revspecs.len());
        for s in &req.revspecs {
            ids.push(match call.rev(s).await {
                Ok(id) => call.ids.changeset_out(id).await?,
                Err(s) if s.code() == tonic::Code::NotFound => Bytes::new(),
                Err(e) => return Err(e),
            });
        }
        Ok(wire::ResolveResponse { ids })
    }

    async fn git_map(
        &self,
        ctx: &CallCtx,
        req: wire::GitMapRequest,
    ) -> Result<wire::GitMapResponse, Status> {
        use asc_rpc::convert::WireId;
        let call = self.core.call(ctx).await?;
        if req.asc.len() > MAX_LIST || req.git.len() > MAX_LIST {
            return Err(Status::invalid_argument("too many ids"));
        }
        let mut asc_to_git = Vec::with_capacity(req.asc.len());
        for b in &req.asc {
            let id = ChangesetId::from_slice(b).map_err(invalid)?;
            asc_to_git.push(match call.engine.get_changeset(id).await {
                Ok(_) => Bytes::copy_from_slice(call.ids.git_of(id).await?.as_bytes()),
                Err(commit::Error::MissingChangeset(_)) => Bytes::new(),
                Err(e) => return Err(commit_status(e)),
            });
        }
        let git: Vec<GitOid> = req
            .git
            .iter()
            .map(|b| GitOid::from_slice(b).map_err(invalid))
            .collect::<Result<_, _>>()?;
        let found = call
            .ids
            .asc_of_git(asc_core::git::ObjectKind::Commit, &git)
            .await?;
        let git_to_asc = found
            .into_iter()
            .map(|c| c.map_or_else(Bytes::new, |c| Bytes::copy_from_slice(&c)))
            .collect();
        Ok(wire::GitMapResponse {
            asc_to_git,
            git_to_asc,
        })
    }

    async fn merge_base(
        &self,
        ctx: &CallCtx,
        req: wire::MergeBaseRequest,
    ) -> Result<wire::MergeBaseResponse, Status> {
        let call = self.core.call(ctx).await?;
        if req.b.len() > MAX_LIST {
            return Err(Status::invalid_argument("too many ids"));
        }
        let a = call.ids.changeset_in(&req.a).await?;
        let mut others = Vec::with_capacity(req.b.len());
        for b in &req.b {
            others.push(call.ids.changeset_in(b).await?);
        }
        let bases = call
            .engine
            .merge_base(a, &others, req.all)
            .await
            .map_err(graph_status)?;
        Ok(wire::MergeBaseResponse {
            bases: call.ids.changesets_out(&bases).await?,
        })
    }

    async fn is_ancestor(
        &self,
        ctx: &CallCtx,
        req: wire::IsAncestorRequest,
    ) -> Result<wire::IsAncestorResponse, Status> {
        let call = self.core.call(ctx).await?;
        let a = call.ids.changeset_in(&req.ancestor).await?;
        let b = call.ids.changeset_in(&req.descendant).await?;
        Ok(wire::IsAncestorResponse {
            result: call.engine.is_ancestor(a, b).await.map_err(graph_status)?,
        })
    }

    async fn log(
        &self,
        ctx: &CallCtx,
        req: wire::LogRequest,
    ) -> Result<RpcStream<wire::LogResponse>, Status> {
        use base64::Engine as _;
        use base64::engine::general_purpose::URL_SAFE_NO_PAD;
        let call = self.core.call(ctx).await?;
        let mut query = Self::query(&req)?;
        let token = if req.page_token.is_empty() {
            None
        } else if req.page_token.len() > 64 * 1024 {
            return Err(Status::invalid_argument("page token is too long"));
        } else {
            Some(PageToken(URL_SAFE_NO_PAD.decode(&req.page_token).map_err(
                |_| Status::invalid_argument("malformed page token"),
            )?))
        };
        if token.is_none() {
            query.include = if req.include.is_empty() {
                // Like `git log`, no start point means the default branch.
                match call.rev("HEAD").await {
                    Ok(id) => vec![id],
                    Err(s) if s.code() == tonic::Code::NotFound => Vec::new(),
                    Err(e) => return Err(e),
                }
            } else {
                Self::revs(&call, &req.include).await?
            };
            query.exclude = Self::revs(&call, &req.exclude).await?;
        }
        Ok(spawn_stream(move |out| async move {
            if query.include.is_empty() && token.is_none() {
                return out.send(wire::LogResponse::default()).await;
            }
            let mut token = token;
            loop {
                let page = call
                    .engine
                    .log(&query, token.as_ref())
                    .await
                    .map_err(graph_status)?;
                debug_assert!(page.entries.len() <= LOG_PAGE);
                let items: Vec<_> = page
                    .entries
                    .iter()
                    .map(|e| (e.id, Some((e.generation, e.git))))
                    .collect();
                let mut responses = Vec::new();
                for chunk in items.chunks(LOG_MSG) {
                    let entries = call.log_entries(chunk).await?;
                    responses.push(wire::LogResponse {
                        count: entries.len() as u64,
                        entries,
                        ..wire::LogResponse::default()
                    });
                }
                if responses.is_empty() {
                    responses.push(wire::LogResponse::default());
                }
                if let (Some(t), Some(last)) = (&page.next, responses.last_mut()) {
                    last.next_page_token = URL_SAFE_NO_PAD.encode(&t.0);
                }
                for r in responses {
                    out.send(r).await?;
                }
                match page.next {
                    Some(t) => token = Some(t),
                    None => return Ok(()),
                }
            }
        }))
    }

    async fn diff(
        &self,
        ctx: &CallCtx,
        req: wire::DiffRequest,
    ) -> Result<RpcStream<wire::DiffResponse>, Status> {
        use wire::DiffFormat as F;
        let call = self.core.call(ctx).await?;
        let format =
            F::try_from(req.format).map_err(|_| Status::invalid_argument("unknown diff format"))?;
        let spec = pathspec(&req.paths)?;
        let mut base = call.rev(&req.base).await?;
        let head = call.rev(&req.head).await?;
        if req.merge_base {
            base = *call
                .engine
                .merge_base(base, &[head], false)
                .await
                .map_err(graph_status)?
                .first()
                .ok_or_else(|| Status::failed_precondition("no merge base"))?;
        }
        let cap = match req.max_files as usize {
            0 => DIFF_DEFAULT_FILES,
            n => n.min(DIFF_MAX_FILES),
        };
        let opts = DiffOptions {
            context: if req.context == 0 {
                3
            } else {
                req.context as usize
            },
            renames: req.renames.then(RenameOptions::default),
            nul: req.null_terminated,
            ..DiffOptions::default()
        };
        let (a, b) = (call.root(base).await?, call.root(head).await?);
        let mut changes = call
            .engine
            .diff_trees(a, b, spec.as_ref(), &opts)
            .await
            .map_err(diff_status)?;
        let truncated = changes.len() > cap;
        changes.truncate(cap);
        Ok(spawn_stream(move |out| async move {
            let wire_changes: Vec<wire::PathChange> =
                changes.iter().map(|c| call.tree_change_wire(c)).collect();
            for chunk in wire_changes.chunks(DIFF_CHANGES_CHUNK) {
                out.send(wire::DiffResponse {
                    changes: chunk.to_vec(),
                    ..wire::DiffResponse::default()
                })
                .await?;
            }
            let ctx = DiffCtx {
                opts: &opts,
                content: &*call.engine,
                attrs: &NoAttributes,
            };
            let mut buf: Vec<u8> = Vec::new();
            let flush = |buf: &mut Vec<u8>| wire::DiffResponse {
                text: Bytes::from(std::mem::take(buf)),
                ..wire::DiffResponse::default()
            };
            match format {
                F::Names | F::Status | F::Raw => {
                    for chunk in changes.chunks(DIFF_CHANGES_CHUNK) {
                        match format {
                            F::Names => diff_format::write_name_only(&mut buf, chunk, &opts),
                            F::Status => diff_format::write_name_status(&mut buf, chunk, &opts),
                            _ => diff_format::write_raw(&mut buf, chunk, &opts),
                        }
                        .map_err(diff_status)?;
                        out.send(flush(&mut buf)).await?;
                    }
                }
                F::Patch | F::Numstat => {
                    for c in &changes {
                        let _guard = call.budget(read_cost(c)).await?;
                        if format == F::Patch {
                            diff::write_patch(&mut buf, &ctx, std::slice::from_ref(c)).await
                        } else {
                            diff::write_numstat(&mut buf, &ctx, std::slice::from_ref(c)).await
                        }
                        .map_err(diff_status)?;
                        if buf.len() >= DIFF_TEXT_CHUNK {
                            out.send(flush(&mut buf)).await?;
                        }
                    }
                }
                F::Stat => {
                    let _guard = call
                        .budget(changes.iter().map(read_cost).max().unwrap_or(0))
                        .await?;
                    diff::write_stat(&mut buf, &ctx, &changes)
                        .await
                        .map_err(diff_status)?;
                }
            }
            out.send(wire::DiffResponse {
                text: Bytes::from(buf),
                truncated,
                ..wire::DiffResponse::default()
            })
            .await?;
            Ok(())
        }))
    }
}

/// `LogEntry`s in one `Log` message.
const LOG_MSG: usize = 512;

/// Bytes of file content a patch of `c` holds at once: both sides, unless one is binary by size.
fn read_cost(c: &TreeChange) -> u64 {
    let sizes: Vec<u64> = [&c.old, &c.new]
        .into_iter()
        .flatten()
        .filter(|e| e.kind != Kind::Gitlink)
        .map(|e| e.size)
        .collect();
    if sizes.iter().any(|&s| s > BIG_FILE) {
        0
    } else {
        sizes.iter().sum()
    }
}

/// Registers the `reads` and `history` groups. The returned hooks take enrichers (C18, C29) and
/// the real text budget (U69).
pub fn register(ext: &Extensions, registry: &Arc<Registry>) -> Arc<ReadHooks> {
    let hooks = Arc::new(ReadHooks::default());
    let core = Arc::new(Core {
        registry: registry.clone(),
        hooks: hooks.clone(),
    });
    ext.handlers
        .set_reads(Arc::new(ServerReads::new(core.clone())))
        .expect("the reads handler group is registered once");
    ext.handlers
        .set_history(Arc::new(ServerHistory { core }))
        .expect("the history handler group is registered once");
    hooks
}

#[cfg(test)]
mod tests {
    use std::io::{Cursor, Read as _};
    use std::path::Path;
    use std::process::Stdio;

    use asc_core::content::{Body, ingest};
    use asc_core::id::RefTarget;
    use asc_core::path::RefName;
    use asc_core::sig::Signature;
    use asc_engine::commit::{NewChangeset, PathPolicy};
    use asc_engine::derive::{index_commit_git, index_tree_git};
    use asc_git::realgit::RealGit;
    use asc_rpc::auth::Principal;
    use asc_rpc::server::RepoName;
    use asc_store::codec::encode_stored;
    use asc_store::refs::{Actor, Expect, RefTxn, RefUpdate, Refs};

    use super::*;
    use crate::ingest::testutil::{registry, repo};

    type Op<'a> = (&'a str, Option<(Kind, &'a [u8])>);

    fn w<'a>(path: &'a str, content: &'a [u8]) -> Op<'a> {
        (path, Some((Kind::File, content)))
    }

    fn x<'a>(path: &'a str, content: &'a [u8]) -> Op<'a> {
        (path, Some((Kind::Exec, content)))
    }

    fn l<'a>(path: &'a str, target: &'a [u8]) -> Op<'a> {
        (path, Some((Kind::Symlink, target)))
    }

    fn d(path: &str) -> Op<'_> {
        (path, None)
    }

    struct Fx {
        reg: Arc<Registry>,
        entry: Arc<RepoEntry>,
        reads: ServerReads,
        hist: ServerHistory,
        hooks: Arc<ReadHooks>,
    }

    async fn fx() -> Fx {
        let reg = registry().await;
        let entry = repo(&reg).await;
        let hooks = Arc::new(ReadHooks::default());
        let core = Arc::new(Core {
            registry: reg.clone(),
            hooks: hooks.clone(),
        });
        Fx {
            reads: ServerReads::new(core.clone()),
            hist: ServerHistory { core },
            reg,
            entry,
            hooks,
        }
    }

    fn ctx(mode: IdMode) -> CallCtx {
        CallCtx {
            repo: RepoName::parse("o/r").unwrap(),
            id_mode: mode,
            principal: Principal { id: "alice".into() },
            op_id: None,
            client: None,
        }
    }

    fn repo_ref(mode: IdMode) -> Option<wire::RepoRef> {
        Some(wire::RepoRef {
            name: "o/r".into(),
            id_mode: mode.to_wire(),
        })
    }

    async fn put(e: &RepoEntry, kind: Kind, content: &[u8]) -> Change {
        let got = ingest(Cursor::new(content)).unwrap();
        match got.body {
            Body::Inline(b) => {
                e.objects
                    .put(&ObjKey::File(got.file), encode_stored(&b))
                    .await
                    .unwrap();
            }
            Body::Chunked { list, chunks } => {
                for (id, data) in chunks {
                    e.objects
                        .put(&ObjKey::Chunk(id), encode_stored(&data))
                        .await
                        .unwrap();
                }
                use asc_core::codec::Enveloped;
                e.objects
                    .put(
                        &ObjKey::ChunkList(got.file),
                        encode_stored(&list.to_envelope()),
                    )
                    .await
                    .unwrap();
            }
        }
        Change::write(kind, got.file, got.size, got.git, None).unwrap()
    }

    /// A changeset with `ops` applied to the first parent's tree.
    async fn mk(
        e: &RepoEntry,
        parents: &[ChangesetId],
        ops: &[Op<'_>],
        msg: &str,
        time: i64,
    ) -> ChangesetId {
        let engine = e.backend.engine();
        let base = match parents.first() {
            Some(&p) => engine.get_changeset(p).await.unwrap().root,
            None => Eng::empty_tree_id(),
        };
        let mut changes = Vec::new();
        for (path, op) in ops {
            let change = match op {
                Some((kind, content)) => put(e, *kind, content).await,
                None => Change::Delete,
            };
            changes.push((RepoPath::new(*path).unwrap(), change));
        }
        changes.sort_by(|a, b| a.0.cmp(&b.0));
        let root = engine.apply(base, &changes).await.unwrap();
        let s = Signature::new(b"A".to_vec(), b"a@x".to_vec(), time, 0).unwrap();
        engine
            .create_changeset(
                NewChangeset {
                    parents: parents.to_vec(),
                    author: s.clone(),
                    committer: s,
                    message: msg.as_bytes().to_vec(),
                    extra: Vec::new(),
                    root,
                    copies: Vec::new(),
                },
                PathPolicy::Native { windows_safe: true },
            )
            .await
            .unwrap()
    }

    /// Records the git map rows of `cs`: the commit, and every tree and blob below it.
    async fn index(e: &RepoEntry, cs: ChangesetId) {
        let engine = e.backend.engine();
        index_commit_git(engine, cs, &*e.git_map).await.unwrap();
        let mut todo = vec![engine.get_changeset(cs).await.unwrap().root];
        while let Some(t) = todo.pop() {
            index_tree_git(engine, t, &*e.git_map).await.unwrap();
            for en in engine.get_tree(t).await.unwrap().entries() {
                if en.kind == Kind::Dir {
                    todo.push(TreeId::from_bytes(en.id));
                }
            }
        }
    }

    async fn set_ref(f: &Fx, name: &str, cs: ChangesetId) {
        let txn = RefTxn {
            updates: vec![RefUpdate {
                name: RefName::new(name.as_bytes()).unwrap(),
                expect: Expect::Any,
                new: Some(RefTarget::Changeset(cs)),
                expires_at: None,
            }],
            actor: Actor {
                id: "t".into(),
                via: "test".into(),
            },
            reason: "test".into(),
            op_id: uuid::Uuid::new_v4(),
        };
        f.reg.refs().update(f.entry.key, &txn).await.unwrap();
    }

    async fn all<T: Send + 'static>(s: RpcStream<T>) -> Vec<T> {
        s.try_collect().await.unwrap()
    }

    async fn all_err<T: Send + 'static>(s: RpcStream<T>) -> Status {
        match s.try_collect::<Vec<_>>().await {
            Err(e) => e,
            Ok(_) => panic!("the stream ended without an error"),
        }
    }

    async fn read_all(
        f: &Fx,
        mode: IdMode,
        at: wire::read_file_request::At,
        offset: u64,
        length: u64,
    ) -> (Vec<u8>, u64) {
        let s = f
            .reads
            .read_file(
                &ctx(mode),
                wire::ReadFileRequest {
                    repo: repo_ref(mode),
                    at: Some(at),
                    offset,
                    length,
                },
            )
            .await
            .unwrap();
        let mut data = Vec::new();
        let mut total = 0;
        for m in all(s).await {
            data.extend_from_slice(&m.data);
            total = m.total_size;
        }
        (data, total)
    }

    fn rev_path(s: &str) -> wire::read_file_request::At {
        wire::read_file_request::At::RevPath(s.into())
    }

    fn git_run(dir: &Path, args: &[&str], stdin: &[u8], time: i64) -> Vec<u8> {
        use std::io::Write as _;
        let real = RealGit::resolve(None, None).expect("git is installed");
        let mut cmd = real.command().unwrap();
        cmd.args(args)
            .current_dir(dir)
            .env("GIT_AUTHOR_NAME", "A")
            .env("GIT_AUTHOR_EMAIL", "a@x")
            .env("GIT_AUTHOR_DATE", format!("{time} +0000"))
            .env("GIT_COMMITTER_NAME", "A")
            .env("GIT_COMMITTER_EMAIL", "a@x")
            .env("GIT_COMMITTER_DATE", format!("{time} +0000"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = cmd.spawn().unwrap();
        child.stdin.take().unwrap().write_all(stdin).unwrap();
        let out = child.wait_with_output().unwrap();
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        out.stdout
    }

    fn git_text(dir: &Path, args: &[&str]) -> String {
        String::from_utf8(git_run(dir, args, b"", 0)).unwrap()
    }

    /// Mirrors one changeset into the git repo at `dir`; returns git's commit id.
    fn git_commit(
        dir: &Path,
        parent: Option<&str>,
        ops: &[Op<'_>],
        msg: &str,
        time: i64,
    ) -> String {
        for (path, op) in ops {
            match op {
                Some((kind, content)) => {
                    let oid = String::from_utf8(git_run(
                        dir,
                        &["hash-object", "-w", "--stdin"],
                        content,
                        0,
                    ))
                    .unwrap()
                    .trim()
                    .to_owned();
                    let mode = match kind {
                        Kind::Exec => "100755",
                        Kind::Symlink => "120000",
                        _ => "100644",
                    };
                    git_run(
                        dir,
                        &[
                            "update-index",
                            "--add",
                            "--cacheinfo",
                            &format!("{mode},{oid},{path}"),
                        ],
                        b"",
                        0,
                    );
                }
                None => {
                    git_run(dir, &["update-index", "--force-remove", path], b"", 0);
                }
            }
        }
        let tree = git_text(dir, &["write-tree"]).trim().to_owned();
        let mut args = vec!["commit-tree", tree.as_str()];
        if let Some(p) = parent {
            args.extend(["-p", p]);
        }
        args.extend(["-F", "-"]);
        String::from_utf8(git_run(dir, &args, msg.as_bytes(), time))
            .unwrap()
            .trim()
            .to_owned()
    }

    fn hex(b: &[u8]) -> String {
        b.iter().map(|x| format!("{x:02x}")).collect()
    }

    // ------------------------------------------------------------------------ git parity

    #[tokio::test]
    async fn git_mode_equals_the_git_cli() {
        let f = fx().await;
        let dir = tempfile::tempdir().unwrap();
        git_run(dir.path(), &["init", "-q"], b"", 0);
        let ops1 = [
            w("a.txt", b"hello\nworld\n"),
            x("run.sh", b"#!/bin/sh\n"),
            w("d/b.txt", b"bee\n"),
            l("link", b"a.txt"),
        ];
        let ops2 = [
            w("a.txt", b"hello\nthere\nworld\n"),
            d("d/b.txt"),
            w("d/e/c.txt", b"sea sea\n"),
        ];
        let c1 = mk(&f.entry, &[], &ops1, "one\n", 1_700_000_000).await;
        let c2 = mk(&f.entry, &[c1], &ops2, "two\n\nbody\n", 1_700_000_100).await;
        let g1 = git_commit(dir.path(), None, &ops1, "one\n", 1_700_000_000);
        let g2 = git_commit(dir.path(), Some(&g1), &ops2, "two\n\nbody\n", 1_700_000_100);
        index(&f.entry, c1).await;
        index(&f.entry, c2).await;
        let git = IdMode::Git;

        // The commit ids are git's own.
        let ids = IdCodec::new(git, &f.reg, &f.entry);
        assert_eq!(ids.git_of(c1).await.unwrap().to_string(), g1);
        assert_eq!(ids.git_of(c2).await.unwrap().to_string(), g2);

        // Log: git ids, newest first, like `git log --format=%H`.
        let logs = all(f
            .hist
            .log(
                &ctx(git),
                wire::LogRequest {
                    repo: repo_ref(git),
                    include: vec![g2.clone()],
                    ..Default::default()
                },
            )
            .await
            .unwrap())
        .await;
        let got: Vec<String> = logs
            .iter()
            .flat_map(|r| &r.entries)
            .map(|e| hex(&e.id))
            .collect();
        let want: Vec<String> = git_text(dir.path(), &["log", "--format=%H", &g2])
            .lines()
            .map(str::to_owned)
            .collect();
        assert_eq!(got, want);
        let first = &logs[0].entries[0];
        assert_eq!(hex(&first.parents[0]), g1);
        assert_eq!(first.message.as_ref(), b"two\n\nbody\n");

        // ReadTree: recursive and flat listings equal `git ls-tree`.
        for (recursive, flags) in [(true, &["ls-tree", "-r"][..]), (false, &["ls-tree"][..])] {
            let mut args = flags.to_vec();
            args.push(&g2);
            let want: Vec<String> = git_text(dir.path(), &args)
                .lines()
                .map(|l| {
                    let (meta, path) = l.split_once('\t').unwrap();
                    let mut it = meta.split(' ');
                    let (mode, _kind, oid) =
                        (it.next().unwrap(), it.next().unwrap(), it.next().unwrap());
                    format!("{mode} {oid} {path}")
                })
                .collect();
            let s = f
                .reads
                .read_tree(
                    &ctx(git),
                    wire::ReadTreeRequest {
                        repo: repo_ref(git),
                        rev: g2.clone(),
                        recursive,
                        ..Default::default()
                    },
                )
                .await
                .unwrap();
            let got: Vec<String> = all(s)
                .await
                .iter()
                .flat_map(|r| &r.entries)
                .map(|e| {
                    format!(
                        "{:06o} {} {}",
                        e.mode,
                        hex(&e.id),
                        String::from_utf8_lossy(&e.path)
                    )
                })
                .collect();
            assert_eq!(got, want, "recursive={recursive}");
        }

        // ReadFile by rev:path and by blob id equals `git cat-file blob`.
        let (data, total) = read_all(&f, git, rev_path(&format!("{g2}:a.txt")), 0, 0).await;
        assert_eq!(data, b"hello\nthere\nworld\n");
        assert_eq!(total, data.len() as u64);
        let blob = git_text(dir.path(), &["rev-parse", &format!("{g2}:d/e/c.txt")])
            .trim()
            .to_owned();
        let oid: Vec<u8> = (0..20)
            .map(|i| u8::from_str_radix(&blob[2 * i..2 * i + 2], 16).unwrap())
            .collect();
        let (data, _) = read_all(
            &f,
            git,
            wire::read_file_request::At::FileId(oid.into()),
            0,
            0,
        )
        .await;
        assert_eq!(
            data,
            git_run(dir.path(), &["cat-file", "blob", &blob], b"", 0)
        );

        // Diff: names, status and the patch text equal `git diff`.
        for (fmt, args) in [
            (
                wire::DiffFormat::Names,
                vec!["diff", "--no-renames", "--name-only"],
            ),
            (
                wire::DiffFormat::Status,
                vec!["diff", "--no-renames", "--name-status"],
            ),
            (wire::DiffFormat::Patch, vec!["diff", "--no-renames"]),
            (
                wire::DiffFormat::Numstat,
                vec!["diff", "--no-renames", "--numstat"],
            ),
        ] {
            let s = f
                .hist
                .diff(
                    &ctx(git),
                    wire::DiffRequest {
                        repo: repo_ref(git),
                        base: g1.clone(),
                        head: g2.clone(),
                        format: fmt as i32,
                        ..Default::default()
                    },
                )
                .await
                .unwrap();
            let rs = all(s).await;
            let text: Vec<u8> = rs.iter().flat_map(|r| r.text.to_vec()).collect();
            let mut a = args.clone();
            a.extend([g1.as_str(), g2.as_str()]);
            assert_eq!(
                String::from_utf8(text).unwrap(),
                git_text(dir.path(), &a),
                "{fmt:?}"
            );
            let changes: Vec<&wire::PathChange> = rs.iter().flat_map(|r| &r.changes).collect();
            assert_eq!(changes.len(), 3);
            if fmt == wire::DiffFormat::Names {
                assert_eq!(changes[2].new_git.len(), 20);
            }
        }

        // ReadChangeset, Resolve, MergeBase, IsAncestor, GitMap and LastCommits speak git ids.
        let rc = f
            .reads
            .read_changeset(
                &ctx(git),
                wire::ReadChangesetRequest {
                    repo: repo_ref(git),
                    rev: g2.clone(),
                },
            )
            .await
            .unwrap();
        assert_eq!(hex(&rc.header.unwrap().id), g2);
        assert_eq!(
            hex(&rc.root_git),
            git_text(dir.path(), &["rev-parse", &format!("{g2}^{{tree}}")]).trim()
        );
        assert_eq!(rc.root_tree, rc.root_git);
        let statuses: Vec<(String, String)> = rc
            .changes
            .iter()
            .map(|c| {
                (
                    String::from_utf8_lossy(&c.path).into_owned(),
                    c.status.clone(),
                )
            })
            .collect();
        assert_eq!(
            statuses,
            [
                ("a.txt".into(), "M".into()),
                ("d/b.txt".into(), "D".into()),
                ("d/e/c.txt".into(), "A".into())
            ]
        );
        let res = f
            .hist
            .resolve(
                &ctx(git),
                wire::ResolveRequest {
                    repo: repo_ref(git),
                    revspecs: vec![format!("{g2}~1"), "nope".into(), "HEAD".into()],
                },
            )
            .await
            .unwrap();
        assert_eq!(hex(&res.ids[0]), g1);
        assert!(res.ids[1].is_empty());
        assert!(res.ids[2].is_empty(), "no branch yet");
        set_ref(&f, "refs/heads/master", c2).await;
        let res = f
            .hist
            .resolve(
                &ctx(git),
                wire::ResolveRequest {
                    repo: repo_ref(git),
                    revspecs: vec!["HEAD".into(), "master".into()],
                },
            )
            .await
            .unwrap();
        assert_eq!(hex(&res.ids[0]), g2);
        assert_eq!(res.ids[0], res.ids[1]);
        let g1b: Vec<u8> = (0..20)
            .map(|i| u8::from_str_radix(&g1[2 * i..2 * i + 2], 16).unwrap())
            .collect();
        let g2b: Vec<u8> = (0..20)
            .map(|i| u8::from_str_radix(&g2[2 * i..2 * i + 2], 16).unwrap())
            .collect();
        let mb = f
            .hist
            .merge_base(
                &ctx(git),
                wire::MergeBaseRequest {
                    repo: repo_ref(git),
                    a: g2b.clone().into(),
                    b: vec![g1b.clone().into()],
                    all: false,
                },
            )
            .await
            .unwrap();
        assert_eq!(hex(&mb.bases[0]), g1);
        let anc = f
            .hist
            .is_ancestor(
                &ctx(git),
                wire::IsAncestorRequest {
                    repo: repo_ref(git),
                    ancestor: g1b.clone().into(),
                    descendant: g2b.clone().into(),
                },
            )
            .await
            .unwrap();
        assert!(anc.result);
        let gm = f
            .hist
            .git_map(
                &ctx(git),
                wire::GitMapRequest {
                    repo: repo_ref(git),
                    asc: vec![c2.as_bytes().to_vec().into()],
                    git: vec![g1b.clone().into()],
                },
            )
            .await
            .unwrap();
        assert_eq!(hex(&gm.asc_to_git[0]), g2);
        assert_eq!(gm.git_to_asc[0].as_ref(), c1.as_bytes());
        let lc = f
            .reads
            .last_commits(
                &ctx(git),
                wire::LastCommitsRequest {
                    repo: repo_ref(git),
                    rev: g2.clone(),
                    dir: Bytes::new(),
                    names: vec!["a.txt".into(), "run.sh".into(), "ghost".into()],
                },
            )
            .await
            .unwrap();
        assert_eq!(hex(&lc.by_name["a.txt"].id), g2);
        assert_eq!(hex(&lc.by_name["run.sh"].id), g1);
        assert!(!lc.by_name.contains_key("ghost"));

        // Blame lines are attributed to git ids.
        let b = all(f
            .reads
            .blame(
                &ctx(git),
                wire::BlameRequest {
                    repo: repo_ref(git),
                    rev: g2.clone(),
                    path: "a.txt".into(),
                    ..Default::default()
                },
            )
            .await
            .unwrap())
        .await;
        let origins: Vec<String> = b
            .iter()
            .flat_map(|r| &r.hunks)
            .map(|h| hex(&h.origin))
            .collect();
        assert_eq!(origins, [g1.clone(), g2.clone(), g1.clone()]);
        assert!(b[0].commits.iter().all(|c| c.id.len() == 20));
    }

    #[tokio::test]
    async fn asc_mode_speaks_32_byte_ids_and_authorises_by_repo() {
        let f = fx().await;
        let c1 = mk(&f.entry, &[], &[w("a", b"1\n")], "one\n", 1).await;
        let rc = f
            .reads
            .read_changeset(
                &ctx(IdMode::Asc),
                wire::ReadChangesetRequest {
                    repo: repo_ref(IdMode::Asc),
                    rev: c1.to_string(),
                },
            )
            .await
            .unwrap();
        let h = rc.header.unwrap();
        assert_eq!(&h.id[..], c1.as_bytes());
        assert_eq!(h.git.len(), 20);
        assert_eq!(rc.root_tree.len(), 32);
        // An unknown repository is not found, whatever the id mode.
        let mut other = ctx(IdMode::Asc);
        other.repo = RepoName::parse("o/missing").unwrap();
        let err = f
            .reads
            .read_changeset(
                &other,
                wire::ReadChangesetRequest {
                    repo: repo_ref(IdMode::Asc),
                    rev: c1.to_string(),
                },
            )
            .await
            .unwrap_err();
        assert_eq!(err.code(), tonic::Code::NotFound);
        // A changeset of another repository is invisible here.
        let o = f
            .reg
            .create(&RepoName::parse("o/other").unwrap(), &Default::default())
            .await
            .unwrap();
        let theirs = mk(&o, &[], &[w("a", b"secret\n")], "x\n", 2).await;
        let err = f
            .reads
            .read_changeset(
                &ctx(IdMode::Asc),
                wire::ReadChangesetRequest {
                    repo: repo_ref(IdMode::Asc),
                    rev: theirs.to_string(),
                },
            )
            .await
            .unwrap_err();
        assert_eq!(err.code(), tonic::Code::NotFound);
    }

    // ------------------------------------------------------------------------ files and trees

    #[tokio::test]
    async fn read_file_streams_ranges_and_verifies_chunks() {
        let f = fx().await;
        // Pseudo-random bytes so the chunker finds several chunks.
        let mut state = 0x1234_5678_u64;
        let big: Vec<u8> = (0..9 * 1024 * 1024)
            .map(|_| {
                state = state
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                (state >> 33) as u8
            })
            .collect();
        let c = mk(
            &f.entry,
            &[],
            &[w("big.bin", &big), w("small.txt", b"0123456789")],
            "m\n",
            1,
        )
        .await;
        let rev = c.to_string();
        let (data, total) =
            read_all(&f, IdMode::Asc, rev_path(&format!("{rev}:big.bin")), 0, 0).await;
        assert_eq!(total, big.len() as u64);
        assert!(data == big, "whole file");
        // A range that starts mid-chunk and ends in another one.
        let (data, _) = read_all(
            &f,
            IdMode::Asc,
            rev_path(&format!("{rev}:big.bin")),
            3_000_000,
            2_500_000,
        )
        .await;
        assert!(data == big[3_000_000..5_500_000], "ranged read");
        // Past the end the range is empty and the total is still reported.
        let (data, total) =
            read_all(&f, IdMode::Asc, rev_path(&format!("{rev}:small.txt")), 99, 5).await;
        assert!(data.is_empty());
        assert_eq!(total, 10);
        let (data, _) = read_all(&f, IdMode::Asc, rev_path(&format!("{rev}:small.txt")), 3, 4).await;
        assert_eq!(data, b"3456");
        // By file id, chunked or not.
        let id = FileId::of(&big);
        let (data, total) = read_all(
            &f,
            IdMode::Asc,
            wire::read_file_request::At::FileId(id.as_bytes().to_vec().into()),
            0,
            0,
        )
        .await;
        assert_eq!((total, data.len()), (big.len() as u64, big.len()));
        // Errors: directories, missing paths, bad specs.
        let c2 = mk(&f.entry, &[c], &[w("dir/x", b"x")], "m\n", 2).await;
        for (spec, code) in [
            (format!("{c2}:dir"), tonic::Code::FailedPrecondition),
            (format!("{c2}:nope"), tonic::Code::NotFound),
            ("no-colon".to_owned(), tonic::Code::InvalidArgument),
            (format!("{c2}:../x"), tonic::Code::InvalidArgument),
        ] {
            let err = f
                .reads
                .read_file(
                    &ctx(IdMode::Asc),
                    wire::ReadFileRequest {
                        repo: repo_ref(IdMode::Asc),
                        at: Some(rev_path(&spec)),
                        ..Default::default()
                    },
                )
                .await
                .err()
                .unwrap();
            assert_eq!(err.code(), code, "{spec}");
        }
        // A missing chunk is reported as damage, not as a short file.
        let ObjKey::ChunkList(_) = ObjKey::ChunkList(id) else {
            unreachable!()
        };
        let raw = f
            .entry
            .objects
            .get(&ObjKey::ChunkList(id))
            .await
            .unwrap()
            .unwrap();
        let list = decode_envelope::<ChunkList>(&raw).unwrap();
        f.entry
            .objects
            .delete(&ObjKey::Chunk(list.chunks[1].0))
            .await
            .unwrap();
        let s = f
            .reads
            .read_file(
                &ctx(IdMode::Asc),
                wire::ReadFileRequest {
                    repo: repo_ref(IdMode::Asc),
                    at: Some(rev_path(&format!("{rev}:big.bin"))),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(all_err(s).await.code(), tonic::Code::DataLoss);
    }

    #[tokio::test]
    async fn read_tree_pages_resume_after_the_cursor() {
        let f = fx().await;
        let names: Vec<String> = (0..7).map(|i| format!("n{i}")).collect();
        let mut ops = vec![
            w("a/x", b"1"),
            w("a/y/z", b"2"),
            w("a/y/zz", b"3"),
            w("a.b", b"4"),
        ];
        ops.extend(names.iter().map(|n| w(n.as_str(), b"n")));
        let c = mk(&f.entry, &[], &ops, "m\n", 1).await;
        for (path, recursive) in [("", true), ("", false), ("a", true), ("a/y", false)] {
            let list = |limit: u32, after: &str| {
                let req = wire::ReadTreeRequest {
                    repo: repo_ref(IdMode::Asc),
                    rev: c.to_string(),
                    path: path.as_bytes().to_vec().into(),
                    recursive,
                    limit,
                    after: after.as_bytes().to_vec().into(),
                };
                let f = &f;
                async move {
                    let s = f.reads.read_tree(&ctx(IdMode::Asc), req).await.unwrap();
                    all(s)
                        .await
                        .into_iter()
                        .flat_map(|r| r.entries)
                        .map(|e| String::from_utf8(e.path.to_vec()).unwrap())
                        .collect::<Vec<_>>()
                }
            };
            let whole = list(0, "").await;
            assert!(!whole.is_empty());
            let mut paged = Vec::new();
            let mut after = String::new();
            loop {
                let page = list(2, &after).await;
                assert!(page.len() <= 2);
                if page.is_empty() {
                    break;
                }
                after = page.last().unwrap().clone();
                paged.extend(page);
            }
            assert_eq!(paged, whole, "{path:?} recursive={recursive}");
        }
        // The response names the directory's own tree.
        let s = f
            .reads
            .read_tree(
                &ctx(IdMode::Asc),
                wire::ReadTreeRequest {
                    repo: repo_ref(IdMode::Asc),
                    rev: c.to_string(),
                    path: "a".into(),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        let first = &all(s).await[0];
        assert_eq!(first.tree_id.len(), 32);
        assert_eq!(first.tree_git.len(), 20);
        assert_eq!(first.entries[0].path.as_ref(), b"a/x");
        assert_eq!(first.entries[0].name.as_ref(), b"x");
        // Bad requests.
        for (req, code) in [
            (
                wire::ReadTreeRequest {
                    path: "a/x".into(),
                    ..Default::default()
                },
                tonic::Code::FailedPrecondition,
            ),
            (
                wire::ReadTreeRequest {
                    path: "zzz".into(),
                    ..Default::default()
                },
                tonic::Code::NotFound,
            ),
            (
                wire::ReadTreeRequest {
                    after: "b/c".into(),
                    path: "a".into(),
                    ..Default::default()
                },
                tonic::Code::InvalidArgument,
            ),
            (
                wire::ReadTreeRequest {
                    path: "../a".into(),
                    ..Default::default()
                },
                tonic::Code::InvalidArgument,
            ),
        ] {
            let req = wire::ReadTreeRequest {
                repo: repo_ref(IdMode::Asc),
                rev: c.to_string(),
                ..req
            };
            let err = f
                .reads
                .read_tree(&ctx(IdMode::Asc), req)
                .await
                .err()
                .unwrap();
            assert_eq!(err.code(), code);
        }
    }

    #[tokio::test]
    async fn stat_reports_paths_and_objects() {
        let f = fx().await;
        let c = mk(
            &f.entry,
            &[],
            &[w("a/b.txt", b"bee"), x("run", b"#!")],
            "m\n",
            1,
        )
        .await;
        let r = f
            .reads
            .stat(
                &ctx(IdMode::Asc),
                wire::StatRequest {
                    repo: repo_ref(IdMode::Asc),
                    rev: c.to_string(),
                    paths: vec![
                        "a/b.txt".into(),
                        "a".into(),
                        "run".into(),
                        "".into(),
                        "gone".into(),
                    ],
                    objects: vec![
                        wire::ObjKey {
                            kind: wire::ObjKind::File as i32,
                            id: FileId::of(b"bee").as_bytes().to_vec().into(),
                        },
                        wire::ObjKey {
                            kind: wire::ObjKind::File as i32,
                            id: vec![9; 32].into(),
                        },
                        wire::ObjKey {
                            kind: wire::ObjKind::Changeset as i32,
                            id: c.as_bytes().to_vec().into(),
                        },
                    ],
                },
            )
            .await
            .unwrap();
        assert_eq!(r.exists, [true, true, true, true, false, true, false, true]);
        assert_eq!((r.entries[0].mode, r.entries[0].size), (0o100_644, 3));
        assert_eq!(r.entries[1].mode, 0o040_000);
        assert_eq!(r.entries[2].mode, 0o100_755);
        assert_eq!(r.entries[3].mode, 0o040_000);
        // Too many items are refused.
        let err = f
            .reads
            .stat(
                &ctx(IdMode::Asc),
                wire::StatRequest {
                    repo: repo_ref(IdMode::Asc),
                    rev: c.to_string(),
                    paths: vec!["a".into(); MAX_NAMES + 1],
                    objects: vec![],
                },
            )
            .await
            .unwrap_err();
        assert_eq!(err.code(), tonic::Code::InvalidArgument);
    }

    // ------------------------------------------------------------------------ attributes, grep

    #[tokio::test]
    async fn check_attr_reads_attributes_from_the_tree() {
        let f = fx().await;
        let c = mk(
            &f.entry,
            &[],
            &[
                w(
                    ".gitattributes",
                    b"*.txt text eol=lf\n*.bin binary\n* -foo\n",
                ),
                w(
                    "sub/.gitattributes",
                    b"*.txt eol=crlf\nspecial.txt export-ignore diff=x\n",
                ),
                w("a.txt", b"a"),
                w("sub/b.txt", b"b"),
                w("sub/special.txt", b"s"),
                w("sub/z.bin", b"z"),
            ],
            "m\n",
            1,
        )
        .await;
        let r = f
            .reads
            .check_attr(
                &ctx(IdMode::Asc),
                wire::CheckAttrRequest {
                    repo: repo_ref(IdMode::Asc),
                    rev: c.to_string(),
                    paths: vec![
                        "a.txt".into(),
                        "sub/b.txt".into(),
                        "sub/special.txt".into(),
                        "sub/z.bin".into(),
                        "other".into(),
                    ],
                    names: vec![
                        "text".into(),
                        "eol".into(),
                        "export-ignore".into(),
                        "diff".into(),
                        "foo".into(),
                    ],
                },
            )
            .await
            .unwrap();
        let v = |i: usize, n: &str| r.per_path[i].values[n].clone();
        assert_eq!(
            (
                v(0, "text").as_str(),
                v(0, "eol").as_str(),
                v(0, "foo").as_str()
            ),
            ("set", "lf", "unset")
        );
        assert_eq!(v(1, "eol"), "crlf", "the nearer file wins");
        assert_eq!(v(2, "export-ignore"), "set");
        assert_eq!(v(2, "diff"), "x");
        assert_eq!(v(3, "diff"), "unset", "binary macro");
        assert_eq!(v(4, "text"), "unspecified");
        for bad in [
            vec![],
            vec!["-x".to_owned()],
            vec!["a b".to_owned()],
            vec!["x".to_owned(); MAX_ATTR_NAMES + 1],
        ] {
            let err = f
                .reads
                .check_attr(
                    &ctx(IdMode::Asc),
                    wire::CheckAttrRequest {
                        repo: repo_ref(IdMode::Asc),
                        rev: c.to_string(),
                        paths: vec!["a.txt".into()],
                        names: bad,
                    },
                )
                .await
                .unwrap_err();
            assert_eq!(err.code(), tonic::Code::InvalidArgument);
        }
    }

    #[tokio::test]
    async fn ascattributes_replaces_gitattributes_in_its_directory() {
        let f = fx().await;
        let c = mk(
            &f.entry,
            &[],
            &[
                w(".ascattributes", b"* mine\n"),
                w(".gitattributes", b"* theirs\n"),
                w("a", b"a"),
            ],
            "m\n",
            1,
        )
        .await;
        let r = f
            .reads
            .check_attr(
                &ctx(IdMode::Asc),
                wire::CheckAttrRequest {
                    repo: repo_ref(IdMode::Asc),
                    rev: c.to_string(),
                    paths: vec!["a".into()],
                    names: vec!["mine".into(), "theirs".into()],
                },
            )
            .await
            .unwrap();
        assert_eq!(r.per_path[0].values["mine"], "set");
        assert_eq!(r.per_path[0].values["theirs"], "unspecified");
    }

    async fn grep(
        f: &Fx,
        c: ChangesetId,
        pattern: &str,
        fixed: bool,
        ic: bool,
        max: u32,
    ) -> Result<Vec<(String, u32, String)>, Status> {
        let s = f
            .reads
            .grep(
                &ctx(IdMode::Asc),
                wire::GrepRequest {
                    repo: repo_ref(IdMode::Asc),
                    rev: c.to_string(),
                    pattern: pattern.into(),
                    fixed,
                    ignore_case: ic,
                    paths: vec![],
                    max_results: max,
                },
            )
            .await?;
        Ok(s.try_collect::<Vec<_>>()
            .await?
            .into_iter()
            .flat_map(|r| r.matches)
            .map(|m| {
                (
                    String::from_utf8_lossy(&m.path).into_owned(),
                    m.line,
                    String::from_utf8_lossy(&m.text).into_owned(),
                )
            })
            .collect())
    }

    #[tokio::test]
    async fn grep_skips_binaries_and_big_files_and_honours_limits() {
        let f = fx().await;
        let mut big = vec![b'a'; (GREP_MAX_FILE + 1) as usize];
        big.extend_from_slice(b"\nneedle\n");
        let c = mk(
            &f.entry,
            &[],
            &[
                w("a.txt", b"one\nNeedle here\nthree needle\n"),
                w("d/b.txt", b"no match\r\nneedle at end"),
                w("bin.dat", b"needle\0binary"),
                w("big.txt", &big),
                w("empty", b""),
            ],
            "m\n",
            1,
        )
        .await;
        let got = grep(&f, c, "needle", true, false, 0).await.unwrap();
        assert_eq!(
            got,
            [
                ("a.txt".into(), 3, "three needle".into()),
                ("d/b.txt".into(), 2, "needle at end".into())
            ]
        );
        let got = grep(&f, c, "needle", false, true, 0).await.unwrap();
        assert_eq!(got.len(), 3);
        assert_eq!(got[0].1, 2);
        assert_eq!(
            grep(&f, c, "needle", false, true, 2).await.unwrap().len(),
            2
        );
        assert_eq!(
            grep(&f, c, "^three", false, false, 0).await.unwrap().len(),
            1
        );
        // A fixed string is not a regex.
        assert!(grep(&f, c, "(", true, false, 0).await.unwrap().is_empty());
        // Bad or hostile patterns are refused.
        assert_eq!(
            grep(&f, c, "(", false, false, 0).await.unwrap_err().code(),
            tonic::Code::InvalidArgument
        );
        assert_eq!(
            grep(&f, c, "", false, false, 0).await.unwrap_err().code(),
            tonic::Code::InvalidArgument
        );
        assert_eq!(
            grep(&f, c, "(a{1000}){1000}", false, false, 0)
                .await
                .unwrap_err()
                .code(),
            tonic::Code::InvalidArgument
        );
        // A starved text budget sheds the request instead of buffering.
        struct Never;
        #[async_trait]
        impl crate::TextBudget for Never {
            async fn acquire(&self, _: u64) -> Result<TextGuard, crate::TextBudgetExhausted> {
                Err(crate::TextBudgetExhausted)
            }
        }
        assert!(f.hooks.text_budget.set(Arc::new(Never)).is_ok());
        assert_eq!(
            grep(&f, c, "needle", true, false, 0)
                .await
                .unwrap_err()
                .code(),
            tonic::Code::ResourceExhausted
        );
    }

    // ------------------------------------------------------------------------ archive

    fn untar(parts: Vec<wire::ArchiveResponse>) -> Vec<(String, Vec<u8>, u32, tar::EntryType)> {
        let bytes: Vec<u8> = parts.iter().flat_map(|p| p.data.to_vec()).collect();
        let mut a = tar::Archive::new(Cursor::new(bytes));
        a.entries()
            .unwrap()
            .map(|e| {
                let mut e = e.unwrap();
                let mut data = Vec::new();
                e.read_to_end(&mut data).unwrap();
                let kind = e.header().entry_type();
                if kind == tar::EntryType::Symlink {
                    data = e
                        .link_name()
                        .unwrap()
                        .unwrap()
                        .to_string_lossy()
                        .into_owned()
                        .into_bytes();
                }
                (
                    e.path().unwrap().to_string_lossy().into_owned(),
                    data,
                    e.header().mode().unwrap(),
                    kind,
                )
            })
            .collect()
    }

    async fn archive(
        f: &Fx,
        c: ChangesetId,
        format: &str,
        prefix: &str,
        paths: &[&str],
    ) -> Result<Vec<wire::ArchiveResponse>, Status> {
        let s = f
            .reads
            .archive(
                &ctx(IdMode::Asc),
                wire::ArchiveRequest {
                    repo: repo_ref(IdMode::Asc),
                    rev: c.to_string(),
                    format: format.into(),
                    prefix: prefix.into(),
                    paths: paths.iter().map(|p| (*p).into()).collect(),
                },
            )
            .await?;
        s.try_collect().await
    }

    #[tokio::test]
    async fn archive_honours_export_attributes_prefix_and_pathspec() {
        let f = fx().await;
        let c = mk(
            &f.entry,
            &[],
            &[
                w(
                    ".gitattributes",
                    b"ignored.txt export-ignore\nsecret export-ignore\nver.txt export-subst\n",
                ),
                w("keep.txt", b"keep\n"),
                w("ignored.txt", b"no\n"),
                w("secret/a", b"no\n"),
                w("secret/deep/b", b"no\n"),
                w(
                    "ver.txt",
                    b"id=$Format:%H|%h|%an <%ae>|%s|%%$ ok $Format:unclosed\n$Format:%P$\n",
                ),
                x("bin/run", b"#!/bin/sh\n"),
                l("ln", b"keep.txt"),
            ],
            "subject line\n\nthe body\n",
            1_700_000_000,
        )
        .await;
        let git = IdCodec::new(IdMode::Git, &f.reg, &f.entry)
            .git_of(c)
            .await
            .unwrap()
            .to_string();
        let tar = untar(archive(&f, c, "tar", "", &[]).await.unwrap());
        let names: Vec<&str> = tar.iter().map(|e| e.0.as_str()).collect();
        assert_eq!(
            names,
            [".gitattributes", "bin/run", "keep.txt", "ln", "ver.txt"]
        );
        let get = |n: &str| tar.iter().find(|e| e.0 == n).unwrap();
        assert_eq!(get("bin/run").2 & 0o777, 0o755);
        assert_eq!(get("keep.txt").2 & 0o777, 0o644);
        assert_eq!(
            (get("ln").3, get("ln").1.as_slice()),
            (tar::EntryType::Symlink, &b"keep.txt"[..])
        );
        assert_eq!(
            String::from_utf8(get("ver.txt").1.clone()).unwrap(),
            format!(
                "id={git}|{}|A <a@x>|subject line|% ok $Format:unclosed\n\n",
                &git[..7]
            )
        );
        // Prefix and pathspec.
        let tar = untar(
            archive(&f, c, "tar", "proj-1.0", &["keep.txt", "bin"])
                .await
                .unwrap(),
        );
        assert_eq!(
            tar.iter().map(|e| e.0.as_str()).collect::<Vec<_>>(),
            ["proj-1.0/bin/run", "proj-1.0/keep.txt"]
        );
        // Zip.
        let parts = archive(&f, c, "zip", "p/", &[]).await.unwrap();
        let bytes: Vec<u8> = parts.iter().flat_map(|p| p.data.to_vec()).collect();
        let mut z = zip::ZipArchive::new(Cursor::new(bytes)).unwrap();
        let mut names: Vec<String> = (0..z.len())
            .map(|i| z.by_index(i).unwrap().name().unwrap().to_string())
            .collect();
        names.sort();
        assert_eq!(
            names,
            [
                "p/.gitattributes",
                "p/bin/run",
                "p/keep.txt",
                "p/ln",
                "p/ver.txt"
            ]
        );
        let mut keep = String::new();
        z.by_name("p/keep.txt")
            .unwrap()
            .read_to_string(&mut keep)
            .unwrap();
        assert_eq!(keep, "keep\n");
        // Hostile prefixes, formats and pathspecs.
        for (fmt, prefix) in [
            ("tar", "../x"),
            ("tar", "/abs"),
            ("tar", "a//b"),
            ("rar", ""),
            ("tar.gz", ""),
        ] {
            let err = archive(&f, c, fmt, prefix, &[]).await.err().unwrap();
            assert_eq!(err.code(), tonic::Code::InvalidArgument, "{fmt} {prefix}");
        }
    }

    fn test_subst(message: &[u8]) -> Subst {
        let sig = || asc_core::sig::Signature::new(b"A".to_vec(), b"a@x".to_vec(), 0, 0).unwrap();
        let oid = GitOid::from_bytes([7; 20]);
        Subst::new(oid, oid, Vec::new(), sig(), sig(), message)
    }

    #[test]
    fn subst_expansion_is_capped() {
        // 1 MiB of `%B` placeholders over a 2 KiB message would expand to ~175 MB uncapped.
        let subst = test_subst(&vec![b'm'; 2048]);
        let line = b"$Format:%B$\n";
        let data = line.repeat((SUBST_MAX_FILE as usize) / line.len());
        let err = subst
            .apply(&data)
            .expect_err("the cap must stop the expansion");
        assert_eq!(err.code(), tonic::Code::ResourceExhausted);
        // One placeholder repeated within a format is capped the same way.
        let fmt = format!("$Format:{}$", "%B".repeat(100_000));
        let err = subst
            .apply(fmt.as_bytes())
            .expect_err("a repeated format is capped");
        assert_eq!(err.code(), tonic::Code::ResourceExhausted);
        // Expansions under the cap still work.
        let ok = subst.apply(b"x $Format:%s$ y").unwrap();
        assert_eq!(ok.len(), "x ".len() + 2048 + " y".len());
    }

    #[test]
    fn subst_scan_is_linear_on_openers_without_a_closing_dollar() {
        let subst = test_subst(b"m\n");
        // One 1 MiB line of openers, and lines whose only opener never closes.
        let one_line = b"$Format:".repeat((SUBST_MAX_FILE as usize) / 8);
        let lines = b"$Format:abc\n".repeat((SUBST_MAX_FILE as usize) / 12);
        let start = std::time::Instant::now();
        assert!(subst.apply(&one_line).unwrap().len() <= one_line.len());
        assert_eq!(subst.apply(&lines).unwrap(), lines);
        assert!(
            start.elapsed() < std::time::Duration::from_secs(5),
            "{:?}",
            start.elapsed()
        );
    }

    #[tokio::test]
    async fn archive_export_subst_is_capped_and_charged_to_the_text_budget() {
        let f = fx().await;
        let line = b"$Format:%B$\n";
        let big = line.repeat((SUBST_MAX_FILE as usize) / line.len());
        let c = mk(
            &f.entry,
            &[],
            &[
                w(".gitattributes", b"v.txt export-subst\n"),
                w("v.txt", &big),
            ],
            &"m".repeat(1024),
            1,
        )
        .await;
        let err = archive(&f, c, "tar", "", &[]).await.err().unwrap();
        assert_eq!(err.code(), tonic::Code::ResourceExhausted);
        // The expansion buffers are charged to the call's text budget.
        let f = fx().await;
        let c = mk(
            &f.entry,
            &[],
            &[
                w(".gitattributes", b"v.txt export-subst\n"),
                w("v.txt", b"$Format:%H$\n"),
            ],
            "m\n",
            1,
        )
        .await;
        assert_eq!(
            untar(archive(&f, c, "tar", "", &[]).await.unwrap()).len(),
            2
        );
        struct Never;
        #[async_trait]
        impl crate::TextBudget for Never {
            async fn acquire(&self, _: u64) -> Result<TextGuard, crate::TextBudgetExhausted> {
                Err(crate::TextBudgetExhausted)
            }
        }
        assert!(f.hooks.text_budget.set(Arc::new(Never)).is_ok());
        let err = archive(&f, c, "tar", "", &[]).await.err().unwrap();
        assert_eq!(err.code(), tonic::Code::ResourceExhausted);
    }

    #[tokio::test]
    async fn archive_of_a_large_file_streams_in_bounded_messages() {
        let f = fx().await;
        let mut state = 7_u64;
        let big: Vec<u8> = (0..6 * 1024 * 1024)
            .map(|_| {
                state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
                (state >> 40) as u8
            })
            .collect();
        let c = mk(&f.entry, &[], &[w("big", &big)], "m\n", 1).await;
        let parts = archive(&f, c, "tar", "", &[]).await.unwrap();
        assert!(parts.len() > 3);
        assert!(parts.iter().all(|p| p.data.len() <= DATA_MSG + 4096));
        let tar = untar(parts);
        assert!(tar[0].1 == big);
    }

    // ------------------------------------------------------------------------ history

    #[tokio::test]
    async fn last_commits_walks_first_parents_and_memoises() {
        let f = fx().await;
        let c1 = mk(
            &f.entry,
            &[],
            &[w("d/a", b"1"), w("d/b", b"1"), w("top", b"1")],
            "1\n",
            1,
        )
        .await;
        let c2 = mk(&f.entry, &[c1], &[w("d/a", b"2")], "2\n", 2).await;
        let c3 = mk(&f.entry, &[c2], &[w("top", b"3")], "3\n", 3).await;
        let c4 = mk(&f.entry, &[c3], &[w("d/c", b"4"), d("d/b")], "4\n", 4).await;
        let ask = |rev: ChangesetId, dir: &str, names: &[&str]| {
            let f = &f;
            let req = wire::LastCommitsRequest {
                repo: repo_ref(IdMode::Asc),
                rev: rev.to_string(),
                dir: dir.as_bytes().to_vec().into(),
                names: names.iter().map(|n| n.as_bytes().to_vec().into()).collect(),
            };
            async move {
                let r = f.reads.last_commits(&ctx(IdMode::Asc), req).await?;
                Ok::<_, Status>(
                    r.by_name
                        .into_iter()
                        .map(|(n, e)| (n, e.id.to_vec()))
                        .collect::<std::collections::BTreeMap<_, _>>(),
                )
            }
        };
        let id = |c: ChangesetId| c.as_bytes().to_vec();
        let got = ask(c4, "d", &["a", "b", "c", "zz"]).await.unwrap();
        assert_eq!(got.get("a"), Some(&id(c2)));
        assert_eq!(got.get("c"), Some(&id(c4)));
        assert!(!got.contains_key("b") && !got.contains_key("zz"));
        let got = ask(c4, "", &["d", "top"]).await.unwrap();
        assert_eq!(got.get("d"), Some(&id(c4)));
        assert_eq!(got.get("top"), Some(&id(c3)));
        let got = ask(c1, "", &["d", "top"]).await.unwrap();
        assert_eq!(got.get("d"), Some(&id(c1)), "the root commit introduced it");
        // The second ask for the same tip and directory is served from the memo.
        assert!(f.reads.lasts.get(&(c4, b"d".to_vec())).is_some());
        assert_eq!(ask(c4, "d", &["a"]).await.unwrap().get("a"), Some(&id(c2)));
        // A directory that is not there, and a name that is not a name.
        assert_eq!(
            ask(c4, "nope", &["a"]).await.unwrap_err().code(),
            tonic::Code::NotFound
        );
        assert_eq!(
            ask(c4, "d", &["a/b"]).await.unwrap_err().code(),
            tonic::Code::InvalidArgument
        );
    }

    #[tokio::test]
    async fn last_commits_prefers_the_mainline_index_on_a_land_branch() {
        let f = fx().await;
        let c1 = mk(&f.entry, &[], &[w("a", b"1"), w("b", b"1")], "1\n", 1).await;
        let c2 = mk(&f.entry, &[c1], &[w("a", b"2")], "2\n", 2).await;
        set_ref(&f, "refs/heads/master", c2).await;
        let mut tx = f.reg.pool().begin().await.unwrap();
        let step = |cs, paths: &[&str]| crate::mainline::MainlineStep {
            cs,
            paths: paths.iter().map(|p| RepoPath::new(*p).unwrap()).collect(),
        };
        let steps = [step(c1, &["a", "b"]), step(c2, &["a"])];
        crate::mainline::apply(&mut tx, f.entry.ns, "refs/heads/master", None, &steps)
            .await
            .unwrap();
        tx.commit().await.unwrap();
        let before = f.reads.mainline.rows_read();
        let r = f
            .reads
            .last_commits(
                &ctx(IdMode::Asc),
                wire::LastCommitsRequest {
                    repo: repo_ref(IdMode::Asc),
                    rev: "master".into(),
                    dir: Bytes::new(),
                    names: vec!["a".into(), "b".into()],
                },
            )
            .await
            .unwrap();
        assert_eq!(r.by_name["a"].id.as_ref(), c2.as_bytes());
        assert_eq!(r.by_name["b"].id.as_ref(), c1.as_bytes());
        assert!(
            f.reads.mainline.rows_read() > before,
            "the path index answered"
        );
        assert!(f.reads.lasts.is_empty(), "no walk was needed");
    }

    #[tokio::test]
    async fn log_pages_with_cursors_and_filters() {
        let f = fx().await;
        let mut parent = mk(&f.entry, &[], &[w("f", b"0")], "c0\n", 1).await;
        let mut chain = vec![parent];
        for i in 1..LOG_PAGE + 100 {
            let body = format!("x{i}");
            let ops: Vec<Op<'_>> = if i % 1000 == 0 {
                vec![w("g", body.as_bytes())]
            } else {
                vec![]
            };
            parent = mk(&f.entry, &[parent], &ops, &format!("c{i}\n"), 1 + i as i64).await;
            chain.push(parent);
        }
        let tip = *chain.last().unwrap();
        let ask = |req: wire::LogRequest| {
            let f = &f;
            async move {
                all(f
                    .hist
                    .log(
                        &ctx(IdMode::Asc),
                        wire::LogRequest {
                            repo: repo_ref(IdMode::Asc),
                            ..req
                        },
                    )
                    .await
                    .unwrap())
                .await
            }
        };
        let first = ask(wire::LogRequest {
            include: vec![tip.to_string()],
            ..Default::default()
        })
        .await;
        let ids: Vec<Vec<u8>> = first
            .iter()
            .flat_map(|r| r.entries.iter().map(|e| e.id.to_vec()))
            .collect();
        assert_eq!(ids.len(), chain.len());
        assert_eq!(ids[0], tip.as_bytes());
        let token_at = first.iter().position(|r| !r.next_page_token.is_empty());
        assert!(token_at.is_some_and(|i| i + 1 < first.len() || first.len() > 1));
        assert!(first.iter().all(|r| r.entries.len() <= LOG_MSG));
        // A fresh call from the first page's token yields the rest.
        let token = first
            .iter()
            .find(|r| !r.next_page_token.is_empty())
            .unwrap()
            .next_page_token
            .clone();
        let rest = ask(wire::LogRequest {
            page_token: token,
            ..Default::default()
        })
        .await;
        let rest_ids: Vec<Vec<u8>> = rest
            .iter()
            .flat_map(|r| r.entries.iter().map(|e| e.id.to_vec()))
            .collect();
        assert_eq!(rest_ids, ids[LOG_PAGE..]);
        // Limit, skip, path and message filters.
        let some = ask(wire::LogRequest {
            include: vec![tip.to_string()],
            limit: 3,
            skip: 2,
            ..Default::default()
        })
        .await;
        let got: Vec<Vec<u8>> = some
            .iter()
            .flat_map(|r| r.entries.iter().map(|e| e.id.to_vec()))
            .collect();
        assert_eq!(
            got,
            [
                chain[chain.len() - 3].as_bytes().to_vec(),
                chain[chain.len() - 4].as_bytes().to_vec(),
                chain[chain.len() - 5].as_bytes().to_vec()
            ]
        );
        let touching = ask(wire::LogRequest {
            include: vec![tip.to_string()],
            paths: vec!["g".into()],
            ..Default::default()
        })
        .await;
        assert_eq!(touching.iter().map(|r| r.entries.len()).sum::<usize>(), 4);
        let grep = ask(wire::LogRequest {
            include: vec![tip.to_string()],
            grep: "^c42\n".into(),
            ..Default::default()
        })
        .await;
        assert_eq!(grep.iter().map(|r| r.entries.len()).sum::<usize>(), 1);
        // Unsupported modes and bad tokens fail closed.
        for req in [
            wire::LogRequest {
                count_only: true,
                ..Default::default()
            },
            wire::LogRequest {
                page_token: "!!!".into(),
                ..Default::default()
            },
            wire::LogRequest {
                include: vec!["nope".into()],
                ..Default::default()
            },
        ] {
            let err = f
                .hist
                .log(
                    &ctx(IdMode::Asc),
                    wire::LogRequest {
                        repo: repo_ref(IdMode::Asc),
                        ..req
                    },
                )
                .await
                .err()
                .unwrap();
            assert!(matches!(
                err.code(),
                tonic::Code::Unimplemented | tonic::Code::InvalidArgument | tonic::Code::NotFound
            ));
        }
    }

    struct Tag;

    #[async_trait]
    impl LogEntryEnricher for Tag {
        async fn enrich(
            &self,
            _: &RepoEntry,
            ids: &[ChangesetId],
            entries: &mut [wire::LogEntry],
        ) -> Result<(), Status> {
            assert_eq!(ids.len(), entries.len());
            for (id, e) in ids.iter().zip(entries) {
                e.meta = Bytes::copy_from_slice(id.as_bytes());
                e.change = Bytes::from_static(&[7; 16]);
            }
            Ok(())
        }
    }

    #[tokio::test]
    async fn enrichers_see_every_log_entry() {
        let f = fx().await;
        f.hooks.add_enricher(Arc::new(Tag));
        let c1 = mk(&f.entry, &[], &[w("a", b"1\nx\n")], "1\n", 1).await;
        let c2 = mk(&f.entry, &[c1], &[w("a", b"1\ny\n")], "2\n", 2).await;
        index(&f.entry, c1).await;
        index(&f.entry, c2).await;
        let git = IdMode::Git;
        let log = all(f
            .hist
            .log(
                &ctx(git),
                wire::LogRequest {
                    repo: repo_ref(git),
                    include: vec![hex(IdCodec::new(git, &f.reg, &f.entry)
                        .git_of(c2)
                        .await
                        .unwrap()
                        .as_bytes())],
                    ..Default::default()
                },
            )
            .await
            .unwrap())
        .await;
        // In git mode the id is the git oid, and the enricher still receives the asc id.
        assert_eq!(log[0].entries[0].id.len(), 20);
        assert_eq!(log[0].entries[0].meta.as_ref(), c2.as_bytes());
        assert_eq!(log[0].entries[1].change.as_ref(), &[7; 16]);
        let b = all(f
            .reads
            .blame(
                &ctx(IdMode::Asc),
                wire::BlameRequest {
                    repo: repo_ref(IdMode::Asc),
                    rev: c2.to_string(),
                    path: "a".into(),
                    ..Default::default()
                },
            )
            .await
            .unwrap())
        .await;
        assert!(b[0].commits.iter().all(|c| c.change.as_ref() == [7; 16]));
        let rc = f
            .reads
            .read_changeset(
                &ctx(IdMode::Asc),
                wire::ReadChangesetRequest {
                    repo: repo_ref(IdMode::Asc),
                    rev: c2.to_string(),
                },
            )
            .await
            .unwrap();
        assert_eq!(rc.header.unwrap().meta.as_ref(), c2.as_bytes());
    }

    #[tokio::test]
    async fn diff_formats_limits_and_merge_base() {
        let f = fx().await;
        let base = mk(
            &f.entry,
            &[],
            &[
                w("a", b"1\n2\n3\n"),
                w("old", b"same content here\nmore\n"),
                w("c", b"c\n"),
            ],
            "b\n",
            1,
        )
        .await;
        let left = mk(&f.entry, &[base], &[w("a", b"1\nTWO\n3\n")], "l\n", 2).await;
        let right = mk(
            &f.entry,
            &[base],
            &[
                d("old"),
                w("new", b"same content here\nmore\n"),
                w("c", b"c2\n"),
            ],
            "r\n",
            3,
        )
        .await;
        let diff = |req: wire::DiffRequest| {
            let f = &f;
            async move {
                let s = f
                    .hist
                    .diff(
                        &ctx(IdMode::Asc),
                        wire::DiffRequest {
                            repo: repo_ref(IdMode::Asc),
                            ..req
                        },
                    )
                    .await?;
                s.try_collect::<Vec<_>>().await
            }
        };
        let text = |rs: &[wire::DiffResponse]| {
            String::from_utf8(rs.iter().flat_map(|r| r.text.to_vec()).collect()).unwrap()
        };
        // Three-dot semantics: the merge base is the left side.
        let rs = diff(wire::DiffRequest {
            base: left.to_string(),
            head: right.to_string(),
            merge_base: true,
            format: wire::DiffFormat::Status as i32,
            ..Default::default()
        })
        .await
        .unwrap();
        assert_eq!(text(&rs), "M\tc\nA\tnew\nD\told\n");
        let rs = diff(wire::DiffRequest {
            base: left.to_string(),
            head: right.to_string(),
            format: wire::DiffFormat::Status as i32,
            ..Default::default()
        })
        .await
        .unwrap();
        assert_eq!(text(&rs), "M\ta\nM\tc\nA\tnew\nD\told\n");
        // Renames.
        let rs = diff(wire::DiffRequest {
            base: base.to_string(),
            head: right.to_string(),
            format: wire::DiffFormat::Status as i32,
            renames: true,
            ..Default::default()
        })
        .await
        .unwrap();
        assert_eq!(text(&rs), "M\tc\nR100\told\tnew\n");
        assert_eq!(
            rs[0]
                .changes
                .iter()
                .find(|c| c.status == "R")
                .unwrap()
                .similarity,
            100
        );
        // Stat, numstat, pathspec, truncation.
        let rs = diff(wire::DiffRequest {
            base: base.to_string(),
            head: left.to_string(),
            format: wire::DiffFormat::Stat as i32,
            ..Default::default()
        })
        .await
        .unwrap();
        assert!(
            text(&rs).contains(" a | 2 +-") && text(&rs).contains("1 file changed"),
            "{}",
            text(&rs)
        );
        let rs = diff(wire::DiffRequest {
            base: base.to_string(),
            head: right.to_string(),
            format: wire::DiffFormat::Numstat as i32,
            paths: vec!["c".into()],
            ..Default::default()
        })
        .await
        .unwrap();
        assert_eq!(text(&rs), "1\t1\tc\n");
        let rs = diff(wire::DiffRequest {
            base: base.to_string(),
            head: right.to_string(),
            max_files: 1,
            format: wire::DiffFormat::Names as i32,
            ..Default::default()
        })
        .await
        .unwrap();
        assert!(rs.last().unwrap().truncated);
        assert_eq!(rs.iter().map(|r| r.changes.len()).sum::<usize>(), 1);
        // Bad input.
        for req in [
            wire::DiffRequest {
                base: "nope".into(),
                head: left.to_string(),
                ..Default::default()
            },
            wire::DiffRequest {
                base: base.to_string(),
                head: left.to_string(),
                format: 99,
                ..Default::default()
            },
            wire::DiffRequest {
                base: base.to_string(),
                head: left.to_string(),
                paths: vec![":(bad".into()],
                ..Default::default()
            },
        ] {
            assert!(diff(req).await.is_err());
        }
        // A starved budget sheds a patch.
        struct Never;
        #[async_trait]
        impl crate::TextBudget for Never {
            async fn acquire(&self, _: u64) -> Result<TextGuard, crate::TextBudgetExhausted> {
                Err(crate::TextBudgetExhausted)
            }
        }
        f.hooks.text_budget.set(Arc::new(Never)).ok().unwrap();
        let err = diff(wire::DiffRequest {
            base: base.to_string(),
            head: left.to_string(),
            format: wire::DiffFormat::Patch as i32,
            ..Default::default()
        })
        .await
        .unwrap_err();
        assert_eq!(err.code(), tonic::Code::ResourceExhausted);
    }

    #[tokio::test]
    async fn merge_base_and_is_ancestor_in_asc_mode() {
        let f = fx().await;
        let base = mk(&f.entry, &[], &[w("a", b"1")], "b\n", 1).await;
        let l = mk(&f.entry, &[base], &[w("a", b"2")], "l\n", 2).await;
        let r = mk(&f.entry, &[base], &[w("a", b"3")], "r\n", 3).await;
        let mb = f
            .hist
            .merge_base(
                &ctx(IdMode::Asc),
                wire::MergeBaseRequest {
                    repo: repo_ref(IdMode::Asc),
                    a: l.as_bytes().to_vec().into(),
                    b: vec![r.as_bytes().to_vec().into()],
                    all: false,
                },
            )
            .await
            .unwrap();
        assert_eq!(mb.bases[0].as_ref(), base.as_bytes());
        let no = f
            .hist
            .is_ancestor(
                &ctx(IdMode::Asc),
                wire::IsAncestorRequest {
                    repo: repo_ref(IdMode::Asc),
                    ancestor: l.as_bytes().to_vec().into(),
                    descendant: r.as_bytes().to_vec().into(),
                },
            )
            .await
            .unwrap();
        assert!(!no.result);
        // A git-length id is refused in asc mode.
        let err = f
            .hist
            .is_ancestor(
                &ctx(IdMode::Asc),
                wire::IsAncestorRequest {
                    repo: repo_ref(IdMode::Asc),
                    ancestor: vec![1; 20].into(),
                    descendant: r.as_bytes().to_vec().into(),
                },
            )
            .await
            .unwrap_err();
        assert_eq!(err.code(), tonic::Code::InvalidArgument);
    }

    #[tokio::test]
    async fn registration_fills_the_reads_and_history_groups_once() {
        let f = fx().await;
        let ext = Extensions::new();
        let hooks = register(&ext, &f.reg);
        hooks.add_enricher(Arc::new(Tag));
        assert!(
            ext.handlers
                .set_reads(Arc::new(ServerReads::new(Arc::new(Core {
                    registry: f.reg.clone(),
                    hooks
                }))))
                .is_err()
        );
    }
}
