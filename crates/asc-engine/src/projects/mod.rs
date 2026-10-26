//! Project graph, affected and input hashes over the engine (SPEC §4.3, §12A.5 to §12A.7).
//!
//! Derived data is incremental and content keyed: `manifests.v1` follows the parent's
//! manifest set, `parsed.v1` is shared by every set holding the same file, and only manifest
//! and lockfile bytes are ever read.

use std::fmt;
use std::sync::Arc;

use asc_core::id::{ChangesetId, FileId};
use asc_core::path::RepoPath;
use asc_graph::affected::{AffectedError, AffectedProject, ProjectRef};
use asc_graph::config::ResolveError;
use asc_graph::detect::Detector;
use asc_graph::model::{Ecosystem, InputHashes, Issue, ManifestSet, ProjectGraph, SetId};
use asc_graph::pattern::PatternError;
use asc_graph::select::SelectError;
use asc_store::objects::{self, Objects};
use asc_store::refs::Refs;
use async_trait::async_trait;

use crate::{Engine, commit, graph, tree};

mod affected;
mod derive;
mod hash;

pub use derive::Manifests;

/// Graphs kept in memory per engine.
const GRAPH_CACHE_ITEMS: usize = 128;

/// Manifest sets kept in memory per engine.
const SET_CACHE_ITEMS: usize = 1024;

/// Why a project query failed.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Derive(#[from] crate::derive::Error),
    #[error(transparent)]
    Tree(#[from] tree::Error),
    #[error(transparent)]
    Commit(#[from] commit::Error),
    #[error(transparent)]
    Store(#[from] objects::Error),
    #[error(transparent)]
    History(#[from] graph::Error),
    /// A bad `.asc/projects.toml` or an invalid link (`PROJECTS_CONFIG`, SPEC §12A.8).
    #[error("{path}: {message}")]
    Config { path: RepoPath, message: String },
    #[error(transparent)]
    Select(#[from] SelectError),
    #[error(transparent)]
    Pattern(#[from] PatternError),
    #[error(transparent)]
    Resolve(#[from] ResolveError),
    #[error(transparent)]
    Affected(#[from] AffectedError),
    #[error("file {0} is missing from the object store")]
    MissingFile(FileId),
    #[error("{head} and {base} have no common ancestor")]
    NoMergeBase {
        base: ChangesetId,
        head: ChangesetId,
    },
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Identity of a project graph: the id of the manifest set it was linked from.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct GraphId([u8; 32]);

impl GraphId {
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl From<SetId> for GraphId {
    fn from(s: SetId) -> Self {
        Self(*s.as_bytes())
    }
}

impl fmt::Debug for GraphId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.iter().try_for_each(|b| write!(f, "{b:02x}"))
    }
}

/// The project graph at one changeset.
#[derive(Clone, Debug)]
pub struct GraphView {
    pub changeset: ChangesetId,
    pub id: GraphId,
    pub graph: Arc<ProjectGraph>,
}

/// A graph with the manifest set it came from.
#[derive(Debug)]
pub(crate) struct Loaded {
    pub id: SetId,
    pub set: Arc<ManifestSet>,
    pub graph: Arc<ProjectGraph>,
}

/// A request for the affected projects (SPEC §12A.7).
#[derive(Clone, Debug)]
pub struct AffectedQuery {
    pub base: ChangesetId,
    pub head: ChangesetId,
    /// Compare against `base` itself, not its merge base with `head`.
    pub no_merge_base: bool,
    pub target: Option<String>,
    /// `--type`: report only these ecosystems (empty = all).
    pub ecosystems: Vec<Ecosystem>,
    /// `--select` selectors in their text form (empty = all).
    pub select: Vec<String>,
    /// Fill in each project's input hash.
    pub with_hashes: bool,
}

/// The answer to an [`AffectedQuery`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AffectedResult {
    pub base: ChangesetId,
    pub head: ChangesetId,
    pub merge_base: ChangesetId,
    pub all: bool,
    pub projects: Vec<AffectedProject>,
    pub removed: Vec<ProjectRef>,
    pub unowned: Vec<RepoPath>,
    pub issues: Vec<Issue>,
    pub changed_paths: usize,
}

/// Project queries (SPEC §4.3). `Engine` implements it; `asc-rpc` implements it over the wire.
#[async_trait]
pub trait ProjectsApi: Send + Sync {
    async fn project_graph(&self, cs: ChangesetId) -> Result<GraphView>;
    async fn affected(&self, q: &AffectedQuery) -> Result<AffectedResult>;
    /// Input hash of every project for `target` (SPEC §12A.6); an empty target is the
    /// `default` named input.
    async fn project_hashes(&self, cs: ChangesetId, target: &str) -> Result<InputHashes>;
}

#[async_trait]
impl<O: Objects, R: Refs> ProjectsApi for Engine<O, R> {
    async fn project_graph(&self, cs: ChangesetId) -> Result<GraphView> {
        Engine::project_graph(self, cs).await
    }

    async fn affected(&self, q: &AffectedQuery) -> Result<AffectedResult> {
        Engine::affected(self, q).await
    }

    async fn project_hashes(&self, cs: ChangesetId, target: &str) -> Result<InputHashes> {
        Engine::input_hashes(self, cs, target).await
    }
}

/// Per-engine project state: the detector list and the in-memory caches.
pub(crate) struct State {
    pub detectors: &'static [&'static dyn Detector],
    pub graphs: quick_cache::sync::Cache<SetId, Arc<Loaded>>,
    pub sets: quick_cache::sync::Cache<SetId, Arc<ManifestSet>>,
}

impl State {
    pub fn new() -> Self {
        Self {
            detectors: asc_graph::registry::DETECTORS,
            graphs: quick_cache::sync::Cache::new(GRAPH_CACHE_ITEMS),
            sets: quick_cache::sync::Cache::new(SET_CACHE_ITEMS),
        }
    }
}

impl<O: Objects, R: Refs> Engine<O, R> {
    /// Replaces the built-in detectors; memos written under another list are not compatible.
    #[must_use]
    pub fn with_detectors(mut self, detectors: &'static [&'static dyn Detector]) -> Self {
        self.projects.detectors = detectors;
        self
    }

    /// The project graph at `cs`, derived from the manifest set without a checkout.
    pub async fn project_graph(&self, cs: ChangesetId) -> Result<GraphView> {
        let l = self.loaded(cs).await?;
        Ok(GraphView {
            changeset: cs,
            id: l.id.into(),
            graph: l.graph.clone(),
        })
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet, HashSet};
    use std::io::Cursor;
    use std::ops::Range;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::SystemTime;

    use asc_core::content::{Body, ingest};
    use asc_core::id::GitOid;
    use asc_core::object::{Change, Kind};
    use asc_core::sig::Signature;
    use asc_graph::canon::Value;
    use asc_graph::detect::{Claim, LinkCx, ParseFailure};
    use asc_graph::model::{DepKind, Parsed, Project, ProjectKey, ProjectKind};
    use asc_store::bucket::BucketObjects;
    use asc_store::codec::encode_stored;
    use asc_store::key::{KeyPrefix, ObjKey};
    use asc_store::refs::RepoKey;
    use bytes::Bytes;
    use futures::TryStreamExt;
    use futures::stream::BoxStream;

    use super::*;
    use crate::commit::{NewChangeset, PathPolicy};
    use crate::derive::testutil::NoRefs;

    /// Manifests are `pkg.fake` (`name=`, `dep=`, `#` comments); `pkg.global` is eco-global.
    struct Fake {
        parses: AtomicUsize,
    }

    impl Detector for Fake {
        fn ecosystem(&self) -> Ecosystem {
            Ecosystem::Node
        }

        fn claim(&self, path: &RepoPath) -> Option<Claim> {
            match path.file_name().as_bytes() {
                b"pkg.fake" => Some(Claim::Manifest),
                b"pkg.global" => Some(Claim::EcoGlobal),
                _ => None,
            }
        }

        fn parse(&self, claim: Claim, bytes: &[u8]) -> Result<Parsed, ParseFailure> {
            self.parses.fetch_add(1, Ordering::SeqCst);
            if claim != Claim::Manifest {
                return Ok(Parsed(Value::Null));
            }
            let text = std::str::from_utf8(bytes).map_err(|_| ParseFailure::new("not utf-8"))?;
            let (mut name, mut deps) = (String::new(), Vec::new());
            for line in text
                .lines()
                .filter(|l| !l.is_empty() && !l.starts_with('#'))
            {
                match line.split_once('=') {
                    Some(("name", v)) => name = v.to_owned(),
                    Some(("dep", v)) => deps.push(Value::Str(v.to_owned())),
                    _ => return Err(ParseFailure::new(format!("bad line {line:?}"))),
                }
            }
            Value::map([
                ("name".to_owned(), Value::Str(name)),
                ("deps".to_owned(), Value::Array(deps)),
            ])
            .map(Parsed)
            .map_err(|e| ParseFailure::new(e.to_string()))
        }

        fn link(&self, cx: &LinkCx<'_>) -> Vec<Project> {
            let field = |v: &Value, k: &str| match v {
                Value::Map(m) => m.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone()),
                _ => None,
            };
            let mut out = Vec::new();
            for f in cx.files_of(Claim::Manifest) {
                let Some(Value::Str(name)) = field(&f.parsed.0, "name") else {
                    continue;
                };
                let key = ProjectKey {
                    eco: Ecosystem::Node,
                    manifest: f.path.clone(),
                };
                let deps = match field(&f.parsed.0, "deps") {
                    Some(Value::Array(a)) => a,
                    _ => Vec::new(),
                };
                out.push((
                    Project::new(key, name.into_bytes(), ProjectKind::Package),
                    deps,
                ));
            }
            let by_name: BTreeMap<Vec<u8>, ProjectKey> = out
                .iter()
                .map(|(p, _)| (p.name.clone(), p.key.clone()))
                .collect();
            out.into_iter()
                .map(|(mut p, deps)| {
                    for d in deps {
                        if let Value::Str(d) = d
                            && let Some(k) = by_name.get(d.as_bytes())
                        {
                            p.deps.push((k.clone(), DepKind::Normal));
                        }
                    }
                    p
                })
                .collect()
        }
    }

    static COUNTED: Fake = Fake {
        parses: AtomicUsize::new(0),
    };
    static COUNTED_LIST: [&dyn Detector; 1] = [&COUNTED];
    static PLAIN: Fake = Fake {
        parses: AtomicUsize::new(0),
    };
    static PLAIN_LIST: [&dyn Detector; 1] = [&PLAIN];

    /// A store that records every `File` object read.
    struct Spy {
        inner: BucketObjects,
        files: Mutex<HashSet<FileId>>,
    }

    impl Spy {
        fn note(&self, key: &ObjKey) {
            if let ObjKey::File(f) = key {
                self.files.lock().unwrap().insert(*f);
            }
        }
    }

    #[async_trait]
    impl Objects for Spy {
        async fn get(&self, key: &ObjKey) -> objects::Result<Option<Bytes>> {
            self.note(key);
            self.inner.get(key).await
        }
        async fn get_stored(&self, key: &ObjKey) -> objects::Result<Option<Bytes>> {
            self.note(key);
            self.inner.get_stored(key).await
        }
        async fn get_range(&self, key: &ObjKey, r: Range<u64>) -> objects::Result<Option<Bytes>> {
            self.note(key);
            self.inner.get_range(key, r).await
        }
        async fn put(&self, key: &ObjKey, stored: Bytes) -> objects::Result<()> {
            self.inner.put(key, stored).await
        }
        async fn has_many(&self, keys: &[ObjKey]) -> objects::Result<Vec<bool>> {
            self.inner.has_many(keys).await
        }
        async fn delete(&self, key: &ObjKey) -> objects::Result<()> {
            self.inner.delete(key).await
        }
        fn list(&self, prefix: KeyPrefix) -> BoxStream<'_, objects::Result<(ObjKey, SystemTime)>> {
            self.inner.list(prefix)
        }
    }

    type Eng = Engine<BucketObjects, NoRefs>;

    fn engine(list: &'static [&'static dyn Detector]) -> Eng {
        Engine::new(
            Arc::new(BucketObjects::memory()),
            Arc::new(NoRefs),
            RepoKey(1),
        )
        .with_detectors(list)
    }

    fn rp(s: &str) -> RepoPath {
        RepoPath::new(s).unwrap()
    }

    async fn put<O: Objects, R: Refs>(e: &Engine<O, R>, content: &[u8]) -> Change {
        let got = ingest(Cursor::new(content)).unwrap();
        let Body::Inline(b) = got.body else {
            unreachable!("test files are small")
        };
        e.objects
            .put(&ObjKey::File(got.file), encode_stored(&b))
            .await
            .unwrap();
        Change::write(Kind::File, got.file, got.size, got.git, None).unwrap()
    }

    /// A child of `parent` with `edits` applied (`None` deletes); `n` keeps ids distinct.
    async fn commit<O: Objects, R: Refs>(
        e: &Engine<O, R>,
        parent: Option<ChangesetId>,
        edits: &[(&str, Option<&str>)],
        n: usize,
    ) -> ChangesetId {
        let base = match parent {
            Some(p) => e.get_changeset(p).await.unwrap().root,
            None => Engine::<O, R>::empty_tree_id(),
        };
        let mut changes = Vec::new();
        for (path, content) in edits {
            let change = match content {
                Some(c) => put(e, c.as_bytes()).await,
                None => Change::Delete,
            };
            changes.push((rp(path), change));
        }
        let root = e.apply(base, &changes).await.unwrap();
        let sig = Signature::new(b"A".to_vec(), b"a@x".to_vec(), 1_700_000_000, 0).unwrap();
        e.create_changeset(
            NewChangeset {
                parents: parent.into_iter().collect(),
                author: sig.clone(),
                committer: sig,
                message: format!("c{n}\n").into_bytes(),
                extra: Vec::new(),
                root,
                copies: Vec::new(),
            },
            PathPolicy::Native { windows_safe: true },
        )
        .await
        .unwrap()
    }

    fn query(base: ChangesetId, head: ChangesetId) -> AffectedQuery {
        AffectedQuery {
            base,
            head,
            no_merge_base: true,
            target: None,
            ecosystems: Vec::new(),
            select: Vec::new(),
            with_hashes: false,
        }
    }

    fn ids(r: &AffectedResult) -> Vec<String> {
        r.projects.iter().map(|p| p.project.id.clone()).collect()
    }

    const BASE: &[(&str, Option<&str>)] = &[
        ("a/pkg.fake", Some("name=a\n")),
        ("a/src.txt", Some("a1")),
        ("b/pkg.fake", Some("name=b\ndep=a\n")),
        ("b/src.txt", Some("b1")),
        ("b/README.md", Some("r1")),
        ("c/pkg.fake", Some("name=c\n")),
        ("c/src.txt", Some("c1")),
    ];

    #[tokio::test]
    async fn long_history_parses_each_manifest_content_once() {
        let e = engine(&COUNTED_LIST);
        let mut ids = vec![commit(&e, None, BASE, 0).await];
        let mut distinct = 3;
        for i in 1..2000usize {
            let body = format!("name=a\n# rev {i}\n");
            let log = format!("log/x{}.txt", i % 64);
            let v = format!("v{i}");
            let touches = i % 20 == 0;
            let edits: Vec<(&str, Option<&str>)> = if touches {
                distinct += 1;
                vec![("a/pkg.fake", Some(&body))]
            } else {
                vec![(&log, Some(&v))]
            };
            let parent = *ids.last().unwrap();
            ids.push(commit(&e, Some(parent), &edits, i).await);
        }
        for &c in &ids {
            e.project_graph(c).await.unwrap();
        }
        assert_eq!(COUNTED.parses.load(Ordering::SeqCst), distinct);
        // A changeset without a manifest change reuses its parent's set and parses nothing.
        for (i, w) in ids.windows(2).enumerate() {
            if (i + 1) % 20 != 0 {
                let (p, c) = (
                    e.get_derived::<super::Manifests>(w[0]).await.unwrap(),
                    e.get_derived::<super::Manifests>(w[1]).await.unwrap(),
                );
                assert_eq!(p, c);
            }
        }
    }

    #[tokio::test]
    async fn only_manifest_bytes_are_read() {
        let spy = Arc::new(Spy {
            inner: BucketObjects::memory(),
            files: Mutex::new(HashSet::new()),
        });
        let e = Engine::new(spy.clone(), Arc::new(NoRefs), RepoKey(1)).with_detectors(&PLAIN_LIST);
        let mut files: Vec<(String, Option<String>)> = (0..40)
            .map(|i| (format!("a/deep/f{i}.txt"), Some(format!("body {i}"))))
            .collect();
        files.extend(
            BASE.iter()
                .map(|(p, c)| ((*p).to_owned(), c.map(str::to_owned))),
        );
        let edits: Vec<(&str, Option<&str>)> = files
            .iter()
            .map(|(p, c)| (p.as_str(), c.as_deref()))
            .collect();
        let c1 = commit(&e, None, &edits, 0).await;
        let c2 = commit(&e, Some(c1), &[("a/deep/f1.txt", Some("changed"))], 1).await;
        spy.files.lock().unwrap().clear();
        e.project_graph(c2).await.unwrap();
        e.input_hashes(c2, "build").await.unwrap();
        e.affected(&query(c1, c2)).await.unwrap();
        let manifests: HashSet<FileId> = BASE
            .iter()
            .filter(|(p, _)| p.ends_with("pkg.fake"))
            .map(|(_, c)| FileId::of(c.unwrap().as_bytes()))
            .collect();
        let read = spy.files.lock().unwrap().clone();
        assert!(!read.is_empty());
        assert!(read.is_subset(&manifests), "read non-manifest files");
    }

    #[tokio::test]
    async fn editing_a_file_changes_its_owner_and_dependents_only() {
        let e = engine(&PLAIN_LIST);
        let toml = "[targets.lint]\ninputs = [\"default\"]\n";
        let mut files = BASE.to_vec();
        files.push((".asc/projects.toml", Some(toml)));
        let c1 = commit(&e, None, &files, 0).await;
        let c2 = commit(&e, Some(c1), &[("a/src.txt", Some("a2"))], 1).await;
        let by_id = |h: &InputHashes| -> BTreeMap<String, [u8; 32]> {
            h.0.iter()
                .map(|(k, v)| (k.manifest.to_string(), *v))
                .collect()
        };
        let changed = |before: BTreeMap<String, [u8; 32]>, after: BTreeMap<String, [u8; 32]>| {
            after
                .iter()
                .filter(|(k, v)| before[*k] != **v)
                .map(|(k, _)| k.clone())
                .collect::<Vec<_>>()
        };
        for (target, expect) in [
            ("build", vec!["a/pkg.fake", "b/pkg.fake"]),
            ("lint", vec!["a/pkg.fake"]),
            ("", vec!["a/pkg.fake"]),
        ] {
            let h1 = by_id(&e.input_hashes(c1, target).await.unwrap());
            let h2 = by_id(&e.input_hashes(c2, target).await.unwrap());
            assert_eq!(changed(h1, h2), expect, "target {target:?}");
        }
    }

    #[tokio::test]
    async fn excluded_files_leave_the_hash_alone() {
        let e = engine(&PLAIN_LIST);
        let toml = "[targets.build]\ninputs = [\"default\", \"^default\", \"!**/*.md\"]\n";
        let mut files = BASE.to_vec();
        files.push((".asc/projects.toml", Some(toml)));
        let c1 = commit(&e, None, &files, 0).await;
        let c2 = commit(&e, Some(c1), &[("b/README.md", Some("r2"))], 1).await;
        let c3 = commit(&e, Some(c2), &[("b/src.txt", Some("b2"))], 2).await;
        let h = |c| e.input_hashes(c, "build");
        assert_eq!(h(c1).await.unwrap(), h(c2).await.unwrap());
        assert_ne!(h(c2).await.unwrap(), h(c3).await.unwrap());
    }

    #[tokio::test]
    async fn config_change_affects_everything() {
        let e = engine(&PLAIN_LIST);
        let c1 = commit(&e, None, BASE, 0).await;
        let c2 = commit(
            &e,
            Some(c1),
            &[(".asc/projects.toml", Some("[global]\nunowned = \"all\"\n"))],
            1,
        )
        .await;
        let r = e.affected(&query(c1, c2)).await.unwrap();
        assert!(r.all);
        assert_eq!(ids(&r), ["node:a", "node:b", "node:c"]);
        let p = r.projects[0]
            .reasons
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>();
        assert_eq!(p, ["global:.asc/projects.toml"]);
    }

    #[tokio::test]
    async fn bad_config_is_a_config_error() {
        let e = engine(&PLAIN_LIST);
        let c = commit(
            &e,
            None,
            &[
                ("a/pkg.fake", Some("name=a\n")),
                (".asc/projects.toml", Some("nope = 1\n")),
            ],
            0,
        )
        .await;
        let err = e.project_graph(c).await.unwrap_err();
        assert!(matches!(err, Error::Config { .. }), "{err}");
    }

    #[tokio::test]
    async fn affected_follows_dependents_and_merge_base() {
        let e = engine(&PLAIN_LIST);
        let c1 = commit(&e, None, BASE, 0).await;
        let main = commit(&e, Some(c1), &[("c/src.txt", Some("c2"))], 1).await;
        let topic = commit(&e, Some(c1), &[("a/src.txt", Some("a2"))], 2).await;
        let mut q = query(main, topic);
        q.no_merge_base = false;
        q.with_hashes = true;
        let r = e.affected(&q).await.unwrap();
        assert_eq!(r.merge_base, c1);
        assert_eq!(ids(&r), ["node:a", "node:b"]);
        assert!(r.projects[0].direct && !r.projects[1].direct);
        assert!(r.projects.iter().all(|p| p.input_hash.is_some()));
        assert_eq!(r.changed_paths, 1);
    }

    #[tokio::test]
    async fn gitlinks_are_part_of_the_set() {
        let e = engine(&PLAIN_LIST);
        let c1 = commit(&e, None, BASE, 0).await;
        let link = Change::Gitlink {
            commit: GitOid::from_bytes([7; 20]),
        };
        let root = e.get_changeset(c1).await.unwrap().root;
        let root = e.apply(root, &[(rp("vendor/lib"), link)]).await.unwrap();
        let sig = Signature::new(b"A".to_vec(), b"a@x".to_vec(), 1_700_000_000, 0).unwrap();
        let c2 = e
            .create_changeset(
                NewChangeset {
                    parents: vec![c1],
                    author: sig.clone(),
                    committer: sig,
                    message: b"g\n".to_vec(),
                    extra: Vec::new(),
                    root,
                    copies: Vec::new(),
                },
                PathPolicy::Native { windows_safe: true },
            )
            .await
            .unwrap();
        let (_, with) = e.manifest_set(c2).await.unwrap();
        assert!(
            with.entries()
                .iter()
                .any(|x| x.1 == Kind::Gitlink && x.0 == rp("vendor/lib"))
        );
        let c3 = commit(&e, Some(c2), &[("vendor/lib", None)], 3).await;
        let (_, without) = e.manifest_set(c3).await.unwrap();
        assert!(without.entries().iter().all(|x| x.1 != Kind::Gitlink));
        assert_eq!(without, e.manifest_set(c1).await.unwrap().1);
    }

    // --- oracle -----------------------------------------------------------------------

    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
        fn below(&mut self, n: usize) -> usize {
            (self.next() % n as u64) as usize
        }
    }

    const DIRS: [&str; 5] = ["p0", "p1", "p2", "p1/n", "p2/m"];

    fn manifest(dir: &str, rng: &mut Rng) -> String {
        let mut s = format!("name={}\n", dir.replace('/', "_"));
        for d in DIRS {
            if d != dir && rng.below(3) == 0 {
                s += &format!("dep={}\n", d.replace('/', "_"));
            }
        }
        s
    }

    /// The projects of a tree the slow way: every file read, every manifest parsed here.
    struct Naive {
        /// manifest path -> (file id, name, dependency names)
        projects: BTreeMap<String, ([u8; 32], String, Vec<String>)>,
        files: BTreeMap<String, [u8; 32]>,
    }

    impl Naive {
        async fn load(e: &Eng, cs: ChangesetId) -> Self {
            let root = e.get_changeset(cs).await.unwrap().root;
            let mut walk = e.walk(root, None);
            let (mut projects, mut files) = (BTreeMap::new(), BTreeMap::new());
            while let Some((path, entry)) = walk.try_next().await.unwrap() {
                let p = path.to_string();
                if p.ends_with("pkg.fake") {
                    let bytes = e
                        .read_file(FileId::from_bytes(entry.id), entry.size)
                        .await
                        .unwrap();
                    let text = String::from_utf8(bytes.to_vec()).unwrap();
                    let mut name = String::new();
                    let mut deps = Vec::new();
                    for l in text.lines() {
                        match l.split_once('=') {
                            Some(("name", v)) => name = v.to_owned(),
                            Some(("dep", v)) => deps.push(v.to_owned()),
                            _ => {}
                        }
                    }
                    projects.insert(p.clone(), (entry.id, name, deps));
                }
                files.insert(p, entry.id);
            }
            Self { projects, files }
        }

        /// The deepest project whose directory holds `path`.
        fn owner(&self, path: &str) -> Option<String> {
            let mut dir = path.rsplit_once('/').map(|x| x.0.to_owned());
            while let Some(d) = dir {
                let m = format!("{d}/pkg.fake");
                if self.projects.contains_key(&m) {
                    return Some(m);
                }
                dir = d.rsplit_once('/').map(|x| x.0.to_owned());
            }
            None
        }

        fn deps_of(&self, manifest: &str) -> BTreeSet<String> {
            let (_, _, deps) = &self.projects[manifest];
            self.projects
                .iter()
                .filter(|(_, (_, n, _))| deps.contains(n))
                .map(|(m, _)| m.clone())
                .collect()
        }
    }

    fn oracle(base: &Naive, head: &Naive) -> BTreeSet<String> {
        let paths: BTreeSet<&String> = base.files.keys().chain(head.files.keys()).collect();
        let mut direct = BTreeSet::new();
        for (m, (id, _, _)) in &head.projects {
            match base.projects.get(m) {
                None => {
                    direct.insert(m.clone());
                }
                Some((bid, _, _)) => {
                    if bid != id || base.deps_of(m) != head.deps_of(m) {
                        direct.insert(m.clone());
                    }
                }
            }
        }
        for p in paths {
            if base.files.get(p) == head.files.get(p) || p.ends_with("pkg.fake") {
                continue;
            }
            let mut owners = vec![head.owner(p)];
            if !head.files.contains_key(p) {
                owners.push(base.owner(p));
            }
            for o in owners.into_iter().flatten() {
                if head.projects.contains_key(&o) {
                    direct.insert(o);
                }
            }
        }
        let mut seen = direct.clone();
        let mut queue: Vec<String> = direct.into_iter().collect();
        while let Some(q) = queue.pop() {
            for m in head.projects.keys() {
                if head.deps_of(m).contains(&q) && seen.insert(m.clone()) {
                    queue.push(m.clone());
                }
            }
        }
        seen
    }

    #[tokio::test]
    async fn matches_the_naive_oracle_on_random_pairs() {
        let e = engine(&PLAIN_LIST);
        let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
        let mut state: BTreeMap<String, String> = BTreeMap::new();
        let mut first: Vec<(String, Option<String>)> = Vec::new();
        for d in DIRS {
            first.push((format!("{d}/pkg.fake"), Some(manifest(d, &mut rng))));
            for f in 0..3 {
                first.push((format!("{d}/f{f}.txt"), Some(format!("{d} {f}"))));
            }
        }
        let as_edits =
            |v: &[(String, Option<String>)]| -> Vec<(String, Option<String>)> { v.to_vec() };
        let mut history = Vec::new();
        let mut step = as_edits(&first);
        let mut parent = None;
        for n in 0..16 {
            for (p, c) in &step {
                match c {
                    Some(c) => state.insert(p.clone(), c.clone()),
                    None => state.remove(p),
                };
            }
            let edits: Vec<(&str, Option<&str>)> = step
                .iter()
                .map(|(p, c)| (p.as_str(), c.as_deref()))
                .collect();
            let c = commit(&e, parent, &edits, n).await;
            history.push(c);
            parent = Some(c);
            // Next step: a few random edits against the current state.
            let mut next: BTreeMap<String, Option<String>> = BTreeMap::new();
            for _ in 0..1 + rng.below(3) {
                let d = DIRS[rng.below(DIRS.len())];
                let m = format!("{d}/pkg.fake");
                match rng.below(10) {
                    0..=4 => {
                        let f = format!("{d}/f{}.txt", rng.below(3));
                        next.insert(f, Some(format!("v{}", rng.next())));
                    }
                    5 | 6 => {
                        let v = if state.contains_key(&m) {
                            None
                        } else {
                            Some(manifest(d, &mut rng))
                        };
                        next.insert(m, v);
                    }
                    7 | 8 if state.contains_key(&m) => {
                        next.insert(m, Some(manifest(d, &mut rng)));
                    }
                    _ => {
                        let victim = state
                            .keys()
                            .filter(|k| !k.ends_with("pkg.fake"))
                            .nth(rng.below(10));
                        if let Some(v) = victim {
                            next.insert(v.clone(), None);
                        }
                    }
                }
            }
            if next.is_empty() {
                next.insert("p0/f0.txt".to_owned(), Some(format!("v{}", rng.next())));
            }
            step = next.into_iter().collect();
        }
        let mut naive = Vec::new();
        for &c in &history {
            naive.push(Naive::load(&e, c).await);
        }
        for _ in 0..500 {
            let (i, j) = (rng.below(history.len()), rng.below(history.len()));
            let got: BTreeSet<String> = e
                .affected(&query(history[i], history[j]))
                .await
                .unwrap()
                .projects
                .iter()
                .map(|p| p.project.key.manifest.to_string())
                .collect();
            assert_eq!(got, oracle(&naive[i], &naive[j]), "pair {i} -> {j}");
        }
    }
}
