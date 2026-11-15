//! Loose-object renderer (docs/design/GIT-VIEW.md, unit 1): objects rendered from asc's own
//! export functions are written under a temporary `objects/` directory and read back by real git
//! (`cat-file`, `fsck --strict`) and by gix, byte for byte against the git export of the same
//! history and against known git object ids.

use std::collections::BTreeMap;
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use asc_core::codec::Enveloped;
use asc_core::content::{Body, ingest};
use asc_core::git::{
    EXTRA_GIT_COMMIT, EXTRA_GIT_GPGSIG, ObjectKind, blob_oid, canonicalise_commit,
    canonicalise_tag, export_commit, export_tag, object_oid, parse_commit, parse_tag, render_tree,
    tree_oid,
};
use asc_core::id::{ChangesetId, GitOid, TagId, TreeId};
use asc_core::object::{Change, Entry, Kind, SortedMap, TagRecord, Tree};
use asc_core::path::{Name, RefName, RepoPath};
use asc_core::sig::Signature;
use asc_engine::Engine;
use asc_engine::commit::{NewChangeset, PathPolicy};
use asc_git::export::{ExportRoot, Exporter};
use asc_git::odb::Odb;
use asc_git::realgit::{RealGit, pinned_command};
use asc_git::tree_override;
use asc_git::view::object::{LooseObject, render};
use asc_store::bucket::BucketObjects;
use asc_store::codec::encode_stored;
use asc_store::key::ObjKey;
use asc_store::objects::Objects;
use asc_store::refs::{RefEntry, RefError, RefLogEntry, RefTxn, Refs, RepoKey};
use async_trait::async_trait;

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

type Eng = Engine<BucketObjects, NoRefs>;

fn git(dir: &Path, args: &[&str]) -> Vec<u8> {
    let out = pinned_command()
        .args(args)
        .current_dir(dir)
        .output()
        .expect("spawn git");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    out.stdout
}

fn git_text(dir: &Path, args: &[&str]) -> String {
    String::from_utf8(git(dir, args))
        .expect("utf8")
        .trim()
        .to_owned()
}

fn bare_repo() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    git(dir.path(), &["init", "-q", "--bare", "."]);
    dir
}

/// The view's side of the experiment: a bare repository whose `objects/` holds only files the
/// renderer produced, written the way the namespace would serve them.
struct View {
    dir: tempfile::TempDir,
    written: BTreeMap<GitOid, ObjectKind>,
}

impl View {
    fn new() -> Self {
        Self {
            dir: bare_repo(),
            written: BTreeMap::new(),
        }
    }

    fn path(&self) -> &Path {
        self.dir.path()
    }

    fn loose_path(&self, oid: GitOid) -> PathBuf {
        let hex = oid.to_string();
        self.path().join("objects").join(&hex[..2]).join(&hex[2..])
    }

    /// Renders `body` as a loose object, checks that it carries the name git would give it, and
    /// stores it. Returns the oid.
    fn put(&mut self, kind: ObjectKind, body: &[u8]) -> GitOid {
        let oid = object_oid(kind, body);
        let file = render(kind, body).expect("render");
        assert_eq!(
            file.len() as u64,
            LooseObject::new(kind, body.len() as u64)
                .expect("layout")
                .size()
        );
        let path = self.loose_path(oid);
        std::fs::create_dir_all(path.parent().expect("fan-out directory")).expect("mkdir");
        std::fs::write(&path, file).expect("write loose object");
        self.written.insert(oid, kind);
        oid
    }

    fn fsck(&self) {
        git(
            self.path(),
            &["fsck", "--strict", "--no-dangling", "--full"],
        );
    }

    fn set_ref(&self, name: &str, oid: GitOid) {
        git(self.path(), &["update-ref", name, &oid.to_string()]);
    }

    /// Every object, through real git and through gix, equals `expected`.
    fn assert_reads_back(&self, expected: &[(ObjectKind, GitOid, Vec<u8>)]) {
        let odb = Odb::open(self.path()).expect("gix odb");
        for (kind, oid, body) in expected {
            let name = kind_name(*kind);
            assert_eq!(
                git_text(self.path(), &["cat-file", "-t", &oid.to_string()]),
                name
            );
            assert_eq!(
                git_text(self.path(), &["cat-file", "-s", &oid.to_string()]),
                body.len().to_string()
            );
            assert_eq!(
                git(self.path(), &["cat-file", name, &oid.to_string()]),
                *body,
                "{name} {oid}"
            );
            let (read_kind, read_body) = odb.read_object_verified(*oid).expect("gix read");
            assert_eq!(read_kind, *kind);
            assert_eq!(read_body, *body);
        }
    }
}

fn kind_name(kind: ObjectKind) -> &'static str {
    match kind {
        ObjectKind::Blob => "blob",
        ObjectKind::Tree => "tree",
        ObjectKind::Commit => "commit",
        ObjectKind::Tag => "tag",
    }
}

fn golden(name: &str) -> Vec<u8> {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../asc-core/tests/golden/git")
        .join(name);
    std::fs::read(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display()))
}

/// `(file, git oid)` rows of one kind from the golden `expected.txt`, whose ids come from real git.
fn golden_rows(kind: &str) -> Vec<(String, GitOid)> {
    String::from_utf8(golden("expected.txt"))
        .expect("utf8")
        .lines()
        .map(|l| l.split(' ').collect::<Vec<_>>())
        .filter(|f| f[0] == kind)
        .map(|f| (f[1].to_owned(), f[2].parse().expect("oid")))
        .collect()
}

/// Reads a raw git tree back into a `Tree` with placeholder asc ids.
fn tree_from_git(mut raw: &[u8]) -> Tree {
    let mut entries = Vec::new();
    while !raw.is_empty() {
        let sp = raw.iter().position(|&c| c == b' ').expect("mode");
        let nul = raw.iter().position(|&c| c == 0).expect("name");
        let kind = match &raw[..sp] {
            b"100644" => Kind::File,
            b"100755" => Kind::Exec,
            b"120000" => Kind::Symlink,
            b"40000" => Kind::Dir,
            b"160000" => Kind::Gitlink,
            m => panic!("mode {m:?}"),
        };
        entries.push(Entry {
            name: Name::new(&raw[sp + 1..nul]).expect("name"),
            kind,
            id: [0; 32],
            size: 0,
            git: GitOid::from_bytes(raw[nul + 1..nul + 21].try_into().expect("oid")),
        });
        raw = &raw[nul + 21..];
    }
    entries.sort_by(|a, b| a.name.cmp(&b.name));
    Tree::new(entries, true).expect("tree")
}

fn write_a() -> Vec<(RepoPath, Change)> {
    let change = Change::Write {
        kind: Kind::File,
        file: asc_core::id::FileId::of(b"x"),
        size: 1,
        git: blob_oid(b"x"),
        copy_from: None,
    };
    vec![(RepoPath::new("a").expect("path"), change)]
}

/// Known git ids: every golden object, rendered by the same asc functions an export uses, is
/// stored under the id real git computed for it, and real git and gix read it back.
#[test]
fn view_object_golden_objects_have_git_s_ids_and_bytes() {
    let mut view = View::new();
    let mut expected = Vec::new();
    for (file, oid) in golden_rows("blob") {
        let body = golden(&file);
        assert_eq!(view.put(ObjectKind::Blob, &body), oid, "{file}");
        expected.push((ObjectKind::Blob, oid, body));
    }
    for (file, oid) in golden_rows("tree") {
        let raw = golden(&file);
        let body = render_tree(&tree_from_git(&raw));
        assert_eq!(body, raw, "{file}");
        assert_eq!(view.put(ObjectKind::Tree, &body), oid, "{file}");
        expected.push((ObjectKind::Tree, oid, body));
    }
    let mut raw_commits = 0;
    for (file, oid) in golden_rows("commit") {
        let raw = golden(&file);
        let parsed = parse_commit(&raw).expect("parse");
        let parents = (1u8..).map(|n| ChangesetId::from_bytes([n; 32]));
        let cs = canonicalise_commit(
            &raw,
            TreeId::from_bytes([9; 32]),
            parents.take(parsed.parents.len()).collect(),
            write_a(),
        )
        .expect("canonicalise");
        raw_commits += usize::from(cs.extra.get(&EXTRA_GIT_COMMIT.to_vec()).is_some());
        let body = export_commit(&cs, parsed.tree, &parsed.parents).expect("export");
        assert_eq!(view.put(ObjectKind::Commit, &body), oid, "{file}");
        expected.push((ObjectKind::Commit, oid, body));
    }
    assert!(raw_commits > 0, "the golden set holds a raw git.commit");
    for (file, oid) in golden_rows("tag") {
        let raw = golden(&file);
        let parsed = parse_tag(&raw).expect("parse");
        let tag = canonicalise_tag(&raw, ChangesetId::from_bytes([1; 32])).expect("canonicalise");
        let body = export_tag(&tag, parsed.object).expect("export");
        assert_eq!(view.put(ObjectKind::Tag, &body), oid, "{file}");
        expected.push((ObjectKind::Tag, oid, body));
    }
    view.assert_reads_back(&expected);
    // Hash names are checked by git on `hash-object`, independently of the renderer.
    for (kind, oid, body) in &expected {
        let out = git_hash_object(view.path(), kind_name(*kind), body);
        assert_eq!(out, oid.to_string());
    }
}

fn git_hash_object(dir: &Path, kind: &str, body: &[u8]) -> String {
    use std::io::Write;
    use std::process::Stdio;
    let mut child = pinned_command()
        .args(["hash-object", "-t", kind, "--stdin", "--literally"])
        .current_dir(dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn git");
    let mut stdin = child.stdin.take().expect("stdin");
    let body = body.to_vec();
    let feeder = std::thread::spawn(move || stdin.write_all(&body));
    let out = child.wait_with_output().expect("hash-object");
    feeder.join().expect("feeder").expect("write");
    assert!(out.status.success(), "hash-object {kind}");
    String::from_utf8(out.stdout)
        .expect("utf8")
        .trim()
        .to_owned()
}

struct Fixture {
    objects: Arc<BucketObjects>,
    engine: Eng,
    git: RealGit,
    blobs: BTreeMap<GitOid, Vec<u8>>,
}

fn sig() -> Signature {
    Signature::new(b"A".to_vec(), b"a@x".to_vec(), 1_700_000_000, 0).expect("sig")
}

impl Fixture {
    fn new() -> Self {
        let objects = Arc::new(BucketObjects::memory());
        let engine = Engine::new(objects.clone(), Arc::new(NoRefs), RepoKey(1));
        Self {
            objects,
            engine,
            git: RealGit::resolve(None, None).expect("real git"),
            blobs: BTreeMap::new(),
        }
    }

    async fn file(&mut self, content: &[u8]) -> Change {
        let got = ingest(Cursor::new(content)).expect("ingest");
        let stored = match got.body {
            Body::Inline(b) => encode_stored(&b),
            other => panic!("test files are small: {other:?}"),
        };
        self.objects
            .put(&ObjKey::File(got.file), stored)
            .await
            .expect("put file");
        self.blobs.insert(got.git, content.to_vec());
        Change::write(Kind::File, got.file, got.size, got.git, None).expect("change")
    }

    async fn changeset(
        &self,
        parents: &[ChangesetId],
        root: TreeId,
        extra: Vec<(Vec<u8>, Vec<u8>)>,
        copies: Vec<(RepoPath, RepoPath)>,
    ) -> ChangesetId {
        self.engine
            .create_changeset(
                NewChangeset {
                    parents: parents.to_vec(),
                    author: sig(),
                    committer: sig(),
                    message: b"msg\n".to_vec(),
                    extra,
                    root,
                    copies,
                },
                PathPolicy::Native { windows_safe: true },
            )
            .await
            .expect("changeset")
    }

    async fn commit(
        &mut self,
        parents: &[ChangesetId],
        files: &[(&str, &[u8])],
        extra: Vec<(Vec<u8>, Vec<u8>)>,
        copies: Vec<(RepoPath, RepoPath)>,
    ) -> ChangesetId {
        let mut changes = Vec::new();
        for (path, content) in files {
            let change = self.file(content).await;
            changes.push((RepoPath::new(*path).expect("path"), change));
        }
        let base = match parents.first() {
            Some(&p) => self.engine.get_changeset(p).await.expect("parent").root,
            None => Eng::empty_tree_id(),
        };
        let root = self.engine.apply(base, &changes).await.expect("apply");
        self.changeset(parents, root, extra, copies).await
    }

    /// Renders every object reachable from `root` into `view`, the way the resolver will: trees
    /// through `render_tree` or the verified override, blobs from their bytes. Returns the git id
    /// of the tree.
    async fn render_tree_into(
        &self,
        view: &mut View,
        out: &mut Vec<(ObjectKind, GitOid, Vec<u8>)>,
        root: TreeId,
        recorded: Option<GitOid>,
    ) -> GitOid {
        let tree = self.engine.get_tree(root).await.expect("tree");
        let mut body = render_tree(&tree);
        if let Some(oid) = recorded
            && let Some(over) = self
                .objects
                .get(&ObjKey::GitTreeOverride(oid))
                .await
                .expect("override lookup")
        {
            tree_override::verify(&over, &tree).expect("override verifies");
            body = over.to_vec();
        }
        for entry in tree.entries() {
            match entry.kind {
                Kind::Dir => {
                    let got = Box::pin(self.render_tree_into(
                        view,
                        out,
                        TreeId::from_bytes(entry.id),
                        Some(entry.git),
                    ))
                    .await;
                    assert_eq!(got, entry.git, "{}", entry.name);
                }
                Kind::Gitlink => {}
                _ => {
                    let content = self.blobs.get(&entry.git).expect("blob content");
                    assert_eq!(content.len() as u64, entry.size);
                    let oid = view.put(ObjectKind::Blob, content);
                    assert_eq!(oid, entry.git);
                    out.push((ObjectKind::Blob, oid, content.clone()));
                }
            }
        }
        let oid = view.put(ObjectKind::Tree, &body);
        out.push((ObjectKind::Tree, oid, body));
        oid
    }

    async fn render_commit_into(
        &self,
        view: &mut View,
        out: &mut Vec<(ObjectKind, GitOid, Vec<u8>)>,
        id: ChangesetId,
    ) -> GitOid {
        let cs = self.engine.get_changeset(id).await.expect("cs");
        let mut parent_gits = Vec::new();
        for &p in &cs.parents {
            parent_gits.push(Box::pin(self.render_commit_into(view, out, p)).await);
        }
        let root = self.render_tree_into(view, out, cs.root, None).await;
        let body = export_commit(&cs, root, &parent_gits).expect("export commit");
        let oid = view.put(ObjectKind::Commit, &body);
        assert_eq!(oid, self.engine.git_commit_of(id).await.expect("gitcommit"));
        out.push((ObjectKind::Commit, oid, body));
        oid
    }
}

/// The history of the design's test list, exported by asc into one bare repository and rendered
/// loose into another: the same ids, the same bytes, and a repository `fsck --strict` accepts.
#[tokio::test]
async fn view_object_equals_the_git_export_of_the_same_history() {
    let mut fx = Fixture::new();
    let big: Vec<u8> = (0..70_000u32).map(|i| (i * 7 % 253) as u8).collect();
    let a = fx
        .commit(
            &[],
            &[
                ("empty", b""),
                ("big.bin", &big),
                ("d/g.txt", b"fixed"),
                ("src.txt", b"original content"),
            ],
            vec![],
            vec![],
        )
        .await;
    // The signature and the copy header make this a commit with an `asc` header and a gpgsig.
    let signature = b"-----BEGIN PGP SIGNATURE-----\n\nabc\n-----END PGP SIGNATURE-----".to_vec();
    let b = fx
        .commit(
            &[a],
            &[("dst.txt", b"original content"), ("big.bin", b"short now")],
            vec![(EXTRA_GIT_GPGSIG.to_vec(), signature)],
            vec![(
                RepoPath::new("dst.txt").expect("dst"),
                RepoPath::new("src.txt").expect("src"),
            )],
        )
        .await;
    // A tree whose recorded git body is not the canonical render: git's own legacy file mode.
    let leaf = fx.file(b"leaf").await;
    let Change::Write {
        file,
        size,
        git: blob,
        ..
    } = leaf
    else {
        unreachable!("write change")
    };
    let entry = Entry {
        name: Name::new(b"a".to_vec()).expect("name"),
        kind: Kind::File,
        id: *file.as_bytes(),
        size,
        git: blob,
    };
    let child = Tree::new(vec![entry], false).expect("child");
    let odd_body = {
        let canonical = render_tree(&child);
        let mut body = b"100664".to_vec();
        body.extend_from_slice(&canonical[b"100644".len()..]);
        body
    };
    assert_ne!(odd_body, render_tree(&child));
    let odd = object_oid(ObjectKind::Tree, &odd_body);
    fx.objects
        .put(&ObjKey::GitTreeOverride(odd), encode_stored(&odd_body))
        .await
        .expect("put override");
    let child_id = fx.engine.put_tree(child).await.expect("put child");
    let root = Tree::new(
        vec![Entry {
            name: Name::new(b"odd".to_vec()).expect("name"),
            kind: Kind::Dir,
            id: *child_id.as_bytes(),
            size: 0,
            git: odd,
        }],
        true,
    )
    .expect("root");
    let root_id = fx.engine.put_tree(root).await.expect("put root");
    let c = fx.changeset(&[b], root_id, vec![], vec![]).await;
    let record = TagRecord {
        target: c,
        name: b"v1".to_vec(),
        tagger: Some(sig()),
        message: b"release\n".to_vec(),
        extra: SortedMap::new(vec![]).expect("extra"),
    };
    let tag: TagId = record.id();
    fx.objects
        .put(&ObjKey::Tag(tag), encode_stored(&record.to_envelope()))
        .await
        .expect("put tag");

    // asc's own git export.
    let exported = bare_repo();
    let odb = Odb::open(exported.path()).expect("odb");
    let report = Exporter::new(&fx.engine, &*fx.objects, &fx.git, exported.path())
        .export(&[ExportRoot::Changeset(c), ExportRoot::Tag(tag)], &odb)
        .await
        .expect("export");

    // The same history, rendered loose.
    let mut view = View::new();
    let mut expected = Vec::new();
    let commit_oid = fx.render_commit_into(&mut view, &mut expected, c).await;
    let target = fx.engine.git_commit_of(c).await.expect("target");
    assert_eq!(commit_oid, target);
    let tag_body = export_tag(&record, target).expect("export tag");
    let tag_oid = view.put(ObjectKind::Tag, &tag_body);
    expected.push((ObjectKind::Tag, tag_oid, tag_body));
    assert_eq!(report.tips, vec![commit_oid, tag_oid]);

    view.set_ref("refs/heads/main", commit_oid);
    view.set_ref("refs/tags/v1", tag_oid);
    view.fsck();
    view.assert_reads_back(&expected);

    // Same object set, same kinds, same sizes as the export.
    let listing = |dir: &Path| {
        git_text(
            dir,
            &[
                "cat-file",
                "--batch-all-objects",
                "--batch-check=%(objectname) %(objecttype) %(objectsize)",
            ],
        )
    };
    assert_eq!(listing(view.path()), listing(exported.path()));
    // And every body equals the exporter's.
    for (kind, oid, body) in &expected {
        assert_eq!(
            git(
                exported.path(),
                &["cat-file", kind_name(*kind), &oid.to_string()]
            ),
            *body
        );
    }
    // The object that crosses the block boundary and the empty blob are in the set.
    assert!(
        expected
            .iter()
            .any(|(k, _, b)| *k == ObjectKind::Blob && b.len() == 70_000)
    );
    assert!(
        expected
            .iter()
            .any(|(k, _, b)| *k == ObjectKind::Blob && b.is_empty())
    );
    assert!(expected.iter().any(|(_, oid, _)| *oid == odd));
    assert_eq!(
        git_text(view.path(), &["rev-parse", "refs/tags/v1^{commit}"]),
        commit_oid.to_string()
    );
    let mut history = Vec::new();
    for id in [c, b, a] {
        history.push(
            fx.engine
                .git_commit_of(id)
                .await
                .expect("git commit")
                .to_string(),
        );
    }
    assert_eq!(
        git_text(view.path(), &["log", "--format=%H", "refs/heads/main"]),
        history.join(
            "
"
        )
    );
    // Shared objects are rendered once per reference, stored once.
    let distinct: std::collections::BTreeSet<_> = expected.iter().map(|(_, oid, _)| *oid).collect();
    assert_eq!(view.written.len(), distinct.len());
    assert_eq!(listing(view.path()).lines().count(), distinct.len());
}

/// The sizes the design calls out, written alone: an empty blob, a blob that ends exactly on a
/// block boundary, one that crosses it, and one of several blocks.
#[test]
fn view_object_block_boundary_blobs_pass_fsck() {
    let mut view = View::new();
    let mut expected = Vec::new();
    let mut entries = Vec::new();
    for (i, len) in [0usize, 1, 65_524, 65_525, 70_000, 200_000]
        .into_iter()
        .enumerate()
    {
        let body: Vec<u8> = (0..len).map(|n| (n * 13 % 251) as u8).collect();
        let oid = view.put(ObjectKind::Blob, &body);
        assert_eq!(oid, blob_oid(&body));
        entries.push(Entry {
            name: Name::new(format!("f{i}").into_bytes()).expect("name"),
            kind: Kind::File,
            id: [0; 32],
            size: len as u64,
            git: oid,
        });
        expected.push((ObjectKind::Blob, oid, body));
    }
    let tree = Tree::new(entries, true).expect("tree");
    let body = render_tree(&tree);
    let root = view.put(ObjectKind::Tree, &body);
    assert_eq!(root, tree_oid(&tree));
    expected.push((ObjectKind::Tree, root, body));
    let commit = format!(
        "tree {root}\nauthor A <a@x> 1700000000 +0000\ncommitter A <a@x> 1700000000 +0000\n\nm\n"
    )
    .into_bytes();
    let oid = view.put(ObjectKind::Commit, &commit);
    expected.push((ObjectKind::Commit, oid, commit));
    view.set_ref("refs/heads/main", oid);
    view.fsck();
    view.assert_reads_back(&expected);
    assert_eq!(
        git_text(view.path(), &["rev-parse", "main^{tree}"]),
        root.to_string()
    );
}
