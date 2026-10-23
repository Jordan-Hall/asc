//! `GitView` (U80, SPEC §25.2 and §25.3): the view of a native history, `sync`, the build state
//! machine, mirror mode and rebuild/prune, against real git >= 2.40 through `RealGit`.
#![cfg(feature = "git-compat")]

use std::collections::HashSet;
use std::io::{Cursor, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use asc_core::codec::Enveloped;
use asc_core::content::{Body, ingest};
use asc_core::git::{ObjectKind, export_tag, object_oid};
use asc_core::id::{ChangesetId, GitOid, TagId};
use asc_core::object::{Change, Kind, SortedMap, TagRecord};
use asc_core::path::{RefName, RepoPath};
use asc_core::sig::Signature;
use asc_engine::Engine;
use asc_engine::commit::{NewChangeset, PathPolicy};
use asc_git::compat::view::{RefSource, SourceError};
use asc_git::compat::{
    BuildSlots, FsckMode, GitLimits, GitView, PackRequest, PackService, Readiness, RefUpdate,
    ServerGit, ServerGitConfig, SourceRef, ViewError, ViewKind, ViewState,
};
use asc_git::export::ExportRoot;
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

fn real_git() -> RealGit {
    RealGit::resolve(None, None).expect("real git >= 2.40")
}

fn sig() -> Signature {
    Signature::new(b"A".to_vec(), b"a@x".to_vec(), 1_700_000_000, 0).expect("sig")
}

/// An asc object store with a native history, a server and a view directory.
struct Fixture {
    objects: Arc<BucketObjects>,
    engine: Eng,
    git: RealGit,
    server: ServerGit,
    dir: tempfile::TempDir,
}

impl Fixture {
    fn new() -> Self {
        let objects = Arc::new(BucketObjects::memory());
        let engine = Engine::new(objects.clone(), Arc::new(NoRefs), RepoKey(1));
        let dir = tempfile::tempdir().expect("tempdir");
        let git = real_git();
        let server = ServerGit::new(
            &git,
            ServerGitConfig {
                hooks_dir: dir.path().join("hooks"),
                state_dir: dir.path().join("state"),
                limits: GitLimits::default(),
                fsck: FsckMode::Strict,
            },
        )
        .expect("server git");
        Self {
            objects,
            engine,
            git,
            server,
            dir,
        }
    }

    fn views(&self) -> PathBuf {
        self.dir.path().join("views")
    }

    fn open(&self, id: i64) -> GitView {
        GitView::open(&self.server, &self.views(), id).expect("open view")
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

    async fn commit(&self, parents: &[ChangesetId], files: &[(&str, &[u8])]) -> ChangesetId {
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
        self.engine
            .create_changeset(
                NewChangeset {
                    parents: parents.to_vec(),
                    author: sig(),
                    committer: sig(),
                    message: b"msg\n".to_vec(),
                    extra: vec![],
                    root,
                    copies: vec![],
                },
                PathPolicy::Native { windows_safe: true },
            )
            .await
            .expect("changeset")
    }

    async fn head_ref(&self, name: &str, cs: ChangesetId) -> SourceRef {
        SourceRef {
            name: name.into(),
            target: ExportRoot::Changeset(cs),
            git: self.engine.git_commit_of(cs).await.expect("git id"),
        }
    }

    async fn tag_ref(&self, name: &str, target: ChangesetId) -> SourceRef {
        let record = TagRecord {
            target,
            name: name.as_bytes().to_vec(),
            tagger: Some(sig()),
            message: b"release\n".to_vec(),
            extra: SortedMap::new(vec![]).expect("extra"),
        };
        let id: TagId = record.id();
        self.objects
            .put(&ObjKey::Tag(id), encode_stored(&record.to_envelope()))
            .await
            .expect("put tag");
        let target_git = self.engine.git_commit_of(target).await.expect("target git");
        let raw = export_tag(&record, target_git).expect("render tag");
        SourceRef {
            name: format!("refs/tags/{name}"),
            target: ExportRoot::Tag(id),
            git: object_oid(ObjectKind::Tag, &raw),
        }
    }
}

/// A `RefSource` over a list the test edits.
#[derive(Default)]
struct Source {
    refs: Mutex<Vec<SourceRef>>,
    head: Option<String>,
    estimate: Option<u64>,
    list_calls: AtomicUsize,
}

impl Source {
    fn new(refs: Vec<SourceRef>) -> Self {
        Self {
            refs: Mutex::new(refs),
            head: Some("refs/heads/master".into()),
            estimate: Some(10),
            list_calls: AtomicUsize::new(0),
        }
    }

    fn set(&self, refs: Vec<SourceRef>) {
        *self.refs.lock().unwrap() = refs;
    }
}

#[async_trait]
impl RefSource for Source {
    async fn refs(&self) -> Result<Vec<SourceRef>, SourceError> {
        self.list_calls.fetch_add(1, Ordering::SeqCst);
        Ok(self.refs.lock().unwrap().clone())
    }
    async fn default_branch(&self) -> Result<Option<String>, SourceError> {
        Ok(self.head.clone())
    }
    async fn estimated_objects(&self) -> Result<Option<u64>, SourceError> {
        Ok(self.estimate)
    }
}

/// A source that must never be asked: a mirror is not exported.
struct Forbidden;

#[async_trait]
impl RefSource for Forbidden {
    async fn refs(&self) -> Result<Vec<SourceRef>, SourceError> {
        panic!("a mirror view must not read asc's refs")
    }
}

/// Runs raw git in `dir`; the fixtures below the crate may, with the lint marker.
fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git") // lint:allow-raw-git
        .args(args)
        .current_dir(dir)
        .env("GIT_AUTHOR_NAME", "A")
        .env("GIT_AUTHOR_EMAIL", "a@x")
        .env("GIT_AUTHOR_DATE", "1700000000 +0000")
        .env("GIT_COMMITTER_NAME", "A")
        .env("GIT_COMMITTER_EMAIL", "a@x")
        .env("GIT_COMMITTER_DATE", "1700000000 +0000")
        .stdin(Stdio::null())
        .output()
        .expect("spawn git");
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

fn names(server: &ServerGit, view: &GitView) -> Vec<String> {
    let mut v: Vec<String> = server
        .list_refs(view.path())
        .expect("list")
        .into_iter()
        .map(|(n, _)| n)
        .collect();
    v.sort();
    v
}

#[tokio::test]
async fn the_view_of_a_native_history_has_the_same_shas_as_git_and_the_export() {
    let fx = Fixture::new();
    let a = fx
        .commit(&[], &[("f.txt", b"one"), ("d/g.txt", b"fixed")])
        .await;
    let b = fx.commit(&[a], &[("f.txt", b"two")]).await;
    let source = Source::new(vec![
        fx.head_ref("refs/heads/master", b).await,
        fx.tag_ref("v1", b).await,
    ]);
    let view = fx.open(1);
    assert_eq!(view.state(), ViewState::Absent);
    assert_eq!(view.kind(), ViewKind::Derived);
    let exporter = view.exporter(&fx.engine, &*fx.objects, &fx.git);
    let report = view.sync(&source, &exporter).await.expect("sync");
    assert!(!report.noop);
    assert_eq!(report.refs_set, 2);
    assert_eq!(view.state(), ViewState::Ready);

    let dir = view.path();
    git(dir, &["fsck", "--strict", "--no-dangling"]);
    let (ga, gb) = (
        fx.engine.git_commit_of(a).await.unwrap(),
        fx.engine.git_commit_of(b).await.unwrap(),
    );
    // Same ids as `git commit-tree` over the same trees and as U34's own export.
    let tree_a = git(dir, &["rev-parse", &format!("{ga}^{{tree}}")]);
    let tree_b = git(dir, &["rev-parse", &format!("{gb}^{{tree}}")]);
    let ca = git(dir, &["commit-tree", &tree_a, "-m", "msg"]);
    let cb = git(dir, &["commit-tree", &tree_b, "-p", &ca, "-m", "msg"]);
    assert_eq!(ga.to_string(), ca);
    assert_eq!(gb.to_string(), cb);
    assert_eq!(
        git(dir, &["rev-parse", "refs/heads/master"]),
        gb.to_string()
    );
    assert_eq!(
        git(dir, &["rev-parse", "refs/tags/v1^{commit}"]),
        gb.to_string()
    );
    assert_eq!(git(dir, &["symbolic-ref", "HEAD"]), "refs/heads/master");
    assert_eq!(git(dir, &["show", &format!("{gb}:d/g.txt")]), "fixed");

    // A plain `git clone` of the view sees the same history.
    let clone = fx.dir.path().join("clone");
    git(
        fx.dir.path(),
        &[
            "clone",
            "-q",
            "--bare",
            &dir.to_string_lossy(),
            &clone.to_string_lossy(),
        ],
    );
    assert_eq!(git(&clone, &["rev-parse", "master"]), gb.to_string());
    assert_eq!(git(&clone, &["rev-list", "--count", "master"]), "2");

    // The view holds only the shared refs.
    assert_eq!(
        names(&fx.server, &view),
        vec!["refs/heads/master", "refs/tags/v1"]
    );
}

#[tokio::test]
async fn sync_is_a_noop_when_equal_and_exports_only_the_new_closure() {
    let fx = Fixture::new();
    let a = fx.commit(&[], &[("f", b"one")]).await;
    let source = Source::new(vec![fx.head_ref("refs/heads/master", a).await]);
    let view = fx.open(2);
    let exporter = view.exporter(&fx.engine, &*fx.objects, &fx.git);
    let first = view.sync(&source, &exporter).await.unwrap();
    assert_eq!(first.written, 3, "blob, tree and commit");

    let again = view.sync(&source, &exporter).await.unwrap();
    assert!(again.noop);
    assert_eq!(
        (again.refs_set, again.refs_deleted, again.written),
        (0, 0, 0)
    );

    // A ref move exports only what the new commit adds.
    let b = fx.commit(&[a], &[("f", b"two"), ("g", b"one")]).await;
    source.set(vec![
        fx.head_ref("refs/heads/master", b).await,
        fx.head_ref("refs/heads/topic", a).await,
    ]);
    let moved = view.sync(&source, &exporter).await.unwrap();
    assert!(!moved.noop);
    assert_eq!(moved.refs_set, 2);
    assert_eq!(
        moved.written, 3,
        "blob 'two', the new tree and the new commit; 'one' is shared"
    );
    assert_eq!(
        names(&fx.server, &view),
        vec!["refs/heads/master", "refs/heads/topic"]
    );

    // A ref that left asc's set leaves the view.
    source.set(vec![fx.head_ref("refs/heads/master", b).await]);
    let dropped = view.sync(&source, &exporter).await.unwrap();
    assert_eq!(
        (dropped.refs_set, dropped.refs_deleted, dropped.written),
        (0, 1, 0)
    );
    assert_eq!(names(&fx.server, &view), vec!["refs/heads/master"]);
    git(view.path(), &["fsck", "--strict", "--no-dangling"]);
}

#[tokio::test]
async fn two_concurrent_syncs_build_once() {
    let fx = Fixture::new();
    let a = fx.commit(&[], &[("f", b"one")]).await;
    let b = fx.commit(&[a], &[("f", b"two")]).await;
    let source = Source::new(vec![
        fx.head_ref("refs/heads/master", b).await,
        fx.head_ref("refs/heads/old", a).await,
    ]);
    let view = fx.open(3);
    let exporter = view.exporter(&fx.engine, &*fx.objects, &fx.git);
    let (r1, r2) = tokio::join!(view.sync(&source, &exporter), view.sync(&source, &exporter));
    let (r1, r2) = (r1.unwrap(), r2.unwrap());
    assert_eq!(
        r1.noop as u8 + r2.noop as u8,
        1,
        "exactly one of them builds"
    );
    assert_eq!(r1.refs_set + r2.refs_set, 2);
    assert_eq!(
        r1.written + r2.written,
        6,
        "two blobs, two trees and two commits, once"
    );
}

#[tokio::test]
async fn refs_the_view_must_not_hold_are_refused_and_nothing_changes() {
    let fx = Fixture::new();
    let a = fx.commit(&[], &[("f", b"one")]).await;
    let good = fx.head_ref("refs/heads/master", a).await;
    let view = fx.open(4);
    let exporter = view.exporter(&fx.engine, &*fx.objects, &fx.git);
    let source = Source::new(vec![]);

    for bad in [
        "refs/pull/1/head",
        "refs/users/7/heads/x",
        "refs/snapshots/s",
        "refs/heads/a b",
        "refs/heads/../x",
        "HEAD",
        "refs/notes/x",
        "refs/heads/",
    ] {
        source.set(vec![SourceRef {
            name: bad.into(),
            ..good.clone()
        }]);
        let err = view.sync(&source, &exporter).await.unwrap_err();
        assert!(matches!(err, ViewError::BadRef(_)), "{bad}: {err}");
    }
    source.set(vec![good.clone(), good.clone()]);
    assert!(matches!(
        view.sync(&source, &exporter).await.unwrap_err(),
        ViewError::DuplicateRef(_)
    ));
    // asc's git map and the export disagree: refused, and the view stays empty of refs.
    source.set(vec![SourceRef {
        git: GitOid::from_bytes([9; 20]),
        ..good.clone()
    }]);
    assert!(matches!(
        view.sync(&source, &exporter).await.unwrap_err(),
        ViewError::TipMismatch { .. }
    ));
    assert!(names(&fx.server, &view).is_empty());
    assert_eq!(view.state(), ViewState::Absent);

    // A default branch outside refs/heads is refused too.
    let mut with_head = Source::new(vec![good]);
    with_head.head = Some("refs/tags/x".into());
    assert!(matches!(
        view.sync(&with_head, &exporter).await.unwrap_err(),
        ViewError::BadRef(_)
    ));
}

#[tokio::test]
async fn private_refs_left_in_a_view_are_deleted_by_sync() {
    let fx = Fixture::new();
    let a = fx.commit(&[], &[("f", b"one")]).await;
    let source = Source::new(vec![fx.head_ref("refs/heads/master", a).await]);
    let view = fx.open(5);
    let exporter = view.exporter(&fx.engine, &*fx.objects, &fx.git);
    view.sync(&source, &exporter).await.unwrap();
    let oid = fx.engine.git_commit_of(a).await.unwrap();
    fx.server
        .update_refs(
            view.path(),
            &[
                RefUpdate::Set {
                    name: "refs/pull/1/head".into(),
                    oid,
                },
                RefUpdate::Set {
                    name: "refs/users/7/heads/x".into(),
                    oid,
                },
            ],
        )
        .unwrap();
    let report = view.sync(&source, &exporter).await.unwrap();
    assert_eq!(report.refs_deleted, 2);
    assert_eq!(names(&fx.server, &view), vec!["refs/heads/master"]);
}

#[tokio::test]
async fn the_build_state_machine_answers_building_until_a_big_view_is_ready() {
    let fx = Fixture::new();
    let a = fx.commit(&[], &[("f", b"one")]).await;
    let mut source = Source::new(vec![fx.head_ref("refs/heads/master", a).await]);
    source.estimate = Some(asc_git::compat::INLINE_BUILD_OBJECTS + 1);
    let view = fx.open(6);
    let exporter = view.exporter(&fx.engine, &*fx.objects, &fx.git);

    // Too big to build inline: the caller gets a ticket and answers 503.
    let Readiness::StartBackgroundBuild {
        ticket,
        retry_after,
    } = view.ensure(&source, &exporter).await.unwrap()
    else {
        panic!("expected a background build");
    };
    assert!(retry_after.as_secs() > 0);
    assert_eq!(view.state(), ViewState::Building);
    assert!(names(&fx.server, &view).is_empty(), "nothing exported yet");
    // While it builds, other requests just wait.
    assert!(matches!(
        view.ensure(&source, &exporter).await.unwrap(),
        Readiness::Building { .. }
    ));
    let report = view.build(ticket, &source, &exporter).await.unwrap();
    assert_eq!(report.refs_set, 1);
    assert_eq!(view.state(), ViewState::Ready);
    let Readiness::Ready(r) = view.ensure(&source, &exporter).await.unwrap() else {
        panic!("expected a ready view");
    };
    assert!(r.noop);

    // The ready marker survives a restart.
    assert_eq!(fx.open(6).state(), ViewState::Ready);

    // A dropped ticket returns the view to absent.
    let other = fx.open(60);
    let other_exporter = other.exporter(&fx.engine, &*fx.objects, &fx.git);
    let Readiness::StartBackgroundBuild { ticket, .. } =
        other.ensure(&source, &other_exporter).await.unwrap()
    else {
        panic!("expected a background build");
    };
    drop(ticket);
    assert_eq!(other.state(), ViewState::Absent);

    // An unknown estimate also builds in the background.
    source.estimate = None;
    assert!(matches!(
        other.ensure(&source, &other_exporter).await.unwrap(),
        Readiness::StartBackgroundBuild { .. }
    ));
}

#[tokio::test]
async fn a_small_absent_view_builds_inline_and_a_failed_build_leaves_it_absent() {
    let fx = Fixture::new();
    let a = fx.commit(&[], &[("f", b"one")]).await;
    let good = fx.head_ref("refs/heads/master", a).await;
    let view = fx.open(8);
    let exporter = view.exporter(&fx.engine, &*fx.objects, &fx.git);

    let bad = Source::new(vec![SourceRef {
        name: "refs/pull/1/head".into(),
        ..good.clone()
    }]);
    assert!(view.ensure(&bad, &exporter).await.is_err());
    assert_eq!(
        view.state(),
        ViewState::Absent,
        "a failed build does not stick at 'building'"
    );

    let source = Source::new(vec![good]);
    let Readiness::Ready(report) = view.ensure(&source, &exporter).await.unwrap() else {
        panic!("a small repo builds inline");
    };
    assert_eq!(report.refs_set, 1);
    assert_eq!(view.state(), ViewState::Ready);
}

#[tokio::test]
async fn builds_are_capped_per_replica() {
    let fx = Fixture::new();
    let a = fx.commit(&[], &[("f", b"one")]).await;
    let mut source = Source::new(vec![fx.head_ref("refs/heads/master", a).await]);
    source.estimate = None;
    let slots = BuildSlots::new(1);
    let (v1, v2) = (fx.open(11), fx.open(12));
    v1.set_build_slots(slots.clone());
    v2.set_build_slots(slots.clone());
    let e1 = v1.exporter(&fx.engine, &*fx.objects, &fx.git);
    let e2 = v2.exporter(&fx.engine, &*fx.objects, &fx.git);
    let Readiness::StartBackgroundBuild { ticket, .. } = v1.ensure(&source, &e1).await.unwrap()
    else {
        panic!("expected a background build");
    };
    assert_eq!(slots.in_use(), 1);
    // No slot for the second view: it is told to wait and stays absent.
    assert!(matches!(
        v2.ensure(&source, &e2).await.unwrap(),
        Readiness::Building { .. }
    ));
    assert_eq!(v2.state(), ViewState::Absent);
    v1.build(ticket, &source, &e1).await.unwrap();
    assert_eq!(slots.in_use(), 0, "the slot is released with the ticket");
    assert!(matches!(
        v2.ensure(&source, &e2).await.unwrap(),
        Readiness::StartBackgroundBuild { .. }
    ));
    // A ticket from another view is refused.
    let Readiness::StartBackgroundBuild { ticket, .. } = ({
        let v3 = fx.open(13);
        v3.ensure(&source, &v3.exporter(&fx.engine, &*fx.objects, &fx.git))
            .await
            .unwrap()
    }) else {
        panic!("expected a background build");
    };
    assert!(matches!(
        v1.build(ticket, &source, &e1).await.unwrap_err(),
        ViewError::ForeignTicket
    ));
}

#[tokio::test]
async fn a_mirror_is_its_own_read_only_view_with_no_export() {
    let fx = Fixture::new();
    let mirror = fx.dir.path().join("mirror.git");
    git(
        fx.dir.path(),
        &["init", "-q", "--bare", &mirror.to_string_lossy()],
    );
    let empty_tree = git(&mirror, &["hash-object", "-t", "tree", "-w", "--stdin"]);
    assert_eq!(empty_tree, "4b825dc642cb6eb9a060e54bf8d69288fbee4904");
    let commit = git(
        &mirror,
        &["commit-tree", &empty_tree, "-m", "from upstream"],
    );
    git(&mirror, &["update-ref", "refs/heads/trunk", &commit]);
    git(&mirror, &["symbolic-ref", "HEAD", "refs/heads/trunk"]);
    // What `git clone --mirror` writes is accepted.
    git(
        &mirror,
        &[
            "config",
            "remote.origin.url",
            "https://example.invalid/x.git",
        ],
    );
    git(
        &mirror,
        &["config", "remote.origin.fetch", "+refs/*:refs/*"],
    );
    git(&mirror, &["config", "remote.origin.mirror", "true"]);

    let view = GitView::open_mirror(&fx.server, &mirror).expect("mirror view");
    assert_eq!(view.kind(), ViewKind::Mirror);
    assert_eq!(view.state(), ViewState::Ready);
    assert_eq!(view.path(), mirror);
    let exporter = view.exporter(&fx.engine, &*fx.objects, &fx.git);
    // `Forbidden` panics if asked for asc's refs: a mirror is served from its own directory.
    let report = view.sync(&Forbidden, &exporter).await.unwrap();
    assert!(report.noop && report.written == 0);
    assert!(matches!(
        view.ensure(&Forbidden, &exporter).await.unwrap(),
        Readiness::Ready(r) if r.noop
    ));

    // Reads work; pushes are refused before any child starts.
    let mut child = view
        .spawn_pack(
            PackService::UploadPack,
            &PackRequest {
                advertise_refs: true,
                protocol: None,
            },
        )
        .unwrap();
    child.close_stdin();
    let mut out = String::new();
    child
        .stdout
        .take()
        .unwrap()
        .read_to_string(&mut out)
        .unwrap();
    assert!(child.wait().unwrap().status.success());
    assert!(out.contains(&format!("{commit} refs/heads/trunk")), "{out}");
    assert!(matches!(
        view.spawn_pack(PackService::ReceivePack, &PackRequest::default())
            .map(|_| ())
            .unwrap_err(),
        ViewError::ReadOnlyMirror
    ));

    // A mirror whose config could run code is not served.
    for (key, value) in [
        ("core.hooksPath", "/tmp/x"),
        ("uploadpack.packObjectsHook", "/tmp/x"),
        ("core.fsmonitor", "/tmp/x"),
        ("receive.certNonceSeed", "x"),
        ("remote.origin.promisor", "true"),
        ("filter.lfs.process", "x"),
        ("include.path", "/tmp/x"),
    ] {
        git(&mirror, &["config", key, value]);
        assert!(
            GitView::open_mirror(&fx.server, &mirror).is_err(),
            "{key} must be refused"
        );
        assert!(
            view.verify_config().is_err(),
            "{key} must be refused on a live view"
        );
        git(&mirror, &["config", "--unset-all", key]);
        view.verify_config().unwrap();
    }
    assert!(GitView::open_mirror(&fx.server, &fx.dir.path().join("nope")).is_err());
}

#[tokio::test]
async fn rebuild_discards_and_rederives_and_prune_removes_dead_views() {
    let fx = Fixture::new();
    let a = fx.commit(&[], &[("f", b"one")]).await;
    let source = Source::new(vec![fx.head_ref("refs/heads/master", a).await]);
    let view = fx.open(20);
    let exporter = view.exporter(&fx.engine, &*fx.objects, &fx.git);
    view.sync(&source, &exporter).await.unwrap();
    std::fs::write(view.path().join("junk"), b"x").unwrap();
    let report = view.rebuild(&source, &exporter).await.unwrap();
    assert_eq!(report.refs_set, 1);
    assert_eq!(report.written, 3, "everything is exported again");
    assert!(!view.path().join("junk").exists());
    assert_eq!(view.state(), ViewState::Ready);
    git(view.path(), &["fsck", "--strict", "--no-dangling"]);

    let _other = fx.open(21);
    let _third = fx.open(22);
    std::fs::create_dir_all(fx.views().join("not-a-view")).unwrap();
    std::fs::create_dir_all(fx.views().join("abc.git")).unwrap();
    let live: HashSet<i64> = [20].into_iter().collect();
    let removed = GitView::prune(&fx.views(), &live).unwrap();
    assert_eq!(removed, vec![21, 22]);
    assert!(view.path().is_dir());
    assert!(
        fx.views().join("not-a-view").is_dir(),
        "only <number>.git is touched"
    );
    assert!(fx.views().join("abc.git").is_dir());
    assert!(
        GitView::prune(&fx.dir.path().join("none"), &live)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn the_view_directory_comes_from_a_number_and_must_be_absolute() {
    let fx = Fixture::new();
    assert!(matches!(
        GitView::open(&fx.server, &fx.views(), -1).unwrap_err(),
        ViewError::BadRepoId(-1)
    ));
    assert!(matches!(
        GitView::open(&fx.server, Path::new("relative"), 1).unwrap_err(),
        ViewError::NotAbsolute(_)
    ));
    let view = fx.open(30);
    assert_eq!(view.path(), fx.views().join("30.git"));
    // A half-made view (no HEAD) is made again.
    let half = fx.views().join("31.git");
    std::fs::create_dir_all(half.join("objects")).unwrap();
    let fixed = fx.open(31);
    assert!(fixed.path().join("HEAD").is_file());
    // Reopening keeps what is there.
    let again = fx.open(30);
    assert_eq!(again.state(), ViewState::Absent);
}

#[cfg(unix)]
#[test]
fn a_symlinked_view_directory_is_refused() {
    let fx = Fixture::new();
    std::fs::create_dir_all(fx.views()).unwrap();
    let target = fx.dir.path().join("elsewhere");
    std::fs::create_dir_all(&target).unwrap();
    std::os::unix::fs::symlink(&target, fx.views().join("40.git")).unwrap();
    assert!(matches!(
        GitView::open(&fx.server, &fx.views(), 40).unwrap_err(),
        ViewError::NotADirectory(_)
    ));
}
