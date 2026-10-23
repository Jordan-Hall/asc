//! `HistoryApi` and the history window (SPEC §4.3, §6.3).
//!
//! A fetch keeps only the newest `asc.fetch.historyWindow` generations of each tip. The oldest
//! fetched changesets, whose parents are not local, are recorded as *boundaries*
//! ([`BoundaryStore`]). [`HistorySource`] reports them as [`NodeOrBoundary::Boundary`], and a
//! [`Graph`] given a [`ServerHistory`] continues any walk that reaches one through the remote.

use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use asc_core::codec::decode_envelope;
use asc_core::id::{ChangesetId, GitOid, RefTarget, TagId};
use asc_core::object::{Change, Changeset, TagRecord};
use asc_core::path::RefName;
use asc_store::key::{KeyClass, KeyPrefix, ObjKey};
use asc_store::objects::Objects;
use asc_store::refs::Refs;
use futures::TryStreamExt;

use crate::derive::{Derived, Gen, GitCommit};
use crate::graph::{
    Error, Graph, GraphSource, LogCursor, LogQuery, Node, NodeOrBoundary, RemoteHistory, Result,
};
use crate::graph_skip::SkipSource;
use crate::revspec::{self, Resolved, Resolver};
use crate::tree::Pathspec;
use crate::{Engine, commit};

use asc_core::path::RepoPath;

pub mod compare;
pub mod linehist;
pub mod pickaxe;

/// Largest page a log answers with (also bounds the round trips of a fetch).
pub const LOG_PAGE: usize = 4096;

/// Default `asc.fetch.historyWindow`: generations of history kept below each fetched tip.
pub const DEFAULT_WINDOW: u32 = 10_000;

/// Tree entries one client-requested diff may visit. The diff RPC reads one tree per changed
/// directory for a caller that sets both changesets, so it gets a quarter of the budget local
/// callers have ([`crate::tree::MAX_DIFF_VISITS`]); past it the request fails.
pub const MAX_RPC_DIFF_VISITS: usize = 1 << 20;

/// One changeset of a log, as the wire `LogEntry` carries it for a fetch.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LogEntry {
    pub id: ChangesetId,
    pub generation: u32,
    pub parents: Vec<ChangesetId>,
    /// Git commit id, when the server knows it.
    pub git: Option<GitOid>,
}

/// Opaque position in a paged log.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PageToken(pub Vec<u8>);

/// One page of a log.
#[derive(Clone, Debug, Default)]
pub struct LogEntries {
    pub entries: Vec<LogEntry>,
    /// `None` once the log is exhausted.
    pub next: Option<PageToken>,
}

/// Both directions of the asc/git id map for the ids asked about.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GitMapping {
    pub asc_to_git: Vec<Option<GitOid>>,
    pub git_to_asc: Vec<Option<ChangesetId>>,
}

/// History queries (SPEC §4.3). `Engine` implements it; `asc-rpc` implements it over the wire.
///
/// An id the implementation does not know is never an ancestor and has no merge base.
#[async_trait]
pub trait HistoryApi: Send + Sync {
    /// One page of a log, ordered by generation descending then id. `q.limit` bounds the whole
    /// log; pass the returned `next` back as `page` to continue it.
    async fn log(&self, q: &LogQuery, page: Option<&PageToken>) -> Result<LogEntries>;
    async fn merge_base(
        &self,
        a: ChangesetId,
        others: &[ChangesetId],
        all: bool,
    ) -> Result<Vec<ChangesetId>>;
    async fn is_ancestor(&self, a: ChangesetId, b: ChangesetId) -> Result<bool>;
    /// Leaf changes turning the tree of `a` into the tree of `b`, limited to `paths`.
    async fn diff(
        &self,
        a: ChangesetId,
        b: ChangesetId,
        paths: &[String],
    ) -> Result<Vec<(RepoPath, Change)>>;
    /// Single-commit revspecs; `None` for one that names nothing.
    async fn resolve(&self, specs: &[String]) -> Result<Vec<Option<ChangesetId>>>;
    async fn git_map(&self, asc: &[ChangesetId], git: &[GitOid]) -> Result<GitMapping>;
}

/// A [`HistoryApi`] as the continuation of a windowed walk.
pub struct ServerHistory(pub Arc<dyn HistoryApi>);

#[async_trait]
impl RemoteHistory for ServerHistory {
    async fn log(&self, q: &LogQuery, page: Option<&PageToken>) -> Result<LogEntries> {
        self.0.log(q, page).await
    }
    async fn merge_base(
        &self,
        a: ChangesetId,
        others: &[ChangesetId],
        all: bool,
    ) -> Result<Vec<ChangesetId>> {
        self.0.merge_base(a, others, all).await
    }
    async fn is_ancestor(&self, a: ChangesetId, b: ChangesetId) -> Result<bool> {
        self.0.is_ancestor(a, b).await
    }
}

/// The continuation used while the remote is unreachable: every query fails with the SPEC §6.4
/// offline error.
pub struct OfflineHistory {
    pub remote: String,
}

impl OfflineHistory {
    fn err(&self, operation: &'static str, rpc: &'static str) -> Error {
        Error::Offline {
            remote: self.remote.clone(),
            operation,
            rpc,
        }
    }
}

#[async_trait]
impl RemoteHistory for OfflineHistory {
    async fn log(&self, _: &LogQuery, _: Option<&PageToken>) -> Result<LogEntries> {
        Err(self.err("log", "Log"))
    }
    async fn merge_base(
        &self,
        _: ChangesetId,
        _: &[ChangesetId],
        _: bool,
    ) -> Result<Vec<ChangesetId>> {
        Err(self.err("merge-base", "MergeBase"))
    }
    async fn is_ancestor(&self, _: ChangesetId, _: ChangesetId) -> Result<bool> {
        Err(self.err("is-ancestor", "IsAncestor"))
    }
}

/// Where history boundaries are recorded (`state.db.history_boundary`; SPEC §6.3).
#[async_trait]
pub trait BoundaryStore: Send + Sync {
    async fn list(&self) -> Result<Vec<ChangesetId>>;
    async fn add(&self, ids: &[ChangesetId]) -> Result<()>;
    async fn remove(&self, ids: &[ChangesetId]) -> Result<()>;
}

/// An in-memory [`BoundaryStore`].
#[derive(Default)]
pub struct MemBoundaries(Mutex<HashSet<ChangesetId>>);

#[async_trait]
impl BoundaryStore for MemBoundaries {
    async fn list(&self) -> Result<Vec<ChangesetId>> {
        let mut v: Vec<_> = self.0.lock().unwrap().iter().copied().collect();
        v.sort();
        Ok(v)
    }
    async fn add(&self, ids: &[ChangesetId]) -> Result<()> {
        self.0.lock().unwrap().extend(ids.iter().copied());
        Ok(())
    }
    async fn remove(&self, ids: &[ChangesetId]) -> Result<()> {
        let mut set = self.0.lock().unwrap();
        for id in ids {
            set.remove(id);
        }
        Ok(())
    }
}

/// A [`GraphSource`] over an engine's `gen.v1` memos that knows the history boundaries.
///
/// Fetch writes `gen.v1` for every changeset it brings, so no node here derives through a
/// missing parent.
pub struct HistorySource<'a, O: Objects, R: Refs> {
    engine: &'a Engine<O, R>,
    boundaries: HashSet<ChangesetId>,
    reads: AtomicU64,
}

impl<'a, O: Objects, R: Refs> HistorySource<'a, O, R> {
    /// Loads the boundary set from `store`.
    pub async fn load(engine: &'a Engine<O, R>, store: &dyn BoundaryStore) -> Result<Self> {
        Ok(Self {
            engine,
            boundaries: store.list().await?.into_iter().collect(),
            reads: AtomicU64::new(0),
        })
    }

    async fn node_of(&self, id: ChangesetId) -> Result<Node> {
        let cs = self.changeset(id).await?;
        let generation = self
            .engine
            .get_derived::<Gen>(id)
            .await
            .map_err(|e| derive_err(id, e))?
            .generation;
        Ok(Node {
            generation,
            parents: cs.parents.clone(),
        })
    }
}

fn derive_err(id: ChangesetId, e: crate::derive::Error) -> Error {
    match e {
        crate::derive::Error::Store(e) => Error::Engine(commit::Error::Store(e)),
        crate::derive::Error::Commit(e) => e.into(),
        _ => Error::Engine(commit::Error::Store(asc_store::objects::Error::Corrupt(
            ObjKey::Derived {
                name: Gen::NAME,
                version: Gen::VERSION,
                id: *id.as_bytes(),
            },
        ))),
    }
}

#[async_trait]
impl<O: Objects, R: Refs> GraphSource for HistorySource<'_, O, R> {
    async fn get(&self, id: ChangesetId) -> Result<Node> {
        self.node_of(id).await
    }

    async fn get_boundary(&self, id: ChangesetId) -> Result<NodeOrBoundary> {
        let n = self.node_of(id).await?;
        Ok(if self.boundaries.contains(&id) {
            NodeOrBoundary::Boundary(n)
        } else {
            NodeOrBoundary::Node(n)
        })
    }

    async fn get_many_boundary(&self, ids: &[ChangesetId]) -> Result<Vec<NodeOrBoundary>> {
        let mut out = Vec::with_capacity(ids.len());
        for &id in ids {
            out.push(self.get_boundary(id).await?);
        }
        Ok(out)
    }

    async fn changeset(&self, id: ChangesetId) -> Result<Arc<Changeset>> {
        self.reads.fetch_add(1, Ordering::Relaxed);
        Ok(self.engine.get_changeset(id).await?)
    }

    fn reads(&self) -> u64 {
        self.reads.load(Ordering::Relaxed)
    }
}

/// A graph over `engine`'s windowed history; walks that reach a boundary continue through
/// `remote` (use [`OfflineHistory`] when the server is unreachable).
pub async fn windowed_graph<'a, O: Objects, R: Refs>(
    engine: &'a Engine<O, R>,
    store: &dyn BoundaryStore,
    remote: Arc<dyn RemoteHistory>,
) -> Result<Graph<HistorySource<'a, O, R>>> {
    Ok(Graph::new(HistorySource::load(engine, store).await?).with_remote(remote))
}

// ---- Engine implementation -------------------------------------------------------------

/// `u64::MAX` stands for "no limit" in a page token.
const NO_LIMIT: u64 = u64::MAX;

fn encode_token(remaining: Option<usize>, cursor: &LogCursor) -> PageToken {
    let mut b = remaining
        .map_or(NO_LIMIT, |r| r as u64)
        .to_le_bytes()
        .to_vec();
    b.extend(cursor.to_bytes());
    PageToken(b)
}

fn decode_token(t: &PageToken) -> Result<(Option<usize>, LogCursor)> {
    let bad = || Error::Query("bad page token");
    let (head, rest) = t.0.split_first_chunk::<8>().ok_or_else(bad)?;
    let n = u64::from_le_bytes(*head);
    let remaining = (n != NO_LIMIT)
        .then(|| usize::try_from(n))
        .transpose()
        .map_err(|_| bad())?;
    Ok((remaining, LogCursor::from_bytes(rest).ok_or_else(bad)?))
}

impl<O: Objects, R: Refs> Engine<O, R> {
    fn graph(
        &self,
    ) -> (
        Graph<SkipSource<'_, O, R>, crate::graph_skip::SkipIndex>,
        SkipSource<'_, O, R>,
    ) {
        let src = SkipSource::new(self);
        (Graph::with_jump(src.clone(), src.index()), src)
    }

    async fn memoised_git(&self, id: ChangesetId) -> Option<GitOid> {
        let key = ObjKey::Derived {
            name: GitCommit::NAME,
            version: GitCommit::VERSION,
            id: *id.as_bytes(),
        };
        let bytes = crate::derive::read_memo(&*self.objects, &key)
            .await
            .ok()??;
        GitCommit::decode(&bytes).ok().map(|g| g.commit)
    }
}

impl<O: Objects, R: Refs> Engine<O, R> {
    /// The server-side diff of two changesets, failing after `budget` visited tree entries.
    async fn diff_bounded(
        &self,
        a: ChangesetId,
        b: ChangesetId,
        spec: Option<&Pathspec>,
        budget: usize,
    ) -> Result<Vec<(RepoPath, Change)>> {
        let (ca, cb) = (self.get_changeset(a).await?, self.get_changeset(b).await?);
        self.leafdiff_bounded(ca.root, cb.root, spec, budget)
            .await
            .map_err(|e| Error::Engine(commit::Error::Tree(e)))
    }
}

#[async_trait]
impl<O: Objects, R: Refs> HistoryApi for Engine<O, R> {
    async fn log(&self, q: &LogQuery, page: Option<&PageToken>) -> Result<LogEntries> {
        let (g, _) = self.graph();
        let mut q = q.clone();
        let remaining = match page {
            Some(t) => {
                let (remaining, cursor) = decode_token(t)?;
                q.cursor = Some(cursor);
                remaining
            }
            None => q.limit,
        };
        q.limit = Some(remaining.map_or(LOG_PAGE, |r| r.min(LOG_PAGE)));
        let out = g.log_page(q).await?;
        let nodes = g.nodes(&out.ids).await?;
        let mut entries = Vec::with_capacity(out.ids.len());
        for (&id, n) in out.ids.iter().zip(&nodes) {
            entries.push(LogEntry {
                id,
                generation: n.generation,
                parents: n.parents.clone(),
                git: self.memoised_git(id).await,
            });
        }
        let remaining = remaining.map(|r| r - entries.len());
        let next = out
            .next
            .filter(|_| remaining != Some(0))
            .map(|c| encode_token(remaining, &c));
        Ok(LogEntries { entries, next })
    }

    async fn merge_base(
        &self,
        a: ChangesetId,
        others: &[ChangesetId],
        all: bool,
    ) -> Result<Vec<ChangesetId>> {
        let (g, _) = self.graph();
        match g.merge_base(a, others, all).await {
            Err(e) if e.is_missing() => Ok(Vec::new()),
            other => other,
        }
    }

    async fn is_ancestor(&self, a: ChangesetId, b: ChangesetId) -> Result<bool> {
        let (g, src) = self.graph();
        let known = async {
            let ga = self.get_derived::<Gen>(a).await.ok()?.generation;
            src.prime(b, ga).await.ok()
        };
        known.await;
        match g.is_ancestor(a, b).await {
            Err(e) if e.is_missing() => Ok(false),
            other => other,
        }
    }

    async fn diff(
        &self,
        a: ChangesetId,
        b: ChangesetId,
        paths: &[String],
    ) -> Result<Vec<(RepoPath, Change)>> {
        let spec = (!paths.is_empty())
            .then(|| Pathspec::parse(paths))
            .transpose()
            .map_err(|e| Error::Remote(e.to_string()))?;
        self.diff_bounded(a, b, spec.as_ref(), MAX_RPC_DIFF_VISITS)
            .await
    }

    async fn resolve(&self, specs: &[String]) -> Result<Vec<Option<ChangesetId>>> {
        let r = EngineResolver {
            engine: self,
            src: SkipSource::new(self),
        };
        let mut out = Vec::with_capacity(specs.len());
        for s in specs {
            out.push(match revspec::resolve_str(&r, s).await {
                Ok(Resolved::Single(id)) => Some(id),
                Ok(_) => return Err(Error::Unsupported("resolve answers single commits only")),
                Err(revspec::Error::NotFound(_)) => None,
                Err(revspec::Error::Graph(e)) => return Err(e),
                Err(e) => return Err(Error::Remote(e.to_string())),
            });
        }
        Ok(out)
    }

    async fn git_map(&self, asc: &[ChangesetId], git: &[GitOid]) -> Result<GitMapping> {
        if !git.is_empty() {
            return Err(Error::Unsupported(
                "git to asc lookups need the git map index, which a bare engine does not keep",
            ));
        }
        let mut asc_to_git = Vec::with_capacity(asc.len());
        for &id in asc {
            asc_to_git.push(match self.git_commit_of(id).await {
                Ok(g) => Some(g),
                Err(crate::derive::Error::Commit(commit::Error::MissingChangeset(_))) => None,
                Err(e) => return Err(derive_err(id, e)),
            });
        }
        Ok(GitMapping {
            asc_to_git,
            git_to_asc: Vec::new(),
        })
    }
}

/// Revspec resolution over an engine's refs and objects.
struct EngineResolver<'a, O: Objects, R: Refs> {
    engine: &'a Engine<O, R>,
    src: SkipSource<'a, O, R>,
}

fn store_err(e: impl Into<commit::Error>) -> revspec::Error {
    revspec::Error::Graph(Error::Engine(e.into()))
}

fn ref_err(e: asc_store::refs::RefError) -> revspec::Error {
    revspec::Error::Graph(Error::Remote(e.to_string()))
}

#[async_trait]
impl<O: Objects, R: Refs> GraphSource for EngineResolver<'_, O, R> {
    async fn get(&self, id: ChangesetId) -> Result<Node> {
        self.src.get(id).await
    }
    async fn changeset(&self, id: ChangesetId) -> Result<Arc<Changeset>> {
        self.src.changeset(id).await
    }
    fn reads(&self) -> u64 {
        self.src.reads()
    }
}

#[async_trait]
impl<O: Objects, R: Refs> Resolver for EngineResolver<'_, O, R> {
    async fn resolve_ref(&self, name: &str) -> revspec::Result<Option<RefTarget>> {
        for cand in revspec::dwim_candidates(name) {
            let Ok(n) = RefName::new_onelevel(cand) else {
                continue;
            };
            let e = self
                .engine
                .refs
                .get(self.engine.repo, &n)
                .await
                .map_err(ref_err)?;
            if let Some(e) = e {
                return Ok(Some(e.target));
            }
        }
        Ok(None)
    }

    async fn reflog(&self, name: &str, n: u32) -> revspec::Result<Option<ChangesetId>> {
        let Ok(name) = RefName::new_onelevel(name) else {
            return Ok(None);
        };
        let log = self
            .engine
            .refs
            .log(self.engine.repo, &name, n.saturating_add(1))
            .await
            .map_err(ref_err)?;
        Ok(match log.get(n as usize).and_then(|e| e.new) {
            Some(RefTarget::Changeset(c)) => Some(c),
            _ => None,
        })
    }

    async fn upstream(&self, _: &str) -> revspec::Result<Option<ChangesetId>> {
        Ok(None)
    }

    async fn git_to_asc(&self, _: &str) -> revspec::Result<Vec<ChangesetId>> {
        Ok(Vec::new())
    }

    async fn asc_prefix(&self, prefix: &str) -> revspec::Result<Vec<ChangesetId>> {
        let mut keys = self
            .engine
            .objects
            .list(KeyPrefix::class(KeyClass::Changeset));
        let mut out = Vec::new();
        while let Some((key, _)) = keys.try_next().await.map_err(store_err)? {
            if let ObjKey::Changeset(id) = key
                && id.to_string().starts_with(prefix)
            {
                out.push(id);
            }
        }
        Ok(out)
    }

    async fn peel_tag(&self, tag: TagId) -> revspec::Result<ChangesetId> {
        let data = self
            .engine
            .objects
            .get(&ObjKey::Tag(tag))
            .await
            .map_err(store_err)?
            .ok_or_else(|| revspec::Error::NotFound(tag.to_string()))?;
        let rec: TagRecord = decode_envelope(&data).map_err(store_err)?;
        Ok(rec.target)
    }

    async fn tips(&self) -> revspec::Result<Vec<ChangesetId>> {
        let refs = self
            .engine
            .refs
            .list(self.engine.repo, "refs/")
            .await
            .map_err(ref_err)?;
        let mut out = Vec::new();
        for e in refs {
            match e.target {
                RefTarget::Changeset(c) => out.push(c),
                RefTarget::Tag(t) => out.push(self.peel_tag(t).await?),
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
pub(crate) mod testkit {
    //! Engines over a shared SQLite ref store, a counting remote and cheap history builders.

    use std::io::Cursor;
    use std::path::Path;
    use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};

    use bytes::Bytes;
    use asc_core::codec::Enveloped;
    use asc_core::content::{Body, ingest};
    use asc_core::id::TreeId;
    use asc_core::object::Kind;
    use asc_core::sig::Signature;
    use asc_store::bucket::BucketObjects;
    use asc_store::codec::encode_stored;
    use asc_store::refs::{Actor, Expect, RefEntry, RefTxn, RefUpdate, RepoKey};
    use asc_store::sqlrefs::SqlRefs;
    use futures::stream::BoxStream;
    use sqlx::any::AnyPoolOptions;

    use super::*;
    use crate::commit::{NewChangeset, PathPolicy};
    use crate::sync::{BucketRemote, Error as SyncError, Remote};

    pub type Eng = Engine<BucketObjects, SqlRefs>;

    pub async fn sqlite(dir: &Path, name: &str) -> Arc<SqlRefs> {
        sqlx::any::install_default_drivers();
        let path = dir.join(name).to_string_lossy().replace('\\', "/");
        let pool = AnyPoolOptions::new()
            .max_connections(8)
            .connect(&format!("sqlite:{path}?mode=rwc"))
            .await
            .unwrap();
        let refs = SqlRefs::new(pool);
        refs.migrate().await.unwrap();
        Arc::new(refs)
    }

    pub fn engine(refs: &Arc<SqlRefs>, repo: i64) -> Arc<Eng> {
        Arc::new(Engine::new(
            Arc::new(BucketObjects::memory()),
            refs.clone(),
            RepoKey(repo),
        ))
    }

    fn sig() -> Signature {
        Signature::new(b"A".to_vec(), b"a@x".to_vec(), 1_700_000_000, 0).unwrap()
    }

    /// Stores `content` (chunked when large) and returns the change that writes it.
    pub async fn put_content(e: &Eng, content: &[u8]) -> Change {
        let got = ingest(Cursor::new(content)).unwrap();
        match got.body {
            Body::Inline(b) => {
                e.objects
                    .put(&ObjKey::File(got.file), encode_stored(&b))
                    .await
                    .unwrap();
            }
            Body::Chunked { list, chunks } => {
                for (id, bytes) in chunks {
                    e.objects
                        .put(&ObjKey::Chunk(id), encode_stored(&bytes))
                        .await
                        .unwrap();
                }
                e.objects
                    .put(
                        &ObjKey::ChunkList(got.file),
                        encode_stored(&list.to_envelope()),
                    )
                    .await
                    .unwrap();
            }
        }
        Change::write(Kind::File, got.file, got.size, got.git, None).unwrap()
    }

    /// A changeset on `parents` whose tree is the first parent's with `files` written.
    pub async fn commit_files(
        e: &Eng,
        parents: &[ChangesetId],
        files: &[(&str, &[u8])],
        policy: PathPolicy,
        extra: Vec<(Vec<u8>, Vec<u8>)>,
    ) -> ChangesetId {
        let base = match parents.first() {
            Some(&p) => e.get_changeset(p).await.unwrap().root,
            None => Eng::empty_tree_id(),
        };
        let mut changes = Vec::new();
        for (path, content) in files {
            changes.push((RepoPath::new(*path).unwrap(), put_content(e, content).await));
        }
        let root = e.apply(base, &changes).await.unwrap();
        e.create_changeset(
            NewChangeset {
                parents: parents.to_vec(),
                author: sig(),
                committer: sig(),
                message: format!("{}\n", files.len()).into_bytes(),
                extra,
                root,
                copies: Vec::new(),
            },
            policy,
        )
        .await
        .unwrap()
    }

    pub async fn commit(e: &Eng, parents: &[ChangesetId], content: &[u8]) -> ChangesetId {
        commit_files(
            e,
            parents,
            &[("f.txt", content), ("d/g.txt", b"fixed")],
            PathPolicy::Native { windows_safe: true },
            Vec::new(),
        )
        .await
    }

    /// Cheap changesets that all share one tree; `salt` keeps the ids distinct.
    pub struct Chain {
        pub root: TreeId,
    }

    impl Chain {
        pub async fn new(e: &Eng) -> (Self, ChangesetId) {
            let first = commit(e, &[], b"first").await;
            let root = e.get_changeset(first).await.unwrap().root;
            (Self { root }, first)
        }

        pub async fn add(&self, e: &Eng, parents: &[ChangesetId], salt: u32) -> ChangesetId {
            e.create_changeset(
                NewChangeset {
                    parents: parents.to_vec(),
                    author: sig(),
                    committer: sig(),
                    message: format!("m{salt}\n").into_bytes(),
                    extra: Vec::new(),
                    root: self.root,
                    copies: Vec::new(),
                },
                PathPolicy::Native { windows_safe: true },
            )
            .await
            .unwrap()
        }

        /// `n` changesets in a line starting at `first`.
        pub async fn line(&self, e: &Eng, first: ChangesetId, n: usize) -> Vec<ChangesetId> {
            let mut ids = vec![first];
            for i in 1..n {
                let p = ids[i - 1];
                ids.push(self.add(e, &[p], i as u32).await);
            }
            ids
        }
    }

    pub async fn set_ref(e: &Eng, name: &str, target: ChangesetId) {
        let name = RefName::new(name).unwrap();
        let old = e.refs.get(e.repo, &name).await.unwrap();
        let txn = RefTxn {
            updates: vec![RefUpdate {
                name,
                expect: old.map_or(Expect::Absent, |o| Expect::Value(o.target)),
                new: Some(RefTarget::Changeset(target)),
                expires_at: None,
            }],
            actor: Actor {
                id: "t".into(),
                via: "test".into(),
            },
            reason: "test".into(),
            op_id: uuid::Uuid::now_v7(),
        };
        e.refs.update(e.repo, &txn).await.unwrap();
    }

    /// Round trips and uploads seen by a [`MockRemote`].
    #[derive(Default)]
    pub struct Trips {
        pub lists: AtomicU64,
        pub logs: AtomicU64,
        pub gets: AtomicU64,
        pub got_changesets: AtomicU64,
        pub has: AtomicU64,
        pub puts: AtomicU64,
        pub put_objects: AtomicU64,
        pub history_calls: AtomicU64,
    }

    impl Trips {
        pub fn fetch_round_trips(&self) -> u64 {
            self.lists.load(Ordering::SeqCst)
                + self.logs.load(Ordering::SeqCst)
                + self.gets.load(Ordering::SeqCst)
        }
    }

    struct CountingHistory {
        server: Arc<Eng>,
        trips: Arc<Trips>,
    }

    #[async_trait]
    impl HistoryApi for CountingHistory {
        async fn log(&self, q: &LogQuery, page: Option<&PageToken>) -> Result<LogEntries> {
            self.trips.logs.fetch_add(1, Ordering::SeqCst);
            self.trips.history_calls.fetch_add(1, Ordering::SeqCst);
            HistoryApi::log(&*self.server, q, page).await
        }
        async fn merge_base(
            &self,
            a: ChangesetId,
            others: &[ChangesetId],
            all: bool,
        ) -> Result<Vec<ChangesetId>> {
            self.trips.history_calls.fetch_add(1, Ordering::SeqCst);
            HistoryApi::merge_base(&*self.server, a, others, all).await
        }
        async fn is_ancestor(&self, a: ChangesetId, b: ChangesetId) -> Result<bool> {
            self.trips.history_calls.fetch_add(1, Ordering::SeqCst);
            HistoryApi::is_ancestor(&*self.server, a, b).await
        }
        async fn diff(
            &self,
            a: ChangesetId,
            b: ChangesetId,
            paths: &[String],
        ) -> Result<Vec<(RepoPath, Change)>> {
            HistoryApi::diff(&*self.server, a, b, paths).await
        }
        async fn resolve(&self, specs: &[String]) -> Result<Vec<Option<ChangesetId>>> {
            HistoryApi::resolve(&*self.server, specs).await
        }
        async fn git_map(&self, asc: &[ChangesetId], git: &[GitOid]) -> Result<GitMapping> {
            HistoryApi::git_map(&*self.server, asc, git).await
        }
    }

    /// A bucket remote that counts requests, can fail uploads after N objects and can front a
    /// server engine's history service.
    pub struct MockRemote {
        inner: BucketRemote<BucketObjects, SqlRefs>,
        history: Option<Arc<dyn HistoryApi>>,
        pub trips: Arc<Trips>,
        /// Objects still accepted; negative means no limit.
        pub budget: AtomicI64,
    }

    impl MockRemote {
        /// A plain bucket (no history service) over `objects` and the `repo` row of `refs`.
        pub fn bucket(objects: Arc<BucketObjects>, refs: Arc<SqlRefs>, repo: i64) -> Self {
            Self {
                inner: BucketRemote::new("origin", objects, refs, RepoKey(repo)),
                history: None,
                trips: Arc::new(Trips::default()),
                budget: AtomicI64::new(-1),
            }
        }

        /// A server: the engine's objects and refs plus its history service.
        pub fn server(server: &Arc<Eng>) -> Self {
            let mut r = Self::bucket(server.objects.clone(), server.refs.clone(), server.repo.0);
            r.history = Some(Arc::new(CountingHistory {
                server: server.clone(),
                trips: r.trips.clone(),
            }));
            r
        }
    }

    #[async_trait]
    impl Remote for MockRemote {
        fn name(&self) -> &str {
            "origin"
        }
        async fn list_refs(&self) -> crate::sync::Result<Vec<RefEntry>> {
            self.trips.lists.fetch_add(1, Ordering::SeqCst);
            self.inner.list_refs().await
        }
        async fn has_many(&self, keys: &[ObjKey]) -> crate::sync::Result<Vec<bool>> {
            self.trips.has.fetch_add(1, Ordering::SeqCst);
            self.inner.has_many(keys).await
        }
        fn get_many(
            &self,
            keys: Vec<ObjKey>,
        ) -> BoxStream<'_, crate::sync::Result<(ObjKey, Bytes)>> {
            self.trips.gets.fetch_add(1, Ordering::SeqCst);
            let n = keys
                .iter()
                .filter(|k| matches!(k, ObjKey::Changeset(_)))
                .count();
            self.trips
                .got_changesets
                .fetch_add(n as u64, Ordering::SeqCst);
            self.inner.get_many(keys)
        }
        async fn put_many(&self, objects: Vec<(ObjKey, Bytes)>) -> crate::sync::Result<()> {
            self.trips.puts.fetch_add(1, Ordering::SeqCst);
            let mut accepted = Vec::new();
            for o in objects {
                let left = self.budget.load(Ordering::SeqCst);
                if left == 0 {
                    break;
                }
                if left > 0 {
                    self.budget.fetch_sub(1, Ordering::SeqCst);
                }
                accepted.push(o);
            }
            let n = accepted.len();
            self.inner.put_many(accepted).await?;
            self.trips.put_objects.fetch_add(n as u64, Ordering::SeqCst);
            if self.budget.load(Ordering::SeqCst) == 0 {
                return Err(SyncError::Remote("injected fault".into()));
            }
            Ok(())
        }
        async fn update_refs(&self, updates: &[RefUpdate]) -> crate::sync::Result<()> {
            self.inner.update_refs(updates).await
        }
        async fn ingest(&self, tips: &[ChangesetId]) -> crate::sync::Result<()> {
            self.inner.ingest(tips).await
        }
        fn history(&self) -> Option<Arc<dyn HistoryApi>> {
            self.history.clone()
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::Ordering;

    use futures::TryStreamExt;

    use super::testkit::*;
    use super::*;
    use crate::sync::{Refspec, Remote, SyncOptions};

    async fn local_graph(
        e: &Eng,
    ) -> Graph<HistorySource<'_, asc_store::bucket::BucketObjects, asc_store::sqlrefs::SqlRefs>> {
        Graph::new(
            HistorySource::load(e, &MemBoundaries::default())
                .await
                .unwrap(),
        )
    }

    async fn full_log(e: &Eng, q: LogQuery) -> Vec<ChangesetId> {
        local_graph(e).await.log(q).try_collect().await.unwrap()
    }

    fn from(id: ChangesetId) -> LogQuery {
        LogQuery {
            include: vec![id],
            ..LogQuery::default()
        }
    }

    /// A server with a 40-changeset line, a side branch merged back, and a client that
    /// fetched only `window` generations.
    struct Fixture {
        _dir: tempfile::TempDir,
        server: Arc<Eng>,
        client: Arc<Eng>,
        remote: MockRemote,
        boundaries: MemBoundaries,
        ids: Vec<ChangesetId>,
        side: ChangesetId,
    }

    async fn fixture(window: u32) -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let server_refs = sqlite(dir.path(), "server.db").await;
        let client_refs = sqlite(dir.path(), "client.db").await;
        let server = engine(&server_refs, 1);
        let (chain, first) = Chain::new(&server).await;
        let mut ids = chain.line(&server, first, 40).await;
        let mut side = ids[5];
        for i in 0..3 {
            side = chain.add(&server, &[side], 1000 + i).await;
        }
        let merge = chain.add(&server, &[ids[39], side], 2000).await;
        ids.push(merge);
        let tip = chain.add(&server, &[merge], 3000).await;
        ids.push(tip);
        set_ref(&server, "refs/heads/master", tip).await;
        let client = engine(&client_refs, 2);
        let remote = MockRemote::server(&server);
        let boundaries = MemBoundaries::default();
        let mut opts = SyncOptions::new(&boundaries);
        opts.window = Some(window);
        client
            .fetch(&remote, &Refspec::defaults("origin"), &opts)
            .await
            .unwrap();
        Fixture {
            _dir: dir,
            server,
            client,
            remote,
            boundaries,
            ids,
            side,
        }
    }

    fn server_history(f: &Fixture) -> Arc<dyn RemoteHistory> {
        Arc::new(ServerHistory(f.remote.history().unwrap()))
    }

    #[tokio::test]
    async fn walks_past_the_boundary_match_full_history() {
        let f = fixture(12).await;
        assert!(!f.boundaries.list().await.unwrap().is_empty());
        let g = windowed_graph(&f.client, &f.boundaries, server_history(&f))
            .await
            .unwrap();
        let tip = *f.ids.last().unwrap();
        let want = full_log(&f.server, from(tip)).await;
        let got: Vec<ChangesetId> = g.log(from(tip)).try_collect().await.unwrap();
        assert_eq!(got, want);
        // skip and limit count matches across the boundary.
        let q = LogQuery {
            skip: 10,
            limit: Some(25),
            ..from(tip)
        };
        let got: Vec<ChangesetId> = g.log(q).try_collect().await.unwrap();
        assert_eq!(got, want[10..35]);
        // Exclusion reaches across the boundary too.
        let q = LogQuery {
            exclude: vec![f.ids[3]],
            ..from(tip)
        };
        let got: Vec<ChangesetId> = g.log(q.clone()).try_collect().await.unwrap();
        assert_eq!(got, full_log(&f.server, q).await);
        assert!(got.len() < want.len());
        // Ancestry and merge bases agree with the server's.
        let sg = local_graph(&f.server).await;
        for &a in &[f.ids[0], f.ids[4], f.ids[20], f.side, f.ids[39]] {
            let w = sg.is_ancestor(a, tip).await.unwrap();
            assert_eq!(g.is_ancestor(a, tip).await.unwrap(), w, "is_ancestor {a}");
        }
        assert!(!g.is_ancestor(tip, f.ids[1]).await.unwrap());
        for &(a, b) in &[
            (f.side, f.ids[39]),
            (f.ids[2], f.side),
            (tip, f.side),
            (f.ids[30], f.ids[2]),
        ] {
            let w = sg.merge_base(a, &[b], true).await.unwrap();
            assert_eq!(
                g.merge_base(a, &[b], true).await.unwrap(),
                w,
                "merge_base {a} {b}"
            );
        }
        assert!(f.remote.trips.history_calls.load(Ordering::SeqCst) > 0);
    }

    #[tokio::test]
    async fn local_commits_on_a_windowed_clone_walk_through_the_server() {
        let f = fixture(12).await;
        let tip = *f.ids.last().unwrap();
        // The client needs the tip's trees to build on it; its commit exists on no server.
        let root = f.client.get_changeset(tip).await.unwrap().root;
        f.client.prefetch(&f.remote, root, None).await.unwrap();
        let local = commit(&f.client, &[tip], b"local only").await;
        let gen_key = ObjKey::Derived {
            name: Gen::NAME,
            version: Gen::VERSION,
            id: *local.as_bytes(),
        };
        let g = windowed_graph(&f.client, &f.boundaries, server_history(&f))
            .await
            .unwrap();
        // `warm` needs git ids the window cannot derive; the graph needs only `gen`.
        f.client.get_derived::<Gen>(local).await.unwrap();
        assert!(f.client.objects.has_many(&[gen_key]).await.unwrap()[0]);
        let got: Vec<ChangesetId> = g.log(from(local)).try_collect().await.unwrap();
        let mut want = vec![local];
        want.extend(full_log(&f.server, from(tip)).await);
        assert_eq!(got, want);
        // `side` is an ancestor of the merge under `local`; an old line commit is too.
        assert_eq!(
            g.merge_base(local, &[f.side], false).await.unwrap(),
            vec![f.side]
        );
        assert_eq!(
            g.merge_base(local, &[f.ids[3]], false).await.unwrap(),
            vec![f.ids[3]]
        );
        assert!(g.is_ancestor(f.ids[0], local).await.unwrap());
    }

    #[tokio::test]
    async fn without_a_remote_a_boundary_is_an_error_and_offline_names_the_rpc() {
        let f = fixture(12).await;
        let tip = *f.ids.last().unwrap();
        let offline: Arc<dyn RemoteHistory> = Arc::new(OfflineHistory {
            remote: "origin".into(),
        });
        let g = windowed_graph(&f.client, &f.boundaries, offline)
            .await
            .unwrap();
        // Inside the window everything works offline (the merge's second parent is outside it,
        // so start below the merge).
        let q = LogQuery {
            limit: Some(5),
            ..from(f.ids[39])
        };
        let near: Vec<ChangesetId> = g.log(q).try_collect().await.unwrap();
        assert_eq!(near.len(), 5);
        let err = g.log(from(tip)).try_collect::<Vec<_>>().await.unwrap_err();
        assert_eq!(
            err.to_string(),
            "asc: offline (origin unreachable): log needs Log"
        );
        let err = g.is_ancestor(f.ids[0], tip).await.unwrap_err();
        assert!(
            matches!(
                err,
                Error::Offline {
                    rpc: "IsAncestor",
                    ..
                }
            ),
            "{err}"
        );
        // A graph with no continuation reports the boundary itself.
        let bare = Graph::new(HistorySource::load(&f.client, &f.boundaries).await.unwrap());
        let err = bare.is_ancestor(f.ids[33], tip).await.unwrap_err();
        assert!(matches!(err, Error::Boundary { .. }), "{err}");
    }

    #[tokio::test]
    async fn engine_log_pages_resume_from_tokens() {
        let f = fixture(1000).await;
        let tip = *f.ids.last().unwrap();
        let want = full_log(&f.server, from(tip)).await;
        let q = LogQuery {
            limit: Some(40),
            ..from(tip)
        };
        let mut got = Vec::new();
        let mut token = None;
        loop {
            let page = HistoryApi::log(&*f.server, &q, token.as_ref())
                .await
                .unwrap();
            got.extend(page.entries.iter().map(|e| e.id));
            match page.next {
                Some(t) => token = Some(t),
                None => break,
            }
        }
        assert_eq!(got, want[..40]);
        let bad = PageToken(vec![1, 2]);
        let page = HistoryApi::log(&*f.server, &q, Some(&bad)).await;
        assert!(matches!(page, Err(Error::Query(_))));
    }

    #[tokio::test]
    async fn history_api_answers_queries_for_unknown_ids() {
        let f = fixture(1000).await;
        let tip = *f.ids.last().unwrap();
        let unknown = ChangesetId::of(b"nope");
        assert!(
            !HistoryApi::is_ancestor(&*f.server, unknown, tip)
                .await
                .unwrap()
        );
        assert!(
            HistoryApi::merge_base(&*f.server, unknown, &[tip], true)
                .await
                .unwrap()
                .is_empty()
        );
        let specs = [
            "refs/heads/master".to_owned(),
            "master~1".to_owned(),
            "nonesuch".to_owned(),
        ];
        let got = HistoryApi::resolve(&*f.server, &specs).await.unwrap();
        assert_eq!(got[0], Some(tip));
        assert_eq!(got[1], Some(f.ids[f.ids.len() - 2]));
        assert_eq!(got[2], None);
        let map = HistoryApi::git_map(&*f.server, &[tip], &[]).await.unwrap();
        assert_eq!(map.asc_to_git.len(), 1);
        let git = [GitOid::from_bytes([1; 20])];
        assert!(HistoryApi::git_map(&*f.server, &[], &git).await.is_err());
        let d = HistoryApi::diff(&*f.server, f.ids[0], f.ids[1], &[])
            .await
            .unwrap();
        assert!(d.is_empty(), "same tree, no change");
    }

    #[tokio::test]
    async fn the_diff_rpc_is_bounded() {
        let dir = tempfile::tempdir().unwrap();
        let refs = sqlite(dir.path(), "server.db").await;
        let e = engine(&refs, 1);
        let policy = crate::commit::PathPolicy::Native { windows_safe: true };
        let a = commit_files(&e, &[], &[("a", b"1")], policy, Vec::new()).await;
        let files: Vec<(String, Vec<u8>)> = (0..32).map(|i| (format!("d/f{i}"), vec![i])).collect();
        let files: Vec<(&str, &[u8])> = files
            .iter()
            .map(|(p, c)| (p.as_str(), c.as_slice()))
            .collect();
        let b = commit_files(&e, &[a], &files, policy, Vec::new()).await;
        // The wire entry point uses the RPC budget and the changes fit under it.
        assert_eq!(HistoryApi::diff(&*e, a, b, &[]).await.unwrap().len(), 32);
        // A budget below the number of entries the diff must visit is an error, not a partial list.
        let err = e.diff_bounded(a, b, None, 8).await.unwrap_err();
        assert!(
            matches!(
                err,
                Error::Engine(commit::Error::Tree(crate::tree::Error::TooLarge(8)))
            ),
            "{err}"
        );
    }
}
