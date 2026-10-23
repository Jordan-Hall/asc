//! `CopyClosure` and `RepoAdmin` (PLAN U48c, SPEC §3.3, §8.7).
//!
//! * `CopyClosure` copies the content closure of changesets from one namespace to another and
//!   then runs the destination's own ingest ([`Ingestor`]) over them, so nothing the source says
//!   is trusted: every object is re-hashed on write, and the destination re-validates each new
//!   changeset (SPEC §8.4). Only changesets the source has in its graph can be named, which
//!   keeps the call from reading objects a writer merely uploaded. The service has already
//!   checked write access on the destination and read access on the source.
//! * `RepoAdmin` has six operations. `init` creates a repository (`default_branch` defaults to
//!   `master`, `land_branches` to `[default_branch]`). `fork` creates a new namespace that is
//!   not ready, copies the closure of the source's heads and tags in a background job, and only
//!   then makes the repository visible ([`Registry::open`] finds a repository only once its
//!   `repos.ready` flag is set). `delete` drops the row and refs at once and the namespace after
//!   [`DROP_DELAY`]. `rename` changes the registry name. `size` and `verify` read the repository.
//!
//! Derived index data (`d/…` objects, `ci_*` rows) is never copied (SPEC §3.3); a fork backfills
//! its own. Everything here is bounded by a named limit and fails closed: an unknown operation,
//! a `false` flag in place of an operation, an object that is missing or fails its hash, or a
//! walk past [`MAX_WALK_OBJECTS`] is an error, never a partial success.

use std::collections::HashSet;
use std::sync::{Arc, Mutex, PoisonError, Weak};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use asc_core::codec::decode_envelope;
use asc_core::git::EXTRA_GIT_COMMIT;
use asc_core::id::{ChangesetId, FileId, GitOid, RefTarget, TreeId};
use asc_core::object::{Changeset, ChunkList, Kind, TagRecord, Tree};
use asc_core::path::RefName;
use asc_rpc::errors::RpcError;
use asc_rpc::server::{AdminHandler, CallCtx, MAX_LIST, MAX_STRING, RepoName, wire};
use asc_rpc::{IdMode, WireId};
use asc_store::key::ObjKey;
use asc_store::objects::{self, Objects};
use asc_store::refs::{Actor, Expect, RefEntry, RefTxn, RefUpdate, Refs};
use futures::{StreamExt, TryStreamExt, stream};
use object_store::path::Path;
use object_store::{ObjectStore, ObjectStoreExt};
use sqlx::any::Any;
use sqlx::{AnyPool, Row, Transaction};
use tokio::task::JoinHandle;
use tonic::Status;
use uuid::Uuid;

use crate::graph_sql;
use crate::ingest::Ingestor;
use crate::mirror::{Mirror, MirrorError, MirrorTarget};
use crate::registry::{Extensions, Ns, Registry, RegistryError, RepoEntry, RepoMode};
use crate::update::{PipelineHooks, ServerRefs, info, ingest_status, internal, rpc};

pub mod branchless;

/// Most changesets one `CopyClosure` may name.
pub const MAX_COPY_CHANGESETS: usize = MAX_LIST;
/// Most distinct objects one walk keeps track of. A walk past this fails instead of exhausting
/// memory: a bigger repository needs the operator's tools, not an RPC.
pub const MAX_WALK_OBJECTS: usize = 8_000_000;
/// Most land branches a repository may list.
pub const MAX_LAND_BRANCHES: usize = 64;
/// Most refs a fork, a size or a verify reads.
pub const MAX_ADMIN_REFS: usize = 100_000;
/// Problems a `verify` report lists by name; the rest are counted.
pub const MAX_REPORTED_ISSUES: usize = 20;
/// How long a deleted repository's namespace is kept before it is dropped (SPEC §8.7).
pub const DROP_DELAY: Duration = Duration::from_secs(30 * 24 * 3600);
/// How long a replica owns a fork copy without renewing it.
pub const FORK_LEASE: Duration = Duration::from_secs(300);
/// How long a failed (not permanently) fork waits before another try.
const FORK_RETRY: Duration = Duration::from_secs(60);
/// The default branch when `InitOp.default_branch` is empty (SPEC §2.9).
pub const DEFAULT_BRANCH: &str = "master";
/// Time between passes of the background worker.
const SWEEP_EVERY: Duration = Duration::from_secs(60);
/// Deleted namespaces one pass drops at most.
const DROPS_PER_PASS: i64 = 8;
/// Forks one pass resumes at most.
const FORKS_PER_PASS: i64 = 16;
/// Objects per batch handed from the walk to the copy.
const COPY_BATCH: usize = 256;
/// Objects copied at once within a batch.
const COPY_CONCURRENCY: usize = 16;
/// Ids per `IN (...)` list and rows per page.
const ROW_BATCH: usize = 500;
/// Rows per page when a whole index is copied.
const INDEX_PAGE: i64 = 5000;
/// Files above this size are stored as a chunk list (SPEC §3.1).
const CHUNK_THRESHOLD: u64 = 4 * 1024 * 1024;
/// Tables keyed by a namespace; the drop of a deleted repository clears each. A test checks that
/// every table with an `ns` column is named here.
const NS_TABLES: [&str; 16] = [
    "cs_graph",
    "git_map",
    "file_meta",
    "mainline",
    "path_changes",
    "gc_marks",
    "ci_docs",
    "ci_doc_langs",
    "ci_chunks",
    "ci_manifest",
    "ci_jobs",
    "land_map",
    "land_map_ref",
    "changes",
    "change_versions",
    "review_state",
];

// ------------------------------------------------------------------------------ statuses

fn invalid(m: impl Into<String>) -> Status {
    Status::invalid_argument(m.into())
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

fn secs(d: Duration) -> i64 {
    i64::try_from(d.as_secs()).unwrap_or(i64::MAX)
}

fn sql(context: &str, e: sqlx::Error) -> Status {
    internal(context, &e)
}

fn registry_status(e: RegistryError) -> Status {
    match e {
        RegistryError::NotFound(_) => Status::not_found("no such repository"),
        RegistryError::Exists(_) => Status::already_exists("repository already exists"),
        RegistryError::Invalid(m) => invalid(m),
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

fn bucket_status(e: object_store::Error) -> Status {
    match e {
        object_store::Error::NotFound { .. } => Status::data_loss("a stored object is missing"),
        e => internal("bucket", &e),
    }
}

fn graph_status(e: asc_engine::graph::Error) -> Status {
    internal("graph", &e)
}

fn missing_object(key: &ObjKey) -> Status {
    tracing::error!(key = %key.path(), "object missing from the source namespace");
    Status::data_loss("an object is missing from the source repository")
}

// ------------------------------------------------------------------------------ the walk

/// What the walk does with a source object that is missing or fails its hash.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Damage {
    /// Stop with `DATA_LOSS`.
    Fail,
    /// Count it, name the first few, and go on without its children.
    Collect,
}

/// Problems a [`Damage::Collect`] walk met.
#[derive(Default)]
struct Found {
    missing: u64,
    corrupt: u64,
    named: Vec<String>,
}

/// What a destination already holds; the walk does not descend into it.
#[derive(Clone, Copy)]
struct Have<'a> {
    objects: &'a dyn Objects,
    pool: &'a AnyPool,
    ns: Ns,
}

/// The objects an object refers to.
struct Kids {
    required: Vec<ObjKey>,
    /// Present only when the git id of a directory is not its canonical render.
    optional: Vec<ObjKey>,
}

fn file_key(id: FileId, size: u64) -> ObjKey {
    if size > CHUNK_THRESHOLD {
        ObjKey::ChunkList(id)
    } else {
        ObjKey::File(id)
    }
}

/// The `tree <hex>` oid of a raw git commit kept in `git.commit`.
fn raw_commit_tree(raw: &[u8]) -> Option<GitOid> {
    let line = raw.strip_prefix(b"tree ")?;
    let hex = line.get(..40)?;
    let nibble = |c: u8| match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        _ => None,
    };
    let mut bytes = [0u8; 20];
    for (out, pair) in bytes.iter_mut().zip(hex.chunks_exact(2)) {
        *out = nibble(pair[0])? << 4 | nibble(pair[1])?;
    }
    Some(GitOid::from_bytes(bytes))
}

/// Decodes `data` (the verified bytes of `key`) and names what it refers to. Mirrors the
/// reachability rules of the collector (`asc_store::gc`), so a copy holds what a mark keeps.
fn children(key: &ObjKey, data: &[u8]) -> Result<Kids, String> {
    let bad = |e: &dyn std::fmt::Display| e.to_string();
    let mut kids = Kids {
        required: Vec::new(),
        optional: Vec::new(),
    };
    match key {
        ObjKey::Changeset(_) => {
            let cs: Changeset = decode_envelope(data).map_err(|e| bad(&e))?;
            kids.required
                .extend(cs.parents.iter().map(|&p| ObjKey::Changeset(p)));
            kids.required.push(ObjKey::Tree(cs.root));
            if let Some(raw) = cs.extra.get(&EXTRA_GIT_COMMIT.to_vec())
                && let Some(oid) = raw_commit_tree(raw)
            {
                kids.optional.push(ObjKey::GitTreeOverride(oid));
            }
        }
        ObjKey::Tree(_) => {
            let tree: Tree = decode_envelope(data).map_err(|e| bad(&e))?;
            for e in tree.entries() {
                match e.kind {
                    Kind::Dir => {
                        kids.required.push(ObjKey::Tree(TreeId::from_bytes(e.id)));
                        kids.optional.push(ObjKey::GitTreeOverride(e.git));
                    }
                    Kind::File | Kind::Exec | Kind::Symlink => {
                        kids.required
                            .push(file_key(FileId::from_bytes(e.id), e.size));
                    }
                    Kind::Gitlink => {}
                }
            }
        }
        ObjKey::Tag(_) => {
            let tag: TagRecord = decode_envelope(data).map_err(|e| bad(&e))?;
            kids.required.push(ObjKey::Changeset(tag.target));
        }
        ObjKey::ChunkList(_) => {
            let list: ChunkList = decode_envelope(data).map_err(|e| bad(&e))?;
            kids.required
                .extend(list.chunks.iter().map(|&(c, _)| ObjKey::Chunk(c)));
        }
        _ => {}
    }
    Ok(kids)
}

/// Whether reading `key` can name further objects.
fn has_children(key: &ObjKey) -> bool {
    matches!(
        key,
        ObjKey::Changeset(_) | ObjKey::Tree(_) | ObjKey::Tag(_) | ObjKey::ChunkList(_)
    )
}

struct Frame {
    key: ObjKey,
    kids: std::vec::IntoIter<ObjKey>,
}

/// A depth-first walk over a closure that yields objects children first, in batches, so that a
/// copy which writes each batch in order never leaves an object whose children are absent.
/// Memory is the set of objects seen (at most [`MAX_WALK_OBJECTS`]) plus the current path.
struct Walk<'a> {
    src: &'a dyn Objects,
    have: Option<Have<'a>>,
    damage: Damage,
    /// Entered, or skipped because the destination has them.
    seen: HashSet<ObjKey>,
    stack: Vec<Frame>,
    roots: Vec<ObjKey>,
    found: Found,
    /// Most objects the walk may see; [`MAX_WALK_OBJECTS`] outside the tests.
    cap: usize,
}

impl<'a> Walk<'a> {
    fn new(src: &'a dyn Objects, have: Option<Have<'a>>, damage: Damage) -> Self {
        Self {
            src,
            have,
            damage,
            seen: HashSet::new(),
            stack: Vec::new(),
            roots: Vec::new(),
            found: Found::default(),
            cap: MAX_WALK_OBJECTS,
        }
    }

    /// Queues `roots`; those the destination has are dropped.
    async fn start(&mut self, roots: Vec<ObjKey>) -> Result<(), Status> {
        let mut fresh = self.filter(roots).await?;
        fresh.reverse();
        self.roots = fresh;
        Ok(())
    }

    /// Every key seen, once the walk is done.
    fn into_seen(self) -> HashSet<ObjKey> {
        self.seen
    }

    /// Drops what was seen and what the destination holds; the destination's skips are
    /// remembered so they are asked about once.
    async fn filter(&mut self, cands: Vec<ObjKey>) -> Result<Vec<ObjKey>, Status> {
        let cands: Vec<ObjKey> = cands
            .into_iter()
            .filter(|k| !self.seen.contains(k))
            .collect();
        let Some(have) = self.have else {
            return Ok(cands);
        };
        let (changesets, rest): (Vec<ObjKey>, Vec<ObjKey>) = cands
            .into_iter()
            .partition(|k| matches!(k, ObjKey::Changeset(_)));
        let mut fresh = Vec::new();
        if !changesets.is_empty() {
            let ids: Vec<ChangesetId> = changesets
                .iter()
                .filter_map(|k| match k {
                    ObjKey::Changeset(c) => Some(*c),
                    _ => None,
                })
                .collect();
            // In the destination's graph means ingested, so its whole closure is there.
            let known = graph_sql::known(have.pool, have.ns, &ids)
                .await
                .map_err(graph_status)?;
            for k in changesets {
                let ObjKey::Changeset(c) = k else { continue };
                if known.contains_key(&c) {
                    self.seen.insert(k);
                } else {
                    fresh.push(k);
                }
            }
        }
        if !rest.is_empty() {
            let present = have.objects.has_many(&rest).await.map_err(store_status)?;
            for (k, there) in rest.into_iter().zip(present) {
                if there {
                    self.seen.insert(k);
                } else {
                    fresh.push(k);
                }
            }
        }
        Ok(fresh)
    }

    fn damaged(&mut self, key: ObjKey, missing: bool) -> Result<(), Status> {
        match self.damage {
            Damage::Fail if missing => Err(missing_object(&key)),
            Damage::Fail => {
                tracing::error!(key = %key.path(), "source object is corrupt");
                Err(Status::data_loss(
                    "an object in the source repository is corrupt",
                ))
            }
            Damage::Collect => {
                if missing {
                    self.found.missing += 1;
                } else {
                    self.found.corrupt += 1;
                }
                if self.found.named.len() < MAX_REPORTED_ISSUES {
                    let what = if missing { "missing" } else { "corrupt" };
                    self.found.named.push(format!("{what}: {}", key.path()));
                }
                Ok(())
            }
        }
    }

    /// What `key` refers to, that the destination does not have and the walk has not entered.
    async fn kids_of(&mut self, key: ObjKey) -> Result<Vec<ObjKey>, Status> {
        let data = match self.src.get(&key).await {
            Ok(Some(d)) => d,
            Ok(None) => {
                self.damaged(key, true)?;
                return Ok(Vec::new());
            }
            Err(objects::Error::Corrupt(_) | objects::Error::Codec(..)) => {
                self.damaged(key, false)?;
                return Ok(Vec::new());
            }
            Err(e) => return Err(store_status(e)),
        };
        let kids = match children(&key, &data) {
            Ok(k) => k,
            Err(why) => {
                tracing::error!(key = %key.path(), %why, "source object does not decode");
                self.damaged(key, false)?;
                return Ok(Vec::new());
            }
        };
        let mut all = kids.required;
        if !kids.optional.is_empty() {
            let there = self
                .src
                .has_many(&kids.optional)
                .await
                .map_err(store_status)?;
            all.extend(
                kids.optional
                    .into_iter()
                    .zip(there)
                    .filter(|(_, t)| *t)
                    .map(|(k, _)| k),
            );
        }
        self.filter(all).await
    }

    /// Enters `key`: a leaf is yielded at once, anything else after its children.
    async fn enter(&mut self, key: ObjKey, out: &mut Vec<ObjKey>) -> Result<(), Status> {
        if !self.seen.insert(key) {
            return Ok(());
        }
        if self.seen.len() > self.cap {
            return Err(Status::resource_exhausted(format!(
                "the closure has more than {} objects",
                self.cap
            )));
        }
        let kids = if has_children(&key) {
            self.kids_of(key).await?
        } else {
            Vec::new()
        };
        if kids.is_empty() {
            out.push(key);
        } else {
            self.stack.push(Frame {
                key,
                kids: kids.into_iter(),
            });
        }
        Ok(())
    }

    /// The next batch of at most [`COPY_BATCH`] objects, children before parents; `None` at the
    /// end.
    async fn next_batch(&mut self) -> Result<Option<Vec<ObjKey>>, Status> {
        let mut out = Vec::new();
        loop {
            if out.len() >= COPY_BATCH {
                return Ok(Some(out));
            }
            if let Some(top) = self.stack.last_mut() {
                match top.kids.next() {
                    Some(kid) => self.enter(kid, &mut out).await?,
                    None => {
                        if let Some(done) = self.stack.pop() {
                            out.push(done.key);
                        }
                    }
                }
            } else if let Some(root) = self.roots.pop() {
                self.enter(root, &mut out).await?;
            } else {
                return Ok((!out.is_empty()).then_some(out));
            }
        }
    }
}

// ------------------------------------------------------------------------------ copying

fn tier(key: &ObjKey) -> u8 {
    match key {
        ObjKey::Chunk(_) | ObjKey::File(_) | ObjKey::GitTreeOverride(_) => 0,
        ObjKey::ChunkList(_) => 1,
        ObjKey::Tree(_) => 2,
        _ => 3,
    }
}

async fn copy_one(src: &dyn Objects, dst: &dyn Objects, key: ObjKey) -> Result<(), Status> {
    let stored = src
        .get_stored(&key)
        .await
        .map_err(store_status)?
        .ok_or_else(|| missing_object(&key))?;
    // `put` re-hashes the value against its key: a source that lies cannot plant an object.
    dst.put(&key, stored).await.map_err(store_status)
}

/// Writes `keys` (children before parents) to `dst`. Leaves go first and concurrently, then
/// chunk lists, then trees and changesets in order, so a stored object always has its children
/// stored.
async fn copy_batch(src: &dyn Objects, dst: &dyn Objects, keys: &[ObjKey]) -> Result<u64, Status> {
    for t in 0..4u8 {
        let group: Vec<ObjKey> = keys.iter().filter(|k| tier(k) == t).copied().collect();
        if t < 2 {
            stream::iter(group)
                .map(|k| copy_one(src, dst, k))
                .buffer_unordered(COPY_CONCURRENCY)
                .try_collect::<Vec<()>>()
                .await?;
        } else {
            for k in group {
                copy_one(src, dst, k).await?;
            }
        }
    }
    Ok(keys.len() as u64)
}

/// `CopyClosure`: copies the closure of `req.changesets` from `req.from` to the call's
/// repository, then ingests them there.
pub(crate) async fn copy_closure(
    registry: &Arc<Registry>,
    pipeline: &Arc<PipelineHooks>,
    ctx: &CallCtx,
    req: wire::CopyClosureRequest,
) -> Result<wire::CopyClosureResponse, Status> {
    if ctx.id_mode != IdMode::Asc {
        return Err(Status::unimplemented(
            "git id mode is not served by this mutation",
        ));
    }
    let (Some(from), Some(to)) = (req.from.as_ref(), req.to.as_ref()) else {
        return Err(invalid("missing repository"));
    };
    if to.name != ctx.repo.as_str() {
        return Err(invalid("`to` is not the repository of the call"));
    }
    if IdMode::from_wire(from.id_mode).ok() != Some(IdMode::Asc) {
        return Err(Status::unimplemented(
            "git id mode is not served by this mutation",
        ));
    }
    let from = RepoName::parse(&from.name)?;
    if from == ctx.repo {
        return Err(invalid("the source is the destination"));
    }
    if req.changesets.is_empty() || req.changesets.len() > MAX_COPY_CHANGESETS {
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
    let dst = registry.open(&ctx.repo).await.map_err(registry_status)?;
    if dst.mode == RepoMode::Mirror {
        return Err(rpc(RpcError::MirrorMode(info(vec![ctx.repo.to_string()]))));
    }
    let src = registry.open(&from).await.map_err(registry_status)?;
    // Only changesets the source has ingested: their closure is complete and was validated.
    let known = graph_sql::known(registry.pool(), src.ns, &ids)
        .await
        .map_err(graph_status)?;
    if let Some(unknown) = ids.iter().find(|id| !known.contains_key(id)) {
        return Err(Status::not_found(format!(
            "changeset {unknown} is not in the source repository"
        )));
    }
    let refs = ServerRefs::new(registry.clone(), pipeline.clone());
    let windows_safe = refs.windows_safe(&dst).await?;
    let _gc = pipeline.gc().shared().await.map_err(|e| {
        tracing::warn!(error = %e, "gc fence unavailable");
        Status::unavailable("garbage-collection fence unavailable; retry")
    })?;
    let have = Have {
        objects: &*dst.objects,
        pool: registry.pool(),
        ns: dst.ns,
    };
    let mut walk = Walk::new(&*src.objects, Some(have), Damage::Fail);
    walk.start(ids.iter().map(|&c| ObjKey::Changeset(c)).collect())
        .await?;
    let mut copied = 0;
    while let Some(batch) = walk.next_batch().await? {
        copied += copy_batch(&*src.objects, &*dst.objects, &batch).await?;
    }
    // The destination's own validation, and its graph, git map and stack-metadata rows.
    let targets: Vec<RefTarget> = ids.iter().map(|&c| RefTarget::Changeset(c)).collect();
    Ingestor::new(registry.pool(), registry.refs(), &dst, windows_safe)
        .ingest(&targets, pipeline.observer())
        .await
        .map_err(ingest_status)?;
    Ok(wire::CopyClosureResponse { copied })
}

// ------------------------------------------------------------------------------ names

/// `refs/heads/<name>` must be a valid ref; `HEAD`, a leading `-` and an over-long name are not
/// branch names.
fn branch_ok(name: &str) -> bool {
    name.len() <= MAX_STRING
        && name != "HEAD"
        && !name.starts_with('-')
        && RefName::new(format!("refs/heads/{name}").into_bytes()).is_ok()
}

/// The land branches of a new repository: `[default_branch]` when none are given. A short name
/// or a full `refs/heads/…` ref.
fn land_branches(given: &[String], default_branch: &str) -> Result<Vec<String>, Status> {
    if given.len() > MAX_LAND_BRANCHES {
        return Err(invalid(format!(
            "more than {MAX_LAND_BRANCHES} land branches"
        )));
    }
    if given.is_empty() {
        return Ok(vec![default_branch.to_owned()]);
    }
    let mut out: Vec<String> = Vec::with_capacity(given.len());
    for b in given {
        let short = b.strip_prefix("refs/heads/").unwrap_or(b);
        if (b.starts_with("refs/") && short == b) || !branch_ok(short) {
            return Err(invalid("land branch name"));
        }
        if !out.contains(b) {
            out.push(b.clone());
        }
    }
    Ok(out)
}

// ------------------------------------------------------------------------------ the service

/// The `admin` handler group: `RepoAdmin`.
pub struct AdminService {
    registry: Arc<Registry>,
    mirror: Option<Arc<Mirror>>,
    /// Fork copies running in this process.
    forks: Mutex<HashSet<i64>>,
    me: Weak<AdminService>,
}

/// The `repos` columns a fork copies from its source.
struct SourceRow {
    id: i64,
    default_branch: String,
    windows_safe: bool,
    land_branches: String,
}

impl AdminService {
    pub fn new(registry: Arc<Registry>, mirror: Option<Arc<Mirror>>) -> Arc<Self> {
        Arc::new_cyclic(|me| Self {
            registry,
            mirror,
            forks: Mutex::new(HashSet::new()),
            me: me.clone(),
        })
    }

    fn pool(&self) -> &AnyPool {
        self.registry.pool()
    }

    // -------------------------------------------------------------------------- init

    async fn init(
        &self,
        ctx: &CallCtx,
        op: wire::InitOp,
    ) -> Result<wire::RepoAdminResponse, Status> {
        let default_branch = if op.default_branch.is_empty() {
            DEFAULT_BRANCH.to_owned()
        } else {
            op.default_branch
        };
        if !branch_ok(&default_branch) {
            return Err(invalid("default branch name"));
        }
        let land = land_branches(&op.land_branches, &default_branch)?;
        let mode = match op.mode.as_str() {
            "" | "primary" => RepoMode::Primary,
            "mirror" => RepoMode::Mirror,
            _ => return Err(invalid("mode is `primary` or `mirror`")),
        };
        if op.mirror_url.len() > MAX_STRING {
            return Err(invalid("mirror url is too long"));
        }
        let mirror_url = (!op.mirror_url.is_empty()).then_some(op.mirror_url);
        let mirror = match (mode, &mirror_url) {
            (RepoMode::Primary, Some(_)) => {
                return Err(invalid("a mirror url needs mode `mirror`"));
            }
            (RepoMode::Primary, None) => None,
            (RepoMode::Mirror, url) => {
                let Some(mirror) = &self.mirror else {
                    return Err(Status::failed_precondition(
                        "mirror mode is not enabled on this server",
                    ));
                };
                // A client may name only a host the operator allows (SPEC §8.6). The check runs
                // before anything is created.
                if let Some(url) = url {
                    mirror
                        .sources()
                        .check_client_url(url)
                        .map_err(mirror_status)?;
                }
                Some(mirror)
            }
        };
        let land_json = serde_json::to_string(&land).map_err(|e| internal("land branches", &e))?;
        let mut tx = self
            .registry
            .refs()
            .begin()
            .await
            .map_err(|e| internal("refs", &e))?;
        refuse_leftover_access(&mut tx, ctx.repo.as_str()).await?;
        let inserted = sqlx::query(
            "INSERT INTO repos (name, ns, mode, default_branch, windows_safe, land_branches, created) \
             VALUES ($1, $2, $3, $4, $5, $6, $7)",
        )
        .bind(ctx.repo.as_str())
        .bind(Ns::random().as_bytes().to_vec())
        .bind(mode.as_str())
        .bind(&default_branch)
        .bind(op.windows_safe)
        .bind(&land_json)
        .bind(unix_now())
        .execute(&mut *tx)
        .await;
        match inserted {
            Ok(_) => {}
            Err(sqlx::Error::Database(d)) if d.is_unique_violation() => {
                return Err(Status::already_exists("repository already exists"));
            }
            Err(e) => return Err(sql("repos", e)),
        }
        tx.commit().await.map_err(|e| sql("repos", e))?;
        self.registry.forget(&ctx.repo);
        // A target left under this name by an earlier repository is not this one's. Cleared
        // only now: an init that found the name taken must not touch the live repository's.
        if let Some(mirror) = &self.mirror {
            mirror.sources().remove(&ctx.repo);
        }
        if let (Some(mirror), Some(url)) = (mirror, mirror_url) {
            mirror
                .sources()
                .set(&ctx.repo, MirrorTarget { url, token: None })
                .map_err(mirror_status)?;
        }
        Ok(wire::RepoAdminResponse {
            report: format!(
                "created {} ({}, default branch {default_branch})",
                ctx.repo,
                mode.as_str()
            ),
            ..Default::default()
        })
    }

    // -------------------------------------------------------------------------- fork

    async fn source_row(&self, name: &RepoName) -> Result<SourceRow, Status> {
        // Opening finds only a ready repository.
        let src = self.registry.open(name).await.map_err(registry_status)?;
        let row = sqlx::query(
            "SELECT default_branch, land_branches, \
             CAST(CASE WHEN windows_safe THEN 1 ELSE 0 END AS BIGINT) AS ws \
             FROM repos WHERE id = $1",
        )
        .bind(src.key.0)
        .fetch_optional(self.pool())
        .await
        .map_err(|e| sql("repos", e))?
        .ok_or_else(|| Status::not_found("no such repository"))?;
        Ok(SourceRow {
            id: src.key.0,
            default_branch: row.try_get("default_branch").map_err(|e| sql("repos", e))?,
            windows_safe: row.try_get::<i64, _>("ws").map_err(|e| sql("repos", e))? != 0,
            land_branches: row.try_get("land_branches").map_err(|e| sql("repos", e))?,
        })
    }

    /// Creates the fork's row, not ready, with the copy leased to the caller. Returns the new
    /// repository's id and the lease.
    async fn begin_fork(&self, name: &RepoName, from: &RepoName) -> Result<(i64, i64), Status> {
        if name == from {
            return Err(invalid("a repository cannot fork itself"));
        }
        let src = self.source_row(from).await?;
        let lease = unix_now().saturating_add(secs(FORK_LEASE));
        let mut tx = self
            .registry
            .refs()
            .begin()
            .await
            .map_err(|e| internal("refs", &e))?;
        refuse_leftover_access(&mut tx, name.as_str()).await?;
        let inserted = sqlx::query(
            "INSERT INTO repos (name, ns, mode, default_branch, windows_safe, land_branches, \
             created, ready, fork_from, fork_lease) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)",
        )
        .bind(name.as_str())
        .bind(Ns::random().as_bytes().to_vec())
        .bind(RepoMode::Primary.as_str())
        .bind(&src.default_branch)
        .bind(src.windows_safe)
        .bind(&src.land_branches)
        .bind(unix_now())
        .bind(false)
        .bind(src.id)
        .bind(lease)
        .execute(&mut *tx)
        .await;
        match inserted {
            Ok(_) => {}
            Err(sqlx::Error::Database(d)) if d.is_unique_violation() => {
                return Err(Status::already_exists("repository already exists"));
            }
            Err(e) => return Err(sql("repos", e)),
        }
        let id: i64 = sqlx::query_scalar("SELECT id FROM repos WHERE name = $1")
            .bind(name.as_str())
            .fetch_one(&mut *tx)
            .await
            .map_err(|e| sql("repos", e))?;
        tx.commit().await.map_err(|e| sql("repos", e))?;
        self.registry.forget(name);
        if let Some(mirror) = &self.mirror {
            mirror.sources().remove(name);
        }
        Ok((id, lease))
    }

    /// Runs the copy of fork `id` in the background.
    fn spawn_fork(&self, id: i64, lease: i64, actor: String) {
        let Some(me) = self.me.upgrade() else { return };
        {
            let mut running = self.forks.lock().unwrap_or_else(PoisonError::into_inner);
            if !running.insert(id) {
                return;
            }
        }
        tokio::spawn(async move {
            let result = me.run_fork(id, lease, &actor).await;
            me.forks
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .remove(&id);
            if let Err(status) = result {
                me.fork_failed(id, &status).await;
            }
        });
    }

    /// Records why fork `id` stopped. A failure that retrying cannot fix is kept as the fork's
    /// error; any other lets the lease run out so that a later pass tries again.
    async fn fork_failed(&self, id: i64, status: &Status) {
        use tonic::Code::{
            DataLoss, FailedPrecondition, InvalidArgument, NotFound, ResourceExhausted,
        };
        tracing::warn!(repo = id, code = ?status.code(), "fork copy stopped");
        let permanent = matches!(
            status.code(),
            DataLoss | FailedPrecondition | InvalidArgument | NotFound | ResourceExhausted
        );
        let done = if permanent {
            sqlx::query(
                "UPDATE repos SET fork_error = $1, fork_lease = NULL WHERE id = $2 AND ready = $3",
            )
            .bind(status.message().chars().take(256).collect::<String>())
            .bind(id)
            .bind(false)
            .execute(self.pool())
            .await
        } else {
            sqlx::query("UPDATE repos SET fork_lease = $1 WHERE id = $2 AND ready = $3")
                .bind(unix_now().saturating_add(secs(FORK_RETRY)))
                .bind(id)
                .bind(false)
                .execute(self.pool())
                .await
        };
        if let Err(e) = done {
            tracing::error!(error = %e, "cannot record a fork failure");
        }
    }

    /// Renews the lease on fork `id`; fails when the fork was deleted, finished or taken over.
    async fn renew(&self, id: i64, held: i64) -> Result<i64, Status> {
        let next = unix_now().saturating_add(secs(FORK_LEASE));
        let done = sqlx::query(
            "UPDATE repos SET fork_lease = $1 WHERE id = $2 AND ready = $3 AND fork_lease = $4",
        )
        .bind(next)
        .bind(id)
        .bind(false)
        .bind(held)
        .execute(self.pool())
        .await
        .map_err(|e| sql("repos", e))?;
        if done.rows_affected() == 1 {
            Ok(next)
        } else {
            Err(Status::aborted("the fork was deleted or taken over"))
        }
    }

    /// The copy job of fork `id`, holding `lease`.
    async fn run_fork(&self, id: i64, mut lease: i64, actor: &str) -> Result<(), Status> {
        let row = sqlx::query("SELECT name, fork_from FROM repos WHERE id = $1 AND ready = $2")
            .bind(id)
            .bind(false)
            .fetch_optional(self.pool())
            .await
            .map_err(|e| sql("repos", e))?;
        let Some(row) = row else { return Ok(()) };
        let name: String = row.try_get("name").map_err(|e| sql("repos", e))?;
        let from: Option<i64> = row.try_get("fork_from").map_err(|e| sql("repos", e))?;
        let name = RepoName::parse(&name)?;
        let Some(from) = from else { return Ok(()) };
        let src_name: String = sqlx::query_scalar("SELECT name FROM repos WHERE id = $1")
            .bind(from)
            .fetch_optional(self.pool())
            .await
            .map_err(|e| sql("repos", e))?
            .ok_or_else(|| Status::not_found("the source repository no longer exists"))?;
        let src = self
            .registry
            .open(&RepoName::parse(&src_name)?)
            .await
            .map_err(registry_status)?;
        let (dst, _) = self
            .registry
            .open_unready(&name)
            .await
            .map_err(registry_status)?;

        // The closure of the shared refs: heads and tags. User refs, pull refs and snapshots
        // are private to the source.
        let mut tips: Vec<RefEntry> = Vec::new();
        for prefix in ["refs/heads/", "refs/tags/"] {
            tips.extend(
                self.registry
                    .refs()
                    .list(src.key, prefix)
                    .await
                    .map_err(|e| internal("refs", &e))?
                    .into_iter()
                    .filter(|r| r.expires_at.is_none()),
            );
        }
        if tips.len() > MAX_ADMIN_REFS {
            return Err(Status::resource_exhausted(format!(
                "more than {MAX_ADMIN_REFS} refs"
            )));
        }
        let roots: Vec<ObjKey> = tips
            .iter()
            .map(|r| match r.target {
                RefTarget::Changeset(c) => ObjKey::Changeset(c),
                RefTarget::Tag(t) => ObjKey::Tag(t),
            })
            .collect();
        let (store, src_prefix) = self.registry.namespace(src.ns);
        let (_, dst_prefix) = self.registry.namespace(dst.ns);
        let mut walk = Walk::new(&*src.objects, None, Damage::Fail);
        walk.start(roots).await?;
        let mut copied = 0u64;
        while let Some(batch) = walk.next_batch().await? {
            lease = self.renew(id, lease).await?;
            server_copy(&*store, &src_prefix, &dst_prefix, &batch).await?;
            copy_rows(&self.registry, src.ns, &dst, &batch).await?;
            copied += batch.len() as u64;
        }
        for r in &tips {
            copy_index(self.pool(), src.ns, dst.ns, &r.name.to_string()).await?;
        }
        lease = self.renew(id, lease).await?;

        // The refs, then the ready flag. The fork is invisible until the flag is set, and a
        // rerun finds its refs already in place (`Expect::Any`).
        let txn = RefTxn {
            updates: tips
                .iter()
                .map(|r| RefUpdate {
                    name: r.name.clone(),
                    expect: Expect::Any,
                    new: Some(r.target),
                    expires_at: None,
                })
                .collect(),
            actor: Actor {
                id: actor.to_owned(),
                via: "fork".into(),
            },
            reason: format!("fork of {src_name}"),
            op_id: Uuid::new_v4(),
        };
        if !txn.updates.is_empty() {
            let refs = self.registry.refs();
            let mut tx = refs.begin().await.map_err(|e| internal("refs", &e))?;
            // A fork deleted or renamed while it was copying takes no refs.
            self.registry.guard(&mut tx, &dst).await?;
            refs.update_in_tx(&mut tx, dst.key, &txn)
                .await
                .map_err(|e| internal("refs", &e))?;
            tx.commit().await.map_err(|e| internal("refs", &e))?;
        }
        let done = sqlx::query(
            "UPDATE repos SET ready = $1, fork_from = NULL, fork_lease = NULL, fork_error = NULL              WHERE id = $2 AND ready = $3 AND fork_lease = $4",
        )
        .bind(true)
        .bind(id)
        .bind(false)
        .bind(lease)
        .execute(self.pool())
        .await
        .map_err(|e| sql("repos", e))?;
        if done.rows_affected() != 1 {
            return Err(Status::aborted("the fork was deleted or taken over"));
        }
        self.registry.forget(&name);
        tracing::info!(repo = %name, objects = copied, refs = tips.len(), "fork ready");
        Ok(())
    }

    /// Starts every fork whose lease has run out and that has not failed for good.
    pub async fn resume_forks(&self) -> Result<usize, Status> {
        let now = unix_now();
        let rows = sqlx::query(
            "SELECT id FROM repos WHERE ready = $1 AND fork_from IS NOT NULL \
             AND fork_error IS NULL AND (fork_lease IS NULL OR fork_lease < $2) \
             ORDER BY id LIMIT $3",
        )
        .bind(false)
        .bind(now)
        .bind(FORKS_PER_PASS)
        .fetch_all(self.pool())
        .await
        .map_err(|e| sql("repos", e))?;
        let mut started = 0;
        for row in rows {
            let id: i64 = row.try_get("id").map_err(|e| sql("repos", e))?;
            let lease = now.saturating_add(secs(FORK_LEASE));
            let taken = sqlx::query(
                "UPDATE repos SET fork_lease = $1 WHERE id = $2 AND ready = $3 \
                 AND fork_from IS NOT NULL AND (fork_lease IS NULL OR fork_lease < $4)",
            )
            .bind(lease)
            .bind(id)
            .bind(false)
            .bind(now)
            .execute(self.pool())
            .await
            .map_err(|e| sql("repos", e))?;
            if taken.rows_affected() == 1 {
                self.spawn_fork(id, lease, "system".into());
                started += 1;
            }
        }
        Ok(started)
    }

    // -------------------------------------------------------------------------- delete

    /// Drops the repository's row and refs now and schedules its namespace for [`DROP_DELAY`]
    /// later.
    async fn delete(&self, ctx: &CallCtx) -> Result<wire::RepoAdminResponse, Status> {
        let (entry, _) = self
            .registry
            .open_unready(&ctx.repo)
            .await
            .map_err(registry_status)?;
        let now = unix_now();
        let drop_after = now.saturating_add(secs(DROP_DELAY));
        let mut tx = self
            .registry
            .refs()
            .begin()
            .await
            .map_err(|e| internal("refs", &e))?;
        // The row this call resolved, by id and by name: a replica that renamed or replaced it
        // since leaves nothing to delete here.
        let gone = sqlx::query("DELETE FROM repos WHERE id = $1 AND name = $2")
            .bind(entry.key.0)
            .bind(ctx.repo.as_str())
            .execute(&mut *tx)
            .await
            .map_err(|e| sql("repos", e))?;
        if gone.rows_affected() != 1 {
            return Err(Status::not_found("no such repository"));
        }
        // Tokens and deploy keys name a repository by its name: none may outlive it.
        revoke_access(&mut tx, ctx.repo.as_str(), now).await?;
        for table in ["refs", "mirror_state"] {
            sqlx::query(sqlx::AssertSqlSafe(format!(
                "DELETE FROM {table} WHERE repo_id = $1"
            )))
            .bind(entry.key.0)
            .execute(&mut *tx)
            .await
            .map_err(|e| sql(table, e))?;
        }
        // Effects for a repository that no longer exists cannot be delivered.
        sqlx::query("DELETE FROM outbox WHERE repo_id = $1 AND done_at IS NULL")
            .bind(entry.key.0)
            .execute(&mut *tx)
            .await
            .map_err(|e| sql("outbox", e))?;
        sqlx::query(
            "INSERT INTO repo_drops (ns, repo_id, name, deleted_at, drop_after) \
             VALUES ($1, $2, $3, $4, $5)",
        )
        .bind(entry.ns.as_bytes().to_vec())
        .bind(entry.key.0)
        .bind(ctx.repo.as_str())
        .bind(now)
        .bind(drop_after)
        .execute(&mut *tx)
        .await
        .map_err(|e| sql("repo_drops", e))?;
        tx.commit().await.map_err(|e| sql("repos", e))?;
        self.registry.forget(&ctx.repo);
        crate::authz::token_store::invalidate_grants();
        if let Some(mirror) = &self.mirror {
            mirror.sources().remove(&ctx.repo);
        }
        Ok(wire::RepoAdminResponse {
            report: format!(
                "deleted {}; the namespace is dropped after 30 days",
                ctx.repo
            ),
            ..Default::default()
        })
    }

    /// Drops the namespaces of repositories deleted before `now - DROP_DELAY`: their objects,
    /// then every row keyed by the namespace or the repository. Returns how many it dropped.
    pub async fn purge_due(&self, now: i64) -> Result<usize, Status> {
        let rows = sqlx::query(
            "SELECT ns, repo_id FROM repo_drops WHERE drop_after <= $1 ORDER BY drop_after LIMIT $2",
        )
        .bind(now)
        .bind(DROPS_PER_PASS)
        .fetch_all(self.pool())
        .await
        .map_err(|e| sql("repo_drops", e))?;
        let mut dropped = 0;
        for row in rows {
            let ns_bytes: Vec<u8> = row.try_get("ns").map_err(|e| sql("repo_drops", e))?;
            let repo_id: i64 = row.try_get("repo_id").map_err(|e| sql("repo_drops", e))?;
            let ns = Ns::from_bytes(&ns_bytes)
                .ok_or_else(|| internal("repo_drops", &"ns is not 16 bytes"))?;
            // Objects first: a pass that stops halfway leaves the row, and the next one resumes.
            let (store, prefix) = self.registry.namespace(ns);
            let listed = store.list(Some(&prefix));
            listed
                .map_err(bucket_status)
                .map_ok(|meta| meta.location)
                .map(|loc| {
                    let store = store.clone();
                    async move {
                        let loc = loc?;
                        match store.delete(&loc).await {
                            Ok(()) | Err(object_store::Error::NotFound { .. }) => Ok(()),
                            Err(e) => Err(bucket_status(e)),
                        }
                    }
                })
                .buffer_unordered(COPY_CONCURRENCY)
                .try_collect::<Vec<()>>()
                .await?;
            let mut tx = self
                .registry
                .refs()
                .begin()
                .await
                .map_err(|e| internal("refs", &e))?;
            for table in NS_TABLES {
                sqlx::query(sqlx::AssertSqlSafe(format!(
                    "DELETE FROM {table} WHERE ns = $1"
                )))
                .bind(ns_bytes.clone())
                .execute(&mut *tx)
                .await
                .map_err(|e| sql(table, e))?;
            }
            sqlx::query("DELETE FROM ref_log WHERE repo_id = $1")
                .bind(repo_id)
                .execute(&mut *tx)
                .await
                .map_err(|e| sql("ref_log", e))?;
            sqlx::query("DELETE FROM repo_drops WHERE ns = $1")
                .bind(ns_bytes)
                .execute(&mut *tx)
                .await
                .map_err(|e| sql("repo_drops", e))?;
            tx.commit().await.map_err(|e| sql("repo_drops", e))?;
            tracing::info!(repo = repo_id, "namespace dropped");
            dropped += 1;
        }
        Ok(dropped)
    }

    // -------------------------------------------------------------------------- rename

    async fn rename(&self, ctx: &CallCtx, to: &str) -> Result<wire::RepoAdminResponse, Status> {
        let to = RepoName::parse(to)?;
        if to == ctx.repo {
            return Err(invalid("the new name is the current name"));
        }
        let (entry, _) = self
            .registry
            .open_unready(&ctx.repo)
            .await
            .map_err(registry_status)?;
        let mut tx = self
            .registry
            .refs()
            .begin()
            .await
            .map_err(|e| internal("refs", &e))?;
        let done = sqlx::query("UPDATE repos SET name = $1 WHERE id = $2 AND name = $3")
            .bind(to.as_str())
            .bind(entry.key.0)
            .bind(ctx.repo.as_str())
            .execute(&mut *tx)
            .await;
        match done {
            Ok(r) if r.rows_affected() == 1 => {}
            Ok(_) => return Err(Status::not_found("no such repository")),
            Err(sqlx::Error::Database(d)) if d.is_unique_violation() => {
                return Err(Status::already_exists("repository already exists"));
            }
            Err(e) => return Err(sql("repos", e)),
        }
        // Access follows the repository to its new name, in the same transaction. A new name
        // that already has grants or keys is refused: they would merge with this repository's.
        refuse_leftover_access(&mut tx, to.as_str()).await?;
        move_access(&mut tx, ctx.repo.as_str(), to.as_str()).await?;
        tx.commit().await.map_err(|e| sql("repos", e))?;
        self.registry.forget(&ctx.repo);
        self.registry.forget(&to);
        crate::authz::token_store::invalidate_grants();
        if let Some(mirror) = &self.mirror {
            mirror.sources().rename(&ctx.repo, &to);
        }
        Ok(wire::RepoAdminResponse {
            report: format!("renamed {} to {to}", ctx.repo),
            ..Default::default()
        })
    }

    // -------------------------------------------------------------------------- size, verify

    /// A line for a fork that is not ready, or `None` for a ready repository.
    async fn pending(&self, entry: &RepoEntry, ready: bool) -> Result<Option<String>, Status> {
        if ready {
            return Ok(None);
        }
        let error: Option<String> =
            sqlx::query_scalar("SELECT fork_error FROM repos WHERE id = $1")
                .bind(entry.key.0)
                .fetch_optional(self.pool())
                .await
                .map_err(|e| sql("repos", e))?
                .flatten();
        Ok(Some(match error {
            Some(e) => format!("fork failed: {e}"),
            None => "fork is being prepared".to_owned(),
        }))
    }

    async fn all_roots(&self, entry: &RepoEntry) -> Result<Vec<ObjKey>, Status> {
        let all = self
            .registry
            .refs()
            .list(entry.key, "")
            .await
            .map_err(|e| internal("refs", &e))?;
        if all.len() > MAX_ADMIN_REFS {
            return Err(Status::resource_exhausted(format!(
                "more than {MAX_ADMIN_REFS} refs"
            )));
        }
        Ok(all
            .iter()
            .map(|r| match r.target {
                RefTarget::Changeset(c) => ObjKey::Changeset(c),
                RefTarget::Tag(t) => ObjKey::Tag(t),
            })
            .collect())
    }

    /// `logical` is the stored bytes of the objects reachable from the refs; `unique` is every
    /// byte in the namespace, reachable or not, derived values included.
    async fn size(&self, ctx: &CallCtx) -> Result<wire::RepoAdminResponse, Status> {
        let (entry, ready) = self
            .registry
            .open_unready(&ctx.repo)
            .await
            .map_err(registry_status)?;
        if let Some(report) = self.pending(&entry, ready).await? {
            return Ok(wire::RepoAdminResponse {
                report,
                ..Default::default()
            });
        }
        let mut walk = Walk::new(&*entry.objects, None, Damage::Collect);
        walk.start(self.all_roots(&entry).await?).await?;
        while walk.next_batch().await?.is_some() {}
        let damaged = walk.found.missing + walk.found.corrupt;
        let reachable = walk.into_seen();
        let (store, prefix) = self.registry.namespace(entry.ns);
        let strip = format!("{prefix}/");
        let (mut logical, mut unique) = (0u64, 0u64);
        let mut listed = store.list(Some(&prefix));
        while let Some(meta) = listed.try_next().await.map_err(bucket_status)? {
            unique = unique.saturating_add(meta.size);
            let key = meta
                .location
                .as_ref()
                .strip_prefix(&strip)
                .and_then(ObjKey::parse);
            if key.is_some_and(|k| reachable.contains(&k)) {
                logical = logical.saturating_add(meta.size);
            }
        }
        let mut report = format!("{} reachable objects", reachable.len());
        if damaged > 0 {
            report.push_str(&format!("; {damaged} of them missing or corrupt"));
        }
        Ok(wire::RepoAdminResponse {
            logical_size: logical,
            unique_size: unique,
            report,
        })
    }

    /// Checks that the closure of every ref exists and hashes, and that the git map round-trips
    /// for every reachable changeset: its commit id maps back to it.
    async fn verify(&self, ctx: &CallCtx) -> Result<wire::RepoAdminResponse, Status> {
        let (entry, ready) = self
            .registry
            .open_unready(&ctx.repo)
            .await
            .map_err(registry_status)?;
        if let Some(report) = self.pending(&entry, ready).await? {
            return Ok(wire::RepoAdminResponse {
                report,
                ..Default::default()
            });
        }
        let mut walk = Walk::new(&*entry.objects, None, Damage::Collect);
        walk.start(self.all_roots(&entry).await?).await?;
        let (mut objects, mut changesets) = (0u64, 0u64);
        let mut absent = 0u64;
        let mut git_bad = 0u64;
        let mut named: Vec<String> = Vec::new();
        let note = |named: &mut Vec<String>, line: String| {
            if named.len() < MAX_REPORTED_ISSUES {
                named.push(line);
            }
        };
        while let Some(batch) = walk.next_batch().await? {
            objects += batch.len() as u64;
            // Interior objects were read and hashed by the walk; leaves are only looked for.
            let leaves: Vec<ObjKey> = batch.iter().filter(|k| !has_children(k)).copied().collect();
            let there = entry
                .objects
                .has_many(&leaves)
                .await
                .map_err(store_status)?;
            for (k, t) in leaves.iter().zip(there) {
                if !t {
                    absent += 1;
                    note(&mut named, format!("missing: {}", k.path()));
                }
            }
            let ids: Vec<ChangesetId> = batch
                .iter()
                .filter_map(|k| match k {
                    ObjKey::Changeset(c) => Some(*c),
                    _ => None,
                })
                .collect();
            changesets += ids.len() as u64;
            for id in ids {
                if let Err(why) = self.git_round_trip(&entry, id).await? {
                    git_bad += 1;
                    note(&mut named, format!("git map: {id}: {why}"));
                }
            }
        }
        let found = &walk.found;
        for line in &found.named {
            note(&mut named, line.clone());
        }
        let problems = found.missing + found.corrupt + absent + git_bad;
        let mut report = if problems == 0 {
            format!("ok: {objects} objects, {changesets} changesets")
        } else {
            format!(
                "FAILED: {problems} problems in {objects} objects, {changesets} changesets \
                 ({} missing, {} corrupt, {git_bad} git map)",
                found.missing + absent,
                found.corrupt
            )
        };
        for line in named {
            report.push('\n');
            report.push_str(&line);
        }
        Ok(wire::RepoAdminResponse {
            report,
            ..Default::default()
        })
    }

    /// `Ok(Err(why))` when the git map does not round-trip for `id`.
    async fn git_round_trip(
        &self,
        entry: &RepoEntry,
        id: ChangesetId,
    ) -> Result<Result<(), &'static str>, Status> {
        const COMMIT: i64 = 3;
        let ns = entry.ns.as_bytes().to_vec();
        let mapped: Option<Vec<u8>> =
            sqlx::query_scalar("SELECT git FROM git_map WHERE ns = $1 AND kind = $2 AND asc = $3")
                .bind(ns.clone())
                .bind(COMMIT)
                .bind(id.as_bytes().to_vec())
                .fetch_optional(self.pool())
                .await
                .map_err(|e| sql("git_map", e))?;
        let Some(mapped) = mapped else {
            return Ok(Err("no row for this changeset"));
        };
        let derived = match entry.backend.engine().git_commit_of(id).await {
            Ok(g) => g,
            Err(_) => return Ok(Err("its git commit cannot be derived")),
        };
        if derived.as_bytes().as_slice() != mapped.as_slice() {
            return Ok(Err("the mapped commit is not the derived one"));
        }
        let back: Option<Vec<u8>> =
            sqlx::query_scalar("SELECT asc FROM git_map WHERE ns = $1 AND kind = $2 AND git = $3")
                .bind(ns)
                .bind(COMMIT)
                .bind(mapped)
                .fetch_optional(self.pool())
                .await
                .map_err(|e| sql("git_map", e))?;
        Ok(if back.as_deref() == Some(id.as_bytes().as_slice()) {
            Ok(())
        } else {
            Err("the commit id maps to another changeset")
        })
    }

    // -------------------------------------------------------------------------- worker

    /// Resumes pending forks and drops expired namespaces, now and then every minute.
    pub fn spawn_worker(self: Arc<Self>) -> JoinHandle<()> {
        tokio::spawn(async move {
            loop {
                if let Err(e) = self.resume_forks().await {
                    tracing::warn!(error = %e, "resuming forks failed");
                }
                if let Err(e) = self.purge_due(unix_now()).await {
                    tracing::warn!(error = %e, "dropping namespaces failed");
                }
                tokio::time::sleep(SWEEP_EVERY).await;
            }
        })
    }
}

// ------------------------------------------------------------------------------ name-keyed access

/// Refuses a name that still has token grants or live deploy keys: they were meant for another
/// repository and would reach this one. (`owner/*` and `*` grants are patterns, not rows of a
/// name, and stay.)
async fn refuse_leftover_access(
    tx: &mut sqlx::Transaction<'_, sqlx::Any>,
    name: &str,
) -> Result<(), Status> {
    let grants: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM auth_token_grants WHERE repo = $1")
        .bind(name.to_owned())
        .fetch_one(&mut **tx)
        .await
        .map_err(|e| sql("auth_token_grants", e))?;
    let keys: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM ssh_keys WHERE kind = 'deploy' AND repo_id = $1 AND revoked IS NULL",
    )
    .bind(name.to_owned())
    .fetch_one(&mut **tx)
    .await
    .map_err(|e| sql("ssh_keys", e))?;
    if grants + keys > 0 {
        return Err(Status::failed_precondition(
            "token grants or deploy keys already name this repository; revoke them first",
        ));
    }
    Ok(())
}

/// Revokes every grant and live deploy key that names exactly `name`.
async fn revoke_access(
    tx: &mut sqlx::Transaction<'_, sqlx::Any>,
    name: &str,
    now: i64,
) -> Result<(), Status> {
    sqlx::query("DELETE FROM auth_token_grants WHERE repo = $1")
        .bind(name.to_owned())
        .execute(&mut **tx)
        .await
        .map_err(|e| sql("auth_token_grants", e))?;
    sqlx::query(
        "UPDATE ssh_keys SET revoked = $2 \
         WHERE kind = 'deploy' AND repo_id = $1 AND revoked IS NULL",
    )
    .bind(name.to_owned())
    .bind(now)
    .execute(&mut **tx)
    .await
    .map_err(|e| sql("ssh_keys", e))?;
    Ok(())
}

/// Moves the grants and live deploy keys of exactly `from` to `to`.
async fn move_access(
    tx: &mut sqlx::Transaction<'_, sqlx::Any>,
    from: &str,
    to: &str,
) -> Result<(), Status> {
    sqlx::query("UPDATE auth_token_grants SET repo = $2 WHERE repo = $1")
        .bind(from.to_owned())
        .bind(to.to_owned())
        .execute(&mut **tx)
        .await
        .map_err(|e| sql("auth_token_grants", e))?;
    sqlx::query(
        "UPDATE ssh_keys SET repo_id = $2 \
         WHERE kind = 'deploy' AND repo_id = $1 AND revoked IS NULL",
    )
    .bind(from.to_owned())
    .bind(to.to_owned())
    .execute(&mut **tx)
    .await
    .map_err(|e| sql("ssh_keys", e))?;
    Ok(())
}

fn mirror_status(e: MirrorError) -> Status {
    match e {
        MirrorError::BadUrl(why) => invalid(format!("unusable mirror url: {why}")),
        e => internal("mirror", &e),
    }
}

#[async_trait]
impl AdminHandler for AdminService {
    async fn repo_admin(
        &self,
        ctx: &CallCtx,
        req: wire::RepoAdminRequest,
    ) -> Result<wire::RepoAdminResponse, Status> {
        use wire::repo_admin_request::Op;
        match req.op {
            Some(Op::Init(op)) => self.init(ctx, op).await,
            Some(Op::Fork(op)) => {
                let from = op.from.ok_or_else(|| invalid("missing source"))?;
                let from = RepoName::parse(&from.name)?;
                let (id, lease) = self.begin_fork(&ctx.repo, &from).await?;
                self.spawn_fork(id, lease, ctx.principal.id.clone());
                Ok(wire::RepoAdminResponse {
                    report: "fork is being prepared".into(),
                    ..Default::default()
                })
            }
            // A bare `false` is an unset flag, never a request.
            Some(Op::Delete(true)) => self.delete(ctx).await,
            Some(Op::RenameTo(to)) => self.rename(ctx, &to).await,
            Some(Op::Size(true)) => self.size(ctx).await,
            Some(Op::Verify(true)) => self.verify(ctx).await,
            Some(Op::Delete(false) | Op::Size(false) | Op::Verify(false)) | None => {
                Err(invalid("no operation"))
            }
        }
    }
}

/// The one registration line of this unit (called from `lib.rs`): the `admin` handler group.
/// `mirror` is what `RepoAdmin.init { mode = "mirror" }` sets the source of.
pub fn register(
    ext: &Extensions,
    registry: &Arc<Registry>,
    mirror: Option<Arc<Mirror>>,
) -> Arc<AdminService> {
    let service = AdminService::new(registry.clone(), mirror);
    ext.handlers
        .set_admin(service.clone())
        .expect("the admin handler group is registered once");
    service
}

// ------------------------------------------------------------------------------ fork rows

/// Copies `keys` inside the bucket (S3 `CopyObject`, a local copy, an in-memory copy) without
/// moving their bytes through the server. A fork is built from the source's own verified
/// values; every read of the fork re-hashes them.
async fn server_copy(
    store: &dyn ObjectStore,
    from: &Path,
    to: &Path,
    keys: &[ObjKey],
) -> Result<(), Status> {
    stream::iter(keys.iter().copied())
        .map(|k| async move {
            let rel = k.path();
            let (src, dst) = (
                Path::from(format!("{from}/{rel}")),
                Path::from(format!("{to}/{rel}")),
            );
            store.copy(&src, &dst).await.map_err(bucket_status)
        })
        .buffer_unordered(COPY_CONCURRENCY)
        .try_collect::<Vec<()>>()
        .await
        .map(drop)
}

fn placeholders(n: usize, from: usize) -> String {
    (0..n)
        .map(|i| format!("${}", from + i))
        .collect::<Vec<_>>()
        .join(",")
}

/// Copies the rows that describe `keys` from namespace `src` to `dst`: the graph rows of the
/// changesets (all must exist), and the git map and `file_meta` rows that exist. Only rows of
/// the closure move; nothing about other changesets of the source is copied.
async fn copy_rows(
    registry: &Registry,
    src: Ns,
    dst_entry: &RepoEntry,
    keys: &[ObjKey],
) -> Result<(), Status> {
    let (refs, dst) = (registry.refs(), dst_entry.ns);
    let (mut changesets, mut ids, mut files) = (Vec::new(), Vec::new(), Vec::new());
    for k in keys {
        match k {
            ObjKey::Changeset(c) => {
                changesets.push(c.as_bytes().to_vec());
                ids.push(c.as_bytes().to_vec());
            }
            ObjKey::Tree(t) => ids.push(t.as_bytes().to_vec()),
            ObjKey::Tag(t) => ids.push(t.as_bytes().to_vec()),
            ObjKey::File(f) | ObjKey::ChunkList(f) => {
                ids.push(f.as_bytes().to_vec());
                files.push(f.as_bytes().to_vec());
            }
            _ => {}
        }
    }
    let mut tx = refs.begin().await.map_err(|e| internal("refs", &e))?;
    registry.guard(&mut tx, dst_entry).await?;
    let (s, d) = (src.as_bytes().to_vec(), dst.as_bytes().to_vec());
    for chunk in changesets.chunks(ROW_BATCH) {
        let q = format!(
            "SELECT cs, gen, parents FROM cs_graph WHERE ns = $1 AND cs IN ({})",
            placeholders(chunk.len(), 2)
        );
        let mut query = sqlx::query(sqlx::AssertSqlSafe(q)).bind(s.clone());
        for id in chunk {
            query = query.bind(id.clone());
        }
        let rows = query
            .fetch_all(&mut *tx)
            .await
            .map_err(|e| sql("cs_graph", e))?;
        if rows.len() != chunk.len() {
            return Err(Status::data_loss(
                "a changeset of the source is not in its graph",
            ));
        }
        for row in rows {
            sqlx::query(
                "INSERT INTO cs_graph (ns, cs, gen, parents) VALUES ($1, $2, $3, $4) \
                 ON CONFLICT (ns, cs) DO NOTHING",
            )
            .bind(d.clone())
            .bind(
                row.try_get::<Vec<u8>, _>("cs")
                    .map_err(|e| sql("cs_graph", e))?,
            )
            .bind(
                row.try_get::<i64, _>("gen")
                    .map_err(|e| sql("cs_graph", e))?,
            )
            .bind(
                row.try_get::<Vec<u8>, _>("parents")
                    .map_err(|e| sql("cs_graph", e))?,
            )
            .execute(&mut *tx)
            .await
            .map_err(|e| sql("cs_graph", e))?;
        }
    }
    for chunk in ids.chunks(ROW_BATCH) {
        let q = format!(
            "SELECT git, kind, asc FROM git_map WHERE ns = $1 AND asc IN ({})",
            placeholders(chunk.len(), 2)
        );
        let mut query = sqlx::query(sqlx::AssertSqlSafe(q)).bind(s.clone());
        for id in chunk {
            query = query.bind(id.clone());
        }
        for row in query
            .fetch_all(&mut *tx)
            .await
            .map_err(|e| sql("git_map", e))?
        {
            sqlx::query(
                "INSERT INTO git_map (ns, git, kind, asc) VALUES ($1, $2, $3, $4) \
                 ON CONFLICT (ns, git, kind) DO NOTHING",
            )
            .bind(d.clone())
            .bind(
                row.try_get::<Vec<u8>, _>("git")
                    .map_err(|e| sql("git_map", e))?,
            )
            .bind(
                row.try_get::<i32, _>("kind")
                    .map_err(|e| sql("git_map", e))?,
            )
            .bind(
                row.try_get::<Vec<u8>, _>("asc")
                    .map_err(|e| sql("git_map", e))?,
            )
            .execute(&mut *tx)
            .await
            .map_err(|e| sql("git_map", e))?;
        }
    }
    for chunk in files.chunks(ROW_BATCH) {
        let q = format!(
            "SELECT file, size, git FROM file_meta WHERE ns = $1 AND file IN ({})",
            placeholders(chunk.len(), 2)
        );
        let mut query = sqlx::query(sqlx::AssertSqlSafe(q)).bind(s.clone());
        for id in chunk {
            query = query.bind(id.clone());
        }
        for row in query
            .fetch_all(&mut *tx)
            .await
            .map_err(|e| sql("file_meta", e))?
        {
            sqlx::query(
                "INSERT INTO file_meta (ns, file, size, git) VALUES ($1, $2, $3, $4) \
                 ON CONFLICT (ns, file) DO NOTHING",
            )
            .bind(d.clone())
            .bind(
                row.try_get::<Vec<u8>, _>("file")
                    .map_err(|e| sql("file_meta", e))?,
            )
            .bind(
                row.try_get::<i64, _>("size")
                    .map_err(|e| sql("file_meta", e))?,
            )
            .bind(
                row.try_get::<Vec<u8>, _>("git")
                    .map_err(|e| sql("file_meta", e))?,
            )
            .execute(&mut *tx)
            .await
            .map_err(|e| sql("file_meta", e))?;
        }
    }
    tx.commit().await.map_err(|e| sql("cs_graph", e))
}

/// Copies the mainline and path index rows of the branch `refname` (SPEC §5.1).
async fn copy_index(pool: &AnyPool, src: Ns, dst: Ns, refname: &str) -> Result<(), Status> {
    let (s, d) = (src.as_bytes().to_vec(), dst.as_bytes().to_vec());
    let mut after = -1i64;
    loop {
        let rows = sqlx::query(
            "SELECT pos, cs FROM mainline WHERE ns = $1 AND ref = $2 AND pos > $3 \
             ORDER BY pos LIMIT $4",
        )
        .bind(s.clone())
        .bind(refname)
        .bind(after)
        .bind(INDEX_PAGE)
        .fetch_all(pool)
        .await
        .map_err(|e| sql("mainline", e))?;
        let Some(last) = rows.last() else { break };
        after = last.try_get("pos").map_err(|e| sql("mainline", e))?;
        let mut tx: Transaction<'_, Any> = pool.begin().await.map_err(|e| sql("mainline", e))?;
        for row in &rows {
            sqlx::query(
                "INSERT INTO mainline (ns, ref, pos, cs) VALUES ($1, $2, $3, $4) \
                 ON CONFLICT (ns, ref, pos) DO NOTHING",
            )
            .bind(d.clone())
            .bind(refname)
            .bind(
                row.try_get::<i64, _>("pos")
                    .map_err(|e| sql("mainline", e))?,
            )
            .bind(
                row.try_get::<Vec<u8>, _>("cs")
                    .map_err(|e| sql("mainline", e))?,
            )
            .execute(&mut *tx)
            .await
            .map_err(|e| sql("mainline", e))?;
        }
        tx.commit().await.map_err(|e| sql("mainline", e))?;
    }
    let (mut key, mut pos): (Vec<u8>, i64) = (Vec::new(), -1);
    loop {
        let rows = sqlx::query(
            "SELECT path_key, pos FROM path_changes WHERE ns = $1 AND ref = $2 \
             AND (path_key > $3 OR (path_key = $3 AND pos > $4)) \
             ORDER BY path_key, pos LIMIT $5",
        )
        .bind(s.clone())
        .bind(refname)
        .bind(key.clone())
        .bind(pos)
        .bind(INDEX_PAGE)
        .fetch_all(pool)
        .await
        .map_err(|e| sql("path_changes", e))?;
        let Some(last) = rows.last() else { break };
        key = last
            .try_get("path_key")
            .map_err(|e| sql("path_changes", e))?;
        pos = last.try_get("pos").map_err(|e| sql("path_changes", e))?;
        let mut tx: Transaction<'_, Any> =
            pool.begin().await.map_err(|e| sql("path_changes", e))?;
        for row in &rows {
            sqlx::query(
                "INSERT INTO path_changes (ns, ref, path_key, pos) VALUES ($1, $2, $3, $4) \
                 ON CONFLICT (ns, ref, path_key, pos) DO NOTHING",
            )
            .bind(d.clone())
            .bind(refname)
            .bind(
                row.try_get::<Vec<u8>, _>("path_key")
                    .map_err(|e| sql("path_changes", e))?,
            )
            .bind(
                row.try_get::<i64, _>("pos")
                    .map_err(|e| sql("path_changes", e))?,
            )
            .execute(&mut *tx)
            .await
            .map_err(|e| sql("path_changes", e))?;
        }
        tx.commit().await.map_err(|e| sql("path_changes", e))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use bytes::Bytes;
    use asc_core::content::{Body, ingest as ingest_content};
    use asc_core::git::blob_oid;
    use asc_core::object::Change;
    use asc_core::path::RepoPath;
    use asc_rpc::auth::Principal;
    use asc_store::codec::encode_stored;
    use asc_store::sqlrefs::SqlRefs;
    use object_store::PutPayload;
    use wire::repo_admin_request::Op;

    use super::*;
    use crate::ingest::NoObserver;
    use crate::ingest::testutil::{commit, put_file, registry};
    use crate::registry::NewRepo;

    /// `land_branches` and `windows_safe` of `o/new`.
    const WS_ROW: &str = "SELECT land_branches,         CAST(CASE WHEN windows_safe THEN 1 ELSE 0 END AS BIGINT) AS ws         FROM repos WHERE name = 'o/new'";

    fn name(n: &str) -> RepoName {
        RepoName::parse(n).unwrap()
    }

    fn ctx(n: &str) -> CallCtx {
        CallCtx {
            repo: name(n),
            id_mode: IdMode::Asc,
            principal: Principal { id: "alice".into() },
            op_id: Some(Uuid::new_v4()),
            client: None,
        }
    }

    fn repo_ref(n: &str) -> Option<wire::RepoRef> {
        Some(wire::RepoRef {
            name: n.into(),
            id_mode: IdMode::Asc as i32,
        })
    }

    struct Fx {
        registry: Arc<Registry>,
        pipeline: Arc<PipelineHooks>,
        admin: Arc<AdminService>,
    }

    async fn fx() -> Fx {
        let registry = registry().await;
        Fx {
            admin: AdminService::new(registry.clone(), None),
            pipeline: Arc::new(PipelineHooks::default()),
            registry,
        }
    }

    impl Fx {
        async fn repo(&self, n: &str) -> Arc<RepoEntry> {
            self.registry
                .create(&name(n), &NewRepo::default())
                .await
                .unwrap()
        }

        /// A `RepoAdmin` call on `n`.
        async fn call(&self, n: &str, op: Op) -> Result<wire::RepoAdminResponse, Status> {
            self.admin
                .repo_admin(
                    &ctx(n),
                    wire::RepoAdminRequest {
                        repo: repo_ref(n),
                        op: Some(op),
                    },
                )
                .await
        }

        async fn init(&self, n: &str) -> Arc<RepoEntry> {
            self.call(n, Op::Init(wire::InitOp::default()))
                .await
                .unwrap();
            self.registry.open(&name(n)).await.unwrap()
        }

        async fn copy(&self, from: &str, to: &str, ids: &[ChangesetId]) -> Result<u64, Status> {
            copy_closure(
                &self.registry,
                &self.pipeline,
                &ctx(to),
                wire::CopyClosureRequest {
                    from: repo_ref(from),
                    to: repo_ref(to),
                    changesets: ids
                        .iter()
                        .map(|c| Bytes::copy_from_slice(c.as_bytes()))
                        .collect(),
                },
            )
            .await
            .map(|r| r.copied)
        }

        /// Makes `ids` known to the repository's graph, as an `Ingest` would.
        async fn ingest(&self, e: &RepoEntry, ids: &[ChangesetId]) {
            let targets: Vec<RefTarget> = ids.iter().map(|&c| RefTarget::Changeset(c)).collect();
            Ingestor::new(self.registry.pool(), self.registry.refs(), e, true)
                .ingest(&targets, &NoObserver)
                .await
                .unwrap();
        }

        async fn set_ref(&self, e: &RepoEntry, name: &str, target: RefTarget) {
            self.registry
                .refs()
                .update(
                    e.key,
                    &RefTxn {
                        updates: vec![RefUpdate {
                            name: RefName::new(name.as_bytes()).unwrap(),
                            expect: Expect::Any,
                            new: Some(target),
                            expires_at: None,
                        }],
                        actor: Actor {
                            id: "test".into(),
                            via: "test".into(),
                        },
                        reason: "test".into(),
                        op_id: Uuid::new_v4(),
                    },
                )
                .await
                .unwrap();
        }

        async fn ref_names(&self, e: &RepoEntry) -> Vec<String> {
            self.registry
                .refs()
                .list(e.key, "")
                .await
                .unwrap()
                .iter()
                .map(|r| r.name.to_string())
                .collect()
        }

        /// Every object in the namespace with its stored size.
        async fn listing(&self, ns: Ns) -> Vec<(String, u64)> {
            let (store, prefix) = self.registry.namespace(ns);
            let strip = format!("{prefix}/");
            store
                .list(Some(&prefix))
                .map_ok(|m| {
                    (
                        m.location.as_ref().strip_prefix(&strip).unwrap().to_owned(),
                        m.size,
                    )
                })
                .try_collect()
                .await
                .unwrap()
        }

        async fn put_raw(&self, ns: Ns, key: &ObjKey, stored: Bytes) {
            let (store, prefix) = self.registry.namespace(ns);
            store
                .put(
                    &Path::from(format!("{prefix}/{}", key.path())),
                    PutPayload::from_bytes(stored),
                )
                .await
                .unwrap();
        }

        async fn remove_raw(&self, ns: Ns, key: &ObjKey) {
            let (store, prefix) = self.registry.namespace(ns);
            store
                .delete(&Path::from(format!("{prefix}/{}", key.path())))
                .await
                .unwrap();
        }

        async fn has(&self, e: &RepoEntry, keys: &[ObjKey]) -> Vec<bool> {
            e.objects.has_many(keys).await.unwrap()
        }

        async fn known(&self, e: &RepoEntry, ids: &[ChangesetId]) -> usize {
            graph_sql::known(self.registry.pool(), e.ns, ids)
                .await
                .unwrap()
                .len()
        }

        async fn count(&self, table: &str, ns: Ns) -> i64 {
            sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
                "SELECT COUNT(*) FROM {table} WHERE ns = $1"
            )))
            .bind(ns.as_bytes().to_vec())
            .fetch_one(self.registry.pool())
            .await
            .unwrap()
        }

        async fn scalar(&self, sql: &str, bind: i64) -> i64 {
            sqlx::query_scalar(sqlx::AssertSqlSafe(sql.to_owned()))
                .bind(bind)
                .fetch_one(self.registry.pool())
                .await
                .unwrap()
        }
    }

    /// The keys of the objects a changeset introduces: itself, its root tree and the files of
    /// its `changes`.
    async fn closure_of(e: &RepoEntry, id: ChangesetId) -> Vec<ObjKey> {
        let cs = e.backend.engine().get_changeset(id).await.unwrap();
        let mut keys = vec![ObjKey::Changeset(id), ObjKey::Tree(cs.root)];
        for (_, change) in cs.changes.as_slice() {
            if let Change::Write { file, .. } = change {
                keys.push(ObjKey::File(*file));
            }
        }
        keys
    }

    /// `master` with two commits; returns both.
    async fn history(f: &Fx, e: &RepoEntry) -> (ChangesetId, ChangesetId) {
        let a = commit(e, &[], &[("a.txt", b"one"), ("dir/b.txt", b"two")]).await;
        let b = commit(e, &[a], &[("dir/b.txt", b"three"), ("c.txt", b"four")]).await;
        f.ingest(e, &[a, b]).await;
        f.set_ref(e, "refs/heads/master", RefTarget::Changeset(b))
            .await;
        (a, b)
    }

    // -------------------------------------------------------------------------- CopyClosure

    #[tokio::test]
    async fn copy_closure_copies_the_closure_and_ingests_it() {
        let f = fx().await;
        let src = f.repo("o/src").await;
        let dst = f.repo("o/dst").await;
        let (a, b) = history(&f, &src).await;
        let copied = f.copy("o/src", "o/dst", &[b]).await.unwrap();
        assert!(copied >= 6, "both commits, their trees and files: {copied}");
        // Ingested into the destination's own graph, with its objects.
        assert_eq!(f.known(&dst, &[a, b]).await, 2);
        for id in [a, b] {
            let keys = closure_of(&src, id).await;
            assert!(f.has(&dst, &keys).await.iter().all(|&h| h), "{keys:?}");
        }
        let got = dst.backend.engine().get_changeset(b).await.unwrap();
        assert_eq!(got.parents, vec![a]);
        // The git map was derived in the destination, not copied from the source.
        assert!(f.count("git_map", dst.ns).await > 0);
        // A second call finds everything there.
        assert_eq!(f.copy("o/src", "o/dst", &[b]).await.unwrap(), 0);
    }

    #[tokio::test]
    async fn the_mutations_handler_serves_copy_closure() {
        // `Merge` with a head in another repository calls this handler (U48b).
        use asc_rpc::server::MutationsHandler;
        let f = fx().await;
        let src = f.repo("o/src").await;
        let dst = f.repo("o/dst").await;
        let (_, b) = history(&f, &src).await;
        let handler = crate::mutations::ServerMutations::new(
            f.registry.clone(),
            f.pipeline.clone(),
            Arc::new(crate::read::ReadHooks::default()),
        );
        let got = handler
            .copy_closure(
                &ctx("o/dst"),
                wire::CopyClosureRequest {
                    from: repo_ref("o/src"),
                    to: repo_ref("o/dst"),
                    changesets: vec![Bytes::copy_from_slice(b.as_bytes())],
                },
            )
            .await
            .unwrap();
        assert!(got.copied > 0);
        assert_eq!(f.known(&dst, &[b]).await, 1);
    }

    #[test]
    fn the_admin_group_registers_once() {
        let ext = Extensions::new();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let registry = rt.block_on(registry());
        register(&ext, &registry, None);
        assert!(
            ext.handlers
                .set_admin(AdminService::new(registry, None))
                .is_err()
        );
    }

    #[tokio::test]
    async fn copy_closure_copies_only_what_the_destination_lacks() {
        let f = fx().await;
        let src = f.repo("o/src").await;
        let dst = f.repo("o/dst").await;
        let (a, b) = history(&f, &src).await;
        let first = f.copy("o/src", "o/dst", &[a]).await.unwrap();
        let second = f.copy("o/src", "o/dst", &[b]).await.unwrap();
        // b adds a changeset, a root tree, `dir`'s tree and two files; a.txt is shared.
        assert_eq!(second, 5, "first copy was {first}");
        assert_eq!(f.known(&dst, &[a, b]).await, 2);
    }

    #[tokio::test]
    async fn copy_closure_refuses_a_changeset_the_source_never_ingested() {
        let f = fx().await;
        let src = f.repo("o/src").await;
        let dst = f.repo("o/dst").await;
        // Stored in the source's bucket (a writer uploaded it) but not in its graph.
        let loose = commit(&src, &[], &[("loose.txt", b"loose")]).await;
        let e = f.copy("o/src", "o/dst", &[loose]).await.unwrap_err();
        assert_eq!(e.code(), tonic::Code::NotFound);
        let keys = closure_of(&src, loose).await;
        assert!(f.has(&dst, &keys).await.iter().all(|&h| !h));
        assert_eq!(f.known(&dst, &[loose]).await, 0);
    }

    #[tokio::test]
    async fn copy_closure_copies_no_derived_values() {
        let f = fx().await;
        let src = f.repo("o/src").await;
        let dst = f.repo("o/dst").await;
        let (_, b) = history(&f, &src).await;
        let tdocs = ObjKey::Derived {
            name: "tdocs",
            version: 1,
            id: [9; 32],
        };
        src.objects
            .put(&tdocs, encode_stored(b"index"))
            .await
            .unwrap();
        f.copy("o/src", "o/dst", &[b]).await.unwrap();
        assert_eq!(f.has(&dst, &[tdocs]).await, vec![false]);
        assert_eq!(f.count("ci_docs", dst.ns).await, 0);
    }

    #[tokio::test]
    async fn copy_closure_rejects_objects_that_fail_their_hash() {
        let f = fx().await;
        let src = f.repo("o/src").await;
        let dst = f.repo("o/dst").await;
        let (_, b) = history(&f, &src).await;
        let cs = src.backend.engine().get_changeset(b).await.unwrap();
        let file = closure_of(&src, b).await[2];
        let _ = &cs;
        // A leaf that is not the content its id names.
        f.put_raw(src.ns, &file, encode_stored(b"not what the id says"))
            .await;
        let e = f.copy("o/src", "o/dst", &[b]).await.unwrap_err();
        assert_eq!(e.code(), tonic::Code::DataLoss);
        assert_eq!(f.has(&dst, &[file]).await, vec![false]);
        assert_eq!(f.known(&dst, &[b]).await, 0);
        // An interior object that is not the one its id names.
        let tree = ObjKey::Tree(cs.root);
        f.put_raw(src.ns, &tree, encode_stored(b"junk")).await;
        let e = f.copy("o/src", "o/dst", &[b]).await.unwrap_err();
        assert_eq!(e.code(), tonic::Code::DataLoss);
        assert_eq!(f.known(&dst, &[b]).await, 0);
    }

    #[tokio::test]
    async fn copy_closure_fails_on_a_missing_source_object() {
        let f = fx().await;
        let src = f.repo("o/src").await;
        let dst = f.repo("o/dst").await;
        let (_, b) = history(&f, &src).await;
        let cs = src.backend.engine().get_changeset(b).await.unwrap();
        f.remove_raw(src.ns, &ObjKey::Tree(cs.root)).await;
        let e = f.copy("o/src", "o/dst", &[b]).await.unwrap_err();
        assert_eq!(e.code(), tonic::Code::DataLoss);
        assert_eq!(f.known(&dst, &[b]).await, 0);
    }

    #[tokio::test]
    async fn copy_closure_checks_its_arguments() {
        let f = fx().await;
        let src = f.repo("o/src").await;
        f.repo("o/dst").await;
        let (_, b) = history(&f, &src).await;
        // The same repository on both sides.
        let e = f.copy("o/src", "o/src", &[b]).await.unwrap_err();
        assert_eq!(e.code(), tonic::Code::InvalidArgument);
        // Nothing to copy, and too much.
        let e = f.copy("o/src", "o/dst", &[]).await.unwrap_err();
        assert_eq!(e.code(), tonic::Code::InvalidArgument);
        let many = vec![b; MAX_COPY_CHANGESETS + 1];
        let e = f.copy("o/src", "o/dst", &many).await.unwrap_err();
        assert_eq!(e.code(), tonic::Code::InvalidArgument);
        // An unknown repository.
        let e = f.copy("o/none", "o/dst", &[b]).await.unwrap_err();
        assert_eq!(e.code(), tonic::Code::NotFound);
        // `to` must be the repository the service authorised.
        let mut c = ctx("o/dst");
        let req = |to: &str| wire::CopyClosureRequest {
            from: repo_ref("o/src"),
            to: repo_ref(to),
            changesets: vec![Bytes::copy_from_slice(b.as_bytes())],
        };
        let e = copy_closure(&f.registry, &f.pipeline, &c, req("o/src"))
            .await
            .unwrap_err();
        assert_eq!(e.code(), tonic::Code::InvalidArgument);
        // Git ids are not served.
        c.id_mode = IdMode::Git;
        let e = copy_closure(&f.registry, &f.pipeline, &c, req("o/dst"))
            .await
            .unwrap_err();
        assert_eq!(e.code(), tonic::Code::Unimplemented);
        // A mirror takes no writes.
        f.registry
            .create(
                &name("o/mirror"),
                &NewRepo {
                    mode: RepoMode::Mirror,
                    ..NewRepo::default()
                },
            )
            .await
            .unwrap();
        let e = f.copy("o/src", "o/mirror", &[b]).await.unwrap_err();
        let code = asc_rpc::server::status_rpc_error(&e).map(|r| r.code().to_owned());
        assert_eq!(code.as_deref(), Some("MIRROR_MODE"));
    }

    #[tokio::test]
    async fn the_walk_stops_at_its_object_cap() {
        let f = fx().await;
        let src = f.repo("o/src").await;
        let (_, b) = history(&f, &src).await;
        let mut walk = Walk::new(&*src.objects, None, Damage::Fail);
        walk.cap = 3;
        walk.start(vec![ObjKey::Changeset(b)]).await.unwrap();
        let e = loop {
            match walk.next_batch().await {
                Ok(Some(_)) => {}
                Ok(None) => panic!("the walk finished under the cap"),
                Err(e) => break e,
            }
        };
        assert_eq!(e.code(), tonic::Code::ResourceExhausted);
    }

    #[tokio::test]
    async fn a_walk_yields_children_before_parents() {
        let f = fx().await;
        let src = f.repo("o/src").await;
        let (_, b) = history(&f, &src).await;
        let mut walk = Walk::new(&*src.objects, None, Damage::Fail);
        walk.start(vec![ObjKey::Changeset(b)]).await.unwrap();
        let mut order = Vec::new();
        while let Some(batch) = walk.next_batch().await.unwrap() {
            order.extend(batch);
        }
        let at = |k: &ObjKey| order.iter().position(|o| o == k).unwrap();
        for k in &order {
            let data = match src.objects.get(k).await.unwrap() {
                Some(d) if has_children(k) => d,
                _ => continue,
            };
            for kid in children(k, &data).unwrap().required {
                assert!(at(&kid) < at(k), "{kid:?} before {k:?}");
            }
        }
        // Each object once.
        assert_eq!(order.len(), order.iter().collect::<HashSet<_>>().len());
    }

    // -------------------------------------------------------------------------- init

    #[tokio::test]
    async fn init_with_an_empty_default_branch_is_master() {
        let f = fx().await;
        let e = f
            .call(
                "o/new",
                Op::Init(wire::InitOp {
                    windows_safe: true,
                    ..Default::default()
                }),
            )
            .await
            .unwrap();
        assert!(e.report.contains("master"), "{}", e.report);
        let entry = f.registry.open(&name("o/new")).await.unwrap();
        assert_eq!(entry.default_branch, "master");
        assert_eq!(entry.mode, RepoMode::Primary);
        let (land, ws): (String, i64) = sqlx::query_as(WS_ROW)
            .fetch_one(f.registry.pool())
            .await
            .unwrap();
        assert_eq!(land, r#"["master"]"#);
        assert_eq!(ws, 1);
    }

    #[tokio::test]
    async fn init_keeps_the_branches_it_is_given() {
        let f = fx().await;
        f.call(
            "o/new",
            Op::Init(wire::InitOp {
                default_branch: "trunk".into(),
                land_branches: vec!["trunk".into(), "refs/heads/release".into(), "trunk".into()],
                windows_safe: false,
                ..Default::default()
            }),
        )
        .await
        .unwrap();
        assert_eq!(
            f.registry
                .open(&name("o/new"))
                .await
                .unwrap()
                .default_branch,
            "trunk"
        );
        let (land, ws): (String, i64) = sqlx::query_as(WS_ROW)
            .fetch_one(f.registry.pool())
            .await
            .unwrap();
        assert_eq!(land, r#"["trunk","refs/heads/release"]"#);
        assert_eq!(ws, 0);
    }

    #[tokio::test]
    async fn init_refuses_bad_names_and_modes_and_creates_nothing() {
        let f = fx().await;
        for op in [
            wire::InitOp {
                default_branch: "-x".into(),
                ..Default::default()
            },
            wire::InitOp {
                default_branch: "HEAD".into(),
                ..Default::default()
            },
            wire::InitOp {
                default_branch: "a..b".into(),
                ..Default::default()
            },
            wire::InitOp {
                land_branches: vec!["refs/tags/v1".into()],
                ..Default::default()
            },
            wire::InitOp {
                land_branches: vec!["bad name".into()],
                ..Default::default()
            },
            wire::InitOp {
                land_branches: vec!["b".into(); MAX_LAND_BRANCHES + 1],
                ..Default::default()
            },
            wire::InitOp {
                mode: "bogus".into(),
                ..Default::default()
            },
            wire::InitOp {
                mirror_url: "https://forge.example/o/new.git".into(),
                ..Default::default()
            },
        ] {
            let e = f.call("o/new", Op::Init(op.clone())).await.unwrap_err();
            assert_eq!(e.code(), tonic::Code::InvalidArgument, "{op:?}");
        }
        // A mirror needs a server that runs mirrors.
        let e = f
            .call(
                "o/new",
                Op::Init(wire::InitOp {
                    mode: "mirror".into(),
                    ..Default::default()
                }),
            )
            .await
            .unwrap_err();
        assert_eq!(e.code(), tonic::Code::FailedPrecondition);
        let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM repos")
            .fetch_one(f.registry.pool())
            .await
            .unwrap();
        assert_eq!(rows, 0);
    }

    #[tokio::test]
    async fn init_of_an_existing_name_is_refused() {
        let f = fx().await;
        let first = f.init("o/new").await;
        let e = f
            .call("o/new", Op::Init(wire::InitOp::default()))
            .await
            .unwrap_err();
        assert_eq!(e.code(), tonic::Code::AlreadyExists);
        assert_eq!(f.registry.open(&name("o/new")).await.unwrap().ns, first.ns);
    }

    #[tokio::test]
    async fn init_of_a_mirror_applies_the_client_url_policy() {
        use crate::mirror::MirrorConfig;
        use asc_git::realgit::RealGit;
        let f = fx().await;
        let tmp = tempfile::tempdir().unwrap();
        let mirror = Arc::new(Mirror::new(
            f.registry.clone(),
            &f.pipeline,
            RealGit::resolve(None, None).expect("git is installed"),
            MirrorConfig {
                dir: tmp.path().join("mirrors"),
                secret: None,
                forgejo_url: Some("https://forge.example".into()),
                allowed_hosts: Vec::new(),
                interval: Duration::from_secs(60),
                fetch_timeout: Duration::from_secs(60),
            },
        ));
        let admin = AdminService::new(f.registry.clone(), Some(mirror));
        let call = |n: &'static str, url: &'static str| {
            let admin = admin.clone();
            async move {
                admin
                    .repo_admin(
                        &ctx(n),
                        wire::RepoAdminRequest {
                            repo: repo_ref(n),
                            op: Some(Op::Init(wire::InitOp {
                                mode: "mirror".into(),
                                mirror_url: url.into(),
                                ..Default::default()
                            })),
                        },
                    )
                    .await
            }
        };
        // Another host, a local path and a password are all refused, and nothing is created.
        for url in [
            "https://evil.example/o/m.git",
            "/srv/repos/m.git",
            "https://user:pw@forge.example/o/m.git",
            "file:///srv/repos/m.git",
        ] {
            let e = call("o/m", url).await.unwrap_err();
            assert_eq!(e.code(), tonic::Code::InvalidArgument, "{url}");
            assert!(!e.message().contains("pw@"), "{}", e.message());
        }
        assert!(f.registry.open(&name("o/m")).await.is_err());
        // The Forgejo host is allowed; the repository is a mirror.
        call("o/m", "https://forge.example/o/m.git").await.unwrap();
        assert_eq!(
            f.registry.open(&name("o/m")).await.unwrap().mode,
            RepoMode::Mirror
        );
        // With no URL the Forgejo default stands.
        call("o/m2", "").await.unwrap();
    }

    #[tokio::test]
    async fn an_unset_operation_or_a_false_flag_does_nothing() {
        let f = fx().await;
        let e = f.init("o/keep").await;
        f.set_ref(
            &e,
            "refs/heads/master",
            RefTarget::Changeset(ChangesetId::of(b"x")),
        )
        .await;
        for op in [Op::Delete(false), Op::Size(false), Op::Verify(false)] {
            let err = f.call("o/keep", op).await.unwrap_err();
            assert_eq!(err.code(), tonic::Code::InvalidArgument);
        }
        let err = f
            .admin
            .repo_admin(
                &ctx("o/keep"),
                wire::RepoAdminRequest {
                    repo: repo_ref("o/keep"),
                    op: None,
                },
            )
            .await
            .unwrap_err();
        assert_eq!(err.code(), tonic::Code::InvalidArgument);
        // Still there, with its ref.
        let still = f.registry.open(&name("o/keep")).await.unwrap();
        assert_eq!(f.ref_names(&still).await, vec!["refs/heads/master"]);
    }

    // -------------------------------------------------------------------------- fork

    /// A source with `master` (two commits), `dev`, a private draft under a user ref, a pull
    /// ref, an object nothing refers to and a derived value, and a land index.
    struct Source {
        entry: Arc<RepoEntry>,
        a: ChangesetId,
        b: ChangesetId,
        draft: ChangesetId,
        orphan: ObjKey,
        derived: ObjKey,
    }

    async fn source(f: &Fx) -> Source {
        let entry = f.repo("o/src").await;
        let (a, b) = history(f, &entry).await;
        f.set_ref(&entry, "refs/heads/dev", RefTarget::Changeset(a))
            .await;
        let draft = commit(&entry, &[a], &[("secret.txt", b"private")]).await;
        f.ingest(&entry, &[draft]).await;
        f.set_ref(
            &entry,
            "refs/users/7/heads/draft",
            RefTarget::Changeset(draft),
        )
        .await;
        f.set_ref(&entry, "refs/pull/1/head", RefTarget::Changeset(draft))
            .await;
        let Change::Write { file, .. } = put_file(&entry, b"nothing refers to me").await else {
            unreachable!()
        };
        let derived = ObjKey::Derived {
            name: "tdocs",
            version: 1,
            id: [3; 32],
        };
        entry
            .objects
            .put(&derived, encode_stored(b"index"))
            .await
            .unwrap();
        // The land index of `master`, and of a ref a fork does not take.
        let ns = entry.ns.as_bytes().to_vec();
        for (r, pos, cs) in [
            ("refs/heads/master", 0i64, a),
            ("refs/heads/master", 1, b),
            ("refs/users/7/heads/draft", 0, draft),
        ] {
            sqlx::query("INSERT INTO mainline (ns, ref, pos, cs) VALUES ($1, $2, $3, $4)")
                .bind(ns.clone())
                .bind(r)
                .bind(pos)
                .bind(cs.as_bytes().to_vec())
                .execute(f.registry.pool())
                .await
                .unwrap();
            sqlx::query(
                "INSERT INTO path_changes (ns, ref, path_key, pos) VALUES ($1, $2, $3, $4)",
            )
            .bind(ns.clone())
            .bind(r)
            .bind(vec![pos as u8; 16])
            .bind(pos)
            .execute(f.registry.pool())
            .await
            .unwrap();
        }
        Source {
            entry,
            a,
            b,
            draft,
            orphan: ObjKey::File(file),
            derived,
        }
    }

    #[tokio::test]
    async fn a_fork_is_readable_only_after_the_copy_completes() {
        let f = fx().await;
        let s = source(&f).await;
        let (id, lease) = f
            .admin
            .begin_fork(&name("o/fork"), &name("o/src"))
            .await
            .unwrap();
        // The row exists, so the name is taken, but nothing opens it.
        let e = f.registry.open(&name("o/fork")).await.err().unwrap();
        assert!(matches!(e, RegistryError::NotFound(_)));
        let (_, ready) = f.registry.open_unready(&name("o/fork")).await.unwrap();
        assert!(!ready);
        let busy = f.call("o/fork", Op::Size(true)).await.unwrap();
        assert_eq!(busy.report, "fork is being prepared");
        let again = f
            .admin
            .begin_fork(&name("o/fork"), &name("o/src"))
            .await
            .unwrap_err();
        assert_eq!(again.code(), tonic::Code::AlreadyExists);

        f.admin.run_fork(id, lease, "alice").await.unwrap();
        let fork = f.registry.open(&name("o/fork")).await.unwrap();
        assert_ne!(fork.ns, s.entry.ns);
        assert_eq!(fork.default_branch, "master");

        // The shared refs came along; the user ref and the pull ref did not.
        assert_eq!(
            f.ref_names(&fork).await,
            vec!["refs/heads/dev", "refs/heads/master"]
        );
        // The closure of those refs is in the fork and answers reads.
        for id in [s.a, s.b] {
            let keys = closure_of(&s.entry, id).await;
            assert!(f.has(&fork, &keys).await.iter().all(|&h| h));
        }
        assert_eq!(f.known(&fork, &[s.a, s.b]).await, 2);
        assert_eq!(
            fork.backend.engine().git_commit_of(s.b).await.unwrap(),
            s.entry.backend.engine().git_commit_of(s.b).await.unwrap()
        );
        // What the source keeps private, or keeps to itself, did not.
        assert_eq!(f.known(&fork, &[s.draft]).await, 0);
        let draft_keys = closure_of(&s.entry, s.draft).await;
        assert_eq!(f.has(&fork, &draft_keys[..1]).await, vec![false]);
        assert_eq!(
            f.has(&fork, &[s.orphan, s.derived]).await,
            vec![false, false]
        );
        // Rows: the graph and git map of the closure, the land index of copied branches only.
        assert_eq!(f.count("cs_graph", fork.ns).await, 2);
        assert!(f.count("git_map", fork.ns).await > 0);
        assert_eq!(f.count("mainline", fork.ns).await, 2);
        assert_eq!(f.count("path_changes", fork.ns).await, 2);
        // The source is untouched.
        assert_eq!(f.count("cs_graph", s.entry.ns).await, 3);
    }

    #[tokio::test]
    async fn a_fork_made_through_the_rpc_becomes_ready_in_the_background() {
        let f = fx().await;
        source(&f).await;
        let r = f
            .call(
                "o/fork",
                Op::Fork(wire::ForkOp {
                    from: repo_ref("o/src"),
                }),
            )
            .await
            .unwrap();
        assert_eq!(r.report, "fork is being prepared");
        let start = Instant::now();
        let fork = loop {
            match f.registry.open(&name("o/fork")).await {
                Ok(e) => break e,
                Err(_) if start.elapsed() < Duration::from_secs(30) => {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
                Err(e) => panic!("the fork never became ready: {e}"),
            }
        };
        assert_eq!(f.ref_names(&fork).await.len(), 2);
    }

    #[tokio::test]
    async fn a_fork_copies_a_chunked_file() {
        let f = fx().await;
        let entry = f.repo("o/src").await;
        // 5 MiB of incompressible bytes: above the inline threshold, so a chunk list.
        let mut seed = 0x9E37_79B9_7F4A_7C15u64;
        let data: Vec<u8> = (0..5 * 1024 * 1024)
            .map(|_| {
                seed ^= seed << 13;
                seed ^= seed >> 7;
                seed ^= seed << 17;
                (seed >> 24) as u8
            })
            .collect();
        let got = ingest_content(std::io::Cursor::new(&data)).unwrap();
        let Body::Chunked { list, chunks } = got.body else {
            panic!("a 5 MiB file is chunked")
        };
        let chunk_keys: Vec<ObjKey> = chunks.iter().map(|(id, _)| ObjKey::Chunk(*id)).collect();
        for (id, chunk) in &chunks {
            entry
                .objects
                .put(&ObjKey::Chunk(*id), encode_stored(chunk))
                .await
                .unwrap();
        }
        entry
            .objects
            .put(
                &ObjKey::ChunkList(got.file),
                encode_stored(&asc_core::codec::Enveloped::to_envelope(&list)),
            )
            .await
            .unwrap();
        let change = Change::write(Kind::File, got.file, got.size, got.git, None).unwrap();
        let root = entry
            .backend
            .engine()
            .apply(
                asc_engine::Engine::<asc_store::bucket::BucketObjects, SqlRefs>::empty_tree_id(),
                &[(RepoPath::new("big.bin").unwrap(), change)],
            )
            .await
            .unwrap();
        let id = entry
            .backend
            .engine()
            .create_changeset(
                asc_engine::commit::NewChangeset {
                    parents: Vec::new(),
                    author: crate::ingest::testutil::sig(),
                    committer: crate::ingest::testutil::sig(),
                    message: b"big".to_vec(),
                    extra: Vec::new(),
                    root,
                    copies: Vec::new(),
                },
                asc_engine::commit::PathPolicy::Native { windows_safe: true },
            )
            .await
            .unwrap();
        f.ingest(&entry, &[id]).await;
        f.set_ref(&entry, "refs/heads/master", RefTarget::Changeset(id))
            .await;

        // CopyClosure takes the chunks and the list.
        let dst = f.repo("o/dst").await;
        f.copy("o/src", "o/dst", &[id]).await.unwrap();
        assert!(f.has(&dst, &chunk_keys).await.iter().all(|&h| h));
        assert_eq!(
            f.has(&dst, &[ObjKey::ChunkList(got.file)]).await,
            vec![true]
        );
        // A big file has no inline copy, so none is looked for.
        assert_eq!(f.has(&dst, &[ObjKey::File(got.file)]).await, vec![false]);
        // And so does a fork, bytes intact.
        let (fid, lease) = f
            .admin
            .begin_fork(&name("o/fork"), &name("o/src"))
            .await
            .unwrap();
        f.admin.run_fork(fid, lease, "alice").await.unwrap();
        let fork = f.registry.open(&name("o/fork")).await.unwrap();
        assert!(f.has(&fork, &chunk_keys).await.iter().all(|&h| h));
        let read = fork
            .backend
            .engine()
            .read_file(got.file, got.size)
            .await
            .unwrap();
        assert_eq!(read.len(), data.len());
        assert_eq!(blob_oid(&read), got.git);
    }

    #[tokio::test]
    async fn a_fork_that_cannot_copy_stays_unreadable_and_says_why() {
        let f = fx().await;
        let s = source(&f).await;
        let cs = s.entry.backend.engine().get_changeset(s.b).await.unwrap();
        f.remove_raw(s.entry.ns, &ObjKey::Tree(cs.root)).await;
        let (id, lease) = f
            .admin
            .begin_fork(&name("o/fork"), &name("o/src"))
            .await
            .unwrap();
        let e = f.admin.run_fork(id, lease, "alice").await.unwrap_err();
        assert_eq!(e.code(), tonic::Code::DataLoss);
        f.admin.fork_failed(id, &e).await;
        assert!(f.registry.open(&name("o/fork")).await.is_err());
        let r = f.call("o/fork", Op::Size(true)).await.unwrap();
        assert!(r.report.starts_with("fork failed:"), "{}", r.report);
        // A failure retrying cannot fix is not retried.
        assert_eq!(f.admin.resume_forks().await.unwrap(), 0);
        // The owner can delete it.
        f.call("o/fork", Op::Delete(true)).await.unwrap();
        assert!(f.registry.open_unready(&name("o/fork")).await.is_err());
    }

    #[tokio::test]
    async fn an_abandoned_fork_resumes_when_its_lease_ends() {
        let f = fx().await;
        source(&f).await;
        let (id, _) = f
            .admin
            .begin_fork(&name("o/fork"), &name("o/src"))
            .await
            .unwrap();
        // The lease is held by a live copy: nothing to resume.
        assert_eq!(f.admin.resume_forks().await.unwrap(), 0);
        sqlx::query("UPDATE repos SET fork_lease = 1 WHERE id = $1")
            .bind(id)
            .execute(f.registry.pool())
            .await
            .unwrap();
        assert_eq!(f.admin.resume_forks().await.unwrap(), 1);
        let start = Instant::now();
        while f.registry.open(&name("o/fork")).await.is_err() {
            assert!(start.elapsed() < Duration::from_secs(30), "never ready");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(f.admin.resume_forks().await.unwrap(), 0);
    }

    #[tokio::test]
    async fn only_the_holder_of_the_lease_finishes_a_fork() {
        let f = fx().await;
        source(&f).await;
        let (id, lease) = f
            .admin
            .begin_fork(&name("o/fork"), &name("o/src"))
            .await
            .unwrap();
        // Another replica took the copy over.
        sqlx::query("UPDATE repos SET fork_lease = $1 WHERE id = $2")
            .bind(lease + 1000)
            .bind(id)
            .execute(f.registry.pool())
            .await
            .unwrap();
        let e = f.admin.run_fork(id, lease, "alice").await.unwrap_err();
        assert_eq!(e.code(), tonic::Code::Aborted);
        assert!(f.registry.open(&name("o/fork")).await.is_err());
    }

    #[tokio::test]
    async fn a_fork_of_nothing_or_of_itself_is_refused() {
        let f = fx().await;
        let e = f
            .admin
            .begin_fork(&name("o/fork"), &name("o/none"))
            .await
            .unwrap_err();
        assert_eq!(e.code(), tonic::Code::NotFound);
        f.init("o/src").await;
        let e = f
            .admin
            .begin_fork(&name("o/src"), &name("o/src"))
            .await
            .unwrap_err();
        assert_eq!(e.code(), tonic::Code::InvalidArgument);
        // A fork still being prepared cannot be forked.
        f.admin
            .begin_fork(&name("o/fork"), &name("o/src"))
            .await
            .unwrap();
        let e = f
            .admin
            .begin_fork(&name("o/fork2"), &name("o/fork"))
            .await
            .unwrap_err();
        assert_eq!(e.code(), tonic::Code::NotFound);
        let e = f
            .call("o/x", Op::Fork(wire::ForkOp { from: None }))
            .await
            .unwrap_err();
        assert_eq!(e.code(), tonic::Code::InvalidArgument);
    }

    // -------------------------------------------------------------------------- delete

    #[tokio::test]
    async fn delete_drops_the_refs_now_and_the_namespace_after_thirty_days() {
        let f = fx().await;
        let s = source(&f).await;
        let ns = s.entry.ns;
        let id = s.entry.key.0;
        sqlx::query("INSERT INTO outbox (repo_id, op_id, effects, next_at) VALUES ($1, $2, $3, 0)")
            .bind(id)
            .bind(vec![1u8; 16])
            .bind(b"[]".to_vec())
            .execute(f.registry.pool())
            .await
            .unwrap();
        let before = f.listing(ns).await;
        assert!(!before.is_empty());
        f.call("o/src", Op::Delete(true)).await.unwrap();

        // Gone at once: not openable, no refs, the pending effects dropped.
        assert!(f.registry.open(&name("o/src")).await.is_err());
        assert_eq!(
            f.scalar("SELECT COUNT(*) FROM refs WHERE repo_id = $1", id)
                .await,
            0
        );
        assert_eq!(
            f.scalar("SELECT COUNT(*) FROM outbox WHERE repo_id = $1", id)
                .await,
            0
        );
        // Its objects and rows wait.
        assert_eq!(f.listing(ns).await, before);
        assert_eq!(f.count("cs_graph", ns).await, 3);
        let (deleted_at, drop_after): (i64, i64) =
            sqlx::query_as("SELECT deleted_at, drop_after FROM repo_drops WHERE name = 'o/src'")
                .fetch_one(f.registry.pool())
                .await
                .unwrap();
        assert_eq!(drop_after - deleted_at, 30 * 24 * 3600);
        assert_eq!(
            f.admin
                .purge_due(deleted_at + 29 * 24 * 3600)
                .await
                .unwrap(),
            0
        );
        assert_eq!(f.listing(ns).await, before);

        // The name is free, for a repository with a namespace of its own.
        let again = f.init("o/src").await;
        assert_ne!(again.ns, ns);
        assert!(f.ref_names(&again).await.is_empty());

        // After the delay the namespace goes: objects, rows, log.
        assert_eq!(f.admin.purge_due(drop_after + 1).await.unwrap(), 1);
        assert!(f.listing(ns).await.is_empty());
        for table in NS_TABLES {
            assert_eq!(f.count(table, ns).await, 0, "{table}");
        }
        assert_eq!(
            f.scalar("SELECT COUNT(*) FROM ref_log WHERE repo_id = $1", id)
                .await,
            0
        );
        assert_eq!(
            f.scalar("SELECT COUNT(*) FROM repo_drops WHERE 1 = $1", 1)
                .await,
            0
        );
        assert_eq!(f.admin.purge_due(drop_after + 1).await.unwrap(), 0);
    }

    #[tokio::test]
    async fn delete_of_a_missing_repository_is_not_found() {
        let f = fx().await;
        let e = f.call("o/none", Op::Delete(true)).await.unwrap_err();
        assert_eq!(e.code(), tonic::Code::NotFound);
    }

    #[tokio::test]
    async fn purge_clears_every_table_keyed_by_a_namespace() {
        let f = fx().await;
        let rows = sqlx::query(
            "SELECT m.name AS t FROM sqlite_master m, pragma_table_info(m.name) c \
             WHERE m.type = 'table' AND c.name = 'ns'",
        )
        .fetch_all(f.registry.pool())
        .await
        .unwrap();
        let tables: Vec<String> = rows.iter().map(|r| r.get("t")).collect();
        assert!(tables.len() > NS_TABLES.len(), "{tables:?}");
        for t in tables {
            // `repo_drops` is the schedule itself and `repos` holds the namespace of a live
            // repository; every other table must be cleared with the namespace.
            if matches!(t.as_str(), "repo_drops" | "repos") {
                continue;
            }
            assert!(
                NS_TABLES.contains(&t.as_str()),
                "{t} is keyed by a namespace; add it to NS_TABLES"
            );
        }
    }

    // -------------------------------------------------------------------------- rename

    #[tokio::test]
    async fn rename_moves_the_repository_with_its_refs() {
        let f = fx().await;
        let s = source(&f).await;
        let key = s.entry.key;
        f.call("o/src", Op::RenameTo("p/moved".into()))
            .await
            .unwrap();
        assert!(f.registry.open(&name("o/src")).await.is_err());
        let moved = f.registry.open(&name("p/moved")).await.unwrap();
        assert_eq!(moved.key, key);
        assert_eq!(moved.ns, s.entry.ns);
        assert!(
            f.ref_names(&moved)
                .await
                .contains(&"refs/heads/master".to_owned())
        );
        // The old name is free.
        f.init("o/src").await;
    }

    #[tokio::test]
    async fn rename_refuses_a_taken_name_and_a_bad_one() {
        let f = fx().await;
        let a = f.init("o/a").await;
        let b = f.init("o/b").await;
        for (to, code) in [
            ("o/b", tonic::Code::AlreadyExists),
            ("o/a", tonic::Code::InvalidArgument),
            ("no-slash", tonic::Code::InvalidArgument),
            ("", tonic::Code::InvalidArgument),
            ("o/.hidden", tonic::Code::InvalidArgument),
        ] {
            let e = f.call("o/a", Op::RenameTo(to.into())).await.unwrap_err();
            assert_eq!(e.code(), code, "{to}");
        }
        assert_eq!(f.registry.open(&name("o/a")).await.unwrap().ns, a.ns);
        assert_eq!(f.registry.open(&name("o/b")).await.unwrap().ns, b.ns);
        let e = f
            .call("o/none", Op::RenameTo("o/c".into()))
            .await
            .unwrap_err();
        assert_eq!(e.code(), tonic::Code::NotFound);
    }

    #[tokio::test]
    async fn another_replica_sees_a_delete_and_a_new_repository_of_the_same_name() {
        let f = fx().await;
        let old = f.init("o/r").await;
        // A second replica: its own registry over the same database.
        let other = Registry::new(f.registry.pool().clone(), "memory:///")
            .unwrap()
            .with_entry_ttl(Duration::ZERO);
        let seen = other.open(&name("o/r")).await.unwrap();
        assert_eq!(seen.ns, old.ns);
        f.call("o/r", Op::Delete(true)).await.unwrap();
        assert!(other.open(&name("o/r")).await.is_err());
        let new = f.init("o/r").await;
        let seen = other.open(&name("o/r")).await.unwrap();
        assert_eq!(seen.ns, new.ns);
        assert_ne!(seen.ns, old.ns);
        // The same repository keeps its entry (and its caches) across a refresh.
        let again = other.open(&name("o/r")).await.unwrap();
        assert!(Arc::ptr_eq(&seen, &again));
        // Within the time-to-live a replica does not ask the database at all.
        let slow = Registry::new(f.registry.pool().clone(), "memory:///").unwrap();
        let first = slow.open(&name("o/r")).await.unwrap();
        f.call("o/r", Op::Delete(true)).await.unwrap();
        assert!(Arc::ptr_eq(&first, &slow.open(&name("o/r")).await.unwrap()));
    }

    // -------------------------------------------------------------------------- stale entries

    /// A second replica: its own registry over `f`'s database whose cache never expires on its own.
    fn replica(f: &Fx) -> Arc<Registry> {
        Arc::new(
            Registry::new(f.registry.pool().clone(), "memory:///")
                .unwrap()
                .with_entry_ttl(Duration::from_secs(3600)),
        )
    }

    /// `UpdateRefs` step 8 for one new branch, against `entry` as a request resolved it.
    async fn push_branch(
        registry: &Arc<Registry>,
        f: &Fx,
        entry: &RepoEntry,
        branch: &str,
        cs: ChangesetId,
    ) -> Result<(), Status> {
        let txn = RefTxn {
            updates: vec![RefUpdate {
                name: RefName::new(format!("refs/heads/{branch}").into_bytes()).unwrap(),
                expect: Expect::Any,
                new: Some(RefTarget::Changeset(cs)),
                expires_at: None,
            }],
            actor: Actor {
                id: "alice".into(),
                via: "test".into(),
            },
            reason: "UpdateRefs".into(),
            op_id: Uuid::new_v4(),
        };
        crate::update::ServerRefs::new(registry.clone(), f.pipeline.clone())
            .commit(
                entry,
                &txn,
                &wire::UpdateRefsResponse::default(),
                b"{}",
                None,
            )
            .await
    }

    async fn op_results(f: &Fx) -> i64 {
        sqlx::query_scalar("SELECT COUNT(*) FROM op_results")
            .fetch_one(f.registry.pool())
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn a_mutation_prepared_against_a_deleted_repository_fails_and_the_new_one_is_untouched() {
        let f = fx().await;
        let a = replica(&f);
        let old = f.init("o/r").await;
        let cs = commit(&old, &[], &[("a", b"1")]).await;
        // Replica A resolves o/r and caches it; replica B deletes it and creates it again.
        let stale = a.open(&name("o/r")).await.unwrap();
        f.call("o/r", Op::Delete(true)).await.unwrap();
        let new = f.init("o/r").await;
        assert_ne!(new.key, old.key);
        // The cache still answers the old repository (the reads' staleness is bounded by the TTL).
        assert!(Arc::ptr_eq(&stale, &a.open(&name("o/r")).await.unwrap()));

        let e = push_branch(&a, &f, &stale, "x", cs).await.unwrap_err();
        assert_eq!(e.code(), tonic::Code::NotFound);
        // Ingest takes the same check.
        let held = commit(&stale, &[], &[("a", b"1")]).await;
        let e = Ingestor::new(a.pool(), a.refs(), &stale, true)
            .ingest(&[RefTarget::Changeset(held)], &NoObserver)
            .await
            .unwrap_err();
        assert!(matches!(
            e,
            crate::ingest::IngestError::Stale { gone: true }
        ));
        // Nothing was written anywhere, and the failed check dropped the stale entry.
        assert!(f.ref_names(&new).await.is_empty());
        assert!(f.ref_names(&old).await.is_empty());
        assert_eq!(op_results(&f).await, 0);
        let graph: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM cs_graph WHERE ns = $1")
            .bind(new.ns.as_bytes().to_vec())
            .fetch_one(f.registry.pool())
            .await
            .unwrap();
        assert_eq!(graph, 0);
        assert!(!Arc::ptr_eq(&stale, &a.open(&name("o/r")).await.unwrap()));
        // The new repository takes writes through its own entry.
        let cs = commit(&new, &[], &[("a", b"1")]).await;
        push_branch(&a, &f, &a.open(&name("o/r")).await.unwrap(), "x", cs)
            .await
            .unwrap();
        assert_eq!(f.ref_names(&new).await, ["refs/heads/x"]);
    }

    #[tokio::test]
    async fn a_mutation_through_a_cached_entry_of_a_renamed_repository_fails() {
        let f = fx().await;
        let a = replica(&f);
        let repo = f.init("o/a").await;
        let cs = commit(&repo, &[], &[("a", b"1")]).await;
        let stale = a.open(&name("o/a")).await.unwrap();
        f.call("o/a", Op::RenameTo("o/b".into())).await.unwrap();
        // The id still exists, under another name: authorised for o/a, it must not write.
        let e = push_branch(&a, &f, &stale, "x", cs).await.unwrap_err();
        assert_eq!(e.code(), tonic::Code::FailedPrecondition);
        let held = commit(&stale, &[], &[("a", b"1")]).await;
        let e = Ingestor::new(a.pool(), a.refs(), &stale, true)
            .ingest(&[RefTarget::Changeset(held)], &NoObserver)
            .await
            .unwrap_err();
        assert!(matches!(
            e,
            crate::ingest::IngestError::Stale { gone: false }
        ));
        assert!(f.ref_names(&repo).await.is_empty());
        assert_eq!(op_results(&f).await, 0);
        // Under its new name the repository takes the write.
        let moved = a.open(&name("o/b")).await.unwrap();
        push_branch(&a, &f, &moved, "x", cs).await.unwrap();
        assert_eq!(f.ref_names(&repo).await, ["refs/heads/x"]);
        // A delete addressed to the old name finds nothing, and the renamed repository stays.
        let e = f.call("o/a", Op::Delete(true)).await.unwrap_err();
        assert_eq!(e.code(), tonic::Code::NotFound);
        assert!(f.registry.open(&name("o/b")).await.is_ok());
    }

    #[tokio::test]
    async fn a_rename_and_a_ref_update_racing_leave_one_consistent_outcome() {
        let f = fx().await;
        for i in 0..8 {
            let (from, to) = (format!("o/from{i}"), format!("o/to{i}"));
            let repo = f.init(&from).await;
            let cs = commit(&repo, &[], &[("a", b"1")]).await;
            let (push, renamed) = tokio::join!(
                push_branch(&f.registry, &f, &repo, "x", cs),
                f.call(&from, Op::RenameTo(to.clone())),
            );
            // The rename names the row it resolved, so nothing else can win over it.
            renamed.unwrap();
            let refs = f.ref_names(&repo).await;
            match push {
                // The update committed first: the repository carries it to its new name.
                Ok(()) => assert_eq!(refs, ["refs/heads/x"], "{i}"),
                // The rename committed first: the update failed whole.
                Err(e) => {
                    assert_eq!(e.code(), tonic::Code::FailedPrecondition, "{i}");
                    assert!(refs.is_empty(), "{i}");
                }
            }
            assert_eq!(f.registry.open(&name(&to)).await.unwrap().key, repo.key);
        }
        // The other order is fixed: once the rename has committed the update cannot apply.
        let repo = f.init("o/late").await;
        let cs = commit(&repo, &[], &[("a", b"1")]).await;
        f.call("o/late", Op::RenameTo("o/later".into()))
            .await
            .unwrap();
        let e = push_branch(&f.registry, &f, &repo, "x", cs)
            .await
            .unwrap_err();
        assert_eq!(e.code(), tonic::Code::FailedPrecondition);
    }

    // -------------------------------------------------------------------------- mirror targets

    fn mirror_admin(f: &Fx, tmp: &std::path::Path) -> (Arc<AdminService>, Arc<Mirror>) {
        use crate::mirror::MirrorConfig;
        use asc_git::realgit::RealGit;
        let mirror = Arc::new(Mirror::new(
            f.registry.clone(),
            &f.pipeline,
            RealGit::resolve(None, None).expect("git is installed"),
            MirrorConfig {
                dir: tmp.join("mirrors"),
                secret: None,
                forgejo_url: Some("https://forge.example".into()),
                allowed_hosts: Vec::new(),
                interval: Duration::from_secs(60),
                fetch_timeout: Duration::from_secs(60),
            },
        ));
        (
            AdminService::new(f.registry.clone(), Some(mirror.clone())),
            mirror,
        )
    }

    async fn admin_call(
        admin: &Arc<AdminService>,
        n: &str,
        op: Op,
    ) -> Result<wire::RepoAdminResponse, Status> {
        admin
            .repo_admin(
                &ctx(n),
                wire::RepoAdminRequest {
                    repo: repo_ref(n),
                    op: Some(op),
                },
            )
            .await
    }

    fn init_mirror(url: &str) -> Op {
        Op::Init(wire::InitOp {
            mode: "mirror".into(),
            mirror_url: url.into(),
            ..Default::default()
        })
    }

    #[tokio::test]
    async fn mirror_targets_follow_a_rename_and_vanish_on_delete() {
        let f = fx().await;
        let tmp = tempfile::tempdir().unwrap();
        let (admin, mirror) = mirror_admin(&f, tmp.path());
        let target = |n: &str| mirror.sources().client_target(&name(n)).map(|t| t.url);
        let url = "https://forge.example/o/m.git";

        admin_call(&admin, "o/m", init_mirror(url)).await.unwrap();
        assert_eq!(target("o/m").as_deref(), Some(url));
        // An init that finds the name taken leaves the live repository's target alone.
        let e = admin_call(
            &admin,
            "o/m",
            init_mirror("https://forge.example/o/other.git"),
        )
        .await
        .unwrap_err();
        assert_eq!(e.code(), tonic::Code::AlreadyExists);
        assert_eq!(target("o/m").as_deref(), Some(url));

        admin_call(&admin, "o/m", Op::RenameTo("o/n".into()))
            .await
            .unwrap();
        assert_eq!(target("o/m"), None);
        assert_eq!(target("o/n").as_deref(), Some(url));

        admin_call(&admin, "o/n", Op::Delete(true)).await.unwrap();
        assert_eq!(target("o/n"), None);

        // A target left under a name (an earlier process, a repository gone) is cleared by init.
        mirror
            .sources()
            .set(
                &name("o/n"),
                crate::mirror::MirrorTarget {
                    url: "https://forge.example/o/stale.git".into(),
                    token: None,
                },
            )
            .unwrap();
        admin_call(&admin, "o/n", init_mirror("")).await.unwrap();
        assert_eq!(target("o/n"), None);
        mirror
            .sources()
            .set(
                &name("o/q"),
                crate::mirror::MirrorTarget {
                    url: "https://forge.example/o/stale.git".into(),
                    token: None,
                },
            )
            .unwrap();
        admin_call(&admin, "o/q", init_mirror("https://forge.example/o/q.git"))
            .await
            .unwrap();
        assert_eq!(
            target("o/q").as_deref(),
            Some("https://forge.example/o/q.git")
        );
    }

    // -------------------------------------------------------------------------- name-keyed access

    async fn deploy_key(f: &Fx, id: &str, repo: &str) {
        sqlx::query(
            "INSERT INTO ssh_keys (id, kind, principal, repo_id, algo, public_key, fingerprint, \
             created) VALUES ($1, 'deploy', 'x', $2, 'ssh-ed25519', $3, $4, 1)",
        )
        .bind(id.to_owned())
        .bind(repo.to_owned())
        .bind(id.as_bytes().to_vec())
        .bind(id.as_bytes().to_vec())
        .execute(f.registry.pool())
        .await
        .unwrap();
    }

    async fn live_keys(f: &Fx, repo: &str) -> Vec<String> {
        sqlx::query_scalar(
            "SELECT id FROM ssh_keys WHERE repo_id = $1 AND revoked IS NULL ORDER BY id",
        )
        .bind(repo.to_owned())
        .fetch_all(f.registry.pool())
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn a_deleted_and_recreated_name_does_not_inherit_tokens_or_deploy_keys() {
        use crate::authz::token_store::{
            NewToken, TokenKey, TokenKind, TokenStore, TokenStoreAuthz,
        };
        use asc_rpc::auth::{Access, AuthError, Authorizer, BearerToken};

        let f = fx().await;
        f.init("o/r").await;
        let store = Arc::new(TokenStore::new(
            f.registry.pool().clone(),
            TokenKey::generate().unwrap(),
        ));
        let authz = TokenStoreAuthz::new(store.clone());
        let mint = |kind, user: Option<&str>, repos: &[&str]| {
            let store = store.clone();
            let (user, repos) = (
                user.map(str::to_owned),
                repos.iter().map(|r| (*r).to_owned()).collect::<Vec<_>>(),
            );
            async move {
                store
                    .create(&NewToken {
                        kind,
                        user,
                        repos,
                        access: Access::Write,
                        expires_in: None,
                        label: String::new(),
                    })
                    .await
                    .unwrap()
                    .secret()
                    .to_owned()
            }
        };
        let pat = mint(TokenKind::Pat, Some("alice"), &["o/r"]).await;
        let deploy = mint(TokenKind::Deploy, None, &["o/r"]).await;
        let wild = mint(TokenKind::Pat, Some("bob"), &["o/*"]).await;
        let check = |t: &str, repo: &str| {
            let t = BearerToken::new(t).unwrap();
            let authz = &authz;
            let repo = repo.to_owned();
            async move { authz.authorize(Some(&t), &repo, Access::Read).await }
        };
        deploy_key(&f, "k1", "o/r").await;
        deploy_key(&f, "k2", "o/other").await;
        // All three work, and now sit in the token store's cache.
        for t in [&pat, &deploy, &wild] {
            check(t, "o/r").await.unwrap();
        }

        f.call("o/r", Op::Delete(true)).await.unwrap();
        // Its tokens and keys are gone with it; the cache does not outlive the delete.
        assert!(live_keys(&f, "o/r").await.is_empty());
        assert_eq!(live_keys(&f, "o/other").await, ["k2"]);
        f.init("o/r").await;
        assert_eq!(check(&pat, "o/r").await, Err(AuthError::Forbidden));
        assert!(check(&deploy, "o/r").await.is_err());
        // A pattern grant is configuration of the owner, not of one repository: it stays.
        check(&wild, "o/r").await.unwrap();
    }

    #[tokio::test]
    async fn rename_moves_access_and_a_wildcard_is_untouched() {
        use crate::authz::token_store::{
            NewToken, TokenKey, TokenKind, TokenStore, TokenStoreAuthz,
        };
        use asc_rpc::auth::{Access, AuthError, Authorizer, BearerToken};

        let f = fx().await;
        f.init("o/a").await;
        let store = Arc::new(TokenStore::new(
            f.registry.pool().clone(),
            TokenKey::generate().unwrap(),
        ));
        let authz = TokenStoreAuthz::new(store.clone());
        let mut secrets = Vec::new();
        for (kind, user, repos) in [
            (TokenKind::Pat, Some("alice"), vec!["o/a", "o/keep"]),
            (TokenKind::Deploy, None, vec!["o/a"]),
            (TokenKind::Pat, Some("bob"), vec!["o/*"]),
        ] {
            secrets.push(
                store
                    .create(&NewToken {
                        kind,
                        user: user.map(str::to_owned),
                        repos: repos.iter().map(|r| (*r).to_owned()).collect(),
                        access: Access::Write,
                        expires_in: None,
                        label: String::new(),
                    })
                    .await
                    .unwrap()
                    .secret()
                    .to_owned(),
            );
        }
        let [pat, deploy, wild] = [&secrets[0], &secrets[1], &secrets[2]];
        let check = |t: &str, repo: &str| {
            let t = BearerToken::new(t).unwrap();
            let authz = &authz;
            let repo = repo.to_owned();
            async move { authz.authorize(Some(&t), &repo, Access::Read).await }
        };
        deploy_key(&f, "k1", "o/a").await;
        for t in [pat, deploy, wild] {
            check(t, "o/a").await.unwrap();
        }

        f.call("o/a", Op::RenameTo("o/b".into())).await.unwrap();
        assert_eq!(live_keys(&f, "o/a").await, Vec::<String>::new());
        assert_eq!(live_keys(&f, "o/b").await, ["k1"]);
        // Access moved with the repository; a new repository on the old name has none.
        check(pat, "o/b").await.unwrap();
        check(deploy, "o/b").await.unwrap();
        check(pat, "o/keep").await.unwrap();
        assert!(check(deploy, "o/a").await.is_err());
        f.init("o/a").await;
        assert_eq!(check(pat, "o/a").await, Err(AuthError::Forbidden));
        assert!(check(deploy, "o/a").await.is_err());
        // The wildcard is the same row as before: it covers both names by pattern.
        check(wild, "o/a").await.unwrap();
        check(wild, "o/b").await.unwrap();
        let wild_rows: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM auth_token_grants WHERE repo = 'o/*'")
                .fetch_one(f.registry.pool())
                .await
                .unwrap();
        assert_eq!(wild_rows, 1);
    }

    #[tokio::test]
    async fn init_and_rename_refuse_a_name_that_still_has_grants_or_keys() {
        let f = fx().await;
        f.init("o/a").await;
        // Access recorded for a name that is not a repository yet (or any more).
        sqlx::query(
            "INSERT INTO auth_token_grants (token_id, repo, access) VALUES ('t', 'o/new', 'write')",
        )
        .execute(f.registry.pool())
        .await
        .unwrap();
        deploy_key(&f, "k9", "o/fork").await;
        for n in ["o/new", "o/fork"] {
            let e = f
                .call(n, Op::Init(wire::InitOp::default()))
                .await
                .unwrap_err();
            assert_eq!(e.code(), tonic::Code::FailedPrecondition, "{n}");
            assert!(f.registry.open(&name(n)).await.is_err(), "{n}");
            let e = f.call("o/a", Op::RenameTo(n.into())).await.unwrap_err();
            assert_eq!(e.code(), tonic::Code::FailedPrecondition, "{n}");
        }
        // The refused rename changed nothing.
        assert!(f.registry.open(&name("o/a")).await.is_ok());
        // A revoked key does not count.
        sqlx::query("UPDATE ssh_keys SET revoked = 1 WHERE id = 'k9'")
            .execute(f.registry.pool())
            .await
            .unwrap();
        f.call("o/fork", Op::Init(wire::InitOp::default()))
            .await
            .unwrap();
    }

    // -------------------------------------------------------------------------- size, verify

    #[tokio::test]
    async fn size_counts_reachable_bytes_and_all_bytes() {
        let f = fx().await;
        let s = source(&f).await;
        let r = f.call("o/src", Op::Size(true)).await.unwrap();
        let listing = f.listing(s.entry.ns).await;
        let all: u64 = listing.iter().map(|(_, n)| n).sum();
        assert_eq!(r.unique_size, all);
        // Reachable from any ref: a, b, and the draft (the user ref and the pull ref), with
        // every tree under their roots.
        let mut keys: HashSet<String> = HashSet::new();
        for id in [s.a, s.b, s.draft] {
            keys.extend(closure_of(&s.entry, id).await.iter().map(ObjKey::path));
            let root = s
                .entry
                .backend
                .engine()
                .get_changeset(id)
                .await
                .unwrap()
                .root;
            let mut trees = vec![root];
            while let Some(t) = trees.pop() {
                keys.insert(ObjKey::Tree(t).path());
                let tree = s.entry.backend.engine().get_tree(t).await.unwrap();
                for e in tree.entries() {
                    match e.kind {
                        Kind::Dir => trees.push(TreeId::from_bytes(e.id)),
                        Kind::File | Kind::Exec | Kind::Symlink => {
                            keys.insert(ObjKey::File(FileId::from_bytes(e.id)).path());
                        }
                        Kind::Gitlink => {}
                    }
                }
            }
        }
        let reachable: u64 = listing
            .iter()
            .filter(|(k, _)| keys.contains(k))
            .map(|(_, n)| n)
            .sum();
        assert!(reachable > 0 && reachable < all);
        assert_eq!(r.logical_size, reachable);
        // The orphan and the derived values are in `unique` only.
        let orphan = listing
            .iter()
            .find(|(k, _)| *k == s.orphan.path())
            .unwrap()
            .1;
        assert!(r.unique_size - r.logical_size >= orphan);
        assert!(listing.iter().any(|(k, _)| *k == s.derived.path()));
    }

    #[tokio::test]
    async fn verify_passes_a_sound_repository() {
        let f = fx().await;
        source(&f).await;
        let r = f.call("o/src", Op::Verify(true)).await.unwrap();
        assert!(r.report.starts_with("ok: "), "{}", r.report);
    }

    #[tokio::test]
    async fn verify_reports_missing_corrupt_and_unmapped_objects() {
        let f = fx().await;
        let s = source(&f).await;
        let cs = s.entry.backend.engine().get_changeset(s.b).await.unwrap();
        let file = closure_of(&s.entry, s.b).await[2];
        f.remove_raw(s.entry.ns, &file).await;
        let r = f.call("o/src", Op::Verify(true)).await.unwrap();
        assert!(r.report.starts_with("FAILED: "), "{}", r.report);
        assert!(
            r.report.contains(&format!("missing: {}", file.path())),
            "{}",
            r.report
        );

        let tree = ObjKey::Tree(cs.root);
        f.put_raw(s.entry.ns, &tree, encode_stored(b"junk")).await;
        let r = f.call("o/src", Op::Verify(true)).await.unwrap();
        assert!(
            r.report.contains(&format!("corrupt: {}", tree.path())),
            "{}",
            r.report
        );

        sqlx::query("DELETE FROM git_map WHERE ns = $1 AND kind = 3")
            .bind(s.entry.ns.as_bytes().to_vec())
            .execute(f.registry.pool())
            .await
            .unwrap();
        let r = f.call("o/src", Op::Verify(true)).await.unwrap();
        assert!(r.report.contains("git map: "), "{}", r.report);
        assert!(
            r.report.contains("no row for this changeset"),
            "{}",
            r.report
        );
    }

    #[tokio::test]
    async fn verify_catches_a_git_map_that_points_elsewhere() {
        let f = fx().await;
        let s = source(&f).await;
        sqlx::query("UPDATE git_map SET git = $1 WHERE ns = $2 AND kind = 3 AND asc = $3")
            .bind(vec![7u8; 20])
            .bind(s.entry.ns.as_bytes().to_vec())
            .bind(s.b.as_bytes().to_vec())
            .execute(f.registry.pool())
            .await
            .unwrap();
        let r = f.call("o/src", Op::Verify(true)).await.unwrap();
        assert!(r.report.contains("not the derived one"), "{}", r.report);
    }

    #[tokio::test]
    async fn an_empty_repository_has_size_zero_and_verifies() {
        let f = fx().await;
        let e = f.init("o/empty").await;
        let r = f.call("o/empty", Op::Size(true)).await.unwrap();
        assert_eq!(r.logical_size, 0);
        assert_eq!(r.unique_size, 0, "{:?}", f.listing(e.ns).await);
        let r = f.call("o/empty", Op::Verify(true)).await.unwrap();
        assert!(r.report.starts_with("ok: 0 objects"), "{}", r.report);
    }

    #[test]
    fn land_branch_lists() {
        let l = |v: &[&str], d: &str| {
            land_branches(&v.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>(), d)
        };
        assert_eq!(l(&[], "master").unwrap(), vec!["master"]);
        assert_eq!(
            l(&["a", "refs/heads/b", "a"], "x").unwrap(),
            vec!["a", "refs/heads/b"]
        );
        for bad in ["refs/tags/t", "refs/", "-a", "a b", "", "a/../b", "HEAD"] {
            assert!(l(&[bad], "master").is_err(), "{bad}");
        }
    }

    #[test]
    fn a_raw_commit_names_its_tree() {
        let raw = b"tree 0123456789abcdef0123456789abcdef01234567\nauthor x\n";
        let oid = raw_commit_tree(raw).unwrap();
        assert_eq!(oid.to_string(), "0123456789abcdef0123456789abcdef01234567");
        assert!(raw_commit_tree(b"tree 0123").is_none());
        assert!(raw_commit_tree(b"tree ZZZZ456789abcdef0123456789abcdef01234567").is_none());
        assert!(raw_commit_tree(b"parent x").is_none());
    }
}
