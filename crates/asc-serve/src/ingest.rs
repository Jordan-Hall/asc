//! Server ingest verification (SPEC §8.4, §2.6, §2.8; PLAN U43b).
//!
//! [`Ingestor::ingest`] is `UpdateRefs` pipeline step 4: it walks the new changesets of a push
//! until it meets one already in `cs_graph`, validates each in topological order, derives its git
//! ids eagerly, and records the graph rows. Nothing a client says about sizes or git ids is
//! trusted: file sizes and blob oids come from `file_meta`, which the server computes from the
//! bytes. A new tree is compared with the corresponding tree of `parents[0]`, so only changed
//! entries are looked at and the cost is O(changed paths x depth).

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use asc_core::change::RewriteOp;
use asc_core::codec::decode_envelope;
use asc_core::content::ChunkAssembler;
use asc_core::git::{BlobOidHasher, ObjectKind, blob_oid, export_tag, object_oid, tree_oid};
use asc_core::id::{ChangesetId, FileId, GitOid, RefTarget, TreeId};
use asc_core::object::{Change, Changeset, ChunkList, Kind, TagRecord, Tree};
use asc_core::path::{MAX_TREE_DEPTH, Name, RepoPath, casefold, checkout_safe, windows_valid};
use asc_engine::Engine;
use asc_engine::commit;
use asc_engine::derive::{self, GitMapRow, GitMapSink, index_commit_git, index_tree_git};
use asc_engine::tree;
use asc_store::bucket::BucketObjects;
use asc_store::key::ObjKey;
use asc_store::objects::{self, Objects};
use asc_store::refs::RepoKey;
use asc_store::sqlrefs::SqlRefs;
use async_trait::async_trait;
use sqlx::any::Any;
use sqlx::{AnyPool, Row, Transaction};

use crate::graph_sql::{self, NewNode};
use crate::registry::{Ns, RepoEntry};

/// Most changesets one push may add.
pub const MAX_NEW_CHANGESETS: usize = 200_000;
/// Most tree entries examined for one changeset.
const MAX_CHECK_ENTRIES: usize = 4_000_000;
/// Most leaves one changeset's `changes` may list. `changes` is checked against the tree diff
/// (SPEC §2.6 rule 3), so a changeset whose diff has more leaves cannot be verified and is
/// refused, not accepted unverified: no consumer could tell the two apart.
pub const MAX_VERIFIED_CHANGES: usize = 500_000;
/// Most missing keys reported at once.
const MAX_MISSING: usize = 4096;
/// Changesets read per `cs_graph` lookup while walking the closure.
const DISCOVER_BATCH: usize = 256;
/// File ids per `file_meta` lookup.
const META_BATCH: usize = 500;
/// Longest accepted raw git tree body for an override.
const MAX_OVERRIDE_BYTES: usize = 64 << 20;

type RepoEngine = Engine<BucketObjects, SqlRefs>;

/// Why an ingest was refused.
#[derive(Debug, thiserror::Error)]
pub enum IngestError {
    /// Objects the push refers to are not stored (`MISSING_OBJECTS`).
    #[error("missing objects: {0:?}")]
    Missing(Vec<ObjKey>),
    /// An object breaks a rule (`INVALID_OBJECT`).
    #[error("invalid object {key}: {why}")]
    Invalid { key: String, why: String },
    /// A path rule of SPEC §2.8 failed (`PATH_POLICY`).
    #[error("path policy: {path}: {why}")]
    PathPolicy { path: String, why: &'static str },
    #[error("push adds more than {MAX_NEW_CHANGESETS} changesets")]
    TooLarge,
    /// The new changesets cost more than the pusher has left (SPEC §21.3.7, `QUOTA_EXCEEDED`);
    /// nothing was recorded.
    #[error("{0}")]
    QuotaExceeded(String),
    #[error("store: {0}")]
    Store(String),
    #[error("database: {0}")]
    Sql(#[from] sqlx::Error),
    /// The repository was deleted (`gone`) or renamed since the request resolved it; nothing was
    /// written.
    #[error("the repository changed (gone: {gone})")]
    Stale { gone: bool },
}

fn invalid(key: impl std::fmt::Display, why: impl Into<String>) -> IngestError {
    IngestError::Invalid {
        key: key.to_string(),
        why: why.into(),
    }
}

fn from_store(e: objects::Error) -> IngestError {
    match e {
        objects::Error::Corrupt(k) | objects::Error::Codec(k, _) => {
            invalid(k, "does not match its key")
        }
        e => IngestError::Store(e.to_string()),
    }
}

fn from_tree(e: tree::Error) -> IngestError {
    match e {
        tree::Error::MissingTree(t) => IngestError::Missing(vec![ObjKey::Tree(t)]),
        tree::Error::Store(e) => from_store(e),
        e => invalid("tree", e.to_string()),
    }
}

fn from_commit(e: commit::Error) -> IngestError {
    match e {
        commit::Error::MissingChangeset(c) | commit::Error::MissingParent(c) => {
            IngestError::Missing(vec![ObjKey::Changeset(c)])
        }
        commit::Error::Store(e) => from_store(e),
        commit::Error::Tree(e) => from_tree(e),
        e => invalid("changeset", e.to_string()),
    }
}

fn from_derive(e: derive::Error) -> IngestError {
    match e {
        derive::Error::Store(e) => from_store(e),
        derive::Error::Commit(e) => from_commit(e),
        derive::Error::Tree(e) => from_tree(e),
        derive::Error::Git(e) => invalid("changeset", e.to_string()),
        e => IngestError::Store(e.to_string()),
    }
}

// ------------------------------------------------------------------------------ observer

/// Why an [`IngestObserver`] refused or failed.
#[derive(Debug, thiserror::Error)]
pub enum ObserverError {
    /// The pushed data breaks a rule (`INVALID_OBJECT`).
    #[error("invalid object {key}: {why}")]
    Invalid { key: String, why: String },
    #[error("{0}")]
    Failed(String),
    #[error("database: {0}")]
    Sql(#[from] sqlx::Error),
}

impl From<ObserverError> for IngestError {
    fn from(e: ObserverError) -> Self {
        match e {
            ObserverError::Invalid { key, why } => IngestError::Invalid { key, why },
            ObserverError::Failed(m) => IngestError::Store(m),
            ObserverError::Sql(e) => IngestError::Sql(e),
        }
    }
}

/// Hook for stack metadata (SPEC §20.4): called inside the transaction that records the graph
/// rows (`on_ingested`) and inside the step-8 transaction (`on_rewritten`). C18 registers the
/// real one (it checks that every `ChangeMeta` names a changeset in `closure`).
#[async_trait]
pub trait IngestObserver: Send + Sync {
    /// `new` are the changesets this ingest added, parents first. `closure` holds their keys and
    /// the keys of any tags the push carries.
    async fn on_ingested(
        &self,
        _tx: &mut Transaction<'_, Any>,
        _repo: RepoKey,
        _new: &[ChangesetId],
        _closure: &[ObjKey],
    ) -> Result<(), ObserverError> {
        Ok(())
    }

    /// `pairs` are `(old, new)` ids of changesets the server rewrote (land, merge).
    async fn on_rewritten(
        &self,
        _tx: &mut Transaction<'_, Any>,
        _repo: RepoKey,
        _pairs: &[(ChangesetId, ChangesetId)],
        _kind: RewriteOp,
    ) -> Result<(), ObserverError> {
        Ok(())
    }
}

/// The default observer: does nothing.
pub struct NoObserver;

impl IngestObserver for NoObserver {}

// ------------------------------------------------------------------------------ file_meta

/// What the server computed for a file's bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FileMeta {
    pub size: u64,
    /// Git blob oid.
    pub git: GitOid,
}

/// `file_meta` rows for one namespace, filled from the stored bytes on first use.
struct FileMetaStore {
    pool: AnyPool,
    ns: Ns,
    objects: Arc<BucketObjects>,
    cache: HashMap<FileId, FileMeta>,
    probed: HashSet<FileId>,
    lookups: u64,
}

impl FileMetaStore {
    fn new(pool: AnyPool, ns: Ns, objects: Arc<BucketObjects>) -> Self {
        Self {
            pool,
            ns,
            objects,
            cache: HashMap::new(),
            probed: HashSet::new(),
            lookups: 0,
        }
    }

    /// Loads the rows of `ids` that are stored, one query per [`META_BATCH`].
    async fn prefetch(&mut self, ids: impl IntoIterator<Item = FileId>) -> Result<(), IngestError> {
        let want: Vec<FileId> = ids
            .into_iter()
            .filter(|id| self.probed.insert(*id))
            .collect();
        for chunk in want.chunks(META_BATCH) {
            self.lookups += chunk.len() as u64;
            let marks = (0..chunk.len())
                .map(|i| format!("${}", i + 2))
                .collect::<Vec<_>>()
                .join(",");
            let sql = format!(
                "SELECT file, size, git FROM file_meta WHERE ns = $1 AND file IN ({marks})"
            );
            let mut q =
                sqlx::query(sqlx::AssertSqlSafe(sql.clone())).bind(self.ns.as_bytes().to_vec());
            for id in chunk {
                q = q.bind(id.as_bytes().to_vec());
            }
            for row in q.fetch_all(&self.pool).await? {
                let file: Vec<u8> = row.try_get("file")?;
                let size: i64 = row.try_get("size")?;
                let git: Vec<u8> = row.try_get("git")?;
                let (Ok(file), Ok(git), Ok(size)) = (
                    <[u8; 32]>::try_from(file.as_slice()),
                    <[u8; 20]>::try_from(git.as_slice()),
                    u64::try_from(size),
                ) else {
                    continue; // a damaged row is recomputed below
                };
                self.cache.insert(
                    FileId::from_bytes(file),
                    FileMeta {
                        size,
                        git: GitOid::from_bytes(git),
                    },
                );
            }
        }
        Ok(())
    }

    /// The metadata of `file`, computing and storing it when no row exists.
    async fn get(&mut self, file: FileId) -> Result<FileMeta, IngestError> {
        self.prefetch([file]).await?;
        if let Some(m) = self.cache.get(&file) {
            return Ok(*m);
        }
        let meta = self.compute(file).await?;
        sqlx::query(
            "INSERT INTO file_meta (ns, file, size, git) VALUES ($1, $2, $3, $4) \
             ON CONFLICT (ns, file) DO NOTHING",
        )
        .bind(self.ns.as_bytes().to_vec())
        .bind(file.as_bytes().to_vec())
        .bind(
            i64::try_from(meta.size)
                .map_err(|_| invalid(ObjKey::File(file), "file is too large"))?,
        )
        .bind(meta.git.as_bytes().to_vec())
        .execute(&self.pool)
        .await?;
        self.cache.insert(file, meta);
        Ok(meta)
    }

    /// Size and blob oid from the stored bytes; a chunked file is streamed once.
    async fn compute(&self, file: FileId) -> Result<FileMeta, IngestError> {
        let missing = |k: ObjKey| IngestError::Missing(vec![k]);
        if let Some(data) = self
            .objects
            .get(&ObjKey::File(file))
            .await
            .map_err(from_store)?
        {
            return Ok(FileMeta {
                size: data.len() as u64,
                git: blob_oid(&data),
            });
        }
        let list_key = ObjKey::ChunkList(file);
        let list_bytes = self
            .objects
            .get(&list_key)
            .await
            .map_err(from_store)?
            .ok_or_else(|| missing(ObjKey::File(file)))?;
        let list: ChunkList =
            decode_envelope(&list_bytes).map_err(|e| invalid(list_key, e.to_string()))?;
        let size = list.size;
        let ids: Vec<_> = list.chunks.iter().map(|&(id, _)| id).collect();
        let mut assembler =
            ChunkAssembler::new(list, file).map_err(|e| invalid(list_key, e.to_string()))?;
        let mut hasher = BlobOidHasher::new(size);
        for id in ids {
            let chunk = self
                .objects
                .get(&ObjKey::Chunk(id))
                .await
                .map_err(from_store)?
                .ok_or_else(|| missing(ObjKey::Chunk(id)))?;
            assembler
                .push(&chunk)
                .map_err(|e| invalid(list_key, e.to_string()))?;
            hasher.update(&chunk);
        }
        assembler
            .finish()
            .map_err(|e| invalid(list_key, e.to_string()))?;
        let git = hasher
            .finalize()
            .map_err(|e| invalid(list_key, e.to_string()))?;
        Ok(FileMeta { size, git })
    }
}

// ------------------------------------------------------------------------------ ingest

/// Ingests the closure of some ref targets into one repository.
pub struct Ingestor<'a> {
    entry: &'a RepoEntry,
    pool: &'a AnyPool,
    refs: &'a SqlRefs,
    engine: Arc<RepoEngine>,
    files: FileMetaStore,
    windows_safe: bool,
    /// Cap on the leaves of one changeset's `changes`; [`MAX_VERIFIED_CHANGES`] in production.
    max_changes: usize,
    /// Trees fully checked during this ingest.
    verified: HashSet<TreeId>,
    /// The verified owner id of the principal ingesting (0 for none), recorded in `cs_ledger`.
    ingester: i64,
    /// Most bytes the new changesets may cost (SPEC §21.3.7); `None` for no limit here.
    budget: Option<u64>,
    /// [`changeset_cost`] of every changeset checked.
    costs: HashMap<ChangesetId, u64>,
    /// What the last successful [`Ingestor::ingest`] recorded.
    added_bytes: u64,
    /// The principal whose staged change metas (`changes::staged`) join the closure of this ingest.
    staged_for: Option<String>,
}

impl<'a> Ingestor<'a> {
    /// `windows_safe` is the repository's `repos.windows_safe` (SPEC §2.8).
    pub fn new(
        pool: &'a AnyPool,
        refs: &'a SqlRefs,
        entry: &'a RepoEntry,
        windows_safe: bool,
    ) -> Self {
        Self {
            entry,
            pool,
            refs,
            engine: entry.backend.engine().clone(),
            files: FileMetaStore::new(pool.clone(), entry.ns, entry.objects.clone()),
            windows_safe,
            max_changes: MAX_VERIFIED_CHANGES,
            verified: HashSet::new(),
            ingester: 0,
            budget: None,
            costs: HashMap::new(),
            added_bytes: 0,
            staged_for: None,
        }
    }

    /// Adds to the closure the change metas `principal` uploaded for the changesets this ingest
    /// adds (SPEC section 20.2). Without it the `m/` objects of a push are stored and never
    /// indexed.
    pub fn staged_for(mut self, principal: &str) -> Self {
        self.staged_for = Some(principal.to_owned());
        self
    }

    /// Records `owner`, the verified owner id of the ingesting principal, in `cs_ledger` for every
    /// changeset this ingest adds, so what stays unreferenced is attributable and whoever later
    /// names it from a user ref pays for it.
    pub fn by(mut self, owner: Option<i64>) -> Self {
        self.ingester = owner.filter(|&o| o > 0).unwrap_or(0);
        self
    }

    /// Refuses (with [`IngestError::QuotaExceeded`], before any row is written) an ingest whose
    /// new changesets cost more than `bytes`.
    pub fn with_budget(mut self, bytes: u64) -> Self {
        self.budget = Some(bytes);
        self
    }

    /// The bytes the last successful ingest charged to `cs_ledger`: the sum of
    /// [`changeset_cost`] over the changesets it added.
    pub fn added_bytes(&self) -> u64 {
        self.added_bytes
    }

    /// Lowers the `changes` cap, so a test can reach it without half a million leaves.
    #[cfg(test)]
    fn with_max_changes(mut self, max: usize) -> Self {
        self.max_changes = max;
        self
    }

    /// `file_meta` rows this ingest asked for (cache misses of one query per id).
    pub fn file_meta_lookups(&self) -> u64 {
        self.files.lookups
    }

    /// Pipeline step 4. Returns the changesets added, parents first. Changesets already in
    /// `cs_graph` (from `Ingest`/`Commit`) are skipped. The graph rows and `observer` run in one
    /// transaction; git-map rows are idempotent and written first.
    pub async fn ingest(
        &mut self,
        targets: &[RefTarget],
        observer: &dyn IngestObserver,
    ) -> Result<Vec<ChangesetId>, IngestError> {
        let mut roots = Vec::new();
        let mut tags = Vec::new();
        for t in targets {
            match *t {
                RefTarget::Changeset(c) => roots.push(c),
                RefTarget::Tag(t) => {
                    let tag = self.read_tag(t).await?;
                    roots.push(tag.target);
                    tags.push((t, tag));
                }
            }
        }
        let (parents_of, known_gen) = self.discover(&roots).await?;
        let mut missing = Vec::new();
        let order = topological(&parents_of)?;
        let mut generations = known_gen;
        let mut nodes = Vec::with_capacity(order.len());
        for &id in &order {
            let parents = &parents_of[&id];
            let generation = parents
                .iter()
                .map(|p| {
                    generations
                        .get(p)
                        .copied()
                        .ok_or_else(|| invalid(ObjKey::Changeset(id), "unknown parent"))
                })
                .try_fold(0u32, |m, g| g.map(|g| m.max(g)))?
                .saturating_add(1);
            generations.insert(id, generation);
            nodes.push((id, generation));
            match self.check_and_index(id).await {
                Ok(()) => {}
                Err(IngestError::Missing(keys)) => {
                    missing.extend(keys);
                    if missing.len() >= MAX_MISSING {
                        break;
                    }
                }
                Err(e) => return Err(e),
            }
        }
        if !missing.is_empty() {
            missing.sort_by_key(ObjKey::path);
            missing.dedup();
            missing.truncate(MAX_MISSING);
            return Err(IngestError::Missing(missing));
        }
        for (t, tag) in &tags {
            self.index_tag(*t, tag).await?;
        }
        if order.is_empty() {
            self.added_bytes = 0;
            return Ok(order);
        }
        let ledger: Vec<(ChangesetId, u64)> = order
            .iter()
            .map(|id| (*id, self.costs.get(id).copied().unwrap_or(0)))
            .collect();
        let cost = ledger.iter().fold(0u64, |a, (_, b)| a.saturating_add(*b));
        if let Some(budget) = self.budget
            && cost > budget
        {
            return Err(IngestError::QuotaExceeded("user byte quota".into()));
        }
        let rows: Vec<NewNode<'_>> = nodes
            .iter()
            .map(|&(id, generation)| NewNode {
                id,
                generation,
                parents: &parents_of[&id],
            })
            .collect();
        let mut closure: Vec<ObjKey> = order.iter().map(|&c| ObjKey::Changeset(c)).collect();
        closure.extend(tags.iter().map(|(t, _)| ObjKey::Tag(*t)));
        if let Some(principal) = &self.staged_for {
            let metas = crate::changes::staged::take(self.pool, self.entry.ns, principal, &order)
                .await
                .map_err(|e| IngestError::Store(e.to_string()))?;
            closure.extend(metas.into_iter().map(ObjKey::ChangeMeta));
        }
        let mut tx = self
            .refs
            .begin()
            .await
            .map_err(|e| IngestError::Store(e.to_string()))?;
        crate::registry::guard_repo(&mut tx, self.entry)
            .await
            .map_err(|s| match s.code() {
                tonic::Code::NotFound => IngestError::Stale { gone: true },
                tonic::Code::FailedPrecondition => IngestError::Stale { gone: false },
                _ => IngestError::Store(s.message().to_owned()),
            })?;
        graph_sql::insert_nodes(&mut tx, self.entry.ns, &rows).await?;
        crate::cloud::quota::record(&mut tx, self.entry.ns, self.ingester, &ledger).await?;
        observer
            .on_ingested(&mut tx, self.entry.key, &order, &closure)
            .await?;
        if let Some(principal) = &self.staged_for {
            crate::changes::staged::consume(&mut tx, self.entry.ns, principal, &order)
                .await
                .map_err(|e| IngestError::Store(e.to_string()))?;
        }
        tx.commit().await?;
        self.added_bytes = cost;
        Ok(order)
    }

    async fn read_tag(&self, id: asc_core::id::TagId) -> Result<TagRecord, IngestError> {
        let key = ObjKey::Tag(id);
        let data = self
            .entry
            .objects
            .get(&key)
            .await
            .map_err(from_store)?
            .ok_or_else(|| IngestError::Missing(vec![key]))?;
        let tag: TagRecord = decode_envelope(&data).map_err(|e| invalid(key, e.to_string()))?;
        for (k, _) in tag.extra.as_slice() {
            match k.as_slice() {
                b"git.gpgsig" => {}
                b"git.tag" => return Err(invalid(key, "raw git tag extras are import-only")),
                other => {
                    return Err(invalid(
                        key,
                        format!(
                            "extra key {:?} is not allowed",
                            String::from_utf8_lossy(other)
                        ),
                    ));
                }
            }
        }
        Ok(tag)
    }

    async fn index_tag(&self, id: asc_core::id::TagId, tag: &TagRecord) -> Result<(), IngestError> {
        let target = self
            .engine
            .git_commit_of(tag.target)
            .await
            .map_err(from_derive)?;
        let raw = export_tag(tag, target).map_err(|e| invalid(ObjKey::Tag(id), e.to_string()))?;
        self.entry
            .git_map
            .put(&[GitMapRow {
                git: object_oid(ObjectKind::Tag, &raw),
                kind: ObjectKind::Tag,
                asc: *id.as_bytes(),
            }])
            .await
            .map_err(from_derive)
    }

    /// Walks parents from `roots` until a changeset is in `cs_graph`. Iterative and bounded by
    /// [`MAX_NEW_CHANGESETS`]. Returns the new changesets' parents, the known ancestors'
    /// generations, and fails with the changesets that are not stored.
    async fn discover(
        &self,
        roots: &[ChangesetId],
    ) -> Result<
        (
            HashMap<ChangesetId, Vec<ChangesetId>>,
            HashMap<ChangesetId, u32>,
        ),
        IngestError,
    > {
        let mut seen: HashSet<ChangesetId> = roots.iter().copied().collect();
        let mut frontier: Vec<ChangesetId> = seen.iter().copied().collect();
        let mut parents_of = HashMap::new();
        let mut known_gen = HashMap::new();
        let mut missing = Vec::new();
        while !frontier.is_empty() {
            let at = frontier.len().saturating_sub(DISCOVER_BATCH);
            let batch = frontier.split_off(at);
            let known = graph_sql::known(self.pool, self.entry.ns, &batch)
                .await
                .map_err(|e| IngestError::Store(e.to_string()))?;
            for id in batch {
                if let Some(g) = known.get(&id) {
                    known_gen.insert(id, *g);
                    continue;
                }
                if parents_of.len() >= MAX_NEW_CHANGESETS {
                    return Err(IngestError::TooLarge);
                }
                match self.engine.get_changeset(id).await {
                    Ok(cs) => {
                        for p in &cs.parents {
                            if seen.insert(*p) {
                                frontier.push(*p);
                            }
                        }
                        parents_of.insert(id, cs.parents.clone());
                    }
                    Err(commit::Error::MissingChangeset(_)) => {
                        if missing.len() < MAX_MISSING {
                            missing.push(ObjKey::Changeset(id));
                        }
                    }
                    Err(e) => return Err(from_commit(e)),
                }
            }
        }
        // A changeset whose parent is not stored cannot be ordered; report the parent.
        if !missing.is_empty() {
            return Err(IngestError::Missing(missing));
        }
        Ok((parents_of, known_gen))
    }

    /// Validates one changeset (SPEC §2.6 rules 1 to 6, §8.4, §2.8) and writes its git-map rows.
    async fn check_and_index(&mut self, id: ChangesetId) -> Result<(), IngestError> {
        let key = ObjKey::Changeset(id);
        let cs = self.engine.get_changeset(id).await.map_err(from_commit)?;
        check_extra(&cs, key)?;
        self.costs
            .insert(id, crate::cloud::quota::changeset_cost(&cs));
        let base = match cs.parents.first() {
            None => RepoEngine::empty_tree_id(),
            Some(&p) => {
                self.engine
                    .get_changeset(p)
                    .await
                    .map_err(from_commit)?
                    .root
            }
        };
        // Rules 2, 3 and 6: `changes` is exactly the diff against parents[0]. Every consumer
        // that reads `changes` (mainline path index, blame, history filters, land overlap,
        // project sets) trusts this check. The diff is bounded; past the cap the changeset is
        // refused (fail closed), because the stored object has no "unverified" marker.
        let (expected, _) = self
            .engine
            .leafdiff_limited(base, cs.root, None, tree::MAX_DIFF_VISITS, self.max_changes)
            .await
            .map_err(from_tree)?;
        if expected.len() > self.max_changes {
            return Err(invalid(
                key,
                format!(
                    "changes cannot be verified: the tree diff has more than {} leaves",
                    self.max_changes
                ),
            ));
        }
        compare_changes(&expected, cs.changes.as_slice(), key)?;
        let changed = self.check_trees(cs.root, base, key).await?;
        self.check_writes(&cs, base, key).await?;
        if self.windows_safe {
            self.check_windows_paths(&cs).await?;
        }
        self.engine.warm(id).await.map_err(from_derive)?;
        index_commit_git(&self.engine, id, &*self.entry.git_map)
            .await
            .map_err(from_derive)?;
        for t in changed {
            index_tree_git(&self.engine, t, &*self.entry.git_map)
                .await
                .map_err(from_derive)?;
        }
        Ok(())
    }

    /// Rules 4 and 5 for every `Write`: the file's recorded size and blob oid match `file_meta`,
    /// and a copy source exists in the first parent.
    async fn check_writes(
        &mut self,
        cs: &Changeset,
        base: TreeId,
        key: ObjKey,
    ) -> Result<(), IngestError> {
        let files = cs.changes.as_slice().iter().filter_map(|(_, c)| match c {
            Change::Write { file, .. } => Some(*file),
            _ => None,
        });
        self.files.prefetch(files).await?;
        for (path, change) in cs.changes.as_slice() {
            let Change::Write {
                file,
                size,
                git,
                copy_from,
                ..
            } = change
            else {
                continue;
            };
            let meta = self.files.get(*file).await?;
            if meta.size != *size || meta.git != *git {
                return Err(invalid(
                    key,
                    format!("size or git oid of {path} does not match its content"),
                ));
            }
            if let Some(src) = copy_from
                && self
                    .engine
                    .lookup(base, src)
                    .await
                    .map_err(from_tree)?
                    .is_none()
            {
                return Err(invalid(
                    key,
                    format!("copy source of {path} is not in the first parent"),
                ));
            }
        }
        Ok(())
    }

    /// Compares each new tree with the tree at the same place under `base` and verifies every
    /// entry that differs. Returns the trees that were new. Iterative, with a depth and entry
    /// bound.
    async fn check_trees(
        &mut self,
        root: TreeId,
        base: TreeId,
        key: ObjKey,
    ) -> Result<Vec<TreeId>, IngestError> {
        let mut changed = Vec::new();
        let mut budget = MAX_CHECK_ENTRIES;
        let mut stack = vec![(root, Some(base), 0usize)];
        while let Some((new_id, old_id, depth)) = stack.pop() {
            if Some(new_id) == old_id || !self.verified.insert(new_id) {
                continue;
            }
            if depth > MAX_TREE_DEPTH {
                return Err(invalid(key, "tree nesting is too deep"));
            }
            let new = self.engine.get_tree(new_id).await.map_err(from_tree)?;
            let old = match old_id {
                Some(o) => Some(self.engine.get_tree(o).await.map_err(from_tree)?),
                None => None,
            };
            budget = budget
                .checked_sub(new.entries().len())
                .ok_or_else(|| invalid(key, "changeset touches too many tree entries"))?;
            changed.push(new_id);
            let old_entries = old.as_deref().map_or(&[][..], Tree::entries);
            // Entries are sorted, so one merge pass pairs them with the old tree.
            let mut diff = Vec::new();
            let mut oi = 0;
            for e in new.entries() {
                while oi < old_entries.len() && old_entries[oi].name < e.name {
                    oi += 1;
                }
                let old_e = old_entries.get(oi).filter(|o| o.name == e.name);
                if old_e != Some(e) {
                    diff.push((e, old_e));
                }
            }
            self.files
                .prefetch(
                    diff.iter()
                        .filter(|(e, _)| is_file(e.kind))
                        .map(|(e, _)| FileId::from_bytes(e.id)),
                )
                .await?;
            for (e, old_e) in diff {
                match e.kind {
                    k if is_file(k) => {
                        let m = self.files.get(FileId::from_bytes(e.id)).await?;
                        if m.size != e.size || m.git != e.git {
                            return Err(invalid(
                                key,
                                format!("tree entry {:?} does not match its content", e.name),
                            ));
                        }
                    }
                    Kind::Gitlink => {
                        if e.size != 0 {
                            return Err(invalid(key, "gitlink entry has a size"));
                        }
                    }
                    _ => {
                        let child_id = TreeId::from_bytes(e.id);
                        let child = self.engine.get_tree(child_id).await.map_err(from_tree)?;
                        if child.entries().is_empty() {
                            return Err(invalid(key, format!("directory {:?} is empty", e.name)));
                        }
                        let total = child
                            .entries()
                            .iter()
                            .try_fold(0u64, |a, c| a.checked_add(c.size));
                        if total != Some(e.size) {
                            return Err(invalid(
                                key,
                                format!("directory {:?} has a wrong size", e.name),
                            ));
                        }
                        if tree_oid(&child) != e.git {
                            self.check_override(e.git, &child, key).await?;
                        }
                        let old_child = old_e
                            .filter(|o| o.kind == Kind::Dir)
                            .map(|o| TreeId::from_bytes(o.id));
                        stack.push((child_id, old_child, depth + 1));
                    }
                }
            }
        }
        Ok(changed)
    }

    /// A directory whose git oid is not its canonical render needs a stored override whose body
    /// hashes to that oid and lists the same entries.
    async fn check_override(
        &self,
        git: GitOid,
        child: &Tree,
        key: ObjKey,
    ) -> Result<(), IngestError> {
        let bad = || {
            invalid(
                key,
                "directory git oid matches neither its render nor an override",
            )
        };
        let okey = ObjKey::GitTreeOverride(git);
        let body = self
            .entry
            .objects
            .get(&okey)
            .await
            .map_err(from_store)?
            .ok_or_else(bad)?;
        if body.len() > MAX_OVERRIDE_BYTES
            || object_oid(ObjectKind::Tree, &body) != git
            || asc_git::tree_override::verify(&body, child).is_err()
        {
            return Err(bad());
        }
        Ok(())
    }

    /// SPEC §2.8 native rule for a repository with `windows_safe`: each added or changed path is
    /// checkout-safe, Windows-valid in every component, and has no case-fold sibling.
    async fn check_windows_paths(&self, cs: &Changeset) -> Result<(), IngestError> {
        let policy = |path: &RepoPath, why| IngestError::PathPolicy {
            path: path.to_string(),
            why,
        };
        let mut dirs: HashMap<Vec<u8>, DirInfo> = HashMap::new();
        for (path, change) in cs.changes.as_slice() {
            if matches!(change, Change::Delete) {
                continue;
            }
            if !checkout_safe(path) {
                return Err(policy(path, "not safe to check out (git-fidelity Q-7)"));
            }
            let mut dir_key: Vec<u8> = Vec::new();
            let mut dir_id = cs.root;
            let mut comps = path.components().peekable();
            while let Some(comp) = comps.next() {
                let name = Name::new(comp).map_err(|e| invalid("path", e.to_string()))?;
                if !windows_valid(&name) {
                    return Err(policy(path, "invalid on Windows"));
                }
                if !dirs.contains_key(&dir_key) {
                    let tree = self.engine.get_tree(dir_id).await.map_err(from_tree)?;
                    let mut folds = HashMap::new();
                    for e in tree.entries() {
                        *folds.entry(casefold(e.name.as_bytes())).or_insert(0usize) += 1;
                    }
                    dirs.insert(dir_key.clone(), (tree, folds));
                }
                let (tree, folds) = &dirs[&dir_key];
                if folds.get(&casefold(comp)).copied().unwrap_or(0) > 1 {
                    return Err(policy(path, "collides in case with a sibling"));
                }
                if comps.peek().is_none() {
                    break;
                }
                match tree.get(&name) {
                    Some(e) if e.kind == Kind::Dir => dir_id = TreeId::from_bytes(e.id),
                    _ => break,
                }
                if !dir_key.is_empty() {
                    dir_key.push(b'/');
                }
                dir_key.extend_from_slice(comp);
            }
        }
        Ok(())
    }
}

/// A directory read by the case-collision check, with its folded-name counts.
type DirInfo = (Arc<Tree>, HashMap<Vec<u8>, usize>);

fn is_file(k: Kind) -> bool {
    matches!(k, Kind::File | Kind::Exec | Kind::Symlink)
}

/// `extra` keys of a new changeset (SPEC §2.6): `git.commit` is import-only (CARRY).
fn check_extra(cs: &Changeset, key: ObjKey) -> Result<(), IngestError> {
    for (k, v) in cs.extra.as_slice() {
        match k.as_slice() {
            b"git.gpgsig" => {}
            b"asc.snapshot" if v.is_empty() => {}
            b"git.commit" => return Err(invalid(key, "raw git extras are import-only")),
            other => {
                return Err(invalid(
                    key,
                    format!(
                        "extra key {:?} is not allowed",
                        String::from_utf8_lossy(other)
                    ),
                ));
            }
        }
    }
    Ok(())
}

/// A change without its `copy_from` annotation.
fn bare(change: &Change) -> Change {
    match change {
        Change::Write {
            kind,
            file,
            size,
            git,
            ..
        } => Change::Write {
            kind: *kind,
            file: *file,
            size: *size,
            git: *git,
            copy_from: None,
        },
        other => other.clone(),
    }
}

/// Both lists are sorted by path; the first difference (ignoring `copy_from`) is an error.
fn compare_changes(
    expected: &[(RepoPath, Change)],
    got: &[(RepoPath, Change)],
    key: ObjKey,
) -> Result<(), IngestError> {
    let bad = |p: &RepoPath| invalid(key, format!("changes do not match the tree diff at {p}"));
    let (mut i, mut j) = (0, 0);
    loop {
        match (expected.get(i), got.get(j)) {
            (None, None) => return Ok(()),
            (Some((p, _)), None) | (None, Some((p, _))) => return Err(bad(p)),
            (Some((pe, ce)), Some((pg, asc))) => {
                if pe != pg || bare(ce) != bare(asc) {
                    return Err(bad(pe.min(pg)));
                }
                i += 1;
                j += 1;
            }
        }
    }
}

/// Orders the new changesets parents first (Kahn's algorithm, no recursion).
fn topological(
    parents_of: &HashMap<ChangesetId, Vec<ChangesetId>>,
) -> Result<Vec<ChangesetId>, IngestError> {
    let mut waiting: HashMap<ChangesetId, usize> = HashMap::with_capacity(parents_of.len());
    let mut children: HashMap<ChangesetId, Vec<ChangesetId>> = HashMap::new();
    let mut ready = Vec::new();
    for (&id, parents) in parents_of {
        let mut n = 0;
        for p in parents {
            if parents_of.contains_key(p) {
                n += 1;
                children.entry(*p).or_default().push(id);
            }
        }
        if n == 0 {
            ready.push(id);
        } else {
            waiting.insert(id, n);
        }
    }
    ready.sort();
    let mut order = Vec::with_capacity(parents_of.len());
    while let Some(id) = ready.pop() {
        order.push(id);
        for c in children.remove(&id).unwrap_or_default() {
            let n = waiting.get_mut(&c).expect("child is waiting");
            *n -= 1;
            if *n == 0 {
                waiting.remove(&c);
                ready.push(c);
            }
        }
    }
    if order.len() != parents_of.len() {
        return Err(invalid("push", "changeset graph has a cycle"));
    }
    Ok(order)
}

#[cfg(test)]
pub(crate) mod testutil {
    //! Builders shared by the ingest and update tests.

    use std::sync::Arc;

    use asc_core::git::blob_oid;
    use asc_core::id::{ChangesetId, FileId, TreeId};
    use asc_core::object::{Change, Kind};
    use asc_core::path::RepoPath;
    use asc_core::sig::Signature;
    use asc_engine::commit::{NewChangeset, PathPolicy};
    use asc_rpc::server::RepoName;
    use asc_store::codec::encode_stored;
    use asc_store::key::ObjKey;
    use asc_store::objects::Objects;

    use crate::migrate;
    use crate::registry::{Registry, RepoEntry};

    pub async fn registry() -> Arc<Registry> {
        let pool = migrate::connect("sqlite::memory:", 1).await.unwrap();
        migrate::migrate(&pool).await.unwrap();
        Arc::new(Registry::new(pool, "memory:///").unwrap())
    }

    pub async fn repo(r: &Registry) -> Arc<RepoEntry> {
        r.create(&RepoName::parse("o/r").unwrap(), &Default::default())
            .await
            .unwrap()
    }

    pub fn sig() -> Signature {
        Signature::new(b"A".to_vec(), b"a@x".to_vec(), 1, 0).unwrap()
    }

    pub async fn put_file(e: &RepoEntry, bytes: &[u8]) -> Change {
        let id = FileId::of(bytes);
        e.objects
            .put(&ObjKey::File(id), encode_stored(bytes))
            .await
            .unwrap();
        Change::write(Kind::File, id, bytes.len() as u64, blob_oid(bytes), None).unwrap()
    }

    /// Applies `files` (path, bytes) on top of `base` and returns the new root.
    pub async fn tree(e: &RepoEntry, base: TreeId, files: &[(&str, &[u8])]) -> TreeId {
        let mut changes = Vec::new();
        for (p, b) in files {
            changes.push((RepoPath::new(*p).unwrap(), put_file(e, b).await));
        }
        changes.sort_by(|a, b| a.0.cmp(&b.0));
        e.backend.engine().apply(base, &changes).await.unwrap()
    }

    /// A valid stored changeset with `files` written on top of its first parent's tree.
    pub async fn commit(
        e: &RepoEntry,
        parents: &[ChangesetId],
        files: &[(&str, &[u8])],
    ) -> ChangesetId {
        let engine = e.backend.engine();
        let base = match parents.first() {
            Some(&p) => engine.get_changeset(p).await.unwrap().root,
            None => asc_engine::Engine::<
                asc_store::bucket::BucketObjects,
                asc_store::sqlrefs::SqlRefs,
            >::empty_tree_id(),
        };
        let root = tree(e, base, files).await;
        engine
            .create_changeset(
                NewChangeset {
                    parents: parents.to_vec(),
                    author: sig(),
                    committer: sig(),
                    message: b"m".to_vec(),
                    extra: Vec::new(),
                    root,
                    copies: Vec::new(),
                },
                PathPolicy::Native { windows_safe: true },
            )
            .await
            .unwrap()
    }
}

#[cfg(test)]
mod cost {
    use super::testutil::*;
    use super::*;

    #[tokio::test]
    async fn one_file_change_in_a_wide_directory_is_depth_plus_one_lookups() {
        let r = registry().await;
        let e = repo(&r).await;
        let names: Vec<String> = (0..5000).map(|i| format!("dir/f{i:05}")).collect();
        let data: Vec<Vec<u8>> = (0..5000)
            .map(|i| format!("content {i}").into_bytes())
            .collect();
        let files: Vec<(&str, &[u8])> = names
            .iter()
            .zip(&data)
            .map(|(n, d)| (n.as_str(), d.as_slice()))
            .collect();
        let a = commit(&e, &[], &files).await;
        let b = commit(&e, &[a], &[("dir/f02500", b"changed")]).await;

        let mut first = Ingestor::new(r.pool(), r.refs(), &e, true);
        first
            .ingest(&[RefTarget::Changeset(a)], &NoObserver)
            .await
            .unwrap();
        assert!(first.file_meta_lookups() >= 5000);

        let mut second = Ingestor::new(r.pool(), r.refs(), &e, true);
        let added = second
            .ingest(&[RefTarget::Changeset(b)], &NoObserver)
            .await
            .unwrap();
        assert_eq!(added, vec![b]);
        let depth = 2; // dir/file
        assert!(
            second.file_meta_lookups() <= depth + 1,
            "{} lookups",
            second.file_meta_lookups()
        );
    }
}

#[cfg(test)]
mod tests {
    use super::testutil::*;
    use super::*;

    #[tokio::test]
    async fn closure_stops_at_known_changesets_and_orders_parents_first() {
        let r = registry().await;
        let e = repo(&r).await;
        let a = commit(&e, &[], &[("a", b"1")]).await;
        let b = commit(&e, &[a], &[("b", b"2")]).await;
        let c = commit(&e, &[b], &[("c", b"3")]).await;
        let mut ing = Ingestor::new(r.pool(), r.refs(), &e, true);
        assert_eq!(
            ing.ingest(&[RefTarget::Changeset(b)], &NoObserver)
                .await
                .unwrap(),
            vec![a, b]
        );
        let mut ing = Ingestor::new(r.pool(), r.refs(), &e, true);
        assert_eq!(
            ing.ingest(&[RefTarget::Changeset(c)], &NoObserver)
                .await
                .unwrap(),
            vec![c]
        );
        let mut ing = Ingestor::new(r.pool(), r.refs(), &e, true);
        assert!(
            ing.ingest(&[RefTarget::Changeset(c)], &NoObserver)
                .await
                .unwrap()
                .is_empty()
        );
        // git ids are derived eagerly.
        let git = e.backend.engine().git_commit_of(c).await.unwrap();
        let row = sqlx::query("SELECT asc FROM git_map WHERE git = $1 AND kind = 3")
            .bind(git.as_bytes().to_vec())
            .fetch_one(r.pool())
            .await
            .unwrap();
        let asc: Vec<u8> = row.try_get("asc").unwrap();
        assert_eq!(asc, c.as_bytes().to_vec());
    }

    #[tokio::test]
    async fn missing_changeset_and_file_are_reported() {
        let r = registry().await;
        let e = repo(&r).await;
        let ghost = ChangesetId::from_bytes([7; 32]);
        let mut ing = Ingestor::new(r.pool(), r.refs(), &e, true);
        match ing
            .ingest(&[RefTarget::Changeset(ghost)], &NoObserver)
            .await
        {
            Err(IngestError::Missing(k)) => assert_eq!(k, vec![ObjKey::Changeset(ghost)]),
            other => panic!("{other:?}"),
        }
    }

    #[tokio::test]
    async fn windows_paths_are_refused_only_when_the_repo_is_windows_safe() {
        let r = registry().await;
        let e = repo(&r).await;
        // The engine would refuse this itself, so build the changeset with the import policy.
        let engine = e.backend.engine();
        let root = tree(&e, asc_engine::Engine::<asc_store::bucket::BucketObjects, asc_store::sqlrefs::SqlRefs>::empty_tree_id(), &[("a:b", b"x")]).await;
        let id = engine
            .create_changeset(
                asc_engine::commit::NewChangeset {
                    parents: vec![],
                    author: sig(),
                    committer: sig(),
                    message: b"m".to_vec(),
                    extra: vec![],
                    root,
                    copies: vec![],
                },
                asc_engine::commit::PathPolicy::Import,
            )
            .await
            .unwrap();
        let mut ing = Ingestor::new(r.pool(), r.refs(), &e, true);
        assert!(matches!(
            ing.ingest(&[RefTarget::Changeset(id)], &NoObserver).await,
            Err(IngestError::PathPolicy { .. })
        ));
        let mut ing = Ingestor::new(r.pool(), r.refs(), &e, false);
        ing.ingest(&[RefTarget::Changeset(id)], &NoObserver)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn a_diff_past_the_changes_cap_is_refused_not_accepted_unverified() {
        let r = registry().await;
        let e = repo(&r).await;
        let id = commit(&e, &[], &[("a", b"1"), ("b", b"2"), ("c", b"3")]).await;
        let mut capped = Ingestor::new(r.pool(), r.refs(), &e, true).with_max_changes(2);
        match capped
            .ingest(&[RefTarget::Changeset(id)], &NoObserver)
            .await
        {
            Err(IngestError::Invalid { why, .. }) => assert!(why.contains("cannot be verified")),
            other => panic!("{other:?}"),
        }
        let mut exact = Ingestor::new(r.pool(), r.refs(), &e, true).with_max_changes(3);
        exact
            .ingest(&[RefTarget::Changeset(id)], &NoObserver)
            .await
            .unwrap();
    }
}
