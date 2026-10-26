//! `manifests.v1`, `mset.v1`, `parsed.v1` and `pgraph.v1` (SPEC §12A.5).

use std::collections::BTreeMap;
use std::sync::Arc;

use asc_core::codec::decode_envelope;
use asc_core::id::{ChangesetId, FileId, GitOid, TreeId};
use asc_core::object::{Change, Changeset, ChunkList, Kind};
use asc_core::path::RepoPath;
use asc_graph::config::{self, ProjectTarget, ResolvedConfig};
use asc_graph::detect::{Claim, Detector, ParseFailure, claim_of};
use asc_graph::link::{LinkInput, link_graph};
use asc_graph::model::{
    Derived as Envelope, Ecosystem, MAX_LOCKFILE_BYTES, MAX_MANIFEST_BYTES, ManifestSet, Parsed,
    ProjectGraph, SetId, parsed_key,
};
use asc_graph::own::in_node_modules;
use asc_graph::pattern::IgnoreSet;
use asc_store::key::ObjKey;
use asc_store::objects::Objects;
use asc_store::refs::Refs;
use async_trait::async_trait;
use bytes::Bytes;
use futures::{StreamExt, TryStreamExt};

use super::{Error, Loaded, Result};
use crate::Engine;
use crate::derive::{self, Derived, read_memo, write_memo};

const MSET: &str = "mset";
const PARSED: &str = "parsed";
const PGRAPH: &str = "pgraph";

/// Files read and parsed concurrently while linking a cold graph.
const PARSE_WINDOW: usize = 16;

/// A manifest set entry without its path: kind and content id.
type SetEntry = (Kind, [u8; 32]);

/// `manifests.v1`: the id of the changeset's manifest set (SPEC §12A.5).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Manifests {
    pub set: SetId,
}

fn memo(name: &'static str, id: [u8; 32]) -> ObjKey {
    ObjKey::Derived {
        name,
        version: 1,
        id,
    }
}

/// `SetId` has no byte constructor; its postcard form is the 32 raw bytes.
fn set_id_from(b: &[u8]) -> Option<SetId> {
    if b.len() != 32 {
        return None;
    }
    postcard::from_bytes(b).ok()
}

/// A gitlink's 20-byte commit id, zero padded.
pub(crate) fn pad_oid(oid: &GitOid) -> [u8; 32] {
    let mut id = [0; 32];
    id[..20].copy_from_slice(oid.as_bytes());
    id
}

pub(crate) fn is_asc_config(path: &RepoPath) -> bool {
    matches!(path.as_bytes(), b".asc/projects.toml" | b".asc/ci.toml")
}

/// True for paths a manifest set holds. Config independent: disabled ecosystems and `ignore`
/// are applied when the graph is linked, so the set depends on the tree alone.
fn in_set(detectors: &[&dyn Detector], path: &RepoPath) -> bool {
    !in_node_modules(path)
        && (is_asc_config(path) || detectors.iter().any(|d| d.claim(path).is_some()))
}

fn claim_name(c: Claim) -> &'static str {
    match c {
        Claim::Manifest => "manifest",
        Claim::Lockfile => "lockfile",
        Claim::EcoGlobal => "eco-global",
    }
}

fn config_err(path: &RepoPath, message: impl ToString) -> Error {
    Error::Config {
        path: path.clone(),
        message: message.to_string(),
    }
}

/// File bytes, or a note that the file is over the limit (nothing is read then).
enum Blob {
    Bytes(Bytes),
    TooLarge,
}

#[async_trait]
impl Derived for Manifests {
    const NAME: &'static str = "manifests";
    const VERSION: u32 = 1;
    const EAGER: bool = false;

    async fn derive<O: Objects, R: Refs>(
        e: &Engine<O, R>,
        cs: &Changeset,
        _id: ChangesetId,
        parents: &[Self],
    ) -> derive::Result<Self> {
        let set = match (parents.first(), cs.parents.first()) {
            (Some(p), Some(pid)) => e.patched_set(cs, *pid, p.set).await,
            _ => e.root_set(cs.root).await,
        };
        set.map(|set| Self { set }).map_err(|err| match err {
            Error::Derive(d) => d,
            Error::Tree(t) => t.into(),
            Error::Store(s) => s.into(),
            Error::Commit(c) => c.into(),
            _ => derive::Error::BadValue { name: Self::NAME },
        })
    }

    fn encode(&self) -> Vec<u8> {
        self.set.as_bytes().to_vec()
    }

    fn decode(b: &[u8]) -> derive::Result<Self> {
        set_id_from(b)
            .map(|set| Self { set })
            .ok_or(derive::Error::BadValue { name: Self::NAME })
    }
}

impl<O: Objects, R: Refs> Engine<O, R> {
    /// The manifest set of `cs` with its id.
    pub(crate) async fn manifest_set(&self, cs: ChangesetId) -> Result<(SetId, Arc<ManifestSet>)> {
        let m = self.get_derived::<Manifests>(cs).await?;
        Ok((m.set, self.set_or_rebuild(m.set, cs).await?))
    }

    /// The set `id` of changeset `owner`; the evictable `mset` memo is rebuilt from the tree
    /// when it is gone.
    async fn set_or_rebuild(&self, id: SetId, owner: ChangesetId) -> Result<Arc<ManifestSet>> {
        if let Some(s) = self.load_set(id).await? {
            return Ok(s);
        }
        let root = self.get_changeset(owner).await?.root;
        let set = self.set_from_tree(root).await?;
        Ok(self.put_set(set).await?.1)
    }

    async fn root_set(&self, root: TreeId) -> Result<SetId> {
        let set = self.set_from_tree(root).await?;
        Ok(self.put_set(set).await?.0)
    }

    async fn load_set(&self, id: SetId) -> Result<Option<Arc<ManifestSet>>> {
        if let Some(s) = self.projects.sets.get(&id) {
            return Ok(Some(s));
        }
        let Some(b) = read_memo(&*self.objects, &memo(MSET, *id.as_bytes())).await? else {
            return Ok(None);
        };
        let Ok(set) = ManifestSet::from_envelope(&b) else {
            return Ok(None);
        };
        if set.id() != id {
            return Ok(None);
        }
        let set = Arc::new(set);
        self.projects.sets.insert(id, set.clone());
        Ok(Some(set))
    }

    /// Stores a set.
    async fn put_set(&self, set: ManifestSet) -> Result<(SetId, Arc<ManifestSet>)> {
        let id = set.id();
        if let Some(s) = self.projects.sets.get(&id) {
            return Ok((id, s));
        }
        write_memo(
            &*self.objects,
            &memo(MSET, *id.as_bytes()),
            &set.to_envelope(),
        )
        .await?;
        let set = Arc::new(set);
        self.projects.sets.insert(id, set.clone());
        Ok((id, set))
    }

    /// A root changeset's set: one walk of its tree.
    async fn set_from_tree(&self, root: TreeId) -> Result<ManifestSet> {
        let mut entries = Vec::new();
        let mut walk = self.walk(root, None);
        while let Some((path, e)) = walk.try_next().await? {
            match e.kind {
                Kind::Gitlink => entries.push((path, Kind::Gitlink, pad_oid(&e.git))),
                kind if in_set(self.projects.detectors, &path) => entries.push((path, kind, e.id)),
                _ => {}
            }
        }
        ManifestSet::new(entries).map_err(|e| config_err(&root_path(), e))
    }

    /// The parent's set with this changeset's claimed changes applied; the parent's own id
    /// when none changes it.
    async fn patched_set(&self, cs: &Changeset, parent: ChangesetId, set: SetId) -> Result<SetId> {
        let base = self.set_or_rebuild(set, parent).await?;
        let old = base.entries();
        let find = |p: &RepoPath| old.binary_search_by(|x| x.0.cmp(p)).ok().map(|i| &old[i]);
        // `None` removes the path.
        let mut edits: Vec<(&RepoPath, Option<SetEntry>)> = Vec::new();
        for (path, change) in cs.changes.as_slice() {
            let claimed = in_set(self.projects.detectors, path);
            let new = match change {
                Change::Gitlink { commit } => Some((Kind::Gitlink, pad_oid(commit))),
                Change::Write { kind, file, .. } if claimed => Some((*kind, *file.as_bytes())),
                // A claimed delete, or an unclaimed change over a gitlink, drops the path.
                Change::Write { .. } | Change::Delete => None,
            };
            let had = find(path);
            let differs = match (&new, had) {
                (None, None) => false,
                (None, Some(h)) => claimed || h.1 == Kind::Gitlink,
                (Some(n), Some(h)) => *n != (h.1, h.2),
                (Some(_), None) => true,
            };
            if differs {
                edits.push((path, new));
            }
        }
        if edits.is_empty() {
            return Ok(set);
        }
        let mut map: BTreeMap<RepoPath, SetEntry> = old
            .iter()
            .map(|(p, k, id)| (p.clone(), (*k, *id)))
            .collect();
        for (path, new) in edits {
            match new {
                Some(v) => map.insert(path.clone(), v),
                None => map.remove(path),
            };
        }
        let entries = map.into_iter().map(|(p, (k, id))| (p, k, id)).collect();
        let next = ManifestSet::new(entries).map_err(|e| config_err(&root_path(), e))?;
        Ok(self.put_set(next).await?.0)
    }

    /// Reads a file of at most `limit` bytes; the size is known only after the chunk list.
    async fn read_capped(&self, file: FileId, limit: usize) -> Result<Blob> {
        if let Some(b) = self.objects.get(&ObjKey::File(file)).await? {
            return Ok(if b.len() > limit {
                Blob::TooLarge
            } else {
                Blob::Bytes(b)
            });
        }
        let list = self
            .objects
            .get(&ObjKey::ChunkList(file))
            .await?
            .ok_or(Error::MissingFile(file))?;
        let list = decode_envelope::<ChunkList>(&list).map_err(|_| Error::MissingFile(file))?;
        if list.size > limit as u64 {
            return Ok(Blob::TooLarge);
        }
        self.read_file(file, list.size)
            .await
            .map(Blob::Bytes)
            .map_err(|_| Error::MissingFile(file))
    }

    /// The graph at `cs` with its set.
    pub(crate) async fn loaded(&self, cs: ChangesetId) -> Result<Arc<Loaded>> {
        let (id, set) = self.manifest_set(cs).await?;
        let root = self.get_changeset(cs).await?.root;
        self.graph_of(id, set, root).await
    }

    /// `pgraph.v1`: a function of the set. A cached graph is reused only while its declared
    /// project roots still exist in `root`.
    async fn graph_of(
        &self,
        id: SetId,
        set: Arc<ManifestSet>,
        root: TreeId,
    ) -> Result<Arc<Loaded>> {
        let cached = match self.projects.graphs.get(&id) {
            Some(l) => Some(l),
            None => self.graph_memo(id, &set).await?,
        };
        if let Some(l) = cached
            && self.declared_present(&l.graph, root).await?.is_empty()
        {
            self.projects.graphs.insert(id, l.clone());
            return Ok(l);
        }
        let graph = self.build_graph(&set, root).await?;
        write_memo(
            &*self.objects,
            &memo(PGRAPH, *id.as_bytes()),
            &graph.to_envelope(),
        )
        .await?;
        let l = Arc::new(Loaded {
            id,
            set,
            graph: Arc::new(graph),
        });
        self.projects.graphs.insert(id, l.clone());
        Ok(l)
    }

    async fn graph_memo(&self, id: SetId, set: &Arc<ManifestSet>) -> Result<Option<Arc<Loaded>>> {
        let Some(b) = read_memo(&*self.objects, &memo(PGRAPH, *id.as_bytes())).await? else {
            return Ok(None);
        };
        Ok(ProjectGraph::from_envelope(&b).ok().map(|graph| {
            Arc::new(Loaded {
                id,
                set: set.clone(),
                graph: Arc::new(graph),
            })
        }))
    }

    /// Declared project roots missing from `root`.
    async fn declared_present(&self, graph: &ProjectGraph, root: TreeId) -> Result<Vec<RepoPath>> {
        let mut missing = Vec::new();
        for e in &graph.config.projects {
            if let ProjectTarget::Declared { root: r, .. } = &e.target
                && self.lookup(root, r).await?.is_none()
            {
                missing.push(r.clone());
            }
        }
        Ok(missing)
    }

    async fn read_config(&self, set: &ManifestSet) -> Result<ResolvedConfig> {
        let path = RepoPath::new(".asc/projects.toml").expect("static path");
        let entries = set.entries();
        let Ok(i) = entries.binary_search_by(|x| x.0.cmp(&path)) else {
            return Ok(ResolvedConfig::default());
        };
        let blob = self
            .read_capped(FileId::from_bytes(entries[i].2), MAX_MANIFEST_BYTES)
            .await?;
        let Blob::Bytes(b) = blob else {
            return Err(config_err(&path, "file exceeds the size limit"));
        };
        let text = std::str::from_utf8(&b).map_err(|_| config_err(&path, "not UTF-8"))?;
        config::parse(text).map_err(|e| config_err(&path, e))
    }

    async fn build_graph(&self, set: &ManifestSet, root: TreeId) -> Result<ProjectGraph> {
        let detectors = self.projects.detectors;
        let config = self.read_config(set).await?;
        let ignore = IgnoreSet::new(config.ignore.iter().map(String::as_str))?;
        let mut todo = Vec::new();
        let mut gitlinks = Vec::new();
        for (path, kind, id) in set.entries() {
            if *kind == Kind::Gitlink {
                gitlinks.push(path.clone());
            } else if !is_asc_config(path)
                && !ignore.is_ignored(path)
                && let Some((eco, claim)) = claim_of(detectors, &config, path)
            {
                todo.push((path.clone(), *id, eco, claim));
            }
        }
        let files: Vec<LinkInput> = futures::stream::iter(todo)
            .map(|(path, id, eco, claim)| self.parse_file(path, id, eco, claim))
            .buffered(PARSE_WINDOW)
            .try_collect()
            .await?;
        let mut exist = Vec::new();
        for e in &config.projects {
            if let ProjectTarget::Declared { root: r, .. } = &e.target
                && self.lookup(root, r).await?.is_some()
            {
                exist.push(r.clone());
            }
        }
        let cfg_path = RepoPath::new(".asc/projects.toml").expect("static path");
        link_graph(detectors, &files, &gitlinks, config, &|p| exist.contains(p))
            .map_err(|e| config_err(&cfg_path, e))
    }

    /// One claimed file's parse result, from the `parsed.v1` memo when its content was seen.
    async fn parse_file(
        &self,
        path: RepoPath,
        id: [u8; 32],
        eco: Ecosystem,
        claim: Claim,
    ) -> Result<LinkInput> {
        let file = FileId::from_bytes(id);
        let key = memo(PARSED, parsed_key(eco, claim_name(claim), &file));
        if let Some(b) = read_memo(&*self.objects, &key).await?
            && let Ok(p) = Parsed::from_envelope(&b)
        {
            return Ok(LinkInput {
                path,
                id,
                parsed: Ok(p),
            });
        }
        let limit = if claim == Claim::Lockfile {
            MAX_LOCKFILE_BYTES
        } else {
            MAX_MANIFEST_BYTES
        };
        let parsed = match self.read_capped(file, limit).await? {
            Blob::TooLarge => Err(ParseFailure::new("file exceeds the size limit")),
            Blob::Bytes(bytes) => {
                let detector = self
                    .projects
                    .detectors
                    .iter()
                    .find(|d| d.ecosystem() == eco);
                let result = detector.map_or_else(
                    || Err(ParseFailure::new("no detector")),
                    |d| d.parse(claim, &bytes),
                );
                match result {
                    Ok(p) => match p.validate() {
                        Ok(()) => {
                            write_memo(&*self.objects, &key, &p.to_envelope()).await?;
                            Ok(p)
                        }
                        Err(e) => Err(ParseFailure::new(e.to_string())),
                    },
                    Err(e) => Err(e),
                }
            }
        };
        Ok(LinkInput { path, id, parsed })
    }
}

fn root_path() -> RepoPath {
    RepoPath::new(".asc").expect("static path")
}
