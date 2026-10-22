//! Fetch, push and clone (SPEC §6.3).
//!
//! Negotiation is want-only: a fetch names explicit ids, a push asks `has_many` about the
//! closure it would send. Nothing here lists a whole history; a server remote answers `Log`
//! within the history window, a bucket remote is walked one generation level per round trip.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use asc_core::codec::{CodecError, decode_envelope};
use asc_core::id::{ChangesetId, FileId, RefTarget, TagId, TreeId};
use asc_core::object::{Change, Changeset, ChunkList, Kind, TagRecord};
use asc_core::path::{Name, RefName, RepoPath};
use asc_store::key::ObjKey;
use asc_store::objects::{self, Objects, decode_verified};
use asc_store::refs::{Actor, Expect, RefEntry, RefError, RefTxn, RefUpdate, Refs, RepoKey};
use futures::stream::{self, BoxStream, StreamExt, TryStreamExt};

use crate::derive::{self, Derived, Gen};
use crate::graph::{self, Graph, LogQuery, RemoteHistory};
use crate::history::{
    BoundaryStore, DEFAULT_WINDOW, HistoryApi, HistorySource, LogEntry, OfflineHistory,
    ServerHistory,
};
use crate::tree::{self, Pathspec};
use crate::{Engine, commit};

pub mod changes;

/// Changeset ids per fetch request (SPEC §6.3).
pub const GET_BATCH: usize = 256;
/// Concurrent fetch streams.
pub const GET_STREAMS: usize = 8;
/// Keys per `has_many` request.
pub const HAS_BATCH: usize = 4096;
/// Concurrent upload streams.
pub const PUT_STREAMS: usize = 8;
/// Largest upload batch, in stored bytes.
pub const PUT_BATCH_BYTES: usize = 16 * 1024 * 1024;
/// Objects per upload batch, whatever their size.
const PUT_BATCH_OBJECTS: usize = 1024;
/// Files above this size are stored as chunk lists (SPEC §2.5).
const CHUNK_THRESHOLD: u64 = 4 * 1024 * 1024;
/// Concurrent point reads against a bucket.
const BUCKET_READS: usize = 16;

/// Sync failures.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Store(#[from] objects::Error),
    #[error(transparent)]
    Refs(#[from] RefError),
    #[error(transparent)]
    Commit(#[from] commit::Error),
    #[error(transparent)]
    Tree(#[from] tree::Error),
    #[error(transparent)]
    Graph(#[from] graph::Error),
    #[error(transparent)]
    Derive(#[from] derive::Error),
    #[error("object does not decode: {0}")]
    Codec(#[from] CodecError),
    #[error(transparent)]
    Path(#[from] asc_core::path::PathError),
    #[error("the remote does not have {0}")]
    RemoteMissing(ObjKey),
    #[error("{0} is missing locally")]
    LocalMissing(ObjKey),
    /// A changeset in the push closure carries a raw git commit (SPEC §6.3).
    #[error("asc: import this history on the server (asc serve admin import)")]
    RawGit(ChangesetId),
    #[error("! [rejected] {name} ({reason})")]
    Rejected { name: RefName, reason: &'static str },
    #[error("invalid refspec {0:?}")]
    Refspec(String),
    #[error("the remote sent a log entry for {0} that its body contradicts")]
    Inconsistent(ChangesetId),
    #[error("remote: {0}")]
    Remote(String),
    /// Deciding a fast-forward needed more history than `asc.fetch.ancestryBudget` allows.
    #[error(
        "cannot tell whether {name} fast-forwards: more than {budget} changesets of history \
         would have to be fetched (raise asc.fetch.ancestryBudget or fetch with --unshallow)"
    )]
    AncestryBudget { name: String, budget: usize },
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

// ---- Remote ----------------------------------------------------------------------------

/// A repository to fetch from and push to.
#[async_trait]
pub trait Remote: Send + Sync {
    /// Name used in error messages and remote-tracking refs.
    fn name(&self) -> &str;
    async fn list_refs(&self) -> Result<Vec<RefEntry>>;
    async fn has_many(&self, keys: &[ObjKey]) -> Result<Vec<bool>>;
    /// Stored bytes (`codec ‖ data`) of the keys that exist, in any order.
    fn get_many(&self, keys: Vec<ObjKey>) -> BoxStream<'_, Result<(ObjKey, Bytes)>>;
    /// Stores one batch of stored-bytes objects.
    async fn put_many(&self, objects: Vec<(ObjKey, Bytes)>) -> Result<()>;
    /// Applies the updates atomically; a failed expectation is `Error::Refs(Conflict)`.
    async fn update_refs(&self, updates: &[RefUpdate]) -> Result<()>;
    /// `push --no-ref`: registers the uploaded changesets without a ref (SPEC §8.7).
    async fn ingest(&self, _tips: &[ChangesetId]) -> Result<()> {
        Err(Error::Remote(
            "this remote cannot ingest without a ref".into(),
        ))
    }
    /// The remote's history service, when it has one (a server; not a bucket).
    fn history(&self) -> Option<Arc<dyn HistoryApi>> {
        None
    }
}

/// A shared bucket: objects in `O`, refs in the bucket's own `repos` row of `R` (SPEC §3.4).
pub struct BucketRemote<O: Objects, R: Refs> {
    name: String,
    objects: Arc<O>,
    refs: Arc<R>,
    repo: RepoKey,
}

impl<O: Objects, R: Refs> BucketRemote<O, R> {
    pub fn new(name: impl Into<String>, objects: Arc<O>, refs: Arc<R>, repo: RepoKey) -> Self {
        Self {
            name: name.into(),
            objects,
            refs,
            repo,
        }
    }
}

#[async_trait]
impl<O: Objects, R: Refs> Remote for BucketRemote<O, R> {
    fn name(&self) -> &str {
        &self.name
    }

    async fn list_refs(&self) -> Result<Vec<RefEntry>> {
        Ok(self.refs.list(self.repo, "").await?)
    }

    async fn has_many(&self, keys: &[ObjKey]) -> Result<Vec<bool>> {
        Ok(self.objects.has_many(keys).await?)
    }

    fn get_many(&self, keys: Vec<ObjKey>) -> BoxStream<'_, Result<(ObjKey, Bytes)>> {
        stream::iter(keys)
            .map(move |k| async move { Ok(self.objects.get_stored(&k).await?.map(|b| (k, b))) })
            .buffer_unordered(BUCKET_READS)
            .try_filter_map(|found| async move { Ok(found) })
            .boxed()
    }

    async fn put_many(&self, batch: Vec<(ObjKey, Bytes)>) -> Result<()> {
        stream::iter(batch)
            .map(move |(k, b)| async move { Ok::<_, Error>(self.objects.put(&k, b).await?) })
            .buffer_unordered(BUCKET_READS)
            .try_collect::<()>()
            .await
    }

    async fn update_refs(&self, updates: &[RefUpdate]) -> Result<()> {
        let txn = RefTxn {
            updates: updates.to_vec(),
            actor: Actor {
                id: "asc".into(),
                via: "bucket".into(),
            },
            reason: "push".into(),
            op_id: uuid::Uuid::now_v7(),
        };
        Ok(self.refs.update(self.repo, &txn).await?)
    }

    /// A bucket has no ingest index: the uploaded objects are the whole registration.
    async fn ingest(&self, _tips: &[ChangesetId]) -> Result<()> {
        Ok(())
    }
}

// ---- Refspecs --------------------------------------------------------------------------

/// A fetch refspec: `[+]src:dst` or `^pattern` (negative). One `*` per side.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Refspec {
    src: String,
    dst: String,
    force: bool,
    negative: bool,
}

impl Refspec {
    pub fn parse(s: &str) -> Result<Self> {
        let bad = || Error::Refspec(s.to_owned());
        if let Some(pat) = s.strip_prefix('^') {
            return (!pat.is_empty() && pat.matches('*').count() <= 1)
                .then(|| Self {
                    src: pat.to_owned(),
                    dst: String::new(),
                    force: false,
                    negative: true,
                })
                .ok_or_else(bad);
        }
        let (force, rest) = s.strip_prefix('+').map_or((false, s), |r| (true, r));
        let (src, dst) = rest.split_once(':').ok_or_else(bad)?;
        let stars = |p: &str| p.matches('*').count();
        if src.is_empty() || dst.is_empty() || stars(src) != stars(dst) || stars(src) > 1 {
            return Err(bad());
        }
        Ok(Self {
            src: src.to_owned(),
            dst: dst.to_owned(),
            force,
            negative: false,
        })
    }

    /// `+refs/heads/*:refs/remotes/<remote>/*` and `^refs/heads/agent/*` (SPEC §2.7).
    pub fn defaults(remote: &str) -> Vec<Self> {
        let parse = |s: String| Self::parse(&s).expect("well-formed default refspec");
        vec![
            parse(format!("+refs/heads/*:refs/remotes/{remote}/*")),
            parse("^refs/heads/agent/*".to_owned()),
        ]
    }

    /// The text captured by `*`, when `name` matches the source pattern.
    fn capture<'a>(&self, name: &'a str) -> Option<Option<&'a str>> {
        match self.src.split_once('*') {
            None => (self.src == name).then_some(None),
            Some((pre, post)) => {
                let mid = name.strip_prefix(pre)?.strip_suffix(post)?;
                Some(Some(mid))
            }
        }
    }

    /// The local name `name` maps to, for a positive spec.
    fn map(&self, name: &str) -> Option<String> {
        if self.negative {
            return None;
        }
        let cap = self.capture(name)?;
        Some(match (self.dst.split_once('*'), cap) {
            (Some((pre, post)), Some(mid)) => format!("{pre}{mid}{post}"),
            _ => self.dst.clone(),
        })
    }

    fn excludes(&self, name: &str) -> bool {
        self.negative && self.capture(name).is_some()
    }
}

// ---- Options and reports ---------------------------------------------------------------

/// Moves pushed objects from the store to the cache (SPEC §3.5 demotion).
#[async_trait]
pub trait Demote: Send + Sync {
    async fn demote(&self, keys: &[ObjKey]) -> Result<()>;
}

/// Unpacks a seed snapshot into the cache before a clone fetches (SPEC §12.3).
#[async_trait]
pub trait SeedUnpack: Send + Sync {
    async fn unpack(&self) -> Result<()>;
}

/// What a fetch or push needs besides the engine and the remote.
pub struct SyncOptions<'a> {
    /// Where the history boundaries live (`state.db.history_boundary`).
    pub boundaries: &'a dyn BoundaryStore,
    /// Generations of history kept below each fetched tip; `None` fetches everything.
    pub window: Option<u32>,
    /// After a fetch, warm the trees (then files) of the fetched tips under this pathspec.
    pub prefetch: Option<&'a Pathspec>,
    /// Pushed objects are demoted through this after the ref update.
    pub demote: Option<&'a dyn Demote>,
    /// Seed to unpack before `clone_minimal` fetches.
    pub seed: Option<&'a dyn SeedUnpack>,
    /// Extension hook (C23); a no-op by default.
    pub ext: &'a dyn SyncExt,
    /// Most changesets a fast-forward check may visit or fetch beyond the local window
    /// (`asc.fetch.ancestryBudget`); beyond it the ref move fails with `AncestryBudget`.
    pub ancestry_budget: usize,
}

/// Default for [`SyncOptions::ancestry_budget`].
pub const DEFAULT_ANCESTRY_BUDGET: usize = 100_000;

/// Extension point for sync (a no-op by default; C23 implements it).
pub trait SyncExt: Send + Sync {
    /// Extra objects to upload with a push whose closure holds the `new` changesets.
    fn extra_closure(&self, _new: &[ChangesetId]) -> Vec<ObjKey> {
        Vec::new()
    }
    /// Called with the verified `Log` entries a fetch receives.
    fn on_log_entries(&self, _entries: &[LogEntry]) {}
}

/// The default [`SyncExt`]: does nothing.
pub struct NoSyncExt;

impl SyncExt for NoSyncExt {}

impl<'a> SyncOptions<'a> {
    pub fn new(boundaries: &'a dyn BoundaryStore) -> Self {
        Self {
            boundaries,
            window: Some(DEFAULT_WINDOW),
            prefetch: None,
            demote: None,
            seed: None,
            ext: &NoSyncExt,
            ancestry_budget: DEFAULT_ANCESTRY_BUDGET,
        }
    }
}

/// A remote-tracking ref that moved.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RefChange {
    pub name: RefName,
    pub old: Option<RefTarget>,
    pub new: RefTarget,
}

/// An empty transaction for the remote-tracking refs a sync would move.
fn empty_txn(via: &str, reason: String) -> RefTxn {
    RefTxn {
        updates: Vec::new(),
        actor: Actor {
            id: "asc".into(),
            via: via.into(),
        },
        reason,
        op_id: uuid::Uuid::now_v7(),
    }
}

/// Result of a fetch.
#[derive(Clone, Debug)]
pub struct FetchReport {
    /// The remote-tracking moves, for the caller to apply through `Ctx::update_local_refs`
    /// (SPEC §9.4.1). The engine never writes a local ref.
    pub local_updates: RefTxn,
    pub updated: Vec<RefChange>,
    /// Refs left alone because the update was not a fast-forward and the refspec had no `+`.
    pub rejected: Vec<RefName>,
    /// Changeset bodies downloaded.
    pub changesets: usize,
    /// Trees and files downloaded by `prefetch`.
    pub prefetched: usize,
}

impl Default for FetchReport {
    fn default() -> Self {
        Self {
            local_updates: empty_txn("fetch", "fetch".into()),
            updated: Vec::new(),
            rejected: Vec::new(),
            changesets: 0,
            prefetched: 0,
        }
    }
}

/// How a push treats the remote's current ref values.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PushMode {
    /// Update only when the remote still holds the expected value and the move is a
    /// fast-forward.
    FastForward,
    /// `--force`: overwrite whatever is there.
    Force,
    /// `--force-with-lease`: the expected value must hold, but the move may rewrite.
    ForceWithLease,
    /// `--no-ref`: upload the closure of the tips and `ingest` them.
    NoRef,
}

/// One ref a push moves.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PushUpdate {
    pub dst: RefName,
    /// `None` deletes the ref.
    pub new: Option<RefTarget>,
    /// The remote-tracking value (or the lease); ignored by `Force`.
    pub expect: Expect,
}

/// Result of a push.
#[derive(Clone, Debug)]
pub struct PushReport {
    /// The remote-tracking moves the push implies, for the caller to apply (SPEC §9.4.1).
    pub local_updates: RefTxn,
    /// Changesets that were not on the remote.
    pub changesets: usize,
    pub files: usize,
    pub trees: usize,
    /// Objects sent, whatever their kind.
    pub objects: usize,
    pub bytes: u64,
    /// Refs updated on the remote.
    pub refs: Vec<RefName>,
}

impl Default for PushReport {
    fn default() -> Self {
        Self {
            local_updates: empty_txn("push", "push".into()),
            changesets: 0,
            files: 0,
            trees: 0,
            objects: 0,
            bytes: 0,
            refs: Vec::new(),
        }
    }
}

/// Result of `clone_minimal`.
#[derive(Clone, Debug)]
pub struct CloneReport {
    pub head: Option<ChangesetId>,
    /// The fetch's remote-tracking moves (a copy of `fetch.local_updates`).
    pub local_updates: RefTxn,
    pub fetch: FetchReport,
}

impl Default for CloneReport {
    fn default() -> Self {
        Self {
            head: None,
            local_updates: empty_txn("clone", "clone".into()),
            fetch: FetchReport::default(),
        }
    }
}

// ---- Helpers ---------------------------------------------------------------------------

fn derived_key<D: Derived>(id: ChangesetId) -> ObjKey {
    ObjKey::Derived {
        name: D::NAME,
        version: D::VERSION,
        id: *id.as_bytes(),
    }
}

/// Fetches `keys` from `remote` in `GET_BATCH` batches over `GET_STREAMS` streams.
fn get_batches<'a>(
    remote: &'a dyn Remote,
    keys: Vec<ObjKey>,
) -> impl futures::Stream<Item = Result<Vec<(ObjKey, Bytes)>>> + 'a {
    let batches: Vec<Vec<ObjKey>> = keys.chunks(GET_BATCH).map(<[ObjKey]>::to_vec).collect();
    stream::iter(batches)
        .map(move |b| async move { remote.get_many(b).try_collect::<Vec<_>>().await })
        .buffer_unordered(GET_STREAMS)
}

impl<O: Objects, R: Refs> Engine<O, R> {
    /// Presence of `keys` in the local store, in `HAS_BATCH` requests.
    async fn local_has(&self, keys: &[ObjKey]) -> Result<Vec<bool>> {
        let mut out = Vec::with_capacity(keys.len());
        for chunk in keys.chunks(HAS_BATCH) {
            out.extend(self.objects.has_many(chunk).await?);
        }
        Ok(out)
    }

    async fn remote_has(&self, remote: &dyn Remote, keys: &[ObjKey]) -> Result<Vec<bool>> {
        let batches: Vec<&[ObjKey]> = keys.chunks(HAS_BATCH).collect();
        let parts: Vec<Vec<bool>> = stream::iter(batches)
            .map(|b| remote.has_many(b))
            .buffered(4)
            .try_collect()
            .await?;
        Ok(parts.into_iter().flatten().collect())
    }

    /// The subset of `ids` whose body and `gen.v1` are both local.
    async fn complete_changesets(&self, ids: &[ChangesetId]) -> Result<HashSet<ChangesetId>> {
        let keys: Vec<ObjKey> = ids
            .iter()
            .flat_map(|&i| [ObjKey::Changeset(i), derived_key::<Gen>(i)])
            .collect();
        let has = self.local_has(&keys).await?;
        Ok(ids
            .iter()
            .zip(has.chunks(2))
            .filter(|(_, h)| h[0] && h[1])
            .map(|(&i, _)| i)
            .collect())
    }

    /// A graph over the windowed local history, continued by `remote`'s history service.
    async fn sync_graph<'a>(
        &'a self,
        remote: &dyn Remote,
        boundaries: &dyn BoundaryStore,
    ) -> Result<Graph<HistorySource<'a, O, R>>> {
        let hook: Arc<dyn RemoteHistory> = match remote.history() {
            Some(h) => Arc::new(ServerHistory(h)),
            None => Arc::new(OfflineHistory {
                remote: remote.name().to_owned(),
            }),
        };
        Ok(crate::history::windowed_graph(self, boundaries, hook).await?)
    }

    /// Downloads changeset bodies, storing each after verifying it against its key.
    ///
    /// Only requested keys are accepted; anything else the remote sends is ignored, and a
    /// requested body that never arrives is an error. With `with_gen` the remote's `gen.v1`
    /// values are also requested and returned as unverified claims: they are never stored
    /// here (see [`Engine::store_verified_gens`]).
    async fn fetch_bodies(
        &self,
        remote: &dyn Remote,
        ids: &[ChangesetId],
        with_gen: bool,
    ) -> Result<(Vec<(ChangesetId, Changeset)>, HashMap<ChangesetId, u32>)> {
        let mut requested: HashSet<ObjKey> = HashSet::new();
        let mut keys = Vec::with_capacity(ids.len());
        for &id in ids {
            if requested.insert(ObjKey::Changeset(id)) {
                keys.push(ObjKey::Changeset(id));
                if with_gen {
                    let k = derived_key::<Gen>(id);
                    requested.insert(k);
                    keys.push(k);
                }
            }
        }
        let mut out = Vec::with_capacity(ids.len());
        let mut received: HashSet<ChangesetId> = HashSet::new();
        let mut claims: HashMap<ChangesetId, u32> = HashMap::new();
        let mut batches = std::pin::pin!(get_batches(remote, keys));
        while let Some(batch) = batches.try_next().await? {
            for (key, stored) in batch {
                if !requested.contains(&key) {
                    continue;
                }
                match key {
                    ObjKey::Changeset(id) => {
                        if !received.insert(id) {
                            continue;
                        }
                        let data = decode_verified(&key, &stored)?;
                        out.push((id, decode_envelope::<Changeset>(&data)?));
                        self.objects.put(&key, stored).await?;
                    }
                    ObjKey::Derived { id, .. } => {
                        let id = ChangesetId::from_bytes(id);
                        let claim = asc_store::codec::decode_stored_capped(&stored, 16)
                            .ok()
                            .and_then(|b| Gen::decode(&b).ok());
                        if let Some(g) = claim {
                            claims.insert(id, g.generation);
                        }
                    }
                    _ => {}
                }
            }
        }
        if let Some(gone) = ids.iter().find(|i| !received.contains(i)) {
            return Err(Error::RemoteMissing(ObjKey::Changeset(*gone)));
        }
        Ok((out, claims))
    }

    /// The generations of `ids` that are known from local bodies, and the ids whose body is
    /// local at all. A stored memo is used as is; a body without a memo has its generation
    /// derived from the body (and its local ancestors) when that is possible.
    async fn local_gens(
        &self,
        ids: &[ChangesetId],
    ) -> Result<(HashMap<ChangesetId, u32>, HashSet<ChangesetId>)> {
        let keys: Vec<ObjKey> = ids.iter().map(|&i| ObjKey::Changeset(i)).collect();
        let has = self.local_has(&keys).await?;
        let mut gens = HashMap::new();
        let mut bodies = HashSet::new();
        for (&id, present) in ids.iter().zip(has) {
            if !present {
                continue;
            }
            bodies.insert(id);
            if let Some(b) =
                crate::derive::read_memo(&*self.objects, &derived_key::<Gen>(id)).await?
                && let Ok(g) = Gen::decode(&b)
            {
                gens.insert(id, g.generation);
            } else if let Ok(g) = self.get_derived::<Gen>(id).await {
                gens.insert(id, g.generation);
            }
        }
        Ok((gens, bodies))
    }

    /// Checks remote generation claims against locally verified bodies and stores the ones
    /// that hold (`generation == 1 + max(parent generations)`). Claims that cannot be checked
    /// are dropped (a later `get_derived` computes them from the bodies); claims that
    /// contradict a verified parent are `Error::Inconsistent` when `strict`, else dropped.
    ///
    /// With `trust_window_edge`, an entry whose parents are outside the local store (the
    /// oldest entries of a history window) is accepted as claimed when it exceeds every
    /// parent known locally: nothing local can check it. Such a value only positions the
    /// window edge; ref moves never rely on it because ancestry for them is computed from
    /// bodies (see [`Engine::local_ancestor`]).
    async fn store_verified_gens(
        &self,
        claims: &[(ChangesetId, u32, Vec<ChangesetId>)],
        strict: bool,
        trust_window_edge: bool,
    ) -> Result<()> {
        let in_set: HashMap<ChangesetId, usize> =
            claims.iter().enumerate().map(|(i, c)| (c.0, i)).collect();
        let mut external: Vec<ChangesetId> = claims
            .iter()
            .flat_map(|c| c.2.iter().copied())
            .filter(|p| !in_set.contains_key(p))
            .collect();
        dedup(&mut external);
        let (local, local_bodies) = self.local_gens(&external).await?;
        // Kahn's algorithm over the in-set edges: parents are checked before children.
        let mut waiting: Vec<usize> = vec![0; claims.len()];
        let mut children: Vec<Vec<usize>> = vec![Vec::new(); claims.len()];
        for (i, c) in claims.iter().enumerate() {
            let mut ps = c.2.clone();
            dedup(&mut ps);
            for p in ps {
                if let Some(&j) = in_set.get(&p) {
                    waiting[i] += 1;
                    children[j].push(i);
                }
            }
        }
        let mut queue: Vec<usize> = (0..claims.len()).filter(|&i| waiting[i] == 0).collect();
        let mut value: Vec<Option<u32>> = vec![None; claims.len()];
        let mut done = 0;
        while let Some(i) = queue.pop() {
            done += 1;
            let (id, claim, parents) = &claims[i];
            let mut max = 0u32;
            let mut unknown = false;
            let mut bad = false;
            for p in parents {
                let g = match in_set.get(p) {
                    Some(&j) => {
                        bad |= value[j].is_none();
                        value[j]
                    }
                    None => local.get(p).copied(),
                };
                match g {
                    Some(g) => max = max.max(g),
                    None => unknown = true,
                }
            }
            let outside = parents
                .iter()
                .any(|p| !in_set.contains_key(p) && !local_bodies.contains(p));
            if !unknown {
                if *claim == max.saturating_add(1) {
                    value[i] = Some(*claim);
                } else if strict {
                    return Err(Error::Inconsistent(*id));
                }
            } else if trust_window_edge && outside && !bad && *claim > max {
                value[i] = Some(*claim);
            }
            for &c in &children[i] {
                waiting[c] -= 1;
                if waiting[c] == 0 {
                    queue.push(c);
                }
            }
        }
        if done != claims.len() {
            // A cycle: no honest history has one.
            let stuck = (0..claims.len()).find(|&i| waiting[i] > 0).unwrap_or(0);
            return Err(Error::Inconsistent(claims[stuck].0));
        }
        for (i, v) in value.iter().enumerate() {
            if let Some(g) = v {
                let memo = Gen { generation: *g };
                self.objects
                    .put(
                        &derived_key::<Gen>(claims[i].0),
                        asc_store::codec::encode_stored(&memo.encode()),
                    )
                    .await?;
            }
        }
        Ok(())
    }

    /// Downloads `keys` that are not local, verifying and storing each.
    pub(crate) async fn fetch_objects(
        &self,
        remote: &dyn Remote,
        keys: Vec<ObjKey>,
    ) -> Result<usize> {
        let has = self.local_has(&keys).await?;
        let want: Vec<ObjKey> = keys
            .iter()
            .zip(&has)
            .filter(|(_, h)| !**h)
            .map(|(k, _)| *k)
            .collect();
        let wanted = want.len();
        let asked: HashSet<ObjKey> = want.iter().copied().collect();
        let mut got = HashSet::new();
        let mut batches = std::pin::pin!(get_batches(remote, want.clone()));
        while let Some(batch) = batches.try_next().await? {
            for (key, stored) in batch {
                if !asked.contains(&key) || got.contains(&key) {
                    continue;
                }
                decode_verified(&key, &stored)?;
                self.objects.put(&key, stored).await?;
                got.insert(key);
            }
        }
        if let Some(k) = want.iter().find(|k| !got.contains(k)) {
            return Err(Error::RemoteMissing(*k));
        }
        Ok(wanted)
    }

    // ---- fetch -------------------------------------------------------------------------

    /// Fetches the refs `specs` select and the history they need (SPEC §6.3).
    ///
    /// With a history service, `Log(include = new tips, exclude = known tips)` supplies `gen`
    /// without deriving and only the window below each tip is downloaded; without one the
    /// parents are walked one generation level per round trip.
    pub async fn fetch(
        &self,
        remote: &dyn Remote,
        specs: &[Refspec],
        opts: &SyncOptions<'_>,
    ) -> Result<FetchReport> {
        let remote_refs = remote.list_refs().await?;
        let mut wanted: Vec<(RefName, RefTarget, bool)> = Vec::new();
        for r in remote_refs {
            let name = String::from_utf8_lossy(r.name.as_bytes()).into_owned();
            if specs.iter().any(|s| s.excludes(&name)) {
                continue;
            }
            let Some((dst, force)) = specs
                .iter()
                .find_map(|s| s.map(&name).map(|d| (d, s.force)))
            else {
                continue;
            };
            let dst = RefName::new_onelevel(dst).map_err(|_| Error::Refspec(name))?;
            wanted.push((dst, r.target, force));
        }
        let mut changed = Vec::new();
        for (dst, target, force) in wanted {
            let old = self.refs.get(self.repo, &dst).await?.map(|e| e.target);
            if old != Some(target) {
                changed.push((dst, old, target, force));
            }
        }
        let mut report = FetchReport::default();
        if changed.is_empty() {
            return Ok(report);
        }
        let mut tips = Vec::new();
        let mut exclude = Vec::new();
        for (_, old, target, _) in &changed {
            if let Some(RefTarget::Changeset(c)) = old {
                exclude.push(*c);
            }
            match *target {
                RefTarget::Changeset(c) => tips.push(c),
                RefTarget::Tag(t) => tips.push(self.fetch_tag(remote, t).await?),
            }
        }
        dedup(&mut tips);
        dedup(&mut exclude);
        let known = self.complete_changesets(&tips).await?;
        tips.retain(|t| !known.contains(t));
        let exclude: Vec<ChangesetId> = {
            let have = self.complete_changesets(&exclude).await?;
            exclude.into_iter().filter(|c| have.contains(c)).collect()
        };
        report.changesets = match remote.history() {
            Some(h) => {
                self.pull_history(remote, &*h, &tips, &exclude, opts.window, opts)
                    .await?
            }
            None => self.pull_walk(remote, &tips).await?,
        };
        let graph = self.sync_graph(remote, opts.boundaries).await?;
        let mut txn = Vec::new();
        for (dst, old, new, force) in changed {
            if !force
                && old.is_some()
                && !self
                    .fast_forward(remote, &graph, &dst, old, new, opts)
                    .await?
            {
                report.rejected.push(dst);
                continue;
            }
            txn.push(RefUpdate {
                name: dst.clone(),
                expect: old.map_or(Expect::Absent, Expect::Value),
                new: Some(new),
                expires_at: None,
            });
            report.updated.push(RefChange {
                name: dst,
                old,
                new,
            });
        }
        // The engine never moves a local ref (SPEC 9.4.1): the caller applies this.
        report.local_updates = RefTxn {
            updates: txn,
            ..empty_txn("fetch", format!("fetch {}", remote.name()))
        };
        if let Some(spec) = opts.prefetch {
            let mut roots = Vec::new();
            for c in &report.updated {
                if let Some(id) = self.peel_to_changeset(c.new).await? {
                    roots.push(self.get_changeset(id).await?.root);
                }
            }
            dedup(&mut roots);
            for root in roots {
                report.prefetched += self.prefetch(remote, root, Some(spec)).await?;
            }
        }
        Ok(report)
    }

    /// True when `old -> new` moves a changeset forward (a tag never fast-forwards).
    /// Decided from locally verified bodies, never from the remote's answer. Fails with
    /// `AncestryBudget` when the answer would cost more than the budget.
    async fn fast_forward<S: graph::GraphSource>(
        &self,
        remote: &dyn Remote,
        graph: &Graph<S>,
        name: &RefName,
        old: Option<RefTarget>,
        new: RefTarget,
        opts: &SyncOptions<'_>,
    ) -> Result<bool> {
        match (old, new) {
            (Some(RefTarget::Changeset(o)), RefTarget::Changeset(n)) => {
                match self.local_ancestor(remote, graph, name, o, n, opts).await {
                    Err(e @ Error::AncestryBudget { .. }) => Err(e),
                    other => Ok(other.unwrap_or(false)),
                }
            }
            _ => Ok(false),
        }
    }

    /// True when `a` is `b` or an ancestor of `b`, computed from verified changeset bodies.
    ///
    /// One walk from `b`, resumed after each fetch (the visited set and frontier carry over),
    /// pruned by generation. Bodies past the local history window are fetched from `remote`
    /// (each verified against its id) and their generations stored where the parents allow.
    /// The remote's `IsAncestor` is never consulted, because this answer can move a ref.
    /// More than `opts.ancestry_budget` changesets visited or fetched, or as many rounds,
    /// is `Error::AncestryBudget`.
    async fn local_ancestor<S: graph::GraphSource>(
        &self,
        remote: &dyn Remote,
        graph: &Graph<S>,
        name: &RefName,
        a: ChangesetId,
        b: ChangesetId,
        opts: &SyncOptions<'_>,
    ) -> Result<bool> {
        let budget = opts.ancestry_budget;
        let over = || Error::AncestryBudget {
            name: String::from_utf8_lossy(name.as_bytes()).into_owned(),
            budget,
        };
        let mut walk = graph::AncestryWalk::new(a, b);
        let (mut fetched, mut rounds) = (0usize, 0usize);
        loop {
            match graph.ancestry_step(&mut walk, budget).await? {
                graph::Ancestry::Yes => return Ok(true),
                graph::Ancestry::No => return Ok(false),
                graph::Ancestry::OverBudget => return Err(over()),
                graph::Ancestry::NeedBodies(ids) => {
                    rounds += 1;
                    fetched += ids.len();
                    if fetched > budget || rounds > budget {
                        return Err(over());
                    }
                    let (bodies, _) = self.fetch_bodies(remote, &ids, false).await?;
                    self.derive_local_gens(&bodies).await?;
                }
            }
        }
    }

    /// Computes (from the bodies, never from a claim) the `gen` memo of every body whose
    /// parents' generations are all local.
    async fn derive_local_gens(&self, bodies: &[(ChangesetId, Changeset)]) -> Result<()> {
        let mut parents: Vec<ChangesetId> = bodies
            .iter()
            .flat_map(|(_, cs)| cs.parents.iter().copied())
            .collect();
        dedup(&mut parents);
        let known = self.local_gens(&parents).await?.0;
        for (id, cs) in bodies {
            if cs.parents.iter().all(|p| known.contains_key(p)) {
                self.get_derived::<Gen>(*id).await?;
            }
        }
        Ok(())
    }

    async fn peel_to_changeset(&self, t: RefTarget) -> Result<Option<ChangesetId>> {
        Ok(match t {
            RefTarget::Changeset(c) => Some(c),
            RefTarget::Tag(t) => match self.objects.get(&ObjKey::Tag(t)).await? {
                Some(data) => Some(decode_envelope::<TagRecord>(&data)?.target),
                None => None,
            },
        })
    }

    /// Downloads a tag object and returns the changeset it names.
    async fn fetch_tag(&self, remote: &dyn Remote, tag: TagId) -> Result<ChangesetId> {
        self.fetch_objects(remote, vec![ObjKey::Tag(tag)]).await?;
        self.peel_to_changeset(RefTarget::Tag(tag))
            .await?
            .ok_or(Error::LocalMissing(ObjKey::Tag(tag)))
    }

    /// Server fetch: stream `Log`, write `gen` rows and bodies, record the window's boundary.
    /// Returns the number of bodies downloaded.
    async fn pull_history(
        &self,
        remote: &dyn Remote,
        history: &dyn HistoryApi,
        tips: &[ChangesetId],
        exclude: &[ChangesetId],
        window: Option<u32>,
        opts: &SyncOptions<'_>,
    ) -> Result<usize> {
        let mut seen: HashSet<ChangesetId> = HashSet::new();
        let mut entries: Vec<LogEntry> = Vec::new();
        for &tip in tips {
            if seen.contains(&tip) {
                continue;
            }
            let q = LogQuery {
                include: vec![tip],
                exclude: exclude.to_vec(),
                limit: window.map(|w| w as usize),
                ..LogQuery::default()
            };
            let mut token = None;
            loop {
                let page = history.log(&q, token.as_ref()).await?;
                for e in page.entries {
                    if seen.insert(e.id) {
                        entries.push(e);
                    }
                }
                match page.next {
                    Some(t) => token = Some(t),
                    None => break,
                }
            }
        }
        // A body without its `gen` memo (a fetch that stopped between the two) is fetched again.
        let ids: Vec<ChangesetId> = entries.iter().map(|e| e.id).collect();
        let complete = self.complete_changesets(&ids).await?;
        let fresh: Vec<&LogEntry> = entries
            .iter()
            .filter(|e| !complete.contains(&e.id))
            .collect();
        // Bodies first, verified against their ids; then the log's claims are checked against
        // them. A git id from the log is a hint only: it is never stored as a memo (the
        // `GitCommit` derivation computes it from the body).
        let want: Vec<ChangesetId> = fresh.iter().map(|e| e.id).collect();
        let (bodies, _) = self.fetch_bodies(remote, &want, false).await?;
        let by_id: HashMap<ChangesetId, &LogEntry> = entries.iter().map(|e| (e.id, e)).collect();
        for (id, cs) in &bodies {
            match by_id.get(id) {
                Some(e) if e.parents == cs.parents => {}
                _ => return Err(Error::Inconsistent(*id)),
            }
        }
        // `gen` memos: checked against the verified bodies' parents (local memos or the
        // entries themselves); a contradicting claim fails the fetch before any memo is stored.
        let claims: Vec<(ChangesetId, u32, Vec<ChangesetId>)> = bodies
            .iter()
            .map(|(id, cs)| (*id, by_id[id].generation, cs.parents.clone()))
            .collect();
        self.store_verified_gens(&claims, true, true).await?;
        opts.ext.on_log_entries(&entries);
        self.update_boundaries(&entries, opts.boundaries).await?;
        Ok(bodies.len())
    }

    /// Records the oldest fetched changesets whose parents are not local, and drops stored
    /// boundaries whose parents have arrived.
    async fn update_boundaries(
        &self,
        entries: &[LogEntry],
        store: &dyn BoundaryStore,
    ) -> Result<()> {
        let in_set: HashSet<ChangesetId> = entries.iter().map(|e| e.id).collect();
        let mut outside: Vec<ChangesetId> = entries
            .iter()
            .flat_map(|e| e.parents.iter().copied())
            .filter(|p| !in_set.contains(p))
            .collect();
        dedup(&mut outside);
        let keys: Vec<ObjKey> = outside.iter().map(|&p| ObjKey::Changeset(p)).collect();
        let has = self.local_has(&keys).await?;
        let absent: HashSet<ChangesetId> = outside
            .iter()
            .zip(&has)
            .filter(|(_, h)| !**h)
            .map(|(p, _)| *p)
            .collect();
        let added: Vec<ChangesetId> = entries
            .iter()
            .filter(|e| e.parents.iter().any(|p| absent.contains(p)))
            .map(|e| e.id)
            .collect();
        store.add(&added).await?;
        // Stored boundaries whose parents are all local no longer bound anything.
        let stored = store.list().await?;
        let mut parents = Vec::new();
        let mut bodies = Vec::new();
        for b in stored {
            if let Ok(cs) = self.get_changeset(b).await {
                parents.extend(cs.parents.iter().copied());
                bodies.push((b, cs));
            }
        }
        dedup(&mut parents);
        let keys: Vec<ObjKey> = parents.iter().map(|&p| ObjKey::Changeset(p)).collect();
        let has = self.local_has(&keys).await?;
        let local: HashSet<ChangesetId> = parents
            .iter()
            .zip(&has)
            .filter(|(_, h)| **h)
            .map(|(p, _)| *p)
            .collect();
        let resolved: Vec<ChangesetId> = bodies
            .iter()
            .filter(|(_, cs)| cs.parents.iter().all(|p| local.contains(p)))
            .map(|(b, _)| *b)
            .collect();
        store.remove(&resolved).await?;
        Ok(())
    }

    /// Bucket fetch: request every unknown parent of the frontier in one batch per level,
    /// reading `gen` values beside the bodies when the bucket has them (verified before use).
    async fn pull_walk(&self, remote: &dyn Remote, tips: &[ChangesetId]) -> Result<usize> {
        let mut frontier: Vec<ChangesetId> = tips.to_vec();
        let mut fetched = 0;
        let mut claims: Vec<(ChangesetId, u32, Vec<ChangesetId>)> = Vec::new();
        while !frontier.is_empty() {
            dedup(&mut frontier);
            let keys: Vec<ObjKey> = frontier.iter().map(|&i| ObjKey::Changeset(i)).collect();
            let has = self.local_has(&keys).await?;
            let missing: Vec<ChangesetId> = frontier
                .iter()
                .zip(&has)
                .filter(|(_, h)| !**h)
                .map(|(i, _)| *i)
                .collect();
            let (bodies, gens) = self.fetch_bodies(remote, &missing, true).await?;
            fetched += bodies.len();
            for (id, cs) in &bodies {
                if let Some(&g) = gens.get(id) {
                    claims.push((*id, g, cs.parents.clone()));
                }
            }
            frontier = bodies.into_iter().flat_map(|(_, cs)| cs.parents).collect();
        }
        // The bucket's `gen` values save derivation, but only those that check out against
        // the verified bodies are kept; skip and git-commit memos are never taken from a
        // bucket (they are derived locally on demand).
        self.store_verified_gens(&claims, false, false).await?;
        for &tip in tips {
            self.get_derived::<Gen>(tip).await?;
        }
        Ok(fetched)
    }

    /// Widens the history window by `n` generations below every boundary (`None`: all of it).
    /// Returns the bodies downloaded. A bucket remote already has the full walk.
    pub async fn deepen(
        &self,
        remote: &dyn Remote,
        opts: &SyncOptions<'_>,
        n: Option<u32>,
    ) -> Result<usize> {
        let Some(history) = remote.history() else {
            return Ok(0);
        };
        let mut tips = Vec::new();
        for b in opts.boundaries.list().await? {
            tips.extend(self.get_changeset(b).await?.parents.iter().copied());
        }
        dedup(&mut tips);
        let keys: Vec<ObjKey> = tips.iter().map(|&p| ObjKey::Changeset(p)).collect();
        let has = self.local_has(&keys).await?;
        let tips: Vec<ChangesetId> = tips
            .into_iter()
            .zip(has)
            .filter(|(_, h)| !h)
            .map(|(t, _)| t)
            .collect();
        self.pull_history(remote, &*history, &tips, &[], n, opts)
            .await
    }

    /// `fetch --unshallow`: removes the window.
    pub async fn unshallow(&self, remote: &dyn Remote, opts: &SyncOptions<'_>) -> Result<usize> {
        self.deepen(remote, opts, None).await
    }

    // ---- prefetch and clone ------------------------------------------------------------

    /// Warms the trees below `root` level by level, then the files, under `spec`.
    /// Returns the objects downloaded.
    pub async fn prefetch(
        &self,
        remote: &dyn Remote,
        root: TreeId,
        spec: Option<&Pathspec>,
    ) -> Result<usize> {
        let mut downloaded = 0;
        let mut level: Vec<(TreeId, Option<RepoPath>)> = vec![(root, None)];
        let mut files: Vec<(FileId, u64)> = Vec::new();
        while !level.is_empty() {
            let keys: Vec<ObjKey> = level.iter().map(|(t, _)| ObjKey::Tree(*t)).collect();
            downloaded += self.fetch_objects(remote, keys).await?;
            let mut next = Vec::new();
            for (id, dir) in level {
                let tree = self.get_tree(id).await?;
                for e in tree.entries() {
                    let path = match &dir {
                        Some(d) => d.join(&e.name),
                        None => RepoPath::new(e.name.as_bytes())?,
                    };
                    match e.kind {
                        Kind::Dir => {
                            if spec.is_none_or(|s| s.may_contain_dir(&path)) {
                                next.push((TreeId::from_bytes(e.id), Some(path)));
                            }
                        }
                        Kind::File | Kind::Exec | Kind::Symlink => {
                            if spec.is_none_or(|s| s.matches(&path)) {
                                files.push((FileId::from_bytes(e.id), e.size));
                            }
                        }
                        Kind::Gitlink => {}
                    }
                }
            }
            level = next;
        }
        let mut keys = Vec::new();
        let mut chunked = Vec::new();
        for (f, size) in files {
            if size > CHUNK_THRESHOLD {
                chunked.push(ObjKey::ChunkList(f));
            } else {
                keys.push(ObjKey::File(f));
            }
        }
        downloaded += self.fetch_objects(remote, keys).await?;
        downloaded += self.fetch_objects(remote, chunked.clone()).await?;
        let mut chunks = Vec::new();
        for k in chunked {
            let ObjKey::ChunkList(f) = k else { continue };
            chunks.extend(self.chunk_keys(f).await?);
        }
        downloaded += self.fetch_objects(remote, chunks).await?;
        Ok(downloaded)
    }

    async fn chunk_keys(&self, file: FileId) -> Result<Vec<ObjKey>> {
        let key = ObjKey::ChunkList(file);
        let data = self
            .objects
            .get(&key)
            .await?
            .ok_or(Error::LocalMissing(key))?;
        let list: ChunkList = decode_envelope(&data)?;
        Ok(list.chunks.iter().map(|&(c, _)| ObjKey::Chunk(c)).collect())
    }

    /// Clones one branch: its tip, the root tree and the history within the window, after an
    /// optional seed unpack. Cost is O(window + root tree), whatever the history length.
    pub async fn clone_minimal(
        &self,
        remote: &dyn Remote,
        branch: &str,
        opts: &SyncOptions<'_>,
    ) -> Result<CloneReport> {
        if let Some(seed) = opts.seed {
            seed.unpack().await?;
        }
        let spec = Refspec::parse(&format!(
            "+refs/heads/{branch}:refs/remotes/{}/{branch}",
            remote.name()
        ))?;
        let mut fetch = self.fetch(remote, &[spec], opts).await?;
        let head = match fetch.updated.first() {
            Some(c) => self.peel_to_changeset(c.new).await?,
            None => None,
        };
        if let Some(h) = head {
            let root = self.get_changeset(h).await?.root;
            self.fetch_objects(remote, vec![ObjKey::Tree(root)]).await?;
        }
        let local_updates =
            std::mem::replace(&mut fetch.local_updates, empty_txn("clone", "clone".into()));
        fetch.local_updates = local_updates.clone();
        Ok(CloneReport {
            head,
            local_updates,
            fetch,
        })
    }

    // ---- push --------------------------------------------------------------------------

    /// Uploads the closure of the new targets, then moves the refs (SPEC §6.3).
    pub async fn push(
        &self,
        remote: &dyn Remote,
        updates: &[PushUpdate],
        mode: PushMode,
        opts: &SyncOptions<'_>,
    ) -> Result<PushReport> {
        let graph = self.sync_graph(remote, opts.boundaries).await?;
        let mut tips = Vec::new();
        let mut tags = Vec::new();
        for u in updates {
            match u.new {
                Some(RefTarget::Changeset(c)) => tips.push(c),
                Some(RefTarget::Tag(t)) => {
                    tags.push(t);
                    tips.push(self.fetch_local_tag(t).await?);
                }
                None => {}
            }
        }
        dedup(&mut tips);
        if mode == PushMode::FastForward {
            for u in updates {
                if let (Expect::Value(RefTarget::Changeset(old)), Some(RefTarget::Changeset(new))) =
                    (u.expect, u.new)
                {
                    // From verified bodies: the remote's answer never decides a ref move.
                    let ff = self
                        .local_ancestor(remote, &graph, &u.dst, old, new, opts)
                        .await;
                    if let Err(e @ Error::AncestryBudget { .. }) = ff {
                        return Err(e);
                    }
                    if !ff.unwrap_or(false) {
                        return Err(Error::Rejected {
                            name: u.dst.clone(),
                            reason: "non-fast-forward",
                        });
                    }
                }
            }
        }
        // Changesets the remote lacks: local history of the tips minus what its refs reach.
        let remote_refs = remote.list_refs().await?;
        let known: Vec<ChangesetId> = {
            let cands: Vec<ChangesetId> = remote_refs
                .iter()
                .filter_map(|r| match r.target {
                    RefTarget::Changeset(c) => Some(c),
                    RefTarget::Tag(_) => None,
                })
                .collect();
            let local = self
                .local_has(
                    &cands
                        .iter()
                        .map(|&c| ObjKey::Changeset(c))
                        .collect::<Vec<_>>(),
                )
                .await?;
            cands
                .into_iter()
                .zip(local)
                .filter(|(_, h)| *h)
                .map(|(c, _)| c)
                .collect()
        };
        let candidates: Vec<ChangesetId> = graph
            .log(LogQuery {
                include: tips.clone(),
                exclude: known,
                ..LogQuery::default()
            })
            .try_collect()
            .await?;
        let cand_keys: Vec<ObjKey> = candidates.iter().map(|&c| ObjKey::Changeset(c)).collect();
        let on_remote = self.remote_has(remote, &cand_keys).await?;
        // `log` yields children first; uploads and checks run parents first.
        let new_cs: Vec<ChangesetId> = candidates
            .iter()
            .zip(&on_remote)
            .filter(|(_, h)| !**h)
            .map(|(c, _)| *c)
            .rev()
            .collect();
        let mut bodies = Vec::with_capacity(new_cs.len());
        for &c in &new_cs {
            let cs = self.get_changeset(c).await?;
            if cs.extra.get(&b"git.commit".to_vec()).is_some() {
                return Err(Error::RawGit(c));
            }
            bodies.push(cs);
        }
        let mut report = PushReport {
            changesets: new_cs.len(),
            ..PushReport::default()
        };
        let mut sent: Vec<ObjKey> = Vec::new();

        // Files and chunks, then chunk lists; trees by layers; changesets; tags.
        let mut files: Vec<ObjKey> = Vec::new();
        let mut trees: HashSet<TreeId> = HashSet::new();
        for cs in &bodies {
            for (path, change) in cs.changes.as_slice() {
                self.spine(cs.root, path, &mut trees).await?;
                if let Change::Write { file, size, .. } = change {
                    files.push(if *size > CHUNK_THRESHOLD {
                        ObjKey::ChunkList(*file)
                    } else {
                        ObjKey::File(*file)
                    });
                }
            }
            trees.insert(cs.root);
        }
        dedup_keys(&mut files);
        let missing = self.missing_on_remote(remote, files).await?;
        let (lists, plain): (Vec<ObjKey>, Vec<ObjKey>) = missing
            .into_iter()
            .partition(|k| matches!(k, ObjKey::ChunkList(_)));
        let mut chunks = Vec::new();
        for k in &lists {
            if let ObjKey::ChunkList(f) = k {
                chunks.extend(self.chunk_keys(*f).await?);
            }
        }
        dedup_keys(&mut chunks);
        let chunks = self.missing_on_remote(remote, chunks).await?;
        report.files = plain.len() + lists.len();
        sent.extend(self.upload(remote, chunks, &mut report).await?);
        sent.extend(self.upload(remote, plain, &mut report).await?);
        sent.extend(self.upload(remote, lists, &mut report).await?);

        let tree_keys: Vec<ObjKey> = trees.iter().map(|&t| ObjKey::Tree(t)).collect();
        let missing_trees: HashSet<TreeId> = self
            .missing_on_remote(remote, tree_keys)
            .await?
            .into_iter()
            .filter_map(|k| match k {
                ObjKey::Tree(t) => Some(t),
                _ => None,
            })
            .collect();
        report.trees = missing_trees.len();
        for layer in self.tree_layers(&missing_trees).await? {
            sent.extend(self.upload(remote, layer, &mut report).await?);
        }
        sent.extend(
            self.upload(
                remote,
                new_cs.iter().map(|&c| ObjKey::Changeset(c)).collect(),
                &mut report,
            )
            .await?,
        );
        let tag_keys: Vec<ObjKey> = tags.iter().map(|&t| ObjKey::Tag(t)).collect();
        let tag_keys = self.missing_on_remote(remote, tag_keys).await?;
        sent.extend(self.upload(remote, tag_keys, &mut report).await?);
        let mut extra = opts.ext.extra_closure(&new_cs);
        dedup_keys(&mut extra);
        let extra = self.missing_on_remote(remote, extra).await?;
        sent.extend(self.upload(remote, extra, &mut report).await?);

        if mode == PushMode::NoRef {
            remote.ingest(&tips).await?;
        } else if !updates.is_empty() {
            let refs: Vec<RefUpdate> = updates
                .iter()
                .map(|u| RefUpdate {
                    name: u.dst.clone(),
                    expect: if mode == PushMode::Force {
                        Expect::Any
                    } else {
                        u.expect
                    },
                    new: u.new,
                    expires_at: None,
                })
                .collect();
            remote.update_refs(&refs).await?;
            report.refs = updates.iter().map(|u| u.dst.clone()).collect();
            // The remote-tracking refs the push implies; the caller applies them (SPEC 9.4.1).
            let prefix = b"refs/heads/";
            let tracking: Vec<RefUpdate> = updates
                .iter()
                .filter_map(|u| {
                    let short = u.dst.as_bytes().strip_prefix(prefix.as_slice())?;
                    let name = format!(
                        "refs/remotes/{}/{}",
                        remote.name(),
                        String::from_utf8_lossy(short)
                    );
                    Some(RefUpdate {
                        name: RefName::new_onelevel(name).ok()?,
                        expect: Expect::Any,
                        new: u.new,
                        expires_at: None,
                    })
                })
                .collect();
            report.local_updates = RefTxn {
                updates: tracking,
                ..empty_txn("push", format!("push {}", remote.name()))
            };
        }
        if let Some(d) = opts.demote {
            d.demote(&sent).await?;
        }
        Ok(report)
    }

    async fn fetch_local_tag(&self, tag: TagId) -> Result<ChangesetId> {
        self.peel_to_changeset(RefTarget::Tag(tag))
            .await?
            .ok_or(Error::LocalMissing(ObjKey::Tag(tag)))
    }

    /// Adds the trees on the spine from `root` down to `path`'s directory.
    async fn spine(&self, root: TreeId, path: &RepoPath, out: &mut HashSet<TreeId>) -> Result<()> {
        let comps: Vec<&[u8]> = path.components().collect();
        let mut cur = root;
        for (i, c) in comps.iter().enumerate() {
            let t = self.get_tree(cur).await?;
            out.insert(cur);
            if i + 1 == comps.len() {
                break;
            }
            match Name::new(*c).ok().and_then(|n| t.get(&n).cloned()) {
                Some(e) if e.kind == Kind::Dir => cur = TreeId::from_bytes(e.id),
                _ => break,
            }
        }
        Ok(())
    }

    async fn missing_on_remote(
        &self,
        remote: &dyn Remote,
        keys: Vec<ObjKey>,
    ) -> Result<Vec<ObjKey>> {
        let has = self.remote_has(remote, &keys).await?;
        Ok(keys
            .into_iter()
            .zip(has)
            .filter(|(_, h)| !h)
            .map(|(k, _)| k)
            .collect())
    }

    /// Groups `trees` so every tree comes after the new trees it contains.
    async fn tree_layers(&self, trees: &HashSet<TreeId>) -> Result<Vec<Vec<ObjKey>>> {
        let mut pending: HashMap<TreeId, usize> = HashMap::new();
        let mut parents: HashMap<TreeId, Vec<TreeId>> = HashMap::new();
        for &t in trees {
            let tree = self.get_tree(t).await?;
            let mut n = 0;
            for e in tree.entries() {
                let child = TreeId::from_bytes(e.id);
                if e.kind == Kind::Dir && trees.contains(&child) {
                    n += 1;
                    parents.entry(child).or_default().push(t);
                }
            }
            pending.insert(t, n);
        }
        let mut layer: Vec<TreeId> = pending
            .iter()
            .filter(|(_, n)| **n == 0)
            .map(|(t, _)| *t)
            .collect();
        let mut out = Vec::new();
        while !layer.is_empty() {
            let mut next = Vec::new();
            for t in &layer {
                for p in parents.get(t).into_iter().flatten() {
                    let n = pending.get_mut(p).expect("every tree has a count");
                    *n -= 1;
                    if *n == 0 {
                        next.push(*p);
                    }
                }
            }
            out.push(layer.into_iter().map(ObjKey::Tree).collect());
            layer = next;
        }
        Ok(out)
    }

    /// Sends `keys` in batches of at most `PUT_BATCH_BYTES` over `PUT_STREAMS` streams.
    /// Each call is a barrier: the next kind of object starts after this one finished.
    async fn upload(
        &self,
        remote: &dyn Remote,
        keys: Vec<ObjKey>,
        report: &mut PushReport,
    ) -> Result<Vec<ObjKey>> {
        if keys.is_empty() {
            return Ok(keys);
        }
        let objects = &self.objects;
        let batches = stream::try_unfold(keys.clone().into_iter(), move |mut it| async move {
            let mut batch = Vec::new();
            let mut bytes = 0usize;
            while bytes < PUT_BATCH_BYTES && batch.len() < PUT_BATCH_OBJECTS {
                let Some(k) = it.next() else { break };
                let stored = objects
                    .get_stored(&k)
                    .await?
                    .ok_or(Error::LocalMissing(k))?;
                bytes += stored.len();
                batch.push((k, stored));
            }
            Ok::<_, Error>((!batch.is_empty()).then_some((batch, it)))
        });
        let sizes = batches
            .map_ok(|batch| async move {
                let bytes: u64 = batch.iter().map(|(_, b)| b.len() as u64).sum();
                let n = batch.len();
                remote.put_many(batch).await?;
                Ok::<_, Error>((n, bytes))
            })
            .try_buffer_unordered(PUT_STREAMS);
        let mut sizes = std::pin::pin!(sizes);
        while let Some((n, bytes)) = sizes.try_next().await? {
            report.objects += n;
            report.bytes += bytes;
        }
        Ok(keys)
    }
}

fn dedup<T: Eq + std::hash::Hash + Copy>(v: &mut Vec<T>) {
    let mut seen = HashSet::new();
    v.retain(|x| seen.insert(*x));
}

fn dedup_keys(v: &mut Vec<ObjKey>) {
    dedup(v);
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::Ordering;

    use asc_store::bucket::BucketObjects;
    use asc_store::key::{KeyClass, KeyPrefix};
    use asc_store::sqlrefs::SqlRefs;

    use super::*;
    use crate::commit::PathPolicy;
    use crate::history::testkit::*;
    use crate::history::{MemBoundaries, ServerHistory, windowed_graph};

    fn master() -> RefName {
        RefName::new("refs/heads/master").unwrap()
    }

    fn tracking() -> RefName {
        RefName::new("refs/remotes/origin/master").unwrap()
    }

    async fn count_changesets(e: &Eng) -> usize {
        e.objects
            .list(KeyPrefix::class(KeyClass::Changeset))
            .try_collect::<Vec<_>>()
            .await
            .unwrap()
            .len()
    }

    async fn tracked(e: &Eng) -> Option<RefTarget> {
        e.refs
            .get(e.repo, &tracking())
            .await
            .unwrap()
            .map(|r| r.target)
    }

    /// What the CLI does with a report: apply its remote-tracking moves.
    async fn apply(e: &Eng, txn: &RefTxn) {
        if !txn.updates.is_empty() {
            e.refs.update(e.repo, txn).await.unwrap();
        }
    }

    fn create(id: ChangesetId) -> PushUpdate {
        PushUpdate {
            dst: master(),
            new: Some(RefTarget::Changeset(id)),
            expect: Expect::Absent,
        }
    }

    fn advance(old: ChangesetId, new: ChangesetId) -> PushUpdate {
        PushUpdate {
            dst: master(),
            new: Some(RefTarget::Changeset(new)),
            expect: Expect::Value(RefTarget::Changeset(old)),
        }
    }

    #[test]
    fn refspecs_map_and_negate() {
        let specs = Refspec::defaults("origin");
        assert_eq!(
            specs[0].map("refs/heads/feature/x").as_deref(),
            Some("refs/remotes/origin/feature/x")
        );
        assert_eq!(specs[0].map("refs/tags/v1"), None);
        assert!(specs[1].excludes("refs/heads/agent/claude-1"));
        assert!(!specs[1].excludes("refs/heads/agents"));
        assert!(specs[0].force && !specs[1].force);
        let one = Refspec::parse("refs/tags/v1:refs/tags/v1").unwrap();
        assert_eq!(one.map("refs/tags/v1").as_deref(), Some("refs/tags/v1"));
        assert!(!one.force);
        for bad in ["", "refs/heads/*", "a*:b", "^", "a:b*", "a**:b**"] {
            assert!(Refspec::parse(bad).is_err(), "{bad:?}");
        }
    }

    mod window {
        use super::*;

        async fn server_with_chain(
            dir: &std::path::Path,
            n: usize,
        ) -> (Arc<Eng>, Vec<ChangesetId>) {
            let refs = sqlite(dir, "server.db").await;
            let server = engine(&refs, 1);
            let (chain, first) = Chain::new(&server).await;
            let ids = chain.line(&server, first, n).await;
            set_ref(&server, "refs/heads/master", ids[n - 1]).await;
            (server, ids)
        }

        #[tokio::test]
        async fn a_100k_chain_fetches_the_window_and_log_past_it_equals_the_full_log() {
            const N: usize = 100_000;
            let dir = tempfile::tempdir().unwrap();
            let (server, ids) = server_with_chain(dir.path(), N).await;
            let client = engine(&sqlite(dir.path(), "client.db").await, 2);
            let remote = MockRemote::server(&server);
            let bounds = MemBoundaries::default();
            let opts = SyncOptions::new(&bounds);
            let report = client
                .fetch(&remote, &Refspec::defaults("origin"), &opts)
                .await
                .unwrap();
            assert_eq!(report.changesets, 10_000);
            assert_eq!(count_changesets(&client).await, 10_000);
            assert_eq!(tracked(&client).await, None, "fetch writes no ref");
            apply(&client, &report.local_updates).await;
            assert_eq!(
                tracked(&client).await,
                Some(RefTarget::Changeset(ids[N - 1]))
            );
            let marks = bounds.list().await.unwrap();
            assert_eq!(marks, vec![ids[N - 10_000]]);

            let hist = Arc::new(ServerHistory(remote.history().unwrap()));
            let g = windowed_graph(&client, &bounds, hist).await.unwrap();
            let got: Vec<ChangesetId> = g
                .log(LogQuery {
                    include: vec![ids[N - 1]],
                    ..LogQuery::default()
                })
                .try_collect()
                .await
                .unwrap();
            let want: Vec<ChangesetId> = ids.iter().rev().copied().collect();
            assert_eq!(got, want);
            // Ancestry across the boundary, and the base of an old and a new commit.
            assert!(g.is_ancestor(ids[5], ids[N - 1]).await.unwrap());
            assert_eq!(
                g.merge_base(ids[N - 1], &[ids[7]], false).await.unwrap(),
                vec![ids[7]]
            );
        }

        #[tokio::test]
        async fn a_10k_chain_fetch_makes_at_most_fifty_round_trips() {
            let dir = tempfile::tempdir().unwrap();
            let (server, ids) = server_with_chain(dir.path(), 10_000).await;
            let client = engine(&sqlite(dir.path(), "client.db").await, 2);
            let remote = MockRemote::server(&server);
            let bounds = MemBoundaries::default();
            let opts = SyncOptions::new(&bounds);
            let report = client
                .fetch(&remote, &Refspec::defaults("origin"), &opts)
                .await
                .unwrap();
            assert_eq!(report.changesets, 10_000);
            let trips = remote.trips.fetch_round_trips();
            assert!(trips <= 50, "{trips} round trips");
            assert!(
                bounds.list().await.unwrap().is_empty(),
                "whole history fits"
            );
            // Bodies arrive in batches of 256 and no tree or file is downloaded.
            assert_eq!(
                remote.trips.gets.load(Ordering::SeqCst),
                10_000_u64.div_ceil(256)
            );
            let tip_root = client.get_changeset(ids[9_999]).await.unwrap().root;
            assert!(
                !client
                    .objects
                    .has_many(&[ObjKey::Tree(tip_root)])
                    .await
                    .unwrap()[0]
            );
        }

        #[tokio::test]
        async fn deepen_and_unshallow_widen_the_window() {
            let dir = tempfile::tempdir().unwrap();
            let (server, ids) = server_with_chain(dir.path(), 200).await;
            let client = engine(&sqlite(dir.path(), "client.db").await, 2);
            let remote = MockRemote::server(&server);
            let bounds = MemBoundaries::default();
            let mut opts = SyncOptions::new(&bounds);
            opts.window = Some(50);
            client
                .fetch(&remote, &Refspec::defaults("origin"), &opts)
                .await
                .unwrap();
            assert_eq!(count_changesets(&client).await, 50);
            assert_eq!(bounds.list().await.unwrap(), vec![ids[150]]);
            let got = client.deepen(&remote, &opts, Some(30)).await.unwrap();
            assert_eq!(got, 30);
            assert_eq!(bounds.list().await.unwrap(), vec![ids[120]]);
            let got = client.unshallow(&remote, &opts).await.unwrap();
            assert_eq!(got, 120);
            assert!(bounds.list().await.unwrap().is_empty());
            // The whole log is now local: walking it makes no history calls.
            let calls = remote.trips.history_calls.load(Ordering::SeqCst);
            let hist = Arc::new(ServerHistory(remote.history().unwrap()));
            let g = windowed_graph(&client, &bounds, hist).await.unwrap();
            let n = g
                .count(LogQuery {
                    include: vec![ids[199]],
                    ..LogQuery::default()
                })
                .await
                .unwrap();
            assert_eq!(n, 200);
            assert_eq!(remote.trips.history_calls.load(Ordering::SeqCst), calls);
        }
    }

    #[tokio::test]
    async fn bucket_fetch_walks_levels_and_reads_batches() {
        let dir = tempfile::tempdir().unwrap();
        let refs = sqlite(dir.path(), "bucket.db").await;
        let bucket = engine(&refs, 99);
        let (chain, first) = Chain::new(&bucket).await;
        let ids = chain.line(&bucket, first, 1000).await;
        set_ref(&bucket, "refs/heads/master", ids[999]).await;
        let client = engine(&sqlite(dir.path(), "client.db").await, 2);
        let remote = MockRemote::bucket(bucket.objects.clone(), refs.clone(), 99);
        let bounds = MemBoundaries::default();
        let opts = SyncOptions::new(&bounds);
        let report = client
            .fetch(&remote, &Refspec::defaults("origin"), &opts)
            .await
            .unwrap();
        assert_eq!(report.changesets, 1000);
        apply(&client, &report.local_updates).await;
        let gets = remote.trips.gets.load(Ordering::SeqCst);
        assert!(gets <= 1000 / 256 + 1000, "{gets} batches");
        assert!(
            bounds.list().await.unwrap().is_empty(),
            "bucket keeps the full walk"
        );
        // The whole chain is local, with generations.
        assert_eq!(
            client
                .get_derived::<Gen>(ids[999])
                .await
                .unwrap()
                .generation,
            1000
        );
        // A second fetch finds nothing to do.
        let again = client
            .fetch(&remote, &Refspec::defaults("origin"), &opts)
            .await
            .unwrap();
        assert!(again.updated.is_empty() && again.local_updates.updates.is_empty());
        assert_eq!(again.changesets, 0);
    }

    #[tokio::test]
    async fn bucket_fetch_keeps_verified_gens_and_never_stores_remote_skip_or_git_memos() {
        let dir = tempfile::tempdir().unwrap();
        let refs = sqlite(dir.path(), "bucket.db").await;
        let bucket = engine(&refs, 99);
        let (chain, first) = Chain::new(&bucket).await;
        let ids = chain.line(&bucket, first, 40).await;
        set_ref(&bucket, "refs/heads/master", ids[39]).await;
        bucket.get_derived::<Gen>(ids[39]).await.unwrap();
        bucket.get_derived::<derive::Skip>(ids[39]).await.unwrap();
        let client = engine(&sqlite(dir.path(), "client.db").await, 2);
        let remote = MockRemote::bucket(bucket.objects.clone(), refs.clone(), 99);
        let bounds = MemBoundaries::default();
        client
            .fetch(
                &remote,
                &Refspec::defaults("origin"),
                &SyncOptions::new(&bounds),
            )
            .await
            .unwrap();
        let gens: Vec<ObjKey> = ids.iter().map(|&i| derived_key::<Gen>(i)).collect();
        assert!(
            client
                .objects
                .has_many(&gens)
                .await
                .unwrap()
                .iter()
                .all(|h| *h)
        );
        let skips: Vec<ObjKey> = ids
            .iter()
            .map(|&i| derived_key::<derive::Skip>(i))
            .collect();
        assert!(
            client
                .objects
                .has_many(&skips)
                .await
                .unwrap()
                .iter()
                .all(|h| !*h),
            "a bucket's skip memos are not trusted"
        );
    }

    #[tokio::test]
    async fn agent_refs_cost_no_changeset_downloads() {
        let dir = tempfile::tempdir().unwrap();
        let refs = sqlite(dir.path(), "server.db").await;
        let server = engine(&refs, 1);
        let (chain, first) = Chain::new(&server).await;
        let ids = chain.line(&server, first, 3).await;
        set_ref(&server, "refs/heads/master", ids[2]).await;
        for i in 0..500u32 {
            let c = chain.add(&server, &[ids[0]], 10_000 + i).await;
            set_ref(&server, &format!("refs/heads/agent/a{i}"), c).await;
        }
        let client = engine(&sqlite(dir.path(), "client.db").await, 2);
        let remote = MockRemote::server(&server);
        let bounds = MemBoundaries::default();
        let opts = SyncOptions::new(&bounds);
        let report = client
            .fetch(&remote, &Refspec::defaults("origin"), &opts)
            .await
            .unwrap();
        assert_eq!(report.changesets, 3);
        assert_eq!(remote.trips.got_changesets.load(Ordering::SeqCst), 3);
        assert_eq!(report.updated.len(), 1);
        // Naming one agent branch fetches just that branch.
        let one = Refspec::parse("+refs/heads/agent/a7:refs/remotes/origin/agent/a7").unwrap();
        let report = client.fetch(&remote, &[one], &opts).await.unwrap();
        assert_eq!(report.changesets, 1);
    }

    #[tokio::test]
    async fn fetch_rejects_non_fast_forward_without_plus() {
        let dir = tempfile::tempdir().unwrap();
        let refs = sqlite(dir.path(), "server.db").await;
        let server = engine(&refs, 1);
        let (chain, first) = Chain::new(&server).await;
        let a = chain.add(&server, &[first], 1).await;
        let b = chain.add(&server, &[first], 2).await;
        set_ref(&server, "refs/heads/master", a).await;
        let client = engine(&sqlite(dir.path(), "client.db").await, 2);
        let remote = MockRemote::server(&server);
        let bounds = MemBoundaries::default();
        let opts = SyncOptions::new(&bounds);
        let plain = [Refspec::parse("refs/heads/*:refs/remotes/origin/*").unwrap()];
        let first = client.fetch(&remote, &plain, &opts).await.unwrap();
        apply(&client, &first.local_updates).await;
        assert_eq!(tracked(&client).await, Some(RefTarget::Changeset(a)));
        set_ref(&server, "refs/heads/master", b).await;
        let report = client.fetch(&remote, &plain, &opts).await.unwrap();
        assert_eq!(report.rejected, vec![tracking()]);
        assert_eq!(tracked(&client).await, Some(RefTarget::Changeset(a)));
        let forced = Refspec::defaults("origin");
        let report = client.fetch(&remote, &forced, &opts).await.unwrap();
        assert_eq!(report.updated.len(), 1);
        apply(&client, &report.local_updates).await;
        assert_eq!(tracked(&client).await, Some(RefTarget::Changeset(b)));
    }

    #[tokio::test]
    async fn two_engines_on_one_machine_share_a_file_bucket() {
        let dir = tempfile::tempdir().unwrap();
        let refs = sqlite(dir.path(), "machine.db").await;
        let bucket_dir = tempfile::tempdir().unwrap();
        let bucket = Arc::new(BucketObjects::local(bucket_dir.path()).unwrap());
        let a = engine(&refs, 1);
        let b = engine(&refs, 2);
        let remote = MockRemote::bucket(bucket.clone(), refs.clone(), 99);
        let bounds = MemBoundaries::default();
        let opts = SyncOptions::new(&bounds);

        let c1 = commit(&a, &[], b"one").await;
        let c2 = commit(&a, &[c1], b"two").await;
        let report = a
            .push(&remote, &[create(c2)], PushMode::FastForward, &opts)
            .await
            .unwrap();
        assert_eq!((report.changesets, report.trees, report.files), (2, 3, 3));
        assert_eq!(report.refs, vec![master()]);

        let report = b
            .fetch(&remote, &Refspec::defaults("origin"), &opts)
            .await
            .unwrap();
        assert_eq!(report.changesets, 2);
        assert_eq!(tracked(&b).await, None);
        apply(&b, &report.local_updates).await;
        assert_eq!(tracked(&b).await, Some(RefTarget::Changeset(c2)));
        let root = b.get_changeset(c2).await.unwrap().root;
        assert!(
            !b.objects.has_many(&[ObjKey::Tree(root)]).await.unwrap()[0],
            "want-only"
        );

        // B builds on A's work (after warming the tip's trees) and A fetches it back.
        b.prefetch(&remote, root, None).await.unwrap();
        let c3 = commit(&b, &[c2], b"three").await;
        let report = b
            .push(&remote, &[advance(c2, c3)], PushMode::FastForward, &opts)
            .await
            .unwrap();
        // Only the changed spine travels: one file, the root tree, the changeset.
        assert_eq!(
            (
                report.changesets,
                report.trees,
                report.files,
                report.objects
            ),
            (1, 1, 1, 3)
        );
        let report = a
            .fetch(&remote, &Refspec::defaults("origin"), &opts)
            .await
            .unwrap();
        apply(&a, &report.local_updates).await;
        assert_eq!(tracked(&a).await, Some(RefTarget::Changeset(c3)));
        // A's refs and B's refs are separate rows of the one SqlRefs.
        assert_eq!(tracked(&b).await, Some(RefTarget::Changeset(c2)));
    }

    #[tokio::test]
    async fn push_rules_for_force_lease_and_rejection() {
        let dir = tempfile::tempdir().unwrap();
        let refs = sqlite(dir.path(), "machine.db").await;
        let bucket = Arc::new(BucketObjects::memory());
        let a = engine(&refs, 1);
        let remote = MockRemote::bucket(bucket, refs.clone(), 99);
        let bounds = MemBoundaries::default();
        let opts = SyncOptions::new(&bounds);
        let c1 = commit(&a, &[], b"one").await;
        let c2 = commit(&a, &[c1], b"two").await;
        let side = commit(&a, &[c1], b"side").await;
        a.push(&remote, &[create(c2)], PushMode::FastForward, &opts)
            .await
            .unwrap();
        // A rewrite is not a fast-forward and nothing is uploaded for it.
        let puts = remote.trips.puts.load(Ordering::SeqCst);
        let err = a
            .push(&remote, &[advance(c2, side)], PushMode::FastForward, &opts)
            .await
            .unwrap_err();
        assert!(
            matches!(
                err,
                Error::Rejected {
                    reason: "non-fast-forward",
                    ..
                }
            ),
            "{err}"
        );
        assert_eq!(remote.trips.puts.load(Ordering::SeqCst), puts);
        // A stale expectation is refused by the ref store, even with a lease.
        let stale = advance(c1, side);
        let err = a
            .push(&remote, &[stale], PushMode::ForceWithLease, &opts)
            .await
            .unwrap_err();
        assert!(matches!(err, Error::Refs(RefError::Conflict(_))), "{err}");
        // The right lease allows the rewrite; Force ignores the expectation.
        a.push(
            &remote,
            &[advance(c2, side)],
            PushMode::ForceWithLease,
            &opts,
        )
        .await
        .unwrap();
        let wrong = advance(c1, c2);
        a.push(&remote, &[wrong], PushMode::Force, &opts)
            .await
            .unwrap();
        let held = remote.list_refs().await.unwrap();
        assert_eq!(held[0].target, RefTarget::Changeset(c2));
    }

    #[tokio::test]
    async fn push_without_a_ref_uploads_the_closure_and_ingests() {
        let dir = tempfile::tempdir().unwrap();
        let refs = sqlite(dir.path(), "machine.db").await;
        let a = engine(&refs, 1);
        let remote = MockRemote::bucket(Arc::new(BucketObjects::memory()), refs.clone(), 99);
        let bounds = MemBoundaries::default();
        let c1 = commit(&a, &[], b"one").await;
        let update = PushUpdate {
            dst: RefName::new("refs/heads/unused").unwrap(),
            new: Some(RefTarget::Changeset(c1)),
            expect: Expect::Absent,
        };
        let report = a
            .push(
                &remote,
                &[update],
                PushMode::NoRef,
                &SyncOptions::new(&bounds),
            )
            .await
            .unwrap();
        assert_eq!(report.changesets, 1);
        assert!(report.refs.is_empty());
        assert!(remote.list_refs().await.unwrap().is_empty());
        assert!(remote.has_many(&[ObjKey::Changeset(c1)]).await.unwrap()[0]);
    }

    #[tokio::test]
    async fn a_locally_imported_raw_commit_fails_before_any_upload() {
        let dir = tempfile::tempdir().unwrap();
        let refs = sqlite(dir.path(), "machine.db").await;
        let a = engine(&refs, 1);
        let remote = MockRemote::bucket(Arc::new(BucketObjects::memory()), refs.clone(), 99);
        let bounds = MemBoundaries::default();
        let raw = commit_files(
            &a,
            &[],
            &[("f.txt", b"imported")],
            PathPolicy::Import,
            vec![(b"git.commit".to_vec(), b"tree 0\n\nraw".to_vec())],
        )
        .await;
        let tip = commit(&a, &[raw], b"native on top").await;
        let err = a
            .push(
                &remote,
                &[create(tip)],
                PushMode::FastForward,
                &SyncOptions::new(&bounds),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, Error::RawGit(id) if id == raw), "{err}");
        assert_eq!(
            err.to_string(),
            "asc: import this history on the server (asc serve admin import)"
        );
        assert_eq!(remote.trips.puts.load(Ordering::SeqCst), 0);
        assert!(remote.list_refs().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn an_interrupted_push_resumes_and_uploads_only_the_rest() {
        let dir = tempfile::tempdir().unwrap();
        let refs = sqlite(dir.path(), "machine.db").await;
        let a = engine(&refs, 1);
        let remote = MockRemote::bucket(Arc::new(BucketObjects::memory()), refs.clone(), 99);
        let bounds = MemBoundaries::default();
        let opts = SyncOptions::new(&bounds);
        let mut tip = commit(&a, &[], b"c0").await;
        for i in 1..5 {
            tip = commit(&a, &[tip], format!("c{i}").as_bytes()).await;
        }
        // 5 contents + the shared `fixed` file, 5 roots + `d`, 5 changesets.
        const TOTAL: u64 = 6 + 6 + 5;
        remote.budget.store(4, Ordering::SeqCst);
        let err = a
            .push(&remote, &[create(tip)], PushMode::FastForward, &opts)
            .await
            .unwrap_err();
        assert!(matches!(err, Error::Remote(_)), "{err}");
        assert_eq!(remote.trips.put_objects.load(Ordering::SeqCst), 4);
        assert!(
            remote.list_refs().await.unwrap().is_empty(),
            "no ref before the closure"
        );

        remote.budget.store(-1, Ordering::SeqCst);
        let report = a
            .push(&remote, &[create(tip)], PushMode::FastForward, &opts)
            .await
            .unwrap();
        assert_eq!(report.objects as u64, TOTAL - 4);
        assert_eq!(remote.trips.put_objects.load(Ordering::SeqCst), TOTAL);
        let held = remote.list_refs().await.unwrap();
        assert_eq!(held[0].target, RefTarget::Changeset(tip));
        // A third push has nothing left to send.
        let again = a
            .push(&remote, &[advance(tip, tip)], PushMode::FastForward, &opts)
            .await
            .unwrap();
        assert_eq!(again.objects, 0);
    }

    #[tokio::test]
    async fn chunked_files_travel_as_list_plus_chunks() {
        let dir = tempfile::tempdir().unwrap();
        let refs = sqlite(dir.path(), "machine.db").await;
        let a = engine(&refs, 1);
        let b = engine(&refs, 2);
        let remote = MockRemote::bucket(Arc::new(BucketObjects::memory()), refs.clone(), 99);
        let bounds = MemBoundaries::default();
        let opts = SyncOptions::new(&bounds);
        let mut x = 0x9e37_79b9_7f4a_7c15_u64;
        let big: Vec<u8> = (0..9 * 1024 * 1024)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                (x >> 24) as u8
            })
            .collect();
        let tip = commit_files(
            &a,
            &[],
            &[("big.bin", &big)],
            PathPolicy::Import,
            Vec::new(),
        )
        .await;
        let report = a
            .push(&remote, &[create(tip)], PushMode::FastForward, &opts)
            .await
            .unwrap();
        assert!(
            report.objects >= 4,
            "list, chunks, tree, changeset: {}",
            report.objects
        );
        b.fetch(&remote, &Refspec::defaults("origin"), &opts)
            .await
            .unwrap();
        let root = b.get_changeset(tip).await.unwrap().root;
        let got = b.prefetch(&remote, root, None).await.unwrap();
        assert!(got >= 3, "{got}");
        let ch = b.get_changeset(tip).await.unwrap();
        let (_, change) = &ch.changes.as_slice()[0];
        let Change::Write { file, .. } = change else {
            panic!("a write")
        };
        assert!(
            b.objects
                .has_many(&[ObjKey::ChunkList(*file)])
                .await
                .unwrap()[0]
        );
    }

    #[tokio::test]
    async fn prefetch_warms_trees_then_files_under_a_pathspec() {
        let dir = tempfile::tempdir().unwrap();
        let refs = sqlite(dir.path(), "server.db").await;
        let server = engine(&refs, 1);
        let tip = commit_files(
            &server,
            &[],
            &[("a/x.txt", b"x"), ("b/y.txt", b"y"), ("top.txt", b"t")],
            PathPolicy::Native { windows_safe: true },
            Vec::new(),
        )
        .await;
        set_ref(&server, "refs/heads/master", tip).await;
        let client = engine(&sqlite(dir.path(), "client.db").await, 2);
        let remote = MockRemote::server(&server);
        let bounds = MemBoundaries::default();
        let spec = Pathspec::parse(["a"]).unwrap();
        let mut opts = SyncOptions::new(&bounds);
        opts.prefetch = Some(&spec);
        let report = client
            .fetch(&remote, &Refspec::defaults("origin"), &opts)
            .await
            .unwrap();
        assert_eq!(report.prefetched, 3, "root, a/, a/x.txt");
        let root = client.get_changeset(tip).await.unwrap().root;
        let tree = client.get_tree(root).await.unwrap();
        let entry = |n: &str| tree.get(&Name::new(n).unwrap()).unwrap().clone();
        let has = |k: ObjKey| {
            let client = client.clone();
            async move { client.objects.has_many(&[k]).await.unwrap()[0] }
        };
        assert!(has(ObjKey::Tree(TreeId::from_bytes(entry("a").id))).await);
        assert!(!has(ObjKey::Tree(TreeId::from_bytes(entry("b").id))).await);
        assert!(!has(ObjKey::File(FileId::from_bytes(entry("top.txt").id))).await);
    }

    #[tokio::test]
    async fn clone_minimal_fetches_the_tip_root_tree_and_window_history() {
        let dir = tempfile::tempdir().unwrap();
        let refs = sqlite(dir.path(), "server.db").await;
        let server = engine(&refs, 1);
        let (chain, first) = Chain::new(&server).await;
        let ids = chain.line(&server, first, 300).await;
        set_ref(&server, "refs/heads/master", ids[299]).await;
        set_ref(&server, "refs/heads/other", ids[10]).await;
        let client = engine(&sqlite(dir.path(), "client.db").await, 2);
        let remote = MockRemote::server(&server);
        let bounds = MemBoundaries::default();
        let mut opts = SyncOptions::new(&bounds);
        opts.window = Some(100);
        let report = client
            .clone_minimal(&remote, "master", &opts)
            .await
            .unwrap();
        assert_eq!(report.head, Some(ids[299]));
        assert_eq!(count_changesets(&client).await, 100);
        let root = client.get_changeset(ids[299]).await.unwrap().root;
        assert!(
            client
                .objects
                .has_many(&[ObjKey::Tree(root)])
                .await
                .unwrap()[0]
        );
        // Only the named branch was tracked.
        let other = RefName::new("refs/remotes/origin/other").unwrap();
        assert!(
            client
                .refs
                .get(client.repo, &other)
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn a_seed_unpacks_before_the_fetch() {
        struct Seed(std::sync::atomic::AtomicBool);
        #[async_trait]
        impl SeedUnpack for Seed {
            async fn unpack(&self) -> Result<()> {
                self.0.store(true, Ordering::SeqCst);
                Ok(())
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let refs = sqlite(dir.path(), "server.db").await;
        let server = engine(&refs, 1);
        let c = commit(&server, &[], b"x").await;
        set_ref(&server, "refs/heads/master", c).await;
        let client = engine(&sqlite(dir.path(), "client.db").await, 2);
        let remote = MockRemote::server(&server);
        let bounds = MemBoundaries::default();
        let seed = Seed(Default::default());
        let mut opts = SyncOptions::new(&bounds);
        opts.seed = Some(&seed);
        client
            .clone_minimal(&remote, "master", &opts)
            .await
            .unwrap();
        assert!(seed.0.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn demotion_names_the_pushed_objects() {
        struct Rec(std::sync::Mutex<Vec<ObjKey>>);
        #[async_trait]
        impl Demote for Rec {
            async fn demote(&self, keys: &[ObjKey]) -> Result<()> {
                self.0.lock().unwrap().extend_from_slice(keys);
                Ok(())
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let refs = sqlite(dir.path(), "machine.db").await;
        let a = engine(&refs, 1);
        let remote = MockRemote::bucket(Arc::new(BucketObjects::memory()), refs.clone(), 99);
        let bounds = MemBoundaries::default();
        let rec = Rec(Default::default());
        let mut opts = SyncOptions::new(&bounds);
        opts.demote = Some(&rec);
        let c1 = commit(&a, &[], b"one").await;
        let report = a
            .push(&remote, &[create(c1)], PushMode::FastForward, &opts)
            .await
            .unwrap();
        let keys = rec.0.lock().unwrap();
        assert_eq!(keys.len(), report.objects);
        assert!(keys.contains(&ObjKey::Changeset(c1)));
    }

    /// A remote that lies: wrong generations and git ids in its log, a forged `IsAncestor`,
    /// objects nobody asked for, and a body it claims not to have.
    mod untrusted {
        use asc_core::id::GitOid;

        use super::*;
        use crate::derive::GitCommit;
        use crate::history::{GitMapping, LogEntries, PageToken};

        struct Liar {
            inner: Arc<dyn HistoryApi>,
            skew: u32,
        }

        #[async_trait]
        impl HistoryApi for Liar {
            async fn log(
                &self,
                q: &LogQuery,
                page: Option<&PageToken>,
            ) -> graph::Result<LogEntries> {
                let mut out = self.inner.log(q, page).await?;
                for e in &mut out.entries {
                    e.generation += self.skew;
                    e.git = Some(GitOid::from_bytes([7; 20]));
                }
                Ok(out)
            }
            async fn merge_base(
                &self,
                a: ChangesetId,
                others: &[ChangesetId],
                all: bool,
            ) -> graph::Result<Vec<ChangesetId>> {
                self.inner.merge_base(a, others, all).await
            }
            async fn is_ancestor(&self, _: ChangesetId, _: ChangesetId) -> graph::Result<bool> {
                Ok(true)
            }
            async fn diff(
                &self,
                a: ChangesetId,
                b: ChangesetId,
                paths: &[String],
            ) -> graph::Result<Vec<(RepoPath, Change)>> {
                self.inner.diff(a, b, paths).await
            }
            async fn resolve(&self, specs: &[String]) -> graph::Result<Vec<Option<ChangesetId>>> {
                self.inner.resolve(specs).await
            }
            async fn git_map(
                &self,
                asc: &[ChangesetId],
                git: &[GitOid],
            ) -> graph::Result<GitMapping> {
                self.inner.git_map(asc, git).await
            }
        }

        struct Evil {
            inner: MockRemote,
            history: Option<Arc<dyn HistoryApi>>,
            /// Sent with every `get_many` answer although nobody asked.
            extra: Vec<(ObjKey, Bytes)>,
            /// Never sent, though asked for.
            withhold: Option<ObjKey>,
        }

        impl Evil {
            fn new(inner: MockRemote) -> Self {
                Self {
                    inner,
                    history: None,
                    extra: Vec::new(),
                    withhold: None,
                }
            }
        }

        #[async_trait]
        impl Remote for Evil {
            fn name(&self) -> &str {
                "origin"
            }
            async fn list_refs(&self) -> Result<Vec<RefEntry>> {
                self.inner.list_refs().await
            }
            async fn has_many(&self, keys: &[ObjKey]) -> Result<Vec<bool>> {
                self.inner.has_many(keys).await
            }
            fn get_many(&self, keys: Vec<ObjKey>) -> BoxStream<'_, Result<(ObjKey, Bytes)>> {
                let withhold = self.withhold;
                let real = self
                    .inner
                    .get_many(keys)
                    .try_filter(move |(k, _)| futures::future::ready(Some(*k) != withhold));
                real.chain(stream::iter(self.extra.clone().into_iter().map(Ok)))
                    .boxed()
            }
            async fn put_many(&self, objects: Vec<(ObjKey, Bytes)>) -> Result<()> {
                self.inner.put_many(objects).await
            }
            async fn update_refs(&self, updates: &[RefUpdate]) -> Result<()> {
                self.inner.update_refs(updates).await
            }
            fn history(&self) -> Option<Arc<dyn HistoryApi>> {
                self.history.clone()
            }
        }

        fn lying(honest: &MockRemote, skew: u32) -> Option<Arc<dyn HistoryApi>> {
            Some(Arc::new(Liar {
                inner: honest.history().unwrap(),
                skew,
            }))
        }

        async fn server_chain(dir: &std::path::Path, n: usize) -> (Arc<Eng>, Vec<ChangesetId>) {
            let server = engine(&sqlite(dir, "server.db").await, 1);
            let (chain, first) = Chain::new(&server).await;
            let ids = chain.line(&server, first, n).await;
            set_ref(&server, "refs/heads/master", ids[n - 1]).await;
            (server, ids)
        }

        async fn has_memos<D: Derived>(e: &Eng, ids: &[ChangesetId]) -> Vec<bool> {
            let keys: Vec<ObjKey> = ids.iter().map(|&i| derived_key::<D>(i)).collect();
            e.objects.has_many(&keys).await.unwrap()
        }

        #[tokio::test]
        async fn wrong_generations_from_a_server_log_fail_the_fetch_and_store_no_memo() {
            let dir = tempfile::tempdir().unwrap();
            let (server, ids) = server_chain(dir.path(), 20).await;
            let client = engine(&sqlite(dir.path(), "client.db").await, 2);
            let mut remote = Evil::new(MockRemote::server(&server));
            remote.history = lying(&remote.inner, 5);
            let bounds = MemBoundaries::default();
            let err = client
                .fetch(
                    &remote,
                    &Refspec::defaults("origin"),
                    &SyncOptions::new(&bounds),
                )
                .await
                .unwrap_err();
            assert!(matches!(err, Error::Inconsistent(_)), "{err}");
            assert!(has_memos::<Gen>(&client, &ids).await.iter().all(|h| !*h));
            assert!(
                has_memos::<GitCommit>(&client, &ids)
                    .await
                    .iter()
                    .all(|h| !*h)
            );
        }

        #[tokio::test]
        async fn a_forged_git_id_in_an_honest_log_is_never_stored() {
            let dir = tempfile::tempdir().unwrap();
            let (server, ids) = server_chain(dir.path(), 20).await;
            let client = engine(&sqlite(dir.path(), "client.db").await, 2);
            let mut remote = Evil::new(MockRemote::server(&server));
            remote.history = lying(&remote.inner, 0);
            let bounds = MemBoundaries::default();
            client
                .fetch(
                    &remote,
                    &Refspec::defaults("origin"),
                    &SyncOptions::new(&bounds),
                )
                .await
                .unwrap();
            assert!(has_memos::<Gen>(&client, &ids).await.iter().all(|h| *h));
            assert!(
                has_memos::<GitCommit>(&client, &ids)
                    .await
                    .iter()
                    .all(|h| !*h)
            );
        }

        #[tokio::test]
        async fn a_bucket_with_wrong_generations_is_recomputed_not_trusted() {
            let dir = tempfile::tempdir().unwrap();
            let refs = sqlite(dir.path(), "bucket.db").await;
            let bucket = engine(&refs, 99);
            let (chain, first) = Chain::new(&bucket).await;
            let ids = chain.line(&bucket, first, 10).await;
            set_ref(&bucket, "refs/heads/master", ids[9]).await;
            for &i in &ids {
                bucket
                    .objects
                    .put(
                        &derived_key::<Gen>(i),
                        asc_store::codec::encode_stored(&Gen { generation: 777 }.encode()),
                    )
                    .await
                    .unwrap();
            }
            let client = engine(&sqlite(dir.path(), "client.db").await, 2);
            let remote = MockRemote::bucket(bucket.objects.clone(), refs.clone(), 99);
            let bounds = MemBoundaries::default();
            client
                .fetch(
                    &remote,
                    &Refspec::defaults("origin"),
                    &SyncOptions::new(&bounds),
                )
                .await
                .unwrap();
            for (i, &id) in ids.iter().enumerate() {
                let g = client.get_derived::<Gen>(id).await.unwrap().generation;
                assert_eq!(g, i as u32 + 1);
            }
        }

        #[tokio::test]
        async fn unrequested_objects_are_ignored_and_a_withheld_body_is_an_error() {
            let dir = tempfile::tempdir().unwrap();
            let (server, ids) = server_chain(dir.path(), 5).await;
            let (chain, first) = Chain::new(&server).await;
            let other = chain.add(&server, &[first], 99).await;
            let other_stored = server
                .objects
                .get_stored(&ObjKey::Changeset(other))
                .await
                .unwrap()
                .unwrap();
            let garbage = ObjKey::Changeset(ChangesetId::of(b"nobody asked"));
            let extra = vec![
                (ObjKey::Changeset(other), other_stored),
                (garbage, Bytes::from_static(b"\0junk")),
            ];
            let bounds = MemBoundaries::default();
            for (n, with_history) in [true, false].into_iter().enumerate() {
                let client = engine(&sqlite(dir.path(), &format!("c{n}.db")).await, 2);
                let mut remote = Evil::new(MockRemote::server(&server));
                if !with_history {
                    remote.inner = MockRemote::bucket(
                        server.objects.clone(),
                        server.refs.clone(),
                        server.repo.0,
                    );
                }
                remote.history = with_history.then(|| remote.inner.history().unwrap());
                remote.extra = extra.clone();
                client
                    .fetch(
                        &remote,
                        &Refspec::defaults("origin"),
                        &SyncOptions::new(&bounds),
                    )
                    .await
                    .unwrap();
                let present = client
                    .objects
                    .has_many(&[ObjKey::Changeset(other), garbage])
                    .await
                    .unwrap();
                assert_eq!(present, vec![false, false], "history={with_history}");

                // The same remote now withholds a body it was asked for.
                let client = engine(&sqlite(dir.path(), &format!("w{n}.db")).await, 3);
                remote.withhold = Some(ObjKey::Changeset(ids[2]));
                let err = client
                    .fetch(
                        &remote,
                        &Refspec::defaults("origin"),
                        &SyncOptions::new(&bounds),
                    )
                    .await
                    .unwrap_err();
                assert!(matches!(err, Error::RemoteMissing(_)), "{err}");
            }
        }

        #[tokio::test]
        async fn a_forged_is_ancestor_answer_cannot_fast_forward_a_ref_past_the_window() {
            let dir = tempfile::tempdir().unwrap();
            let (server, ids) = server_chain(dir.path(), 200).await;
            let client = engine(&sqlite(dir.path(), "client.db").await, 2);
            let bounds = MemBoundaries::default();
            let mut opts = SyncOptions::new(&bounds);
            opts.window = Some(50);
            let plain = [Refspec::parse("refs/heads/*:refs/remotes/origin/*").unwrap()];
            let first = client
                .fetch(&MockRemote::server(&server), &plain, &opts)
                .await
                .unwrap();
            apply(&client, &first.local_updates).await;
            assert_eq!(tracked(&client).await, Some(RefTarget::Changeset(ids[199])));
            // The server rewrites master onto a side branch forked far below the window.
            let (chain, _) = Chain::new(&server).await;
            let side = chain.add(&server, &[ids[10]], 4242).await;
            set_ref(&server, "refs/heads/master", side).await;
            let mut remote = Evil::new(MockRemote::server(&server));
            remote.history = lying(&remote.inner, 0);
            let report = client.fetch(&remote, &plain, &opts).await.unwrap();
            assert_eq!(report.rejected, vec![tracking()]);
            assert!(report.local_updates.updates.is_empty());
            assert_eq!(tracked(&client).await, Some(RefTarget::Changeset(ids[199])));
            // The body-only walk gives the same answer on its own.
            let hist = Arc::new(ServerHistory(remote.history().unwrap()));
            let g = windowed_graph(&client, &bounds, hist).await.unwrap();
            assert_eq!(
                g.ancestry_from_bodies(ids[199], side).await.unwrap(),
                graph::Ancestry::No
            );
        }

        #[tokio::test]
        async fn a_real_fast_forward_past_the_window_is_still_accepted() {
            let dir = tempfile::tempdir().unwrap();
            let (server, ids) = server_chain(dir.path(), 120).await;
            let client = engine(&sqlite(dir.path(), "client.db").await, 2);
            let bounds = MemBoundaries::default();
            let mut opts = SyncOptions::new(&bounds);
            opts.window = Some(20);
            let plain = [Refspec::parse("refs/heads/*:refs/remotes/origin/*").unwrap()];
            let remote = MockRemote::server(&server);
            set_ref(&server, "refs/heads/master", ids[30]).await;
            let first = client.fetch(&remote, &plain, &opts).await.unwrap();
            apply(&client, &first.local_updates).await;
            set_ref(&server, "refs/heads/master", ids[119]).await;
            let report = client.fetch(&remote, &plain, &opts).await.unwrap();
            assert!(report.rejected.is_empty());
            apply(&client, &report.local_updates).await;
            assert_eq!(tracked(&client).await, Some(RefTarget::Changeset(ids[119])));
        }

        async fn windowed_client(
            dir: &std::path::Path,
            server: &Arc<Eng>,
            window: u32,
        ) -> (Arc<Eng>, MockRemote, MemBoundaries) {
            let client = engine(&sqlite(dir, "client.db").await, 2);
            let remote = MockRemote::server(server);
            let bounds = MemBoundaries::default();
            let mut opts = SyncOptions::new(&bounds);
            opts.window = Some(window);
            client
                .fetch(&remote, &Refspec::defaults("origin"), &opts)
                .await
                .unwrap();
            (client, remote, bounds)
        }

        #[tokio::test]
        async fn the_ancestry_walk_stops_at_its_budget_and_refuses_the_move() {
            let dir = tempfile::tempdir().unwrap();
            let (server, ids) = server_chain(dir.path(), 120).await;
            let (client, remote, bounds) = windowed_client(dir.path(), &server, 20).await;
            // A local root unrelated to the remote chain: the walk must go to the root to know.
            let lone = commit(&client, &[], b"lone").await;
            let graph = client.sync_graph(&remote, &bounds).await.unwrap();
            let mut opts = SyncOptions::new(&bounds);
            opts.ancestry_budget = 30;
            let before = remote.trips.got_changesets.load(Ordering::SeqCst);
            let err = client
                .local_ancestor(&remote, &graph, &tracking(), lone, ids[119], &opts)
                .await
                .unwrap_err();
            assert!(
                matches!(err, Error::AncestryBudget { budget: 30, .. }),
                "{err}"
            );
            let got = remote.trips.got_changesets.load(Ordering::SeqCst) - before;
            assert!(got <= 30, "{got} bodies fetched past the budget");

            // The same check through a fetch refuses the ref move with the same error.
            let plain = [Refspec::parse("refs/heads/*:refs/remotes/origin/*").unwrap()];
            let tracking_txn = RefTxn {
                updates: vec![RefUpdate {
                    name: tracking(),
                    expect: Expect::Absent,
                    new: Some(RefTarget::Changeset(lone)),
                    expires_at: None,
                }],
                ..empty_txn("test", "seed".into())
            };
            apply(&client, &tracking_txn).await;
            let err = client.fetch(&remote, &plain, &opts).await.unwrap_err();
            assert!(matches!(err, Error::AncestryBudget { .. }), "{err}");
            assert_eq!(tracked(&client).await, Some(RefTarget::Changeset(lone)));

            // With the default budget the answer is a plain "no".
            let opts = SyncOptions::new(&bounds);
            assert!(
                !client
                    .local_ancestor(&remote, &graph, &tracking(), lone, ids[119], &opts)
                    .await
                    .unwrap()
            );
        }

        #[tokio::test]
        async fn the_ancestry_frontier_carries_across_fetch_rounds() {
            let dir = tempfile::tempdir().unwrap();
            let (server, ids) = server_chain(dir.path(), 120).await;
            let (client, remote, bounds) = windowed_client(dir.path(), &server, 20).await;
            let lone = commit(&client, &[], b"lone").await;
            let graph = client.sync_graph(&remote, &bounds).await.unwrap();
            let mut walk = graph::AncestryWalk::new(lone, ids[119]);
            let (mut rounds, mut last) = (0, 0);
            loop {
                match graph.ancestry_step(&mut walk, usize::MAX).await.unwrap() {
                    graph::Ancestry::No => break,
                    graph::Ancestry::NeedBodies(need) => {
                        rounds += 1;
                        // The chain needs one new body per round: the visited set grew by
                        // one and nothing was walked twice.
                        assert_eq!(need.len(), 1);
                        assert!(walk.visited() > last);
                        last = walk.visited();
                        client.fetch_bodies(&remote, &need, false).await.unwrap();
                    }
                    other => panic!("{other:?}"),
                }
            }
            assert_eq!(rounds, 100);
            assert_eq!(walk.visited(), 120);
        }

        #[tokio::test]
        async fn a_claim_is_checked_against_the_generation_of_a_local_body_without_a_memo() {
            let dir = tempfile::tempdir().unwrap();
            let (server, ids) = server_chain(dir.path(), 5).await;
            let client = engine(&sqlite(dir.path(), "client.db").await, 2);
            // The parents' bodies are local, but no memo is: only their bodies vouch for them.
            for &id in &ids[..3] {
                let key = ObjKey::Changeset(id);
                let stored = server.objects.get_stored(&key).await.unwrap().unwrap();
                client.objects.put(&key, stored).await.unwrap();
            }
            assert!(
                has_memos::<Gen>(&client, &ids[..3])
                    .await
                    .iter()
                    .all(|h| !*h)
            );
            let poisoned = [(ids[3], 99, vec![ids[2]])];
            let err = client
                .store_verified_gens(&poisoned, true, true)
                .await
                .unwrap_err();
            assert!(matches!(err, Error::Inconsistent(_)), "{err}");
            // Not strict: the claim is dropped, never stored.
            client
                .store_verified_gens(&poisoned, false, true)
                .await
                .unwrap();
            assert!(!has_memos::<Gen>(&client, &ids[3..4]).await[0]);
            // The true value passes.
            client
                .store_verified_gens(&[(ids[3], 4, vec![ids[2]])], true, true)
                .await
                .unwrap();
            assert!(has_memos::<Gen>(&client, &ids[3..4]).await[0]);
        }

        #[tokio::test]
        async fn push_returns_tracking_updates_and_calls_the_ext_hook() {
            struct Ext(std::sync::Mutex<Vec<usize>>);
            impl SyncExt for Ext {
                fn extra_closure(&self, new: &[ChangesetId]) -> Vec<ObjKey> {
                    self.0.lock().unwrap().push(new.len());
                    Vec::new()
                }
            }
            let dir = tempfile::tempdir().unwrap();
            let refs = sqlite(dir.path(), "machine.db").await;
            let a = engine(&refs, 1);
            let remote = MockRemote::bucket(Arc::new(BucketObjects::memory()), refs.clone(), 99);
            let bounds = MemBoundaries::default();
            let ext = Ext(Default::default());
            let mut opts = SyncOptions::new(&bounds);
            opts.ext = &ext;
            let c1 = commit(&a, &[], b"one").await;
            let report = a
                .push(&remote, &[create(c1)], PushMode::FastForward, &opts)
                .await
                .unwrap();
            assert_eq!(*ext.0.lock().unwrap(), vec![1]);
            assert_eq!(report.local_updates.updates.len(), 1);
            assert_eq!(report.local_updates.updates[0].name, tracking());
            assert_eq!(tracked(&a).await, None, "push writes no local ref");
        }
    }

    #[allow(dead_code)]
    fn assert_send(_: &SqlRefs) {}
}
