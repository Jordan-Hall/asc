//! Server-side mutations: `Commit`, `Ingest` and `Apply` (PLAN U48a, SPEC §8.7).
//!
//! * `Commit` builds a changeset from `FileOp`s on `parent`. With an empty `update_ref` it runs
//!   only `UpdateRefs` pipeline step 4 ([`Ingestor::ingest`]: closure check, §2.6 and §8.4
//!   validation, path policy, eager derivation); no ref, hook, `ref_log` or outbox row is
//!   involved. With a `update_ref` the new changeset goes through the full `UpdateRefs` pipeline
//!   ([`ServerRefs`]) as a fast-forward compare-and-set.
//! * `Ingest` runs step 4 for changesets that `PutObjects` already uploaded.
//! * `Apply` replays a cherry-pick or revert (engine merge) or a `git apply` patch (U27b's
//!   parser) onto `onto`. A conflict is an answer, not an error: the response lists the paths and
//!   no changeset is made.
//!
//! The service has already checked write access and the `x-asc-op-id` header. Every path comes
//! through `checkout_safe`, every whole-file buffer a merge or a patch needs goes through the
//! [`TextBudget`](crate::TextBudget) first, and every size or count is bounded by a constant
//! below. Errors never carry request bytes beyond paths.

use std::collections::{BTreeMap, HashSet};
use std::sync::Arc;

use asc_core::change::{ChangeId, ChangeMeta, RewriteOp};
use asc_core::codec::decode_envelope;
use asc_core::content::{Body, ChunkAssembler, ingest as ingest_content};
use asc_core::git::{BlobOidHasher, blob_oid};
use asc_core::id::{ChangesetId, FileId, GitOid, RefTarget, TreeId};
use asc_core::object::{Change, ChunkList, Entry, Kind};
use asc_core::path::{MAX_TREE_DEPTH, RepoPath, checkout_safe};
use asc_core::sig::Signature;
use asc_engine::commit::{self, NewChangeset, PathPolicy};
use asc_engine::diff::apply as patch;
use asc_engine::graph;
use asc_engine::history::HistoryApi;
use asc_engine::merge::{self, Conflict, ConflictKind, MergeOptions};
use asc_engine::tree::{self, MAX_WALK_PATH_BYTES};
use asc_rpc::errors::RpcError;
use asc_rpc::server::{
    CallCtx, ImportPushHandler, MAX_LIST, MAX_STRING, MutationsHandler, RefsHandler, wire,
};
use asc_rpc::{IdMode, WireId};
use asc_store::codec::encode_stored;
use asc_store::key::ObjKey;
use asc_store::objects::{self, Objects};
use async_trait::async_trait;
use bytes::Bytes;
use tonic::Status;

use crate::changes::index::ChangeIndex;
use crate::idmode::{Eng, IdCodec};
use crate::ingest::{IngestObserver, Ingestor, ObserverError};
use crate::read::ReadHooks;
use crate::registry::{Extensions, Registry, RegistryError, RepoEntry, RepoMode};
use crate::update::{PipelineHooks, ServerRefs, info, ingest_status, internal, rpc};
use crate::{TextBudgetExhausted, TextGuard};

/// Most `FileOp`s in one `Commit`.
pub const MAX_OPS: usize = 10_000;
/// Most bytes of inline content in one `put_content` op.
pub const MAX_CONTENT_BYTES: usize = 16 << 20;
/// Longest commit message the server accepts.
pub const MAX_MESSAGE_BYTES: usize = 1 << 20;
/// Largest patch an `Apply` accepts. A message is at most 17 MiB on the wire.
pub const MAX_PATCH_BYTES: usize = 16 << 20;
/// Most bytes of file content one patch `Apply` may produce.
pub const MAX_PATCH_STAGED_BYTES: u64 = 256 << 20;
/// Most changesets one `Ingest` may name.
pub const MAX_INGEST_IDS: usize = MAX_LIST;
/// Whole-file bytes a cherry-pick or revert reserves from the text budget before it merges. The
/// engine reads the changed files of both sides; the sizes are not known until it does.
pub const MERGE_TEXT_ESTIMATE: u64 = 64 << 20;
/// Most conflicts one `Apply` reports.
const MAX_CONFLICTS: usize = 10_000;
/// Longest subject line a revert message quotes.
const MAX_SUBJECT: usize = 256;

// ------------------------------------------------------------------------------ statuses

fn invalid(m: impl Into<String>) -> Status {
    Status::invalid_argument(m.into())
}

fn exhausted(_: TextBudgetExhausted) -> Status {
    Status::resource_exhausted("text memory budget exhausted; retry shortly")
}

fn registry_status(e: RegistryError) -> Status {
    match e {
        RegistryError::NotFound(_) => Status::not_found("no such repository"),
        e => internal("registry", &e),
    }
}

fn store_status(e: objects::Error) -> Status {
    match e {
        objects::Error::Corrupt(_) | objects::Error::Codec(..) => {
            tracing::error!(error = %e, "stored object is corrupt");
            Status::data_loss("stored object is corrupt")
        }
        e => internal("object store", &e),
    }
}

fn tree_status(e: tree::Error) -> Status {
    match e {
        tree::Error::Store(e) => store_status(e),
        tree::Error::MissingTree(_) => Status::data_loss("a tree is missing"),
        tree::Error::Codec(_) => Status::data_loss("stored object is corrupt"),
        // The rest are the caller's changes not fitting the tree.
        e @ (tree::Error::DuplicatePath(_)
        | tree::Error::PathThroughFile(_)
        | tree::Error::WriteOverDir(_)
        | tree::Error::DeleteDir(_)
        | tree::Error::BadWriteKind(_)
        | tree::Error::TooDeep
        | tree::Error::PathTooLong(_)
        | tree::Error::Path(_)) => invalid(e.to_string()),
        e => internal("tree", &e),
    }
}

fn commit_status(e: commit::Error) -> Status {
    match e {
        commit::Error::Store(e) => store_status(e),
        commit::Error::Tree(e) => tree_status(e),
        commit::Error::MissingChangeset(id) | commit::Error::MissingParent(id) => {
            Status::not_found(format!("changeset {id} is missing"))
        }
        commit::Error::UnsafePath(p)
        | commit::Error::WindowsInvalid(p)
        | commit::Error::CaseCollision(p) => rpc(RpcError::PathPolicy(info(vec![p.to_string()]))),
        e @ (commit::Error::MissingFile { .. } | commit::Error::FileMismatch(_)) => {
            invalid(e.to_string())
        }
        e @ (commit::Error::MissingCopySource(_)
        | commit::Error::CopyNotWrite(_)
        | commit::Error::DuplicateCopy(_)
        | commit::Error::DuplicateParent(_)
        | commit::Error::BadExtra(_)
        | commit::Error::DuplicateExtra(_)
        | commit::Error::Path(_)
        | commit::Error::Object(_)) => invalid(e.to_string()),
        e => internal("changeset", &e),
    }
}

fn graph_status(e: graph::Error) -> Status {
    match e {
        graph::Error::Missing(id) => Status::not_found(format!("changeset {id} is missing")),
        graph::Error::Query(m) => invalid(m),
        graph::Error::Remote(m) => invalid(m),
        graph::Error::Unsupported(m) => Status::unimplemented(m),
        graph::Error::Engine(e) => commit_status(e),
        e => internal("history", &e),
    }
}

fn merge_status(e: merge::Error) -> Status {
    match e {
        merge::Error::Store(e) => store_status(e),
        merge::Error::Commit(e) => commit_status(e),
        merge::Error::Graph(e) => graph_status(e),
        merge::Error::Tree(e) => tree_status(e),
        e @ (merge::Error::NeedsMainline(_)
        | merge::Error::BadMainline { .. }
        | merge::Error::NotLinear(_)) => Status::failed_precondition(e.to_string()),
        e => internal("merge", &e),
    }
}

/// Runs an engine future to completion on a blocking thread. Some engine futures
/// (`Engine::apply`, `create_changeset`, the merges) are not provably `Send`: rustc cannot
/// generalise a closure inside them. The future is built and finished on that thread.
async fn on_blocking_thread<F, Fut, T>(make: F) -> Result<T, Status>
where
    F: FnOnce() -> Fut + Send + 'static,
    Fut: std::future::Future<Output = T>,
    T: Send + 'static,
{
    let handle = tokio::runtime::Handle::current();
    tokio::task::spawn_blocking(move || handle.block_on(make()))
        .await
        .map_err(|e| internal("engine task", &e))
}

impl Opened {
    /// `Engine::apply` of `changes` onto `base`.
    async fn apply_changes(
        &self,
        base: TreeId,
        changes: Vec<(RepoPath, Change)>,
    ) -> Result<TreeId, Status> {
        let engine = self.engine.clone();
        on_blocking_thread(move || async move { engine.apply(base, &changes).await })
            .await?
            .map_err(tree_status)
    }

    /// Builds, validates and stores a changeset.
    async fn create(&self, new: NewChangeset) -> Result<ChangesetId, Status> {
        let engine = self.engine.clone();
        let policy = PathPolicy::Native {
            windows_safe: self.windows_safe,
        };
        on_blocking_thread(move || async move { engine.create_changeset(new, policy).await })
            .await?
            .map_err(commit_status)
    }
}

// ------------------------------------------------------------------------------ the handler

/// The `mutations` and `import_push` handler groups' `Commit`, `Apply` and `Ingest`.
pub struct ServerMutations {
    registry: Arc<Registry>,
    pipeline: Arc<PipelineHooks>,
    hooks: Arc<ReadHooks>,
    refs: ServerRefs,
}

/// One repository opened for one call.
struct Opened {
    entry: Arc<RepoEntry>,
    engine: Arc<Eng>,
    ids: IdCodec,
    windows_safe: bool,
}

impl ServerMutations {
    pub fn new(
        registry: Arc<Registry>,
        pipeline: Arc<PipelineHooks>,
        hooks: Arc<ReadHooks>,
    ) -> Self {
        let refs = ServerRefs::new(registry.clone(), pipeline.clone());
        Self {
            registry,
            pipeline,
            hooks,
            refs,
        }
    }

    /// The repository a call names. Only `id_mode = ASC` is served (as in `UpdateRefs`), and a
    /// mirror takes no writes (SPEC §8.3 step 3).
    async fn open(&self, ctx: &CallCtx) -> Result<Opened, Status> {
        if ctx.id_mode != IdMode::Asc {
            return Err(Status::unimplemented(
                "git id mode is not served by this mutation",
            ));
        }
        let entry = self
            .registry
            .open(&ctx.repo)
            .await
            .map_err(registry_status)?;
        if entry.mode == RepoMode::Mirror {
            return Err(rpc(RpcError::MirrorMode(info(vec![ctx.repo.to_string()]))));
        }
        let windows_safe = self.refs.windows_safe(&entry).await?;
        Ok(Opened {
            ids: IdCodec::new(ctx.id_mode, &self.registry, &entry).for_viewer(&ctx.principal),
            engine: entry.backend.engine().clone(),
            entry,
            windows_safe,
        })
    }

    /// The changeset a revision names; an empty spec names the default branch.
    async fn rev(&self, o: &Opened, spec: &str) -> Result<ChangesetId, Status> {
        if spec.len() > MAX_STRING {
            return Err(invalid("revision is too long"));
        }
        let spec = if spec.is_empty() { "HEAD" } else { spec };
        o.ids
            .resolve(spec, &o.entry.default_branch, graph_status)
            .await?
            .ok_or_else(|| Status::not_found(format!("unknown revision {spec:?}")))
    }

    async fn budget(&self, bytes: u64) -> Result<TextGuard, Status> {
        self.hooks
            .text_budget
            .acquire(bytes)
            .await
            .map_err(exhausted)
    }

    /// Pipeline step 4 for `ids`, with the stack-metadata observer in the same transaction.
    /// The caller holds the shared GC fence.
    async fn ingest_ids(&self, o: &Opened, ids: &[ChangesetId]) -> Result<(), Status> {
        self.ingest_with(o, ids, self.pipeline.observer()).await
    }

    /// Step 4 for a changeset `Commit` just created: the stack-metadata observer runs, and the
    /// changeset gets a draft `ChangeMeta` (SPEC §8.7, M18) in the same transaction.
    async fn ingest_commit(&self, o: &Opened, id: ChangesetId) -> Result<(), Status> {
        let observer = NewChangeMeta {
            inner: self.pipeline.observer(),
            index: ChangeIndex::new(self.registry.clone()),
            cs: id,
        };
        self.ingest_with(o, &[id], &observer).await
    }

    /// The `Ingest` RPC (SPEC §21.3.7): step 4 with the caller recorded as the ingester, then the
    /// caller pays for the closure of `ids` (`cloud_usage` through `UserQuota::claim`). A
    /// caller with no bytes left is refused before anything is recorded. A caller with no
    /// verified owner id is not charged and records no ingester; whoever later names the
    /// changesets from a user ref pays for them.
    async fn ingest_charged(
        &self,
        ctx: &CallCtx,
        o: &Opened,
        ids: &[ChangesetId],
    ) -> Result<(), Status> {
        let owner = crate::refpolicy::accounting_owner_of(&ctx.principal);
        let quota = self.pipeline.user_quota();
        let targets: Vec<RefTarget> = ids.iter().map(|&c| RefTarget::Changeset(c)).collect();
        let mut ingestor = Ingestor::new(
            self.registry.pool(),
            self.registry.refs(),
            &o.entry,
            o.windows_safe,
        )
        .by(owner);
        if let Some(owner) = owner {
            let left = quota
                .remaining(self.registry.pool(), o.entry.ns, owner)
                .await?;
            ingestor = ingestor.with_budget(left);
        }
        ingestor
            .ingest(&targets, self.pipeline.observer())
            .await
            .map_err(ingest_status)?;
        if let Some(owner) = owner {
            let mut tx = self
                .registry
                .refs()
                .begin()
                .await
                .map_err(|e| internal("refs", &e))?;
            self.registry.guard(&mut tx, &o.entry).await?;
            quota.claim(&mut tx, o.entry.ns, owner, ids).await?;
            tx.commit().await.map_err(|e| internal("commit", &e))?;
        }
        Ok(())
    }

    async fn ingest_with(
        &self,
        o: &Opened,
        ids: &[ChangesetId],
        observer: &dyn IngestObserver,
    ) -> Result<(), Status> {
        let targets: Vec<RefTarget> = ids.iter().map(|&c| RefTarget::Changeset(c)).collect();
        Ingestor::new(
            self.registry.pool(),
            self.registry.refs(),
            &o.entry,
            o.windows_safe,
        )
        .ingest(&targets, observer)
        .await
        .map(drop)
        .map_err(ingest_status)
    }

    /// Takes the shared GC fence (SPEC §3.6) for work that writes objects and then ingests them.
    async fn fence(
        &self,
    ) -> Result<crate::update::GcHold<tokio::sync::OwnedRwLockReadGuard<()>>, Status> {
        self.pipeline.gc().shared().await.map_err(|e| {
            tracing::warn!(error = %e, "gc fence unavailable");
            Status::unavailable("garbage-collection fence unavailable; retry")
        })
    }

    /// The changeset a previous attempt of this `op_id` moved `update_ref` to, if any.
    async fn replay(
        &self,
        o: &Opened,
        ctx: &CallCtx,
        update_ref: &str,
    ) -> Result<Option<ChangesetId>, Status> {
        let Some(op) = ctx.op_id else { return Ok(None) };
        let Some(done) = self.refs.stored(&o.entry, op).await? else {
            return Ok(None);
        };
        let moved = match done.results.as_slice() {
            [r] if r.name == update_ref => match r.new.as_ref().map(RefTarget::try_from) {
                Some(Ok(RefTarget::Changeset(c))) => Some(c),
                _ => None,
            },
            _ => None,
        };
        moved
            .map(Some)
            .ok_or_else(|| invalid("op id was used for another operation"))
    }

    /// Moves `update_ref` to `id` through the full `UpdateRefs` pipeline, as a fast-forward
    /// compare-and-set against `expect`.
    async fn move_ref(
        &self,
        ctx: &CallCtx,
        update_ref: &str,
        expect: wire::ref_update::Expect,
        id: ChangesetId,
    ) -> Result<(), Status> {
        let req = wire::UpdateRefsRequest {
            repo: Some(wire::RepoRef {
                name: ctx.repo.to_string(),
                id_mode: IdMode::Asc as i32,
            }),
            updates: vec![wire::RefUpdate {
                name: update_ref.to_owned(),
                expect: Some(expect),
                new: Some(wire::RefTarget::from(&RefTarget::Changeset(id))),
                delete: false,
            }],
            mode: wire::UpdateMode::FastForward as i32,
            hooks: wire::HookMode::Run as i32,
            ..wire::UpdateRefsRequest::default()
        };
        let done = self.refs.update_refs(ctx, req).await?;
        match done.results.as_slice() {
            [r] if r.new.as_ref().map(RefTarget::try_from)
                == Some(Ok(RefTarget::Changeset(id))) =>
            {
                Ok(())
            }
            _ => Err(internal("update_refs", &"unexpected result")),
        }
    }

    /// Makes `id` visible: step 4 only, or the whole pipeline when `update_ref` is set.
    async fn publish(
        &self,
        o: &Opened,
        ctx: &CallCtx,
        id: ChangesetId,
        update_ref: &str,
        expect: wire::ref_update::Expect,
    ) -> Result<(), Status> {
        if update_ref.is_empty() {
            self.ingest_ids(o, &[id]).await
        } else {
            self.move_ref(ctx, update_ref, expect, id).await
        }
    }

    async fn commit_response(
        &self,
        o: &Opened,
        id: ChangesetId,
    ) -> Result<wire::CommitResponse, Status> {
        let git = o.ids.git_of(id).await?;
        Ok(wire::CommitResponse {
            id: Bytes::copy_from_slice(id.as_bytes()),
            git: Bytes::copy_from_slice(git.as_bytes()),
        })
    }
}

// ------------------------------------------------------------------------------ request parts

/// `update_ref`, checked for length; empty means no ref.
fn check_update_ref(update_ref: &str) -> Result<(), Status> {
    if update_ref.len() > MAX_STRING {
        return Err(invalid("update_ref is too long"));
    }
    Ok(())
}

fn signature(s: Option<&wire::Signature>) -> Result<Option<Signature>, Status> {
    s.map(|s| Signature::try_from(s).map_err(|e| invalid(e.to_string())))
        .transpose()
}

/// `(author, committer)`: the committer falls back to the author and the author to
/// `default_author`, then to the committer. At least one signature must be given.
fn signatures(
    author: Option<&wire::Signature>,
    committer: Option<&wire::Signature>,
    default_author: Option<Signature>,
) -> Result<(Signature, Signature), Status> {
    let author = signature(author)?;
    let committer = signature(committer)?;
    let committer = committer
        .or_else(|| author.clone())
        .ok_or_else(|| invalid("a committer is required"))?;
    let author = author
        .or(default_author)
        .unwrap_or_else(|| committer.clone());
    Ok((author, committer))
}

fn check_message(m: &[u8]) -> Result<(), Status> {
    if m.len() > MAX_MESSAGE_BYTES {
        return Err(invalid("message is too long"));
    }
    Ok(())
}

/// A path from the request: a valid repository path that passes `checkout_safe`.
fn op_path(bytes: &[u8]) -> Result<RepoPath, Status> {
    if bytes.len() > MAX_WALK_PATH_BYTES {
        return Err(invalid("path is too long"));
    }
    let path = RepoPath::new(bytes).map_err(|e| invalid(e.to_string()))?;
    if !checkout_safe(&path) {
        return Err(rpc(RpcError::PathPolicy(info(vec![path.to_string()]))));
    }
    Ok(path)
}

/// The tree entry kind of a requested file mode; `0` is the default.
fn kind_of_mode(mode: u32) -> Result<Option<Kind>, Status> {
    match mode {
        0 => Ok(None),
        0o100644 => Ok(Some(Kind::File)),
        0o100755 => Ok(Some(Kind::Exec)),
        0o120000 => Ok(Some(Kind::Symlink)),
        _ => Err(invalid("mode must be 0, 0100644, 0100755 or 0120000")),
    }
}

/// A new file's identity and where its bytes are stored.
pub(crate) struct Stored {
    pub(crate) file: FileId,
    pub(crate) size: u64,
    pub(crate) git: GitOid,
}

/// Stores `data` in the objects of `entry` as a file (chunked above the inline threshold) and returns its identity.
pub(crate) async fn store_file(entry: &RepoEntry, data: &[u8]) -> Result<Stored, Status> {
    let got = ingest_content(std::io::Cursor::new(data)).map_err(|e| invalid(e.to_string()))?;
    match got.body {
        Body::Inline(b) => {
            entry
                .objects
                .put(&ObjKey::File(got.file), encode_stored(&b))
                .await
                .map_err(store_status)?;
        }
        Body::Chunked { list, chunks } => {
            for (id, chunk) in chunks {
                entry
                    .objects
                    .put(&ObjKey::Chunk(id), encode_stored(&chunk))
                    .await
                    .map_err(store_status)?;
            }
            entry
                .objects
                .put(
                    &ObjKey::ChunkList(got.file),
                    encode_stored(&asc_core::codec::Enveloped::to_envelope(&list)),
                )
                .await
                .map_err(store_status)?;
        }
    }
    Ok(Stored {
        file: got.file,
        size: got.size,
        git: got.git,
    })
}

/// The leaf change that writes a stored file with the given kind and no copy source.
pub(crate) fn write_stored(kind: Kind, s: &Stored) -> Result<Change, Status> {
    write(kind, s, None)
}

impl ServerMutations {
    /// Stores `data` as a file (chunked above the inline threshold) and returns its identity.
    async fn store_content(&self, o: &Opened, data: &[u8]) -> Result<Stored, Status> {
        store_file(&o.entry, data).await
    }

    /// Size and blob oid of a stored file, computed from its bytes (a client-named `FileId` is
    /// not trusted to come with either). A chunked file is streamed once and checked against its
    /// id by the assembler.
    async fn identity(&self, o: &Opened, file: FileId) -> Result<Stored, Status> {
        let missing = |k: ObjKey| rpc(RpcError::MissingObjects(info(vec![k.path()])));
        let corrupt = |k: ObjKey| rpc(RpcError::InvalidObject(info(vec![k.path()])));
        if let Some(data) = o
            .entry
            .objects
            .get(&ObjKey::File(file))
            .await
            .map_err(store_status)?
        {
            return Ok(Stored {
                file,
                size: data.len() as u64,
                git: blob_oid(&data),
            });
        }
        let list_key = ObjKey::ChunkList(file);
        let list_bytes = o
            .entry
            .objects
            .get(&list_key)
            .await
            .map_err(store_status)?
            .ok_or_else(|| missing(ObjKey::File(file)))?;
        let list: ChunkList = decode_envelope(&list_bytes).map_err(|_| corrupt(list_key))?;
        let size = list.size;
        let ids: Vec<_> = list.chunks.iter().map(|&(id, _)| id).collect();
        let mut assembler = ChunkAssembler::new(list, file).map_err(|_| corrupt(list_key))?;
        let mut hasher = BlobOidHasher::new(size);
        for id in ids {
            let chunk = o
                .entry
                .objects
                .get(&ObjKey::Chunk(id))
                .await
                .map_err(store_status)?
                .ok_or_else(|| missing(ObjKey::Chunk(id)))?;
            assembler.push(&chunk).map_err(|_| corrupt(list_key))?;
            hasher.update(&chunk);
        }
        assembler.finish().map_err(|_| corrupt(list_key))?;
        let git = hasher.finalize().map_err(|_| corrupt(list_key))?;
        Ok(Stored { file, size, git })
    }
}

// ------------------------------------------------------------------------------ Commit

/// What `Commit` turned its ops into.
struct Planned {
    changes: Vec<(RepoPath, Change)>,
    copies: Vec<(RepoPath, RepoPath)>,
}

impl ServerMutations {
    /// Turns `ops` into leaf changes over the tree `base`.
    async fn plan_ops(
        &self,
        o: &Opened,
        base: TreeId,
        ops: &[wire::FileOp],
    ) -> Result<Planned, Status> {
        use wire::file_op::Op;
        let mut touched: HashSet<RepoPath> = HashSet::with_capacity(ops.len());
        let mut out = Planned {
            changes: Vec::with_capacity(ops.len()),
            copies: Vec::new(),
        };
        let mut claim = |p: &RepoPath| -> Result<(), Status> {
            if touched.insert(p.clone()) {
                Ok(())
            } else {
                Err(invalid(format!("{p} is named by two operations")))
            }
        };
        for op in ops {
            let path = op_path(&op.path)?;
            claim(&path)?;
            let kind = kind_of_mode(op.mode)?;
            match op
                .op
                .as_ref()
                .ok_or_else(|| invalid("an operation needs an action"))?
            {
                Op::PutContent(data) => {
                    if data.len() > MAX_CONTENT_BYTES {
                        return Err(invalid("inline content is too large"));
                    }
                    let kind = kind.unwrap_or(Kind::File);
                    if kind == Kind::Symlink
                        && !patch::symlink_target_allowed(path.as_bytes(), data)
                    {
                        return Err(rpc(RpcError::PathPolicy(info(vec![path.to_string()]))));
                    }
                    let s = self.store_content(o, data).await?;
                    out.changes.push((path, write(kind, &s, None)?));
                }
                Op::PutFileId(id) => {
                    let file = FileId::from_slice(id).map_err(|e| invalid(e.to_string()))?;
                    let s = self.identity(o, file).await?;
                    let kind = kind.unwrap_or(Kind::File);
                    if kind == Kind::Symlink {
                        return Err(invalid("a symlink needs inline content"));
                    }
                    out.changes.push((path, write(kind, &s, None)?));
                }
                Op::Delete(true) => {
                    if kind.is_some() {
                        return Err(invalid("a delete takes no mode"));
                    }
                    out.changes.push((path, Change::Delete));
                }
                Op::Delete(false) => return Err(invalid("an operation needs an action")),
                Op::MoveFrom(from) => {
                    let from = op_path(from)?;
                    claim(&from)?;
                    let entry = tree_entry(o, base, &from).await?;
                    let entry_kind = match entry.kind {
                        k @ (Kind::File | Kind::Exec) => k,
                        _ => return Err(invalid(format!("{from} is not a regular file"))),
                    };
                    out.changes.push((
                        path.clone(),
                        Change::write(
                            kind.unwrap_or(entry_kind),
                            FileId::from_bytes(entry.id),
                            entry.size,
                            entry.git,
                            None,
                        )
                        .map_err(|e| invalid(e.to_string()))?,
                    ));
                    out.changes.push((from.clone(), Change::Delete));
                    out.copies.push((path, from));
                }
            }
        }
        out.changes.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(out)
    }

    async fn commit_inner(
        &self,
        ctx: &CallCtx,
        req: wire::CommitRequest,
    ) -> Result<wire::CommitResponse, Status> {
        let o = self.open(ctx).await?;
        check_update_ref(&req.update_ref)?;
        check_message(&req.message)?;
        if req.sign {
            // Signing needs a server key (SPEC §8.5 step 5: there is no server signature).
            return Err(Status::unimplemented("this server does not sign commits"));
        }
        if req.ops.len() > MAX_OPS {
            return Err(invalid("too many operations"));
        }
        let (author, committer) = signatures(req.author.as_ref(), req.committer.as_ref(), None)?;
        let expect = match req.expect.as_ref() {
            Some(t) => wire::ref_update::Expect::Value(wire::RefTarget::clone(t)),
            None => wire::ref_update::Expect::Absent(true),
        };
        if !req.update_ref.is_empty() {
            // Validate before any write; `UpdateRefs` checks the name again.
            asc_core::path::RefName::new(req.update_ref.as_bytes())
                .map_err(|e| invalid(e.to_string()))?;
            if let Some(id) = self.replay(&o, ctx, &req.update_ref).await? {
                return self.commit_response(&o, id).await;
            }
        }
        // Without a ref the fence spans the object writes and the ingest. With one,
        // `UpdateRefs` takes it itself (the lock is not re-entrant under a waiting sweep).
        let _gc = if req.update_ref.is_empty() {
            Some(self.fence().await?)
        } else {
            None
        };
        let parents = if req.parent.is_empty() {
            Vec::new()
        } else {
            vec![self.rev(&o, &req.parent).await?]
        };
        let base = match parents.first() {
            Some(&p) => o.engine.get_changeset(p).await.map_err(commit_status)?.root,
            None => Eng::empty_tree_id(),
        };
        let planned = self.plan_ops(&o, base, &req.ops).await?;
        let root = o.apply_changes(base, planned.changes).await?;
        let id = o
            .create(NewChangeset {
                parents,
                author,
                committer,
                message: req.message.to_vec(),
                extra: Vec::new(),
                root,
                copies: planned.copies,
            })
            .await?;
        if req.update_ref.is_empty() {
            // The fence is held; this is the whole of the work.
            self.ingest_commit(&o, id).await?;
        } else {
            // The changeset is ingested (with its meta) before the ref moves, so the
            // `UpdateRefs` ingest finds it known. The fence is dropped before `UpdateRefs`
            // takes its own.
            let _gc = self.fence().await?;
            self.ingest_commit(&o, id).await?;
        }
        self.publish(&o, ctx, id, &req.update_ref, expect).await?;
        self.commit_response(&o, id).await
    }
}

/// Wraps the pipeline observer: after it ran, `cs` (when this ingest added it) gets a draft
/// `ChangeMeta` (`op = New`, `preds = []`). An existing row is never replaced.
struct NewChangeMeta<'a> {
    inner: &'a dyn IngestObserver,
    index: ChangeIndex,
    cs: ChangesetId,
}

/// The change id of a changeset a server `Commit` created: derived from the changeset id, so
/// a retry of the same commit names the same change.
fn commit_change_id(cs: ChangesetId) -> ChangeId {
    let key = blake3::derive_key("asc v1 change server commit", cs.as_bytes());
    let mut id = [0u8; 16];
    id.copy_from_slice(&key[..16]);
    ChangeId::from_bytes(id)
}

#[async_trait]
impl IngestObserver for NewChangeMeta<'_> {
    async fn on_ingested(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::any::Any>,
        repo: asc_store::refs::RepoKey,
        new: &[ChangesetId],
        closure: &[ObjKey],
    ) -> Result<(), ObserverError> {
        self.inner.on_ingested(tx, repo, new, closure).await?;
        if new.contains(&self.cs) {
            let meta = ChangeMeta::new(
                self.cs,
                commit_change_id(self.cs),
                Vec::new(),
                RewriteOp::New,
            );
            self.index.write_meta(tx, repo, &meta).await?;
        }
        Ok(())
    }

    async fn on_rewritten(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::any::Any>,
        repo: asc_store::refs::RepoKey,
        pairs: &[(ChangesetId, ChangesetId)],
        kind: RewriteOp,
    ) -> Result<(), ObserverError> {
        self.inner.on_rewritten(tx, repo, pairs, kind).await
    }
}

fn write(kind: Kind, s: &Stored, copy_from: Option<RepoPath>) -> Result<Change, Status> {
    Change::write(kind, s.file, s.size, s.git, copy_from).map_err(|e| invalid(e.to_string()))
}

async fn tree_entry(o: &Opened, root: TreeId, path: &RepoPath) -> Result<Entry, Status> {
    o.engine
        .lookup(root, path)
        .await
        .map_err(tree_status)?
        .ok_or_else(|| Status::not_found(format!("{path} does not exist")))
}

// ------------------------------------------------------------------------------ Ingest

impl ServerMutations {
    async fn ingest_inner(
        &self,
        ctx: &CallCtx,
        req: wire::IngestRequest,
    ) -> Result<wire::IngestResponse, Status> {
        let o = self.open(ctx).await?;
        if req.changesets.is_empty() || req.changesets.len() > MAX_INGEST_IDS {
            return Err(invalid("bad number of changesets"));
        }
        let mut seen = HashSet::new();
        let mut ids = Vec::with_capacity(req.changesets.len());
        for raw in &req.changesets {
            let id = ChangesetId::from_slice(raw).map_err(|e| invalid(e.to_string()))?;
            if seen.insert(id) {
                ids.push(id);
            }
        }
        let _gc = self.fence().await?;
        self.ingest_charged(ctx, &o, &ids).await?;
        Ok(wire::IngestResponse {
            ingested: ids
                .iter()
                .map(|id| Bytes::copy_from_slice(id.as_bytes()))
                .collect(),
        })
    }
}

// ------------------------------------------------------------------------------ Apply

/// The wire form of a merge conflict. `source` is the changeset being picked or reverted.
pub(crate) fn conflict_wire(source: Option<ChangesetId>, c: &Conflict) -> wire::Conflict {
    use wire::ConflictKind as W;
    let id = |e: &Option<Entry>| match e {
        Some(e) if matches!(e.kind, Kind::File | Kind::Exec | Kind::Symlink) => {
            Bytes::copy_from_slice(&e.id)
        }
        _ => Bytes::new(),
    };
    wire::Conflict {
        changeset: source.map_or_else(Bytes::new, |s| Bytes::copy_from_slice(s.as_bytes())),
        path: Bytes::copy_from_slice(c.path.as_bytes()),
        kind: match c.kind {
            ConflictKind::Content => W::Content,
            ConflictKind::AddAdd => W::AddAdd,
            ConflictKind::ModifyDelete => W::ModifyDelete,
            ConflictKind::FileDir => W::FileDir,
            ConflictKind::CaseFold => W::CaseFold,
            ConflictKind::Binary => W::Binary,
        } as i32,
        base_id: id(&c.stages.base),
        ours_id: id(&c.stages.ours),
        theirs_id: id(&c.stages.theirs),
        marked_id: c
            .marked_id
            .map_or_else(Bytes::new, |f| Bytes::copy_from_slice(f.as_bytes())),
    }
}

/// A response that reports conflicts and made no changeset.
fn conflicted(details: Vec<wire::Conflict>) -> wire::ApplyResponse {
    wire::ApplyResponse {
        result: Bytes::new(),
        conflicts: details.iter().map(|c| c.path.clone()).collect(),
        conflict_details: details,
    }
}

/// A patch file's slot in the staged result.
struct Slot {
    content: Vec<u8>,
    kind: Kind,
}

/// What `probe` learned about a file without reading it: its length, kind and, for a tree file,
/// the content id (`None` for a file already staged in memory).
struct Probe {
    len: u64,
    kind: Kind,
    id: Option<FileId>,
}

/// What a patch does to the tree so far: `Some` writes, `None` deletes.
type Overlay = BTreeMap<RepoPath, Option<Slot>>;

fn patch_status(e: &patch::Error) -> Status {
    match e {
        patch::Error::TooExpensive | patch::Error::FileTooLarge | patch::Error::TooLarge => {
            Status::resource_exhausted(e.to_string())
        }
        e => invalid(e.to_string()),
    }
}

impl ServerMutations {
    async fn apply_inner(
        &self,
        ctx: &CallCtx,
        req: wire::ApplyRequest,
    ) -> Result<wire::ApplyResponse, Status> {
        let o = self.open(ctx).await?;
        check_update_ref(&req.update_ref)?;
        check_message(&req.message)?;
        let mode =
            wire::ApplyMode::try_from(req.mode).map_err(|_| invalid("unknown apply mode"))?;
        match mode {
            wire::ApplyMode::Patch => {
                if !req.changeset.is_empty() {
                    return Err(invalid("a patch apply names no changeset"));
                }
                if req.patch.is_empty() || req.patch.len() > MAX_PATCH_BYTES {
                    return Err(invalid("bad patch size"));
                }
            }
            wire::ApplyMode::CherryPick | wire::ApplyMode::Revert => {
                if req.changeset.is_empty() || !req.patch.is_empty() {
                    return Err(invalid("a pick or revert names one changeset and no patch"));
                }
            }
        }
        if !req.update_ref.is_empty() {
            asc_core::path::RefName::new(req.update_ref.as_bytes())
                .map_err(|e| invalid(e.to_string()))?;
            if let Some(id) = self.replay(&o, ctx, &req.update_ref).await? {
                return Ok(wire::ApplyResponse {
                    result: Bytes::copy_from_slice(id.as_bytes()),
                    ..wire::ApplyResponse::default()
                });
            }
        }
        let _gc = if req.update_ref.is_empty() {
            Some(self.fence().await?)
        } else {
            None
        };
        let onto = self.rev(&o, &req.onto).await?;
        let onto_cs = o.engine.get_changeset(onto).await.map_err(commit_status)?;

        // The new tree, the changeset it comes from (if any) and the copies it records.
        let (root, copies, default_author, default_message) = match mode {
            wire::ApplyMode::Patch => {
                let applied = self.apply_patch(&o, onto_cs.root, &req.patch).await?;
                match applied {
                    Err(conflicts) => return Ok(conflicted(conflicts)),
                    Ok((root, copies)) => (root, copies, None, Vec::new()),
                }
            }
            wire::ApplyMode::CherryPick | wire::ApplyMode::Revert => {
                let source = self.rev(&o, &req.changeset).await?;
                let original = o
                    .engine
                    .get_changeset(source)
                    .await
                    .map_err(commit_status)?;
                let _text = self.budget(MERGE_TEXT_ESTIMATE).await?;
                let opts = MergeOptions::default();
                let engine = o.engine.clone();
                let pick = mode == wire::ApplyMode::CherryPick;
                let outcome = on_blocking_thread(move || async move {
                    if pick {
                        engine.cherry_pick(source, onto, None, &opts).await
                    } else {
                        engine.revert(source, onto, None, &opts).await
                    }
                })
                .await?
                .map_err(merge_status)?;
                if !outcome.conflicts.is_empty() {
                    return Ok(conflicted(
                        outcome
                            .conflicts
                            .iter()
                            .take(MAX_CONFLICTS)
                            .map(|c| conflict_wire(Some(source), c))
                            .collect(),
                    ));
                }
                let (author, message) = if mode == wire::ApplyMode::CherryPick {
                    (Some(original.author.clone()), original.message.clone())
                } else {
                    (None, revert_message(&original.message, source))
                };
                (outcome.root, Vec::new(), author, message)
            }
        };
        if root == onto_cs.root {
            return Err(Status::failed_precondition("the result changes nothing"));
        }
        let message = if req.message.is_empty() {
            default_message
        } else {
            req.message.to_vec()
        };
        if message.is_empty() {
            return Err(invalid("a message is required"));
        }
        let (author, committer) =
            signatures(req.author.as_ref(), req.committer.as_ref(), default_author)?;
        let id = o
            .create(NewChangeset {
                parents: vec![onto],
                author,
                committer,
                message,
                extra: Vec::new(),
                root,
                copies,
            })
            .await?;
        // The ref moves only if it still names what the apply was measured against.
        let expect = if req.update_ref.is_empty() {
            wire::ref_update::Expect::Absent(true)
        } else {
            self.ref_expectation(&o, ctx, &req.update_ref, onto).await?
        };
        self.publish(&o, ctx, id, &req.update_ref, expect).await?;
        Ok(wire::ApplyResponse {
            result: Bytes::copy_from_slice(id.as_bytes()),
            ..wire::ApplyResponse::default()
        })
    }

    /// `Apply` and `Merge` have no `expect`: an existing `update_ref` must still name the
    /// changeset the result was measured against, and an absent one is created. The value is
    /// read under the name `UpdateRefs` will write (SPEC §21.5.1): in a branchless repository a
    /// caller's `refs/heads/<n>` is stored as `refs/users/<owner>/heads/<n>`, so reading the
    /// name as given would find nothing and the later compare-and-set would fail on an existing
    /// ref. `classify` maps only into the caller's own prefix, so no other owner's ref is read.
    async fn ref_expectation(
        &self,
        o: &Opened,
        ctx: &CallCtx,
        update_ref: &str,
        onto: ChangesetId,
    ) -> Result<wire::ref_update::Expect, Status> {
        use asc_store::refs::Refs;
        let policy =
            crate::refpolicy::RepoPolicy::load(self.registry.pool(), o.entry.key.0).await?;
        let class = policy.classify(
            crate::refpolicy::draft_owner_of(&ctx.principal, update_ref),
            update_ref,
        )?;
        let name = asc_core::path::RefName::new(class.stored(update_ref).as_bytes())
            .map_err(|e| invalid(e.to_string()))?;
        let current = self
            .registry
            .refs()
            .get(o.entry.key, &name)
            .await
            .map_err(|e| internal("refs", &e))?;
        Ok(match current {
            None => wire::ref_update::Expect::Absent(true),
            Some(_) => {
                wire::ref_update::Expect::Value(wire::RefTarget::from(&RefTarget::Changeset(onto)))
            }
        })
    }

    /// Applies `patch_bytes` to the tree `base`. `Ok(Err(conflicts))` lists files whose hunks do
    /// not apply (nothing is written); `Err` is a refused request.
    async fn apply_patch(
        &self,
        o: &Opened,
        base: TreeId,
        patch_bytes: &[u8],
    ) -> Result<Result<(TreeId, Vec<(RepoPath, RepoPath)>), Vec<wire::Conflict>>, Status> {
        let input = patch_bytes.to_vec();
        let files = tokio::task::spawn_blocking(move || {
            patch::parse(&input, &patch::ParseOptions::default())
        })
        .await
        .map_err(|e| internal("patch parser", &e))?
        .map_err(|e| patch_status(&e))?;
        if files.is_empty() {
            return Err(invalid("the patch changes no file"));
        }
        let work = patch::Work::default();
        let opts = patch::ApplyOptions::default();
        let mut overlay: Overlay = BTreeMap::new();
        let mut conflicts: Vec<wire::Conflict> = Vec::new();
        let mut staged: u64 = 0;
        let mut guards: Vec<TextGuard> = Vec::new();
        for fp in &files {
            if fp.binary.is_some() || fp.binary_no_body {
                return Err(invalid("binary patches are not supported"));
            }
            let dest = fp
                .new_path
                .as_deref()
                .or(fp.old_path.as_deref())
                .ok_or_else(|| invalid("a file patch names no file"))?;
            let src = fp.old_path.as_deref();
            let dest_path = op_path(dest)?;
            let src_path = src.map(op_path).transpose()?;
            if [fp.old_mode, fp.new_mode]
                .iter()
                .flatten()
                .any(|m| !matches!(m & 0o170000, 0o100000))
            {
                return Err(invalid(format!(
                    "{dest_path}: symbolic link and submodule patches are not supported"
                )));
            }
            let conflict = |kind: wire::ConflictKind| wire::Conflict {
                path: Bytes::copy_from_slice(dest_path.as_bytes()),
                kind: kind as i32,
                ..wire::Conflict::default()
            };
            // The file the hunks apply to.
            // Existence and size come from metadata alone; content is read only after the
            // file is under the per-file maximum and its text budget is reserved.
            let src_probe = match (&src_path, fp.is_new) {
                (Some(p), false) => match self.probe(o, base, &overlay, p).await? {
                    Some(probe) => Some(probe),
                    None => {
                        conflicts.push(conflict(wire::ConflictKind::ModifyDelete));
                        continue;
                    }
                },
                _ => None,
            };
            if (fp.is_new || fp.is_rename || fp.is_copy)
                && (fp.is_new || src_path.as_ref() != Some(&dest_path))
                && self.probe(o, base, &overlay, &dest_path).await?.is_some()
            {
                conflicts.push(conflict(wire::ConflictKind::AddAdd));
                continue;
            }
            let base_slot = match (&src_path, src_probe) {
                (Some(p), Some(probe)) => {
                    guards.push(self.budget(probe.len.saturating_mul(2)).await?);
                    Some(self.load(o, &overlay, p, probe).await?)
                }
                _ => None,
            };
            let pre = base_slot.as_ref().map(|s| &s.content[..]);
            let result = match patch::apply_file(pre, fp, opts, &work) {
                Ok(r) => r.content,
                Err(patch::Error::HunkFailed { .. } | patch::Error::RemovalLeavesContent) => {
                    conflicts.push(conflict(wire::ConflictKind::Content));
                    continue;
                }
                Err(patch::Error::Exists(_)) => {
                    conflicts.push(conflict(wire::ConflictKind::AddAdd));
                    continue;
                }
                Err(patch::Error::Missing(_)) => {
                    conflicts.push(conflict(wire::ConflictKind::ModifyDelete));
                    continue;
                }
                Err(e) => return Err(patch_status(&e)),
            };
            if conflicts.len() > MAX_CONFLICTS {
                return Err(Status::resource_exhausted("too many conflicts"));
            }
            let kind = match fp.new_mode {
                Some(0o100755) => Kind::Exec,
                Some(0o100644) => Kind::File,
                Some(m) => return Err(invalid(format!("{dest_path}: unsupported mode {m:o}"))),
                None => base_slot.as_ref().map_or(Kind::File, |b| b.kind),
            };
            if fp.is_rename
                && let Some(old) = &src_path
                && *old != dest_path
            {
                overlay.insert(old.clone(), None);
            }
            let slot = match result {
                Some(content) => {
                    staged = staged.saturating_add(content.len() as u64);
                    if staged > MAX_PATCH_STAGED_BYTES {
                        return Err(Status::resource_exhausted("the patch result is too large"));
                    }
                    Some(Slot { content, kind })
                }
                None => None,
            };
            overlay.insert(dest_path, slot);
        }
        if !conflicts.is_empty() {
            return Ok(Err(conflicts));
        }
        let mut changes = Vec::with_capacity(overlay.len());
        let mut copies = Vec::new();
        for (path, slot) in overlay {
            match slot {
                None => changes.push((path, Change::Delete)),
                Some(slot) => {
                    let s = self.store_content(o, &slot.content).await?;
                    changes.push((path, write(slot.kind, &s, None)?));
                }
            }
        }
        // A copy or rename annotation is only valid where the source is in `base`.
        for fp in &files {
            if (fp.is_rename || fp.is_copy)
                && let (Some(old), Some(new)) = (fp.old_path.as_deref(), fp.new_path.as_deref())
                && old != new
            {
                let (old, new) = (op_path(old)?, op_path(new)?);
                if o.engine
                    .lookup(base, &old)
                    .await
                    .map_err(tree_status)?
                    .is_some()
                    && changes
                        .iter()
                        .any(|(p, c)| *p == new && matches!(c, Change::Write { .. }))
                {
                    copies.push((new, old));
                }
            }
        }
        let root = o.apply_changes(base, changes).await?;
        Ok(Ok((root, copies)))
    }

    /// What `path` is as the patch has left it, without reading content: the overlay first, then
    /// the tree. Directories, symlinks and gitlinks are refused, and so is a file above
    /// `MAX_CONTENT_BYTES`.
    async fn probe(
        &self,
        o: &Opened,
        base: TreeId,
        overlay: &Overlay,
        path: &RepoPath,
    ) -> Result<Option<Probe>, Status> {
        let too_large = |len: u64| {
            if len > MAX_CONTENT_BYTES as u64 {
                Err(invalid(format!("{path}: the file is too large to patch")))
            } else {
                Ok(())
            }
        };
        if let Some(slot) = overlay.get(path) {
            return match slot {
                None => Ok(None),
                Some(s) => {
                    let len = s.content.len() as u64;
                    too_large(len)?;
                    Ok(Some(Probe {
                        len,
                        kind: s.kind,
                        id: None,
                    }))
                }
            };
        }
        if path.as_bytes().len() > MAX_TREE_DEPTH * MAX_WALK_PATH_BYTES {
            return Err(invalid("path is too long"));
        }
        let Some(entry) = o.engine.lookup(base, path).await.map_err(tree_status)? else {
            return Ok(None);
        };
        if !matches!(entry.kind, Kind::File | Kind::Exec) {
            return Err(invalid(format!(
                "{path}: only regular files can be patched"
            )));
        }
        too_large(entry.size)?;
        Ok(Some(Probe {
            len: entry.size,
            kind: entry.kind,
            id: Some(FileId::from_bytes(entry.id)),
        }))
    }

    /// The content `probe` described. The caller has reserved `probe.len * 2` from the text
    /// budget first.
    async fn load(
        &self,
        o: &Opened,
        overlay: &Overlay,
        path: &RepoPath,
        probe: Probe,
    ) -> Result<Slot, Status> {
        let content = match probe.id {
            Some(id) => o
                .engine
                .read_file(id, probe.len)
                .await
                .map_err(|e| internal("read file", &e))?
                .to_vec(),
            None => overlay
                .get(path)
                .and_then(|s| s.as_ref())
                .map(|s| s.content.clone())
                .ok_or_else(|| internal("overlay", &"a staged file vanished"))?,
        };
        Ok(Slot {
            content,
            kind: probe.kind,
        })
    }
}

/// git's default revert message.
fn revert_message(original: &[u8], id: ChangesetId) -> Vec<u8> {
    let subject: Vec<u8> = original
        .split(|&b| b == b'\n')
        .next()
        .unwrap_or_default()
        .iter()
        .copied()
        .take(MAX_SUBJECT)
        .collect();
    let mut out = b"Revert \"".to_vec();
    out.extend_from_slice(&subject);
    out.extend_from_slice(format!("\"\n\nThis reverts commit {id}.\n").as_bytes());
    out
}

// `Merge` (PLAN U48b) shares this module's helpers: it is a child, so it sees what is private here.
#[path = "merge_rpc.rs"]
pub mod merge_rpc;
// `Rebase` and the land content merge (PLAN U68) share them too.
#[path = "land_merge.rs"]
pub mod land_merge;
#[path = "rebase.rs"]
pub mod rebase;

// ------------------------------------------------------------------------------ handlers

#[async_trait]
impl MutationsHandler for ServerMutations {
    async fn commit(
        &self,
        ctx: &CallCtx,
        req: wire::CommitRequest,
    ) -> Result<wire::CommitResponse, Status> {
        self.commit_inner(ctx, req).await
    }

    async fn apply(
        &self,
        ctx: &CallCtx,
        req: wire::ApplyRequest,
    ) -> Result<wire::ApplyResponse, Status> {
        self.apply_inner(ctx, req).await
    }

    async fn merge(
        &self,
        ctx: &CallCtx,
        req: wire::MergeRequest,
    ) -> Result<wire::MergeResponse, Status> {
        self.merge_inner(ctx, req).await
    }

    async fn copy_closure(
        &self,
        ctx: &CallCtx,
        req: wire::CopyClosureRequest,
    ) -> Result<wire::CopyClosureResponse, Status> {
        crate::admin::copy_closure(&self.registry, &self.pipeline, ctx, req).await
    }
}

#[async_trait]
impl ImportPushHandler for ServerMutations {
    async fn ingest(
        &self,
        ctx: &CallCtx,
        req: wire::IngestRequest,
    ) -> Result<wire::IngestResponse, Status> {
        self.ingest_inner(ctx, req).await
    }
}

/// The one registration line of this unit (called from `lib.rs`): the `mutations` group.
/// `hooks` supplies the text budget. The `import_push` group (`Ingest` and `ImportGitPush`) is
/// registered by `import_push::register` over the handler this returns.
pub fn register(
    ext: &Extensions,
    registry: &Arc<Registry>,
    hooks: &Arc<ReadHooks>,
) -> Arc<ServerMutations> {
    let handler = Arc::new(ServerMutations::new(
        registry.clone(),
        ext.pipeline.clone(),
        hooks.clone(),
    ));
    ext.handlers
        .set_mutations(handler.clone())
        .expect("the mutations handler group is registered once");
    handler
}

// ------------------------------------------------------------------------------ tests

#[cfg(test)]
mod fx {
    use asc_core::path::RefName;
    use asc_rpc::auth::Principal;
    use asc_rpc::server::{RepoName, status_rpc_error};
    use uuid::Uuid;

    use super::*;
    use crate::ingest::testutil::{registry, repo};

    pub struct Fx {
        pub registry: Arc<Registry>,
        pub entry: Arc<RepoEntry>,
        pub pipeline: Arc<PipelineHooks>,
        pub hooks: Arc<ReadHooks>,
        pub m: ServerMutations,
    }

    pub async fn fx() -> Fx {
        let registry = registry().await;
        let entry = repo(&registry).await;
        let pipeline = Arc::new(PipelineHooks::default());
        let hooks = Arc::new(ReadHooks::default());
        let m = ServerMutations::new(registry.clone(), pipeline.clone(), hooks.clone());
        Fx {
            registry,
            entry,
            pipeline,
            hooks,
            m,
        }
    }

    pub fn ctx() -> CallCtx {
        CallCtx {
            repo: RepoName::parse("o/r").unwrap(),
            id_mode: IdMode::Asc,
            principal: Principal { id: "alice".into() },
            op_id: Some(Uuid::new_v4()),
            client: None,
            admin: Default::default(),
        }
    }

    /// A call by `who` (a Forgejo identity, `<user_id>:<login>`) with a fresh op id.
    pub fn ctx_as(who: &str) -> CallCtx {
        CallCtx {
            principal: Principal { id: who.into() },
            op_id: Some(Uuid::new_v4()),
            ..ctx()
        }
    }

    /// Makes the repository branchless: a user's `refs/heads/<n>` is stored under `refs/users/`.
    pub async fn make_branchless(f: &Fx) {
        sqlx::query("UPDATE repos SET branchless = TRUE")
            .execute(f.registry.pool())
            .await
            .unwrap();
    }

    /// Points `name` at `cs`, whatever it held.
    pub async fn set_ref(f: &Fx, name: &str, cs: ChangesetId) {
        use asc_store::refs::{Actor, Expect, RefTxn, RefUpdate, Refs as _};
        let txn = RefTxn {
            updates: vec![RefUpdate {
                name: RefName::new(name.as_bytes().to_vec()).unwrap(),
                expect: Expect::Any,
                new: Some(RefTarget::Changeset(cs)),
                expires_at: None,
            }],
            actor: Actor {
                id: "t".into(),
                via: "test".into(),
            },
            reason: "test".into(),
            op_id: Uuid::new_v4(),
        };
        f.registry.refs().update(f.entry.key, &txn).await.unwrap();
    }

    pub fn repo_ref() -> Option<wire::RepoRef> {
        Some(wire::RepoRef {
            name: "o/r".into(),
            id_mode: IdMode::Asc as i32,
        })
    }

    pub fn sig() -> wire::Signature {
        wire::Signature {
            name: Bytes::from_static(b"Al"),
            email: Bytes::from_static(b"al@x"),
            time: 1_700_000_000,
            tz_min: 0,
        }
    }

    /// The `RpcError` code a status carries, else the gRPC code.
    pub fn code(s: &Status) -> String {
        status_rpc_error(s).map_or_else(|| format!("{:?}", s.code()), |e| e.code().to_owned())
    }

    pub fn op(path: &str, op: wire::file_op::Op) -> wire::FileOp {
        wire::FileOp {
            path: Bytes::copy_from_slice(path.as_bytes()),
            op: Some(op),
            mode: 0,
        }
    }

    pub fn put(path: &str, content: &[u8]) -> wire::FileOp {
        op(
            path,
            wire::file_op::Op::PutContent(Bytes::copy_from_slice(content)),
        )
    }

    pub async fn count(f: &Fx, sql: &'static str) -> i64 {
        sqlx::query_scalar::<_, i64>(sql)
            .fetch_one(f.registry.pool())
            .await
            .unwrap()
    }

    pub async fn graph_rows(f: &Fx) -> i64 {
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM cs_graph WHERE ns = $1")
            .bind(f.entry.ns.as_bytes().to_vec())
            .fetch_one(f.registry.pool())
            .await
            .unwrap()
    }

    pub async fn ref_target(f: &Fx, name: &str) -> Option<RefTarget> {
        use asc_store::refs::Refs as _;
        f.registry
            .refs()
            .get(
                f.entry.key,
                &RefName::new(name.as_bytes().to_vec()).unwrap(),
            )
            .await
            .unwrap()
            .map(|e| e.target)
    }

    pub fn id(b: &[u8]) -> ChangesetId {
        ChangesetId::from_slice(b).unwrap()
    }

    /// The bytes at `path` in changeset `cs`.
    pub async fn read(f: &Fx, cs: ChangesetId, path: &str) -> Option<Vec<u8>> {
        let engine = f.entry.backend.engine();
        let root = engine.get_changeset(cs).await.unwrap().root;
        let e = engine
            .lookup(root, &RepoPath::new(path).unwrap())
            .await
            .unwrap()?;
        Some(
            engine
                .read_file(FileId::from_bytes(e.id), e.size)
                .await
                .unwrap()
                .to_vec(),
        )
    }

    pub fn expect_value(t: ChangesetId) -> Option<wire::RefTarget> {
        Some(wire::RefTarget::from(&RefTarget::Changeset(t)))
    }

    pub async fn make_mirror(f: &Fx) {
        sqlx::query("UPDATE repos SET mode = 'mirror'")
            .execute(f.registry.pool())
            .await
            .unwrap();
        f.registry.forget(&ctx().repo);
    }

    pub async fn ingest_one(f: &Fx, cs: ChangesetId) {
        f.m.ingest_inner(
            &ctx(),
            wire::IngestRequest {
                repo: repo_ref(),
                changesets: vec![Bytes::copy_from_slice(cs.as_bytes())],
            },
        )
        .await
        .unwrap();
    }

    pub async fn opened(f: &Fx) -> Opened {
        f.m.open(&ctx()).await.unwrap()
    }
}

#[cfg(test)]
mod ingest {
    use std::sync::Mutex;

    use asc_engine::commit::{NewChangeset, PathPolicy};
    use asc_rpc::server::HistoryHandler;

    use super::fx::*;
    use super::*;
    use crate::ingest::testutil::commit;
    use crate::ingest::{IngestObserver, ObserverError};

    fn request(ids: &[ChangesetId]) -> wire::IngestRequest {
        wire::IngestRequest {
            repo: repo_ref(),
            changesets: ids
                .iter()
                .map(|i| Bytes::copy_from_slice(i.as_bytes()))
                .collect(),
        }
    }

    /// Guard (B08d): `rev` resolves through the viewer-filtered `IdCodec`, so no mutation can
    /// take another owner's `refs/users/<id>/..` as a parent, `onto` or source.
    #[tokio::test]
    async fn rev_cannot_name_another_users_ref() {
        let f = fx().await;
        let a = commit(
            &f.entry,
            &[],
            &[(
                "a", b"1
",
            )],
        )
        .await;
        f.m.ingest_inner(&ctx(), request(&[a])).await.unwrap();
        set_ref(&f, "refs/users/9/heads/feat", a).await;
        set_ref(&f, "refs/heads/main", a).await;
        let o = f.m.open(&ctx_as("7:mallory")).await.unwrap();
        for spec in [
            "refs/users/9/heads/feat",
            "users/9/heads/feat",
            "refs/users/9/heads/feat@{0}",
        ] {
            let e = f.m.rev(&o, spec).await.unwrap_err();
            assert_eq!(e.code(), tonic::Code::NotFound, "{spec}");
        }
        assert_eq!(f.m.rev(&o, "refs/heads/main").await.unwrap(), a);
    }

    #[tokio::test]
    async fn without_a_ref_then_diff_and_merge_base_by_id_succeed_and_ref_log_is_unchanged() {
        let f = fx().await;
        let a = commit(&f.entry, &[], &[("a", b"1\n")]).await;
        let b = commit(&f.entry, &[a], &[("b", b"2\n")]).await;
        let c = commit(&f.entry, &[a], &[("c", b"3\n")]).await;
        // Uploaded but not ingested: no graph rows yet.
        assert_eq!(graph_rows(&f).await, 0);
        let logged = count(&f, "SELECT COUNT(*) FROM ref_log").await;
        let outbox = count(&f, "SELECT COUNT(*) FROM outbox").await;

        let got = f.m.ingest_inner(&ctx(), request(&[b, c])).await.unwrap();
        assert_eq!(got.ingested.len(), 2);
        // The closure is in the graph and eagerly derived into the git map.
        assert_eq!(graph_rows(&f).await, 3);
        assert_eq!(
            count(&f, "SELECT COUNT(*) FROM git_map WHERE kind = 3").await,
            3
        );

        // By id, with no ref anywhere.
        let c2 = ctx();
        let mb = HistoryHandler::merge_base(
            &*f.entry.backend,
            &c2,
            wire::MergeBaseRequest {
                repo: repo_ref(),
                a: Bytes::copy_from_slice(b.as_bytes()),
                b: vec![Bytes::copy_from_slice(c.as_bytes())],
                all: false,
            },
        )
        .await
        .unwrap();
        assert_eq!(mb.bases[0].as_ref(), a.as_bytes());
        let mut diff = HistoryHandler::diff(
            &*f.entry.backend,
            &c2,
            wire::DiffRequest {
                repo: repo_ref(),
                base: a.to_string(),
                head: b.to_string(),
                ..wire::DiffRequest::default()
            },
        )
        .await
        .unwrap();
        assert!(futures::StreamExt::next(&mut diff).await.unwrap().is_ok());

        assert_eq!(count(&f, "SELECT COUNT(*) FROM ref_log").await, logged);
        assert_eq!(count(&f, "SELECT COUNT(*) FROM outbox").await, outbox);
        assert_eq!(count(&f, "SELECT COUNT(*) FROM refs").await, 0);
        // Ingesting again (with a repeat in the list) is a no-op that answers the same.
        let again = f.m.ingest_inner(&ctx(), request(&[b, c, b])).await.unwrap();
        assert_eq!(again.ingested.len(), 2);
        assert_eq!(graph_rows(&f).await, 3);
    }

    struct Seen(Mutex<Vec<ChangesetId>>);

    #[async_trait]
    impl IngestObserver for Seen {
        async fn on_ingested(
            &self,
            _tx: &mut sqlx::Transaction<'_, sqlx::any::Any>,
            _repo: asc_store::refs::RepoKey,
            new: &[ChangesetId],
            _closure: &[ObjKey],
        ) -> Result<(), ObserverError> {
            self.0.lock().unwrap().extend_from_slice(new);
            Ok(())
        }
    }

    #[tokio::test]
    async fn runs_the_observer_in_step_four_with_the_new_changesets() {
        let f = fx().await;
        let seen = Arc::new(Seen(Mutex::new(Vec::new())));
        f.pipeline.set_observer(seen.clone()).ok().unwrap();
        let a = commit(&f.entry, &[], &[("a", b"1\n")]).await;
        f.m.ingest_inner(&ctx(), request(&[a])).await.unwrap();
        assert_eq!(*seen.0.lock().unwrap(), vec![a]);
        // Already known: nothing new, so the observer sees nothing more.
        f.m.ingest_inner(&ctx(), request(&[a])).await.unwrap();
        assert_eq!(seen.0.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn missing_objects_are_reported_and_nothing_is_recorded() {
        let f = fx().await;
        let ghost = ChangesetId::from_bytes([9; 32]);
        let e =
            f.m.ingest_inner(&ctx(), request(&[ghost]))
                .await
                .unwrap_err();
        assert_eq!(code(&e), "MISSING_OBJECTS", "{e:?}");
        assert_eq!(graph_rows(&f).await, 0);
    }

    #[tokio::test]
    async fn raw_git_extras_are_refused() {
        let f = fx().await;
        let a = commit(&f.entry, &[], &[("a", b"1\n")]).await;
        let engine = f.entry.backend.engine();
        let root = engine.get_changeset(a).await.unwrap().root;
        let s = Signature::new(b"A".to_vec(), b"a@x".to_vec(), 1, 0).unwrap();
        let forged = engine
            .create_changeset(
                NewChangeset {
                    parents: Vec::new(),
                    author: s.clone(),
                    committer: s,
                    message: b"m".to_vec(),
                    extra: vec![(b"git.commit".to_vec(), b"tree 0\n".to_vec())],
                    root,
                    copies: Vec::new(),
                },
                PathPolicy::Import,
            )
            .await
            .unwrap();
        let e =
            f.m.ingest_inner(&ctx(), request(&[forged]))
                .await
                .unwrap_err();
        assert_eq!(code(&e), "INVALID_OBJECT", "{e:?}");
        assert_eq!(graph_rows(&f).await, 0);
    }

    #[tokio::test]
    async fn bad_requests_are_refused() {
        let f = fx().await;
        for ids in [
            Vec::new(),
            vec![Bytes::from_static(b"short")],
            vec![Bytes::from_static(&[1; 32]); MAX_INGEST_IDS + 1],
        ] {
            let e =
                f.m.ingest_inner(
                    &ctx(),
                    wire::IngestRequest {
                        repo: repo_ref(),
                        changesets: ids,
                    },
                )
                .await
                .unwrap_err();
            assert_eq!(e.code(), tonic::Code::InvalidArgument);
        }
        let mut git = ctx();
        git.id_mode = IdMode::Git;
        let e = f.m.ingest_inner(&git, request(&[])).await.unwrap_err();
        assert_eq!(e.code(), tonic::Code::Unimplemented);
    }

    #[tokio::test]
    async fn a_mirror_takes_no_ingest() {
        let f = fx().await;
        let a = commit(&f.entry, &[], &[("a", b"1\n")]).await;
        make_mirror(&f).await;
        let e = f.m.ingest_inner(&ctx(), request(&[a])).await.unwrap_err();
        assert_eq!(code(&e), "MIRROR_MODE");
        assert_eq!(graph_rows(&f).await, 0);
    }
}

#[cfg(test)]
mod commit_rpc {
    use std::sync::Mutex;

    use super::fx::*;
    use super::*;
    use crate::ingest::testutil::commit;
    use crate::ingest::{IngestObserver, ObserverError};

    fn request(parent: &str, ops: Vec<wire::FileOp>) -> wire::CommitRequest {
        wire::CommitRequest {
            repo: repo_ref(),
            parent: parent.into(),
            ops,
            message: Bytes::from_static(b"msg\n"),
            author: Some(sig()),
            committer: Some(sig()),
            ..wire::CommitRequest::default()
        }
    }

    fn mixed_ops(file: FileId) -> Vec<wire::FileOp> {
        vec![
            put("dir/new", b"new\n"),
            put("exec", b"#!/bin/sh\n"),
            op("gone", wire::file_op::Op::Delete(true)),
            op(
                "moved",
                wire::file_op::Op::MoveFrom(Bytes::from_static(b"old")),
            ),
            op(
                "by/id",
                wire::file_op::Op::PutFileId(Bytes::copy_from_slice(file.as_bytes())),
            ),
        ]
    }

    #[tokio::test]
    async fn without_a_ref_builds_and_ingests_only() {
        let f = fx().await;
        let base = commit(
            &f.entry,
            &[],
            &[("keep", b"k\n"), ("gone", b"g\n"), ("old", b"o\n")],
        )
        .await;
        ingest_one(&f, base).await;
        let file =
            f.m.store_content(&opened(&f).await, b"shared\n")
                .await
                .unwrap()
                .file;
        let logged = count(&f, "SELECT COUNT(*) FROM ref_log").await;
        let got =
            f.m.commit_inner(&ctx(), request(&base.to_string(), mixed_ops(file)))
                .await
                .unwrap();
        let cs = id(&got.id);
        assert_eq!(got.git.len(), 20);
        assert_eq!(read(&f, cs, "dir/new").await.unwrap(), b"new\n");
        assert_eq!(read(&f, cs, "by/id").await.unwrap(), b"shared\n");
        assert_eq!(read(&f, cs, "moved").await.unwrap(), b"o\n");
        assert_eq!(read(&f, cs, "old").await, None);
        assert_eq!(read(&f, cs, "gone").await, None);
        assert_eq!(read(&f, cs, "keep").await.unwrap(), b"k\n");
        let stored = f.entry.backend.engine().get_changeset(cs).await.unwrap();
        assert_eq!(stored.parents, vec![base]);
        assert!(stored.extra.as_slice().is_empty());
        let exec = f
            .entry
            .backend
            .engine()
            .lookup(stored.root, &RepoPath::new("exec").unwrap())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(exec.kind, Kind::File);
        // Pipeline step 4 only: in the graph, no ref, no log, no outbox.
        assert_eq!(graph_rows(&f).await, 2);
        assert_eq!(count(&f, "SELECT COUNT(*) FROM ref_log").await, logged);
        assert_eq!(count(&f, "SELECT COUNT(*) FROM refs").await, 0);
        assert_eq!(count(&f, "SELECT COUNT(*) FROM outbox").await, 0);
        // The same request again names the same changeset.
        let again =
            f.m.commit_inner(&ctx(), request(&base.to_string(), mixed_ops(file)))
                .await
                .unwrap();
        assert_eq!(again.id, got.id);
    }

    #[tokio::test]
    async fn a_mode_makes_an_executable_file() {
        let f = fx().await;
        let mut p = put("run.sh", b"#!/bin/sh\n");
        p.mode = 0o100755;
        let got =
            f.m.commit_inner(&ctx(), request("", vec![p]))
                .await
                .unwrap();
        let stored = f
            .entry
            .backend
            .engine()
            .get_changeset(id(&got.id))
            .await
            .unwrap();
        let e = f
            .entry
            .backend
            .engine()
            .lookup(stored.root, &RepoPath::new("run.sh").unwrap())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(e.kind, Kind::Exec);
    }

    #[tokio::test]
    async fn a_root_commit_needs_no_parent() {
        let f = fx().await;
        let got =
            f.m.commit_inner(&ctx(), request("", vec![put("a", b"1\n")]))
                .await
                .unwrap();
        let stored = f
            .entry
            .backend
            .engine()
            .get_changeset(id(&got.id))
            .await
            .unwrap();
        assert!(stored.parents.is_empty());
    }

    /// `Commit` takes the client's `expect`, which `UpdateRefs` compares against the stored user
    /// ref: a second commit onto the caller's branch carrying the first one's value succeeds.
    #[tokio::test]
    async fn a_second_commit_onto_an_existing_user_branch_in_a_branchless_repo_succeeds() {
        let f = fx().await;
        make_branchless(&f).await;
        let (alice, bob) = ("5:alice", "6:bob");
        let (a_ref, b_ref) = ("refs/users/5/heads/feat", "refs/users/6/heads/feat");
        let mut req = request("", vec![put("a", b"1\n")]);
        req.update_ref = "refs/heads/feat".into();
        let c1 = id(&f
            .m
            .commit_inner(&ctx_as(alice), req.clone())
            .await
            .unwrap()
            .id);
        let b1 = id(&f.m.commit_inner(&ctx_as(bob), req).await.unwrap().id);
        assert_eq!(ref_target(&f, a_ref).await, Some(RefTarget::Changeset(c1)));
        assert_eq!(ref_target(&f, b_ref).await, Some(RefTarget::Changeset(b1)));

        let mut next = request(&c1.to_string(), vec![put("b", b"2\n")]);
        next.update_ref = "refs/heads/feat".into();
        next.expect = expect_value(c1);
        let c2 = id(&f.m.commit_inner(&ctx_as(alice), next).await.unwrap().id);
        assert_eq!(ref_target(&f, a_ref).await, Some(RefTarget::Changeset(c2)));
        assert_eq!(ref_target(&f, b_ref).await, Some(RefTarget::Changeset(b1)));
    }

    #[tokio::test]
    async fn with_a_ref_it_runs_the_update_refs_pipeline_as_a_compare_and_set() {
        let f = fx().await;
        let mut req = request("", vec![put("a", b"1\n")]);
        req.update_ref = "refs/heads/master".into();
        let first = f.m.commit_inner(&ctx(), req.clone()).await.unwrap();
        let c1 = id(&first.id);
        assert_eq!(
            ref_target(&f, "refs/heads/master").await,
            Some(RefTarget::Changeset(c1))
        );
        assert_eq!(count(&f, "SELECT COUNT(*) FROM ref_log").await, 1);
        assert_eq!(count(&f, "SELECT COUNT(*) FROM outbox").await, 1);

        // No `expect` means "must not exist": a second create is a conflict.
        let e = f.m.commit_inner(&ctx(), req).await.unwrap_err();
        assert_eq!(code(&e), "REF_CONFLICT");

        // A wrong expectation is a conflict; the right one advances the ref.
        let mut next = request(&c1.to_string(), vec![put("b", b"2\n")]);
        next.update_ref = "refs/heads/master".into();
        next.expect = expect_value(ChangesetId::from_bytes([7; 32]));
        let e = f.m.commit_inner(&ctx(), next.clone()).await.unwrap_err();
        assert_eq!(code(&e), "REF_CONFLICT");
        next.expect = expect_value(c1);
        let c = ctx();
        let second = f.m.commit_inner(&c, next.clone()).await.unwrap();
        assert_eq!(
            ref_target(&f, "refs/heads/master").await,
            Some(RefTarget::Changeset(id(&second.id)))
        );
        // A retry of the same op id answers the stored result, even with a later timestamp.
        next.committer.as_mut().unwrap().time += 99;
        let replay = f.m.commit_inner(&c, next).await.unwrap();
        assert_eq!(replay.id, second.id);
        assert_eq!(count(&f, "SELECT COUNT(*) FROM ref_log").await, 2);

        // A rewind is not a fast-forward.
        let mut back = request("", vec![put("z", b"z\n")]);
        back.update_ref = "refs/heads/master".into();
        back.expect = expect_value(id(&second.id));
        let e = f.m.commit_inner(&ctx(), back).await.unwrap_err();
        assert_eq!(code(&e), "NOT_FAST_FORWARD");
    }

    #[tokio::test]
    async fn client_local_ref_namespaces_are_refused() {
        let f = fx().await;
        for name in [
            "refs/for/master",
            "refs/remotes/o/master",
            "refs/stash",
            "topic",
        ] {
            let mut req = request("", vec![put("a", b"1\n")]);
            req.update_ref = name.into();
            let e = f.m.commit_inner(&ctx(), req).await.unwrap_err();
            assert_eq!(e.code(), tonic::Code::InvalidArgument, "{name}: {e:?}");
        }
        assert_eq!(count(&f, "SELECT COUNT(*) FROM refs").await, 0);
    }

    #[tokio::test]
    async fn unsafe_paths_are_refused_before_anything_is_stored() {
        let f = fx().await;
        for p in [
            ".git/hooks/pre-commit",
            ".GIT/config",
            ".git ",
            ".git.",
            "git~1/x",
            "a\\b",
            "a/../b",
            "/abs",
            "",
        ] {
            let content = format!("content of {p}");
            let e =
                f.m.commit_inner(&ctx(), request("", vec![put(p, content.as_bytes())]))
                    .await
                    .unwrap_err();
            assert!(
                e.code() == tonic::Code::InvalidArgument || code(&e) == "PATH_POLICY",
                "{p:?}: {e:?}"
            );
            // Refused before the content reached the store.
            let stored = f
                .entry
                .objects
                .get(&ObjKey::File(FileId::of(content.as_bytes())))
                .await
                .unwrap();
            assert!(stored.is_none(), "{p:?} was stored");
        }
        // A name the repository's Windows policy refuses.
        let e =
            f.m.commit_inner(&ctx(), request("", vec![put("CON", b"x")]))
                .await
                .unwrap_err();
        assert_eq!(code(&e), "PATH_POLICY", "{e:?}");
        assert_eq!(graph_rows(&f).await, 0);
    }

    #[tokio::test]
    async fn malformed_requests_are_refused() {
        let f = fx().await;
        let base = request("", vec![put("a", b"1")]);
        let edit = |edit: fn(&mut wire::CommitRequest)| {
            let mut r = base.clone();
            edit(&mut r);
            r
        };
        let cases: Vec<(&str, wire::CommitRequest, &str)> = vec![
            ("sign", edit(|r| r.sign = true), "Unimplemented"),
            (
                "no signature",
                edit(|r| {
                    r.author = None;
                    r.committer = None;
                }),
                "InvalidArgument",
            ),
            (
                "long message",
                edit(|r| r.message = Bytes::from(vec![b'x'; MAX_MESSAGE_BYTES + 1])),
                "InvalidArgument",
            ),
            (
                "too many ops",
                edit(|r| r.ops = (0..=MAX_OPS).map(|i| put(&format!("f{i}"), b"x")).collect()),
                "InvalidArgument",
            ),
            (
                "same path twice",
                edit(|r| r.ops = vec![put("a", b"1"), put("a", b"2")]),
                "InvalidArgument",
            ),
            (
                "bad mode",
                edit(|r| r.ops[0].mode = 0o777),
                "InvalidArgument",
            ),
            ("no action", edit(|r| r.ops[0].op = None), "InvalidArgument"),
            (
                "unknown parent",
                edit(|r| r.parent = "nope".into()),
                "NotFound",
            ),
            (
                "escaping symlink",
                edit(|r| {
                    r.ops = vec![wire::FileOp {
                        mode: 0o120000,
                        ..put("l", b"../../etc/passwd")
                    }];
                }),
                "PATH_POLICY",
            ),
            (
                "move from nowhere",
                edit(|r| {
                    r.ops = vec![op(
                        "b",
                        wire::file_op::Op::MoveFrom(Bytes::from_static(b"a")),
                    )];
                }),
                "NotFound",
            ),
            (
                "file id that is not stored",
                edit(|r| {
                    r.ops = vec![op(
                        "b",
                        wire::file_op::Op::PutFileId(Bytes::from_static(&[5; 32])),
                    )];
                }),
                "MISSING_OBJECTS",
            ),
            (
                "file id of the wrong length",
                edit(|r| {
                    r.ops = vec![op(
                        "b",
                        wire::file_op::Op::PutFileId(Bytes::from_static(b"x")),
                    )];
                }),
                "InvalidArgument",
            ),
        ];
        for (name, req, want) in cases {
            let e = f.m.commit_inner(&ctx(), req).await.unwrap_err();
            assert_eq!(code(&e), want, "{name}: {e:?}");
        }
        assert_eq!(graph_rows(&f).await, 0);
    }

    #[tokio::test]
    async fn runs_the_observer_for_the_new_changeset() {
        struct Seen(Mutex<Vec<ChangesetId>>);
        #[async_trait]
        impl IngestObserver for Seen {
            async fn on_ingested(
                &self,
                _tx: &mut sqlx::Transaction<'_, sqlx::any::Any>,
                _repo: asc_store::refs::RepoKey,
                new: &[ChangesetId],
                _closure: &[ObjKey],
            ) -> Result<(), ObserverError> {
                self.0.lock().unwrap().extend_from_slice(new);
                Ok(())
            }
        }
        let f = fx().await;
        let seen = Arc::new(Seen(Mutex::new(Vec::new())));
        f.pipeline.set_observer(seen.clone()).ok().unwrap();
        let got =
            f.m.commit_inner(&ctx(), request("", vec![put("a", b"1")]))
                .await
                .unwrap();
        assert_eq!(*seen.0.lock().unwrap(), vec![id(&got.id)]);
    }

    #[tokio::test]
    async fn a_mirror_takes_no_commit() {
        let f = fx().await;
        make_mirror(&f).await;
        let e =
            f.m.commit_inner(&ctx(), request("", vec![put("a", b"1")]))
                .await
                .unwrap_err();
        assert_eq!(code(&e), "MIRROR_MODE");
        assert_eq!(graph_rows(&f).await, 0);
    }
}

#[cfg(test)]
mod apply_rpc {
    use super::fx::*;
    use super::*;
    use crate::TextBudget;
    use crate::ingest::testutil::commit;

    fn pick(onto: ChangesetId, cs: ChangesetId, mode: wire::ApplyMode) -> wire::ApplyRequest {
        wire::ApplyRequest {
            repo: repo_ref(),
            onto: onto.to_string(),
            changeset: cs.to_string(),
            mode: mode as i32,
            committer: Some(sig()),
            ..wire::ApplyRequest::default()
        }
    }

    fn patch_req(onto: ChangesetId, text: &str) -> wire::ApplyRequest {
        wire::ApplyRequest {
            repo: repo_ref(),
            onto: onto.to_string(),
            patch: Bytes::copy_from_slice(text.as_bytes()),
            mode: wire::ApplyMode::Patch as i32,
            message: Bytes::from_static(b"patched\n"),
            committer: Some(sig()),
            ..wire::ApplyRequest::default()
        }
    }

    async fn lines(f: &Fx) -> ChangesetId {
        commit(
            &f.entry,
            &[],
            &[
                ("a.txt", b"one\ntwo\nthree\n"),
                ("keep", b"k\n"),
                ("z.txt", b"z\n"),
            ],
        )
        .await
    }

    const MODIFY: &str = "diff --git a/a.txt b/a.txt\n--- a/a.txt\n+++ b/a.txt\n@@ -1,3 +1,3 @@\n one\n-two\n+TWO\n three\n";

    #[tokio::test]
    async fn cherry_pick_replays_onto_another_line_keeping_the_author() {
        let f = fx().await;
        let base = commit(&f.entry, &[], &[("a", b"1\n"), ("b", b"1\n")]).await;
        let feature = commit(&f.entry, &[base], &[("a", b"2\n")]).await;
        let main = commit(&f.entry, &[base], &[("b", b"2\n")]).await;
        let got =
            f.m.apply_inner(&ctx(), pick(main, feature, wire::ApplyMode::CherryPick))
                .await
                .unwrap();
        assert!(got.conflicts.is_empty());
        let cs = id(&got.result);
        assert_eq!(read(&f, cs, "a").await.unwrap(), b"2\n");
        assert_eq!(read(&f, cs, "b").await.unwrap(), b"2\n");
        let stored = f.entry.backend.engine().get_changeset(cs).await.unwrap();
        assert_eq!(stored.parents, vec![main]);
        assert_eq!(stored.message, b"m");
        let original = f
            .entry
            .backend
            .engine()
            .get_changeset(feature)
            .await
            .unwrap();
        assert_eq!(stored.author, original.author);
        // Ingested with its not-yet-known ancestors (base, main, result), and no ref.
        assert_eq!(graph_rows(&f).await, 3);
        assert_eq!(count(&f, "SELECT COUNT(*) FROM refs").await, 0);
    }

    #[tokio::test]
    async fn revert_undoes_a_change_and_writes_git_style_text() {
        let f = fx().await;
        let base = commit(&f.entry, &[], &[("a", b"1\n")]).await;
        let bad = commit(&f.entry, &[base], &[("a", b"2\n")]).await;
        let later = commit(&f.entry, &[bad], &[("b", b"x\n")]).await;
        let got =
            f.m.apply_inner(&ctx(), pick(later, bad, wire::ApplyMode::Revert))
                .await
                .unwrap();
        let cs = id(&got.result);
        assert_eq!(read(&f, cs, "a").await.unwrap(), b"1\n");
        assert_eq!(read(&f, cs, "b").await.unwrap(), b"x\n");
        let stored = f.entry.backend.engine().get_changeset(cs).await.unwrap();
        assert_eq!(
            String::from_utf8(stored.message.clone()).unwrap(),
            format!("Revert \"m\"\n\nThis reverts commit {bad}.\n")
        );
    }

    #[tokio::test]
    async fn conflicts_are_an_answer_and_make_no_changeset() {
        let f = fx().await;
        let base = commit(&f.entry, &[], &[("a", b"1\n")]).await;
        let mine = commit(&f.entry, &[base], &[("a", b"mine\n")]).await;
        let theirs = commit(&f.entry, &[base], &[("a", b"theirs\n")]).await;
        let got =
            f.m.apply_inner(&ctx(), pick(theirs, mine, wire::ApplyMode::CherryPick))
                .await
                .unwrap();
        assert!(got.result.is_empty());
        assert_eq!(got.conflicts, vec![Bytes::from_static(b"a")]);
        let d = &got.conflict_details[0];
        assert_eq!(d.path.as_ref(), b"a");
        assert_eq!(d.changeset.as_ref(), mine.as_bytes());
        assert_eq!(d.kind, wire::ConflictKind::Content as i32);
        assert!(!d.marked_id.is_empty() && !d.ours_id.is_empty() && !d.theirs_id.is_empty());
        assert_eq!(graph_rows(&f).await, 0);
    }

    #[tokio::test]
    async fn an_empty_result_is_refused() {
        let f = fx().await;
        let base = commit(&f.entry, &[], &[("a", b"1\n")]).await;
        let same = commit(&f.entry, &[base], &[("a", b"2\n")]).await;
        let e =
            f.m.apply_inner(&ctx(), pick(same, same, wire::ApplyMode::CherryPick))
                .await
                .unwrap_err();
        assert_eq!(e.code(), tonic::Code::FailedPrecondition, "{e:?}");
    }

    #[tokio::test]
    async fn a_merge_changeset_needs_a_mainline_and_is_refused() {
        let f = fx().await;
        let a = commit(&f.entry, &[], &[("a", b"1\n")]).await;
        let b = commit(&f.entry, &[a], &[("b", b"1\n")]).await;
        let c = commit(&f.entry, &[a], &[("c", b"1\n")]).await;
        let m = commit(&f.entry, &[b, c], &[("c", b"1\n")]).await;
        let e =
            f.m.apply_inner(&ctx(), pick(a, m, wire::ApplyMode::CherryPick))
                .await
                .unwrap_err();
        assert_eq!(e.code(), tonic::Code::FailedPrecondition, "{e:?}");
    }

    #[tokio::test]
    async fn a_patch_modifies_creates_renames_copies_and_deletes() {
        let f = fx().await;
        let base = lines(&f).await;
        let text = format!(
            "{MODIFY}\
diff --git a/new.txt b/new.txt\nnew file mode 100755\n--- /dev/null\n+++ b/new.txt\n@@ -0,0 +1 @@\n+hello\n\
diff --git a/keep b/moved\nsimilarity index 100%\nrename from keep\nrename to moved\n\
diff --git a/z.txt b/copy.txt\nsimilarity index 100%\ncopy from z.txt\ncopy to copy.txt\n\
diff --git a/z.txt b/z.txt\ndeleted file mode 100644\n--- a/z.txt\n+++ /dev/null\n@@ -1 +0,0 @@\n-z\n"
        );
        let got =
            f.m.apply_inner(&ctx(), patch_req(base, &text))
                .await
                .unwrap();
        assert!(got.conflicts.is_empty(), "{:?}", got.conflict_details);
        let cs = id(&got.result);
        assert_eq!(read(&f, cs, "a.txt").await.unwrap(), b"one\nTWO\nthree\n");
        assert_eq!(read(&f, cs, "new.txt").await.unwrap(), b"hello\n");
        assert_eq!(read(&f, cs, "moved").await.unwrap(), b"k\n");
        assert_eq!(read(&f, cs, "keep").await, None);
        assert_eq!(read(&f, cs, "copy.txt").await.unwrap(), b"z\n");
        assert_eq!(read(&f, cs, "z.txt").await, None);
        let stored = f.entry.backend.engine().get_changeset(cs).await.unwrap();
        assert_eq!(stored.message, b"patched\n");
        assert_eq!(stored.parents, vec![base]);
        let exec = f
            .entry
            .backend
            .engine()
            .lookup(stored.root, &RepoPath::new("new.txt").unwrap())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(exec.kind, Kind::Exec);
        assert_eq!(graph_rows(&f).await, 2);
    }

    #[tokio::test]
    async fn a_hunk_that_does_not_apply_is_a_conflict_and_changes_nothing() {
        let f = fx().await;
        let base = lines(&f).await;
        let stale = MODIFY.replace("-two", "-deux");
        let got =
            f.m.apply_inner(&ctx(), patch_req(base, &stale))
                .await
                .unwrap();
        assert!(got.result.is_empty());
        assert_eq!(got.conflicts, vec![Bytes::from_static(b"a.txt")]);
        assert_eq!(
            got.conflict_details[0].kind,
            wire::ConflictKind::Content as i32
        );
        // A new file over an existing one, and a change to a file that is not there.
        let add = "diff --git a/keep b/keep\nnew file mode 100644\n--- /dev/null\n+++ b/keep\n@@ -0,0 +1 @@\n+x\n";
        let got = f.m.apply_inner(&ctx(), patch_req(base, add)).await.unwrap();
        assert_eq!(
            got.conflict_details[0].kind,
            wire::ConflictKind::AddAdd as i32
        );
        let missing = "diff --git a/nope b/nope\n--- a/nope\n+++ b/nope\n@@ -1 +1 @@\n-a\n+b\n";
        let got =
            f.m.apply_inner(&ctx(), patch_req(base, missing))
                .await
                .unwrap();
        assert_eq!(
            got.conflict_details[0].kind,
            wire::ConflictKind::ModifyDelete as i32
        );
        assert_eq!(graph_rows(&f).await, 0);
    }

    #[tokio::test]
    async fn a_patch_cannot_write_git_aliases_or_escape() {
        let f = fx().await;
        let base = lines(&f).await;
        for path in [
            ".git/hooks/pre-commit",
            ".GIT/config",
            ".git ",
            ".git.",
            "GIT~1/config",
            "../outside",
            "a/../../b",
            ".git::$INDEX_ALLOCATION/x",
        ] {
            let text = format!(
                "diff --git a/{path} b/{path}\nnew file mode 100644\n--- /dev/null\n+++ b/{path}\n@@ -0,0 +1 @@\n+x\n"
            );
            let e =
                f.m.apply_inner(&ctx(), patch_req(base, &text))
                    .await
                    .unwrap_err();
            assert!(
                e.code() == tonic::Code::InvalidArgument || code(&e) == "PATH_POLICY",
                "{path:?}: {e:?}"
            );
        }
        assert_eq!(graph_rows(&f).await, 0);
    }

    #[tokio::test]
    async fn binary_symlink_and_submodule_patches_are_refused() {
        let f = fx().await;
        let base = lines(&f).await;
        let binary = "diff --git a/n.bin b/n.bin
new file mode 100644
index 0000000000000000000000000000000000000000..8352675d67aed6625ece79af41c27fdb4ee2e867
GIT binary patch
literal 3
Kc-muNWC8#H2LJ>B

literal 0
Hc-jL100001

";
        let link = "diff --git a/l b/l\nnew file mode 120000\n--- /dev/null\n+++ b/l\n@@ -0,0 +1 @@\n+../../etc/passwd\n\\ No newline at end of file\n";
        let sub = "diff --git a/s b/s\nnew file mode 160000\n--- /dev/null\n+++ b/s\n@@ -0,0 +1 @@\n+Subproject commit 0123456789012345678901234567890123456789\n";
        for text in [binary, link, sub] {
            let e =
                f.m.apply_inner(&ctx(), patch_req(base, text))
                    .await
                    .unwrap_err();
            assert_eq!(e.code(), tonic::Code::InvalidArgument, "{text}: {e:?}");
        }
        assert_eq!(graph_rows(&f).await, 0);
    }

    #[tokio::test]
    async fn mode_and_size_arguments_are_checked() {
        let f = fx().await;
        let base = lines(&f).await;
        let mut both = patch_req(base, MODIFY);
        both.changeset = base.to_string();
        let mut none = patch_req(base, MODIFY);
        none.patch = Bytes::new();
        // A well-formed patch, one byte over the limit.
        let head = "diff --git a/big b/big
new file mode 100644
--- /dev/null
+++ b/big
@@ -0,0 +1 @@
+";
        let mut huge = patch_req(base, MODIFY);
        huge.patch = Bytes::from(
            [
                head.as_bytes(),
                &vec![b'x'; MAX_PATCH_BYTES - head.len()],
                b"
",
            ]
            .concat(),
        );
        assert_eq!(huge.patch.len(), MAX_PATCH_BYTES + 1);
        let mut pick_with_patch = pick(base, base, wire::ApplyMode::CherryPick);
        pick_with_patch.patch = Bytes::from_static(b"x");
        let mut pick_without = pick(base, base, wire::ApplyMode::CherryPick);
        pick_without.changeset = String::new();
        let mut bad_mode = patch_req(base, MODIFY);
        bad_mode.mode = 77;
        let mut no_message = patch_req(base, MODIFY);
        no_message.message = Bytes::new();
        let mut no_sig = patch_req(base, MODIFY);
        no_sig.committer = None;
        for (name, req) in [
            ("both", both),
            ("no patch", none),
            ("huge", huge),
            ("pick with patch", pick_with_patch),
            ("pick without changeset", pick_without),
            ("bad mode", bad_mode),
            ("no message", no_message),
            ("no signature", no_sig),
        ] {
            let e = f.m.apply_inner(&ctx(), req).await.unwrap_err();
            assert_eq!(e.code(), tonic::Code::InvalidArgument, "{name}: {e:?}");
        }
        assert_eq!(graph_rows(&f).await, 0);
    }

    #[tokio::test]
    async fn whole_file_buffers_go_through_the_text_budget() {
        use std::sync::atomic::{AtomicU64, Ordering};
        struct Count(AtomicU64);
        #[async_trait]
        impl TextBudget for Count {
            async fn acquire(&self, bytes: u64) -> Result<TextGuard, TextBudgetExhausted> {
                self.0.fetch_add(bytes, Ordering::SeqCst);
                Ok(TextGuard::unlimited())
            }
        }
        struct Never;
        #[async_trait]
        impl TextBudget for Never {
            async fn acquire(&self, _: u64) -> Result<TextGuard, TextBudgetExhausted> {
                Err(TextBudgetExhausted)
            }
        }
        let count = Arc::new(Count(AtomicU64::new(0)));
        let f = fx().await;
        f.hooks.text_budget.set(count.clone()).ok().unwrap();
        let base = lines(&f).await;
        f.m.apply_inner(&ctx(), patch_req(base, MODIFY))
            .await
            .unwrap();
        assert!(
            count.0.load(Ordering::SeqCst) >= 14,
            "the patch reserved its file"
        );
        let a = commit(&f.entry, &[base], &[("a.txt", b"two\n")]).await;
        let before = count.0.load(Ordering::SeqCst);
        f.m.apply_inner(&ctx(), pick(base, a, wire::ApplyMode::CherryPick))
            .await
            .unwrap();
        assert_eq!(count.0.load(Ordering::SeqCst) - before, MERGE_TEXT_ESTIMATE);

        let g = fx().await;
        g.hooks.text_budget.set(Arc::new(Never)).ok().unwrap();
        let base = lines(&g).await;
        let e =
            g.m.apply_inner(&ctx(), patch_req(base, MODIFY))
                .await
                .unwrap_err();
        assert_eq!(e.code(), tonic::Code::ResourceExhausted);
        let a = commit(&g.entry, &[base], &[("a.txt", b"two\n")]).await;
        let e =
            g.m.apply_inner(&ctx(), pick(base, a, wire::ApplyMode::CherryPick))
                .await
                .unwrap_err();
        assert_eq!(e.code(), tonic::Code::ResourceExhausted);
        assert_eq!(graph_rows(&g).await, 0);
    }

    /// A changeset whose files are tree entries of the named sizes whose content is then deleted
    /// from the store, so any read of one fails as an internal error.
    async fn unreadable(f: &Fx, files: &[(&str, u64)]) -> ChangesetId {
        use asc_engine::commit::{NewChangeset, PathPolicy};
        let engine = f.entry.backend.engine();
        let mut changes = Vec::new();
        let mut ids = Vec::new();
        for (p, size) in files {
            let content = vec![b'a'; *size as usize];
            let stored = f.m.store_content(&opened(f).await, &content).await.unwrap();
            ids.push(stored.file);
            changes.push((
                RepoPath::new(*p).unwrap(),
                write(Kind::File, &stored, None).unwrap(),
            ));
        }
        changes.sort_by(|a, b| a.0.cmp(&b.0));
        let empty = asc_engine::Engine::<
            asc_store::bucket::BucketObjects,
            asc_store::sqlrefs::SqlRefs,
        >::empty_tree_id();
        let root = engine.apply(empty, &changes).await.unwrap();
        let cs = engine
            .create_changeset(
                NewChangeset {
                    parents: Vec::new(),
                    author: crate::ingest::testutil::sig(),
                    committer: crate::ingest::testutil::sig(),
                    message: b"m".to_vec(),
                    extra: Vec::new(),
                    root,
                    copies: Vec::new(),
                },
                PathPolicy::Native { windows_safe: true },
            )
            .await
            .unwrap();
        for id in ids {
            for key in [ObjKey::File(id), ObjKey::ChunkList(id)] {
                f.entry.objects.delete(&key).await.unwrap();
            }
        }
        cs
    }

    const BIG_MODIFY: &str = "diff --git a/big b/big
--- a/big
+++ b/big
@@ -1 +1 @@
-a
+b
";

    #[tokio::test]
    async fn a_file_above_the_per_file_maximum_is_refused_without_reading_it() {
        let f = fx().await;
        let base = unreadable(&f, &[("big", MAX_CONTENT_BYTES as u64 + 1)]).await;
        let e =
            f.m.apply_inner(&ctx(), patch_req(base, BIG_MODIFY))
                .await
                .unwrap_err();
        assert_eq!(e.code(), tonic::Code::InvalidArgument, "{e:?}");
        assert_eq!(graph_rows(&f).await, 0);
    }

    #[tokio::test]
    async fn the_budget_is_reserved_from_the_entry_size_before_the_read() {
        use std::sync::atomic::{AtomicU64, Ordering};
        struct Cap(u64, AtomicU64);
        #[async_trait]
        impl TextBudget for Cap {
            async fn acquire(&self, bytes: u64) -> Result<TextGuard, TextBudgetExhausted> {
                self.1.store(bytes, Ordering::SeqCst);
                if bytes > self.0 {
                    Err(TextBudgetExhausted)
                } else {
                    Ok(TextGuard::unlimited())
                }
            }
        }
        let f = fx().await;
        let cap = Arc::new(Cap(1000, AtomicU64::new(0)));
        f.hooks.text_budget.set(cap.clone()).ok().unwrap();
        let base = unreadable(&f, &[("big", 4096)]).await;
        let e =
            f.m.apply_inner(&ctx(), patch_req(base, BIG_MODIFY))
                .await
                .unwrap_err();
        // An unreadable blob would be an internal error had the read come first.
        assert_eq!(e.code(), tonic::Code::ResourceExhausted, "{e:?}");
        assert_eq!(cap.1.load(Ordering::SeqCst), 2 * 4096);
        assert_eq!(graph_rows(&f).await, 0);
    }

    #[tokio::test]
    async fn the_add_add_check_reads_no_content() {
        let f = fx().await;
        let base = unreadable(&f, &[("big", MAX_CONTENT_BYTES as u64)]).await;
        let add = "diff --git a/big b/big
new file mode 100644
--- /dev/null
+++ b/big
@@ -0,0 +1 @@
+x
";
        let got = f.m.apply_inner(&ctx(), patch_req(base, add)).await.unwrap();
        assert_eq!(
            got.conflict_details[0].kind,
            wire::ConflictKind::AddAdd as i32
        );
        assert_eq!(graph_rows(&f).await, 0);
    }

    #[tokio::test]
    async fn update_ref_moves_only_a_ref_that_still_names_onto() {
        let f = fx().await;
        let base = lines(&f).await;
        ingest_one(&f, base).await;
        // Create: the ref is absent.
        let mut req = patch_req(base, MODIFY);
        req.update_ref = "refs/heads/topic".into();
        let first = f.m.apply_inner(&ctx(), req.clone()).await.unwrap();
        let c1 = id(&first.result);
        assert_eq!(
            ref_target(&f, "refs/heads/topic").await,
            Some(RefTarget::Changeset(c1))
        );
        assert_eq!(count(&f, "SELECT COUNT(*) FROM ref_log").await, 1);
        // Applying onto `base` again: the ref now names c1, not `base`.
        let mut req2 = patch_req(
            base,
            "diff --git a/z.txt b/z.txt\n--- a/z.txt\n+++ b/z.txt\n@@ -1 +1 @@\n-z\n+Z\n",
        );
        req2.update_ref = "refs/heads/topic".into();
        let e = f.m.apply_inner(&ctx(), req2.clone()).await.unwrap_err();
        assert!(
            matches!(code(&e).as_str(), "REF_CONFLICT" | "NOT_FAST_FORWARD"),
            "{e:?}"
        );
        assert_eq!(
            ref_target(&f, "refs/heads/topic").await,
            Some(RefTarget::Changeset(c1))
        );
        // Onto the current value it advances.
        req2.onto = c1.to_string();
        let second = f.m.apply_inner(&ctx(), req2).await.unwrap();
        assert_eq!(
            ref_target(&f, "refs/heads/topic").await,
            Some(RefTarget::Changeset(id(&second.result)))
        );
    }

    /// In a branchless repository a caller's `refs/heads/feat` is stored as
    /// `refs/users/<owner>/heads/feat`. The compare-and-set must read that stored name, or a
    /// second apply onto the existing branch looks absent and conflicts with itself.
    #[tokio::test]
    async fn apply_onto_an_existing_user_branch_in_a_branchless_repo_advances_it() {
        let f = fx().await;
        make_branchless(&f).await;
        let base = lines(&f).await;
        ingest_one(&f, base).await;
        let (alice, bob) = ("5:alice", "6:bob");
        let (a_ref, b_ref) = ("refs/users/5/heads/feat", "refs/users/6/heads/feat");

        let mut req = patch_req(base, MODIFY);
        req.update_ref = "refs/heads/feat".into();
        let c1 = id(&f.m.apply_inner(&ctx_as(alice), req).await.unwrap().result);
        assert_eq!(ref_target(&f, a_ref).await, Some(RefTarget::Changeset(c1)));
        assert_eq!(ref_target(&f, "refs/heads/feat").await, None);

        // Bob's same-named branch is his own ref and starts absent.
        let mut req = patch_req(base, MODIFY);
        req.update_ref = "refs/heads/feat".into();
        let b1 = id(&f.m.apply_inner(&ctx_as(bob), req).await.unwrap().result);
        assert_eq!(ref_target(&f, b_ref).await, Some(RefTarget::Changeset(b1)));

        // Alice applies onto her branch's value: the ref exists, so the expectation is that value.
        let mut req = patch_req(
            c1,
            "diff --git a/z.txt b/z.txt\n--- a/z.txt\n+++ b/z.txt\n@@ -1 +1 @@\n-z\n+Z\n",
        );
        req.update_ref = "refs/heads/feat".into();
        let c2 = id(&f.m.apply_inner(&ctx_as(alice), req).await.unwrap().result);
        assert_eq!(ref_target(&f, a_ref).await, Some(RefTarget::Changeset(c2)));
        // Bob's ref did not move, and his expectation still reads his own value, not Alice's.
        assert_eq!(ref_target(&f, b_ref).await, Some(RefTarget::Changeset(b1)));
        let mut req = patch_req(
            base,
            "diff --git a/z.txt b/z.txt\n--- a/z.txt\n+++ b/z.txt\n@@ -1 +1 @@\n-z\n+Y\n",
        );
        req.update_ref = "refs/heads/feat".into();
        let e = f.m.apply_inner(&ctx_as(bob), req).await.unwrap_err();
        assert!(
            matches!(code(&e).as_str(), "REF_CONFLICT" | "NOT_FAST_FORWARD"),
            "{e:?}"
        );
        assert_eq!(ref_target(&f, b_ref).await, Some(RefTarget::Changeset(b1)));
        assert_eq!(ref_target(&f, a_ref).await, Some(RefTarget::Changeset(c2)));
    }

    /// Another user's ref is not a base: not an Apply `onto`, not the changeset a pick takes,
    /// not a Commit `parent`, in any spelling. The owner still reaches it.
    #[tokio::test]
    async fn another_users_ref_is_not_a_base() {
        let f = fx().await;
        let base = lines(&f).await;
        ingest_one(&f, base).await;
        set_ref(&f, "refs/users/42/heads/x", base).await;
        let (bob, alice) = ("43:bob", "42:alice");
        let foreign = [
            "refs/users/42/heads/x",
            "users/42/heads/x",
            "users/42/heads/x@{0}",
            "refs/users/42/heads/x@{3}",
            "users/42/heads/x~0",
            "users/42/heads/x^{commit}",
            "master..users/42/heads/x",
            "users/42/heads/x...master",
            "^users/42/heads/x",
        ];
        for spec in foreign {
            let mut onto = patch_req(base, MODIFY);
            onto.onto = spec.into();
            let mut source = pick(base, base, wire::ApplyMode::CherryPick);
            source.changeset = spec.into();
            let commit = wire::CommitRequest {
                repo: repo_ref(),
                parent: spec.into(),
                ops: vec![put("a", b"1")],
                message: Bytes::from_static(b"msg\n"),
                author: Some(sig()),
                committer: Some(sig()),
                ..wire::CommitRequest::default()
            };
            for (what, err) in [
                (
                    "onto",
                    f.m.apply_inner(&ctx_as(bob), onto.clone()).await.err(),
                ),
                (
                    "changeset",
                    f.m.apply_inner(&ctx_as(bob), source.clone()).await.err(),
                ),
                (
                    "parent",
                    f.m.commit_inner(&ctx_as(bob), commit.clone()).await.err(),
                ),
            ] {
                let err = err.unwrap_or_else(|| panic!("bob used {spec} as {what}"));
                assert_eq!(err.code(), tonic::Code::NotFound, "{what} {spec}");
            }
        }
        for spec in ["refs/users/42/heads/x", "users/42/heads/x"] {
            let mut onto = patch_req(base, MODIFY);
            onto.onto = spec.into();
            let got = f.m.apply_inner(&ctx_as(alice), onto).await.unwrap();
            assert!(!got.result.is_empty(), "the owner applies onto {spec}");
        }
    }

    #[tokio::test]
    async fn a_mirror_takes_no_apply() {
        let f = fx().await;
        let base = lines(&f).await;
        make_mirror(&f).await;
        let e =
            f.m.apply_inner(&ctx(), patch_req(base, MODIFY))
                .await
                .unwrap_err();
        assert_eq!(code(&e), "MIRROR_MODE");
    }
}
