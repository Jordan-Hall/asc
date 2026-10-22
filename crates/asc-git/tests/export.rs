//! Git export (U34): output equals git's own, and asc ids survive export and re-import.

use std::io::{Cursor, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::Arc;

use async_trait::async_trait;
use asc_core::codec::Enveloped;
use asc_core::content::{Body, ingest};
use asc_core::git::{
    EXTRA_GIT_GPGSIG, ObjectKind, canonicalise_commit, canonicalise_tag, object_oid, render_tree,
};
use asc_core::id::{ChangesetId, GitOid, TagId};
use asc_core::object::{Change, Entry, Kind, SortedMap, TagRecord, Tree};
use asc_core::path::{Name, RefName, RepoPath};
use asc_core::sig::Signature;
use asc_engine::Engine;
use asc_engine::commit::{NewChangeset, PathPolicy};
use asc_git::export::{ExportReport, ExportRoot, Exporter, PACK_THRESHOLD};
use asc_git::odb::Odb;
use asc_git::realgit::RealGit;
use asc_store::bucket::BucketObjects;
use asc_store::codec::encode_stored;
use asc_store::key::ObjKey;
use asc_store::objects::Objects;
use asc_store::refs::{RefEntry, RefError, RefLogEntry, RefTxn, Refs, RepoKey};

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

struct Fixture {
    objects: Arc<BucketObjects>,
    engine: Eng,
    git: RealGit,
    dir: tempfile::TempDir,
}

impl Fixture {
    fn new() -> Self {
        let objects = Arc::new(BucketObjects::memory());
        let engine = Engine::new(objects.clone(), Arc::new(NoRefs), RepoKey(1));
        let dir = tempfile::tempdir().expect("tempdir");
        git(dir.path(), &["init", "-q", "--bare", "."], b"");
        let git = RealGit::resolve(None, None).expect("real git");
        Self {
            objects,
            engine,
            git,
            dir,
        }
    }

    fn odb(&self) -> Odb {
        Odb::open(self.dir.path()).expect("odb")
    }

    async fn export(&self, roots: &[ExportRoot]) -> ExportReport {
        let ex = Exporter::new(&self.engine, &*self.objects, &self.git, self.dir.path());
        ex.export(roots, &self.odb()).await.expect("export")
    }

    async fn file(&self, content: &[u8]) -> Change {
        let got = ingest(Cursor::new(content)).expect("ingest");
        let Body::Inline(b) = got.body else {
            unreachable!("test files are small")
        };
        self.objects
            .put(&ObjKey::File(got.file), encode_stored(&b))
            .await
            .expect("put file");
        Change::write(Kind::File, got.file, got.size, got.git, None).expect("change")
    }

    async fn commit(
        &self,
        parents: &[ChangesetId],
        files: &[(&str, &[u8])],
        extra: Vec<(Vec<u8>, Vec<u8>)>,
        copies: Vec<(RepoPath, RepoPath)>,
    ) -> ChangesetId {
        let mut changes = Vec::new();
        for (path, content) in files {
            changes.push((
                RepoPath::new(*path).expect("path"),
                self.file(content).await,
            ));
        }
        let base = match parents.first() {
            Some(&p) => self.engine.get_changeset(p).await.expect("parent").root,
            None => Eng::empty_tree_id(),
        };
        let root = self.engine.apply(base, &changes).await.expect("apply");
        self.new_changeset(parents, root, extra, copies).await
    }

    async fn new_changeset(
        &self,
        parents: &[ChangesetId],
        root: asc_core::id::TreeId,
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
}

fn sig() -> Signature {
    Signature::new(b"A".to_vec(), b"a@x".to_vec(), 1_700_000_000, 0).expect("sig")
}

/// Runs git in `dir` with a fixed identity, feeding `stdin`; returns trimmed stdout.
fn git(dir: &Path, args: &[&str], stdin: &[u8]) -> String {
    let mut child = Command::new("git") // lint:allow-raw-git
        .args(args)
        .current_dir(dir)
        .env("GIT_AUTHOR_NAME", "A")
        .env("GIT_AUTHOR_EMAIL", "a@x")
        .env("GIT_AUTHOR_DATE", "1700000000 +0000")
        .env("GIT_COMMITTER_NAME", "A")
        .env("GIT_COMMITTER_EMAIL", "a@x")
        .env("GIT_COMMITTER_DATE", "1700000000 +0000")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn git");
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(stdin)
        .expect("write stdin");
    let out = child.wait_with_output().expect("git output");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout)
        .expect("utf8")
        .trim()
        .to_owned()
}

fn raw_object(dir: &Path, kind: &str, oid: GitOid) -> Vec<u8> {
    let out = Command::new("git") // lint:allow-raw-git
        .args(["cat-file", kind, &oid.to_string()])
        .current_dir(dir)
        .output()
        .expect("cat-file");
    assert!(out.status.success(), "cat-file {kind} {oid}");
    out.stdout
}

#[tokio::test]
async fn native_history_equals_git_commit_tree() {
    let fx = Fixture::new();
    let a = fx
        .commit(
            &[],
            &[("f.txt", b"one"), ("d/g.txt", b"fixed")],
            vec![],
            vec![],
        )
        .await;
    let b = fx.commit(&[a], &[("f.txt", b"two")], vec![], vec![]).await;
    let report = fx.export(&[ExportRoot::Changeset(b)]).await;
    assert!(!report.packed);
    let (ga, gb) = (
        fx.engine.git_commit_of(a).await.expect("a"),
        fx.engine.git_commit_of(b).await.expect("b"),
    );
    assert_eq!(report.tips, vec![gb]);
    let dir = fx.dir.path();
    git(dir, &["fsck", "--strict", "--no-dangling"], b"");
    let tree_a = git(dir, &["rev-parse", &format!("{ga}^{{tree}}")], b"");
    let tree_b = git(dir, &["rev-parse", &format!("{gb}^{{tree}}")], b"");
    let ca = git(dir, &["commit-tree", &tree_a, "-m", "msg"], b"");
    let cb = git(dir, &["commit-tree", &tree_b, "-p", &ca, "-m", "msg"], b"");
    assert_eq!(ga.to_string(), ca);
    assert_eq!(gb.to_string(), cb);
    assert_eq!(git(dir, &["show", &format!("{gb}:f.txt")], b""), "two");
    assert_eq!(git(dir, &["show", &format!("{gb}:d/g.txt")], b""), "fixed");
}

#[tokio::test]
async fn present_objects_are_skipped() {
    let fx = Fixture::new();
    let a = fx.commit(&[], &[("f", b"one")], vec![], vec![]).await;
    let first = fx.export(&[ExportRoot::Changeset(a)]).await;
    assert_eq!(first.written, 3, "blob, tree and commit");
    let again = fx.export(&[ExportRoot::Changeset(a)]).await;
    assert_eq!(again.written, 0);
    assert_eq!(again.tips, first.tips);
    let b = fx
        .commit(&[a], &[("f", b"two"), ("h", b"one")], vec![], vec![])
        .await;
    let next = fx.export(&[ExportRoot::Changeset(b)]).await;
    // "two", "one" is shared with the first commit's blob, plus the new tree and commit.
    assert_eq!(next.written, 3);
    git(fx.dir.path(), &["fsck", "--strict", "--no-dangling"], b"");
}

#[tokio::test]
async fn many_objects_go_into_one_indexed_pack() {
    let fx = Fixture::new();
    let names: Vec<String> = (0..=PACK_THRESHOLD).map(|i| format!("f{i:05}")).collect();
    let contents: Vec<String> = (0..=PACK_THRESHOLD)
        .map(|i| format!("content {i}"))
        .collect();
    let files: Vec<(&str, &[u8])> = names
        .iter()
        .zip(&contents)
        .map(|(n, c)| (n.as_str(), c.as_bytes()))
        .collect();
    let a = fx.commit(&[], &files, vec![], vec![]).await;
    let report = fx.export(&[ExportRoot::Changeset(a)]).await;
    assert!(report.packed);
    assert_eq!(report.written, PACK_THRESHOLD as u64 + 1 + 2);
    let dir = fx.dir.path();
    git(dir, &["fsck", "--strict", "--no-dangling"], b"");
    let counts = git(dir, &["count-objects", "-v"], b"");
    assert!(counts.contains("packs: 1"), "{counts}");
    assert!(counts.contains("count: 0"), "{counts}");
    let leftovers = std::fs::read_dir(dir.join("objects/pack"))
        .expect("pack dir")
        .filter_map(Result::ok)
        .filter(|e| e.file_name().to_string_lossy().starts_with("tmp_asc_"))
        .count();
    assert_eq!(leftovers, 0);
    let g = report.tips[0];
    assert_eq!(
        git(dir, &["show", &format!("{g}:f00007")], b""),
        "content 7"
    );
}

#[tokio::test]
async fn refs_are_set_through_update_ref() {
    let fx = Fixture::new();
    let a = fx.commit(&[], &[("f", b"one")], vec![], vec![]).await;
    let report = fx.export(&[ExportRoot::Changeset(a)]).await;
    let ex = Exporter::new(&fx.engine, &*fx.objects, &fx.git, fx.dir.path());
    ex.set_refs(&[
        ("refs/heads/master", report.tips[0]),
        ("refs/heads/other", report.tips[0]),
    ])
    .expect("set refs");
    let dir = fx.dir.path();
    assert_eq!(
        git(dir, &["rev-parse", "refs/heads/other"], b""),
        report.tips[0].to_string()
    );
    assert!(
        ex.set_refs(&[("refs/heads/missing", GitOid::from_bytes([7; 20]))])
            .is_err()
    );
    assert!(ex.set_refs(&[("bad\nname", report.tips[0])]).is_err());
}

#[tokio::test]
async fn copies_signature_and_tags_round_trip_to_identical_ids() {
    let fx = Fixture::new();
    let a = fx
        .commit(&[], &[("src.txt", b"original content")], vec![], vec![])
        .await;
    let signature = b"-----BEGIN PGP SIGNATURE-----\n\nabc\n-----END PGP SIGNATURE-----".to_vec();
    let b = fx
        .commit(
            &[a],
            &[("dst.txt", b"original content")],
            vec![(EXTRA_GIT_GPGSIG.to_vec(), signature)],
            vec![(
                RepoPath::new("dst.txt").expect("dst"),
                RepoPath::new("src.txt").expect("src"),
            )],
        )
        .await;
    let record = TagRecord {
        target: b,
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
    let report = fx
        .export(&[ExportRoot::Changeset(b), ExportRoot::Tag(tag)])
        .await;
    let dir = fx.dir.path();
    git(dir, &["fsck", "--strict", "--no-dangling"], b"");

    // Re-import with the same primitives the importer uses.
    let (ga, gb) = (
        fx.engine.git_commit_of(a).await.expect("a"),
        fx.engine.git_commit_of(b).await.expect("b"),
    );
    assert_eq!(report.tips[0], gb);
    let raw_b = raw_object(dir, "commit", gb);
    assert!(
        String::from_utf8_lossy(&raw_b).contains("\nasc "),
        "asc header present"
    );
    assert!(String::from_utf8_lossy(&raw_b).contains("\ngpgsig "));
    let original = fx.engine.get_changeset(b).await.expect("cs b");
    let parent_a = fx.engine.get_changeset(a).await.expect("cs a");
    let raw_a = raw_object(dir, "commit", ga);
    let again_a = canonicalise_commit(
        &raw_a,
        parent_a.root,
        vec![],
        parent_a.changes.as_slice().to_vec(),
    )
    .expect("canonicalise a");
    assert_eq!(again_a.id(), a);
    let again_b = canonicalise_commit(
        &raw_b,
        original.root,
        vec![a],
        original.changes.as_slice().to_vec(),
    )
    .expect("canonicalise b");
    assert_eq!(again_b.id(), b);
    let raw_tag = raw_object(dir, "tag", report.tips[1]);
    let again_tag = canonicalise_tag(&raw_tag, b).expect("canonicalise tag");
    assert_eq!(again_tag.id(), tag);
    assert_eq!(
        git(
            dir,
            &["rev-parse", &format!("{}^{{commit}}", report.tips[1])],
            b""
        ),
        gb.to_string()
    );
}

#[tokio::test]
async fn non_canonical_tree_uses_its_override() {
    let fx = Fixture::new();
    let a = fx.file(b"leaf").await;
    let Change::Write {
        file,
        size,
        git: blob,
        ..
    } = a
    else {
        unreachable!("write change")
    };
    let leaf = |name: &str| Entry {
        name: Name::new(name.as_bytes().to_vec()).expect("name"),
        kind: Kind::File,
        id: *file.as_bytes(),
        size,
        git: blob,
    };
    let child = Tree::new(vec![leaf("a")], false).expect("child");
    // The same entries with a zero-padded directory mode, as old git versions wrote.
    let odd_body = {
        let mut body = b"0".to_vec();
        body.extend_from_slice(&render_tree(&child));
        body
    };
    let odd = object_oid(ObjectKind::Tree, &odd_body);
    fx.objects
        .put(&ObjKey::GitTreeOverride(odd), encode_stored(&odd_body))
        .await
        .expect("put override");
    let child_id = fx.engine.put_tree(child).await.expect("put child");
    let root = Tree::new(
        vec![Entry {
            name: Name::new(b"d".to_vec()).expect("d"),
            kind: Kind::Dir,
            id: *child_id.as_bytes(),
            size: 0,
            git: odd,
        }],
        true,
    )
    .expect("root");
    let root_id = fx.engine.put_tree(root).await.expect("put root");
    let cs = fx.new_changeset(&[], root_id, vec![], vec![]).await;
    fx.export(&[ExportRoot::Changeset(cs)]).await;
    assert_eq!(
        raw_object(fx.dir.path(), "tree", odd),
        odd_body,
        "the stored git body is written under the recorded oid"
    );
}
