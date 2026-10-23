//! Scope algebra against a full-walk oracle (SPEC §23.11, I05).

use std::collections::{BTreeSet, HashSet};
use std::io::Cursor;
use std::ops::Range;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use async_trait::async_trait;
use bytes::Bytes;
use asc_codeintel::config::CodeintelConfig;
use asc_codeintel::doc::{DocError, OrdTable};
use asc_codeintel::scope::{OrdSource, ScopeEngine, ScopeQuery};
use asc_core::content::{Body, ingest};
use asc_core::id::{ChangesetId, FileId, TreeId};
use asc_core::object::{Change, Kind};
use asc_core::path::{RefName, RepoPath};
use asc_core::sig::Signature;
use asc_engine::Engine;
use asc_engine::commit::{NewChangeset, PathPolicy};
use asc_store::bucket::BucketObjects;
use asc_store::codec::encode_stored;
use asc_store::key::{KeyPrefix, ObjKey};
use asc_store::objects::{Objects, Result as OResult};
use asc_store::refs::{RefEntry, RefError, RefLogEntry, RefTxn, Refs, RepoKey};
use futures::stream::BoxStream;
use roaring::RoaringBitmap;

struct NoRefs;

#[async_trait]
impl Refs for NoRefs {
    async fn get(&self, _: RepoKey, _: &RefName) -> asc_store::refs::Result<Option<RefEntry>> {
        Ok(None)
    }
    async fn list(&self, _: RepoKey, _: &str) -> asc_store::refs::Result<Vec<RefEntry>> {
        Ok(Vec::new())
    }
    async fn update(&self, _: RepoKey, _: &RefTxn) -> asc_store::refs::Result<()> {
        Err(RefError::Corrupt("unused".into()))
    }
    async fn log(
        &self,
        _: RepoKey,
        _: &RefName,
        _: u32,
    ) -> asc_store::refs::Result<Vec<RefLogEntry>> {
        Ok(Vec::new())
    }
}

/// Counts reads of tree objects.
struct Counting {
    inner: BucketObjects,
    trees: AtomicUsize,
}

#[async_trait]
impl Objects for Counting {
    async fn get(&self, key: &ObjKey) -> OResult<Option<Bytes>> {
        if matches!(key, ObjKey::Tree(_)) {
            self.trees.fetch_add(1, Ordering::SeqCst);
        }
        self.inner.get(key).await
    }
    async fn get_stored(&self, key: &ObjKey) -> OResult<Option<Bytes>> {
        if matches!(key, ObjKey::Tree(_)) {
            self.trees.fetch_add(1, Ordering::SeqCst);
        }
        self.inner.get_stored(key).await
    }
    async fn get_range(&self, key: &ObjKey, r: Range<u64>) -> OResult<Option<Bytes>> {
        self.inner.get_range(key, r).await
    }
    async fn put(&self, key: &ObjKey, stored: Bytes) -> OResult<()> {
        self.inner.put(key, stored).await
    }
    async fn has_many(&self, keys: &[ObjKey]) -> OResult<Vec<bool>> {
        self.inner.has_many(keys).await
    }
    async fn delete(&self, key: &ObjKey) -> OResult<()> {
        self.inner.delete(key).await
    }
    fn list(&self, prefix: KeyPrefix) -> BoxStream<'_, OResult<(ObjKey, SystemTime)>> {
        self.inner.list(prefix)
    }
}

/// Counts ord assignments: a tree derivation makes one per file, a delta one per change.
struct CountOrds {
    inner: Arc<Mutex<OrdTable>>,
    calls: AtomicUsize,
}

impl OrdSource for CountOrds {
    fn ord_for(&self, id: FileId) -> Result<u32, DocError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.inner.ord_for(id)
    }
    fn get(&self, id: &FileId) -> Option<u32> {
        self.inner.get(id)
    }
}

type Eng = Engine<Counting, NoRefs>;

struct Env {
    e: Arc<Eng>,
    objs: Arc<Counting>,
    ords: Arc<Mutex<OrdTable>>,
    counted: Arc<CountOrds>,
    scope: ScopeEngine<Counting, NoRefs, CountOrds>,
}

impl Env {
    fn new() -> Env {
        let objs = Arc::new(Counting {
            inner: BucketObjects::memory(),
            trees: AtomicUsize::new(0),
        });
        let e = Arc::new(Engine::new(objs.clone(), Arc::new(NoRefs), RepoKey(1)));
        let ords = Arc::new(Mutex::new(OrdTable::default()));
        let counted = Arc::new(CountOrds {
            inner: ords.clone(),
            calls: AtomicUsize::new(0),
        });
        let scope = ScopeEngine::new(e.clone(), objs.clone(), counted.clone());
        Env {
            e,
            objs,
            ords,
            counted,
            scope,
        }
    }

    fn tree_reads(&self) -> usize {
        self.objs.trees.load(Ordering::SeqCst)
    }

    async fn write(&self, content: &[u8]) -> Change {
        let got = ingest(Cursor::new(content)).unwrap();
        let Body::Inline(b) = got.body else {
            unreachable!("test files are small")
        };
        self.objs
            .put(&ObjKey::File(got.file), encode_stored(&b))
            .await
            .unwrap();
        Change::write(Kind::File, got.file, got.size, got.git, None).unwrap()
    }

    /// A changeset on `parent` applying `edits` (`None` deletes).
    async fn commit(
        &self,
        parent: Option<ChangesetId>,
        edits: &[(&str, Option<&str>)],
    ) -> ChangesetId {
        let root = match parent {
            Some(p) => self.e.get_changeset(p).await.unwrap().root,
            None => Eng::empty_tree_id(),
        };
        let mut changes = Vec::new();
        for (p, c) in edits {
            let ch = match c {
                Some(c) => self.write(c.as_bytes()).await,
                None => Change::Delete,
            };
            changes.push((RepoPath::new(*p).unwrap(), ch));
        }
        let root = self.e.apply(root, &changes).await.unwrap();
        let sig = Signature::new(b"A".to_vec(), b"a@x".to_vec(), 1_700_000_000, 0).unwrap();
        self.e
            .create_changeset(
                NewChangeset {
                    parents: parent.into_iter().collect(),
                    author: sig.clone(),
                    committer: sig,
                    message: b"m\n".to_vec(),
                    extra: Vec::new(),
                    root,
                    copies: Vec::new(),
                },
                PathPolicy::Native { windows_safe: true },
            )
            .await
            .unwrap()
    }

    /// Oracle: every File/Exec leaf of `root` by a full walk, as `(path, FileId)`.
    async fn walk(&self, root: TreeId) -> Vec<(String, FileId)> {
        let mut out = Vec::new();
        let mut stack = vec![(root, String::new())];
        while let Some((id, prefix)) = stack.pop() {
            let t = self.e.get_tree(id).await.unwrap();
            for en in t.entries() {
                let p = if prefix.is_empty() {
                    String::from_utf8_lossy(en.name.as_bytes()).into_owned()
                } else {
                    format!("{prefix}/{}", String::from_utf8_lossy(en.name.as_bytes()))
                };
                match en.kind {
                    Kind::File | Kind::Exec => out.push((p, FileId::from_bytes(en.id))),
                    Kind::Dir => stack.push((TreeId::from_bytes(en.id), p)),
                    _ => {}
                }
            }
        }
        out
    }

    fn ord_of(&self, f: FileId) -> u32 {
        self.ords.lock().unwrap().get(&f).expect("ord assigned")
    }

    async fn root_of(&self, cs: ChangesetId) -> TreeId {
        self.e.get_changeset(cs).await.unwrap().root
    }

    /// The full-walk oracle set of ords of `cs`.
    async fn oracle(&self, cs: ChangesetId) -> RoaringBitmap {
        let root = self.root_of(cs).await;
        let walked = self.walk(root).await;
        walked
            .into_iter()
            .map(|(_, f)| {
                self.ords
                    .lock()
                    .unwrap()
                    .ord_for(f)
                    .map(|(o, _)| o)
                    .unwrap()
            })
            .collect()
    }
}

fn p(s: &str) -> RepoPath {
    RepoPath::new(s).unwrap()
}

/// 320 files over 40 directories: large enough to store `tdocs`.
fn base_files() -> Vec<(String, String)> {
    let mut v = Vec::new();
    for d in 0..40 {
        for f in 0..8 {
            v.push((format!("d{d:02}/f{f}.txt"), format!("content {d} {f}")));
        }
    }
    v
}

async fn base_commit(env: &Env) -> ChangesetId {
    let files = base_files();
    let edits: Vec<(&str, Option<&str>)> = files
        .iter()
        .map(|(p, c)| (p.as_str(), Some(c.as_str())))
        .collect();
    env.commit(None, &edits).await
}

#[tokio::test]
async fn scope_equals_the_full_walk_through_a_history() {
    let env = Env::new();
    let c0 = base_commit(&env).await;
    let mut ids = vec![c0];
    let steps: Vec<Vec<(&str, Option<&str>)>> = vec![
        vec![
            ("d00/f0.txt", Some("changed")),
            ("new/a.txt", Some("fresh")),
        ],
        vec![("d01/f1.txt", None)],
        vec![("d02/f2.txt", Some("dup")), ("d03/f3.txt", Some("dup"))],
        vec![("d02/f2.txt", None)],
        vec![("d03/f3.txt", None)],
        vec![("x/y/z.txt", Some("deep")), ("d05/f5.txt", Some("changed"))],
        vec![("d00/f0.txt", Some("content 0 0"))],
    ];
    for s in steps {
        let prev = *ids.last().unwrap();
        ids.push(env.commit(Some(prev), &s).await);
    }
    // Derive and store the first revision; every later one is within 64 steps of it.
    let s0 = env.scope.scope(c0).await.unwrap();
    assert_eq!(*s0.bitmap(), env.oracle(c0).await);
    for &cs in &ids[1..] {
        let before = env.tree_reads();
        let ord_calls = env.counted.calls.load(Ordering::SeqCst);
        let got = env.scope.scope(cs).await.unwrap();
        let reads = env.tree_reads() - before;
        assert_eq!(*got.bitmap(), env.oracle(cs).await, "scope of {cs}");
        let assigned = env.counted.calls.load(Ordering::SeqCst) - ord_calls;
        assert!(assigned <= 6, "{assigned} ord lookups: the tree was walked");
        // The delta needs only lookups of the changed paths at the checkpoint.
        assert!(reads <= 12, "{reads} tree reads for a small delta");
        // Counts equal the oracle's too.
        let walked = env.walk(env.root_of(cs).await).await;
        for (_, f) in &walked {
            let o = env.ord_of(*f);
            let want = walked.iter().filter(|(_, g)| g == f).count() as u32;
            assert_eq!(got.count(o), want);
        }
    }
}

#[tokio::test]
async fn duplicate_path_deletion_keeps_the_file_until_both_are_gone() {
    let env = Env::new();
    let c0 = base_commit(&env).await;
    let c1 = env
        .commit(
            Some(c0),
            &[("a/one.txt", Some("same")), ("b/two.txt", Some("same"))],
        )
        .await;
    env.scope.scope(c1).await.unwrap(); // checkpoint, stored
    let same = {
        let r = env.root_of(c1).await;
        env.walk(r)
            .await
            .into_iter()
            .find(|(p, _)| p == "a/one.txt")
            .unwrap()
            .1
    };
    let ord = env.ord_of(same);
    assert_eq!(env.scope.scope(c1).await.unwrap().count(ord), 2);

    let c2 = env.commit(Some(c1), &[("a/one.txt", None)]).await;
    let s2 = env.scope.scope(c2).await.unwrap();
    assert!(s2.bitmap().contains(ord), "one path left");
    assert_eq!(s2.count(ord), 1);
    assert_eq!(*s2.bitmap(), env.oracle(c2).await);

    let c3 = env.commit(Some(c2), &[("b/two.txt", None)]).await;
    let s3 = env.scope.scope(c3).await.unwrap();
    assert!(!s3.bitmap().contains(ord), "both paths gone");
    assert_eq!(*s3.bitmap(), env.oracle(c3).await);

    // A revision that deletes both at once from the checkpoint.
    let c4 = env
        .commit(Some(c1), &[("a/one.txt", None), ("b/two.txt", None)])
        .await;
    assert!(!env.scope.scope(c4).await.unwrap().bitmap().contains(ord));
}

#[tokio::test]
async fn a_checkpoint_past_64_steps_falls_back_to_the_tree() {
    let env = Env::new();
    let c0 = base_commit(&env).await;
    env.scope.scope(c0).await.unwrap();
    let mut tip = c0;
    for i in 0..70 {
        tip = env
            .commit(Some(tip), &[("n/file.txt", Some(&format!("v{i}")))])
            .await;
    }
    let got = env.scope.scope(tip).await.unwrap();
    assert_eq!(*got.bitmap(), env.oracle(tip).await);
}

#[tokio::test]
async fn path_filters_include_and_exclude_with_counts() {
    let env = Env::new();
    let c0 = base_commit(&env).await;
    let c1 = env
        .commit(
            Some(c0),
            &[
                ("a/x.txt", Some("shared")),
                ("b/x.txt", Some("shared")),
                ("a/only.txt", Some("only")),
            ],
        )
        .await;
    let ord = |s: &str| {
        env.ords
            .lock()
            .unwrap()
            .ord_for(FileId::of(s.as_bytes()))
            .unwrap()
            .0
    };
    let shared = ord("shared");
    let only = ord("only");

    let mut q = ScopeQuery::new(c1);
    q.include = vec![p("a")];
    let r = env.scope.resolve(&q).await.unwrap();
    assert_eq!(r, [shared, only].into_iter().collect::<RoaringBitmap>());

    // Excluding a/ from the whole scope keeps `shared`, which also lives at b/.
    let mut q = ScopeQuery::new(c1);
    q.exclude = vec![p("a")];
    let r = env.scope.resolve(&q).await.unwrap();
    assert!(r.contains(shared) && !r.contains(only));

    // Include a and b, exclude b: `shared` stays through a/x.txt.
    let mut q = ScopeQuery::new(c1);
    q.include = vec![p("a"), p("b")];
    q.exclude = vec![p("b")];
    let r = env.scope.resolve(&q).await.unwrap();
    assert_eq!(r, [shared, only].into_iter().collect::<RoaringBitmap>());

    // A file path and a missing path.
    let mut q = ScopeQuery::new(c1);
    q.include = vec![p("a/only.txt"), p("nope/none")];
    let r = env.scope.resolve(&q).await.unwrap();
    assert_eq!(r, [only].into_iter().collect::<RoaringBitmap>());

    // No filter equals the scope.
    let r = env.scope.resolve(&ScopeQuery::new(c1)).await.unwrap();
    assert_eq!(r, env.oracle(c1).await);
}

#[tokio::test]
async fn xdocs_follow_attributes_and_config_per_path() {
    let env = Env::new();
    let c = env
        .commit(
            None,
            &[
                (
                    ".gitattributes",
                    Some("gen/** linguist-generated\n*.bin -text\n"),
                ),
                // .ascattributes wins over .gitattributes in the same directory, so v/ is
                // vendored here and its .gitattributes (marking nothing) is never merged.
                ("v/.ascattributes", Some("* linguist-vendored\n")),
                ("v/.gitattributes", Some("* -linguist-vendored\n")),
                ("v/lib.txt", Some("vendored body")),
                // The same bytes at an excluded and a kept path: in scope.
                ("gen/out.txt", Some("both places")),
                ("src/real.txt", Some("both places")),
                ("gen/only.txt", Some("generated only")),
                ("src/blob.bin", Some("binary by -text")),
                ("src/a.txt", Some("plain")),
                ("docs/skip.md", Some("config excluded")),
                // gitattributes only here: binary macro.
                ("m/.gitattributes", Some("*.dat binary\n")),
                ("m/x.dat", Some("macro binary")),
                ("m/y.txt", Some("kept y")),
            ],
        )
        .await;
    let cfg = CodeintelConfig::parse(b"exclude = [\"docs/**\"]").unwrap();
    let root = env.root_of(c).await;
    let x = env.scope.xdocs(root, &cfg).await.unwrap();
    let ord = |s: &str| env.ord_of(FileId::of(s.as_bytes()));
    assert!(x.contains(ord("vendored body")));
    assert!(x.contains(ord("generated only")));
    assert!(x.contains(ord("binary by -text")));
    assert!(x.contains(ord("config excluded")));
    assert!(x.contains(ord("macro binary")));
    assert!(!x.contains(ord("both places")), "kept at src/real.txt");
    assert!(!x.contains(ord("plain")));
    assert!(!x.contains(ord("kept y")));
    // Reading memos gives the same answer.
    assert_eq!(*env.scope.xdocs(root, &cfg).await.unwrap(), *x);

    // Queries subtract xdocs unless include_excluded.
    let mut q = ScopeQuery::new(c);
    q.rules = Some(&cfg);
    let kept = env.scope.resolve(&q).await.unwrap();
    assert!(kept.contains(ord("both places")) && !kept.contains(ord("vendored body")));
    let all = env.scope.resolve(&ScopeQuery::new(c)).await.unwrap();
    assert!(all.contains(ord("vendored body")));
}

#[tokio::test]
async fn a_changed_attribute_file_changes_xdocs_without_stale_memos() {
    let env = Env::new();
    let mut edits: Vec<(String, String)> = base_files();
    edits.push((
        "d00/.gitattributes".into(),
        "*.txt linguist-generated\n".into(),
    ));
    let refs: Vec<(&str, Option<&str>)> = edits
        .iter()
        .map(|(p, c)| (p.as_str(), Some(c.as_str())))
        .collect();
    let c0 = env.commit(None, &refs).await;
    let cfg = CodeintelConfig::default();
    let r0 = env.root_of(c0).await;
    let x0 = env.scope.xdocs(r0, &cfg).await.unwrap();
    assert_eq!(x0.len(), 8, "the eight files of d00");
    let c1 = env
        .commit(
            Some(c0),
            &[("d00/.gitattributes", Some("*.md linguist-generated\n"))],
        )
        .await;
    let r1 = env.root_of(c1).await;
    assert!(env.scope.xdocs(r1, &cfg).await.unwrap().is_empty());
    // An attribute file at the root changes what the subdirectories inherit.
    let c2 = env
        .commit(
            Some(c1),
            &[(".gitattributes", Some("d01/** linguist-vendored\n"))],
        )
        .await;
    let x2 = env.scope.xdocs(env.root_of(c2).await, &cfg).await.unwrap();
    assert_eq!(x2.len(), 8);
}

#[tokio::test]
async fn stack_diff_keeps_only_the_stack_paths() {
    let env = Env::new();
    let trunk = base_commit(&env).await;
    let s1 = env
        .commit(Some(trunk), &[("d00/f0.txt", Some("stack one"))])
        .await;
    let s2 = env
        .commit(
            Some(s1),
            &[("new/b.txt", Some("stack two")), ("d01/f1.txt", None)],
        )
        .await;
    // Trunk moves on independently.
    let trunk2 = env
        .commit(Some(trunk), &[("d02/f2.txt", Some("trunk edit"))])
        .await;
    let mut q = ScopeQuery::new(s2);
    q.stack_diff_trunk = Some(trunk2);
    let kept = env.scope.resolve(&q).await.unwrap();
    let ord = |s: &str| env.ord_of(FileId::of(s.as_bytes()));
    assert_eq!(
        kept,
        [ord("stack one"), ord("stack two")]
            .into_iter()
            .collect::<RoaringBitmap>()
    );
}

#[tokio::test]
async fn locate_finds_every_path_of_an_ord() {
    let env = Env::new();
    let c0 = base_commit(&env).await;
    let c1 = env
        .commit(
            Some(c0),
            &[
                ("a/one.txt", Some("same")),
                ("b/c/two.txt", Some("same")),
                ("top.txt", Some("same")),
            ],
        )
        .await;
    env.scope.scope(c1).await.unwrap();
    let root = env.root_of(c1).await;
    let ord = env.ord_of(FileId::of(b"same"));
    let got = env.scope.locate(root, ord, 10).await.unwrap();
    let want: BTreeSet<_> = ["a/one.txt", "b/c/two.txt", "top.txt"]
        .map(p)
        .into_iter()
        .collect();
    assert_eq!(got.iter().cloned().collect::<BTreeSet<_>>(), want);
    assert_eq!(env.scope.locate(root, ord, 2).await.unwrap().len(), 2);
    let none = env.scope.locate(root, 9_999_999, 5).await.unwrap();
    assert!(none.is_empty());
    // Every walked path is located.
    let mut seen = HashSet::new();
    for (path, f) in env.walk(root).await.into_iter().take(40) {
        let o = env.ord_of(f);
        assert!(
            env.scope
                .locate(root, o, 100)
                .await
                .unwrap()
                .contains(&p(&path))
        );
        seen.insert(o);
    }
}
