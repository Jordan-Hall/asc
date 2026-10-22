use std::io::Cursor;
use std::sync::Arc;
use std::time::Duration;

use asc_core::content::{Body, ingest};
use asc_core::git::blob_oid;
use asc_core::object::{Change, Kind};
use asc_core::path::RefName;
use asc_core::sig::Signature;
use asc_engine::Engine;
use asc_engine::commit::{NewChangeset, PathPolicy};
use asc_git::index::{FileMode, RenderOptions, render};
use asc_state::state::repos::{self, NewRepo, RemoteKind};
use asc_state::state::stage::StageRow;
use asc_state::state::workspaces::NewWorkspace;
use asc_state::{RepoId, StateDb};
use asc_store::cachedir::CacheDir;
use asc_store::codec::encode_stored;
use asc_store::key::ObjKey;
use asc_store::objects::Objects;
use asc_store::refs::{Actor, Expect, RefTxn, RefUpdate, Refs, RepoKey};
use asc_store::sqlrefs::SqlRefs;
use asc_store::storeset::StoreSet;
use futures::TryStreamExt;

use super::*;

type Eng = Engine<StoreSet, SqlRefs>;

struct World {
    _tmp: tempfile::TempDir,
    home: AscHome,
    db: StateDb,
    root: PathBuf,
    gitdir: PathBuf,
    ws: WorkspaceId,
    repo: RepoKey,
    objects: Arc<StoreSet>,
    engine: Eng,
}

impl World {
    async fn new(mode: Mode) -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let home = AscHome::open(tmp.path().join("home")).unwrap();
        let db = StateDb::open(&home).await.unwrap();
        let repo_id = RepoId::new();
        let repo = repos::insert(
            &db,
            &NewRepo {
                id: repo_id,
                name: "r".into(),
                ns: vec![7; 16],
                remote_url: None,
                remote_kind: RemoteKind::LocalOnly,
                default_branch: "master".into(),
                windows_safe: true,
            },
        )
        .await
        .unwrap();
        let ws = WorkspaceId::new();
        let root = tmp.path().join("work");
        std::fs::create_dir_all(&root).unwrap();
        Projection::create(&home, repo_id, ws, &root).unwrap();
        let path = root.display().to_string();
        workspaces::insert(
            &db,
            &NewWorkspace {
                id: ws,
                repo,
                path: path.clone(),
                mount: path,
                mode,
                head: "refs/heads/master".into(),
                base_cs: None,
                sparse: None,
                ephemeral_until: None,
            },
        )
        .await
        .unwrap();
        let objects = Arc::new(StoreSet {
            store: CacheDir::open(home.store(), home.tmp()).unwrap(),
            cache: CacheDir::open(home.cache(), home.tmp()).unwrap(),
        });
        let engine = Engine::new(objects.clone(), Arc::new(db.refs()), repo);
        let gitdir = home.gitdir(ws);
        Self {
            _tmp: tmp,
            home,
            db,
            root,
            gitdir,
            ws,
            repo,
            objects,
            engine,
        }
    }

    fn sink(&self) -> IndexSink {
        IndexSink::with_home(self.home.clone())
    }

    /// A commit on `parent` writing `files` (also onto disk) with the branch moved to it.
    async fn commit(&self, parent: Option<ChangesetId>, files: &[(&str, &[u8])]) -> ChangesetId {
        let base = match parent {
            Some(p) => self.engine.get_changeset(p).await.unwrap().root,
            None => Eng::empty_tree_id(),
        };
        let mut changes = Vec::new();
        for (path, content) in files {
            let got = ingest(Cursor::new(*content)).unwrap();
            let Body::Inline(bytes) = got.body else {
                panic!("small")
            };
            self.objects
                .put(&ObjKey::File(got.file), encode_stored(&bytes))
                .await
                .unwrap();
            changes.push((
                RepoPath::new(*path).unwrap(),
                Change::write(Kind::File, got.file, got.size, got.git, None).unwrap(),
            ));
            let on_disk = self.root.join(path);
            std::fs::create_dir_all(on_disk.parent().unwrap()).unwrap();
            std::fs::write(on_disk, content).unwrap();
        }
        let root = self.engine.apply(base, &changes).await.unwrap();
        let sig =
            Signature::new(b"T".to_vec(), b"t@example.com".to_vec(), 1_700_000_000, 0).unwrap();
        let cs = self
            .engine
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
            .unwrap();
        let txn = RefTxn {
            updates: vec![RefUpdate {
                name: RefName::new(b"refs/heads/master".to_vec()).unwrap(),
                expect: Expect::Any,
                new: Some(asc_core::id::RefTarget::Changeset(cs)),
                expires_at: None,
            }],
            actor: Actor {
                id: "t".into(),
                via: "test".into(),
            },
            reason: "t".into(),
            op_id: uuid::Uuid::now_v7(),
        };
        Refs::update(&self.db.refs(), self.repo, &txn)
            .await
            .unwrap();
        cs
    }

    /// What a checkout leaves behind: an index of `cs` with the real stat of the files on disk.
    async fn checkout_index(&self, cs: ChangesetId) {
        let root = self.engine.get_changeset(cs).await.unwrap().root;
        let mut entries = Vec::new();
        let mut walk = self.engine.walk(root, None);
        while let Some(item) = walk.try_next().await.unwrap() {
            entries.push(item);
        }
        let stat = PlainStat {
            root: self.root.clone(),
        };
        let idx = render(entries.into_iter(), &stat, &RenderOptions::default());
        asc_git::index::write(&self.gitdir.join("index"), &idx).unwrap();
    }

    fn index(&self) -> Option<IndexFile> {
        IndexFile::read(&self.gitdir.join("index")).ok()
    }
}

fn listing(idx: &IndexFile) -> Vec<(String, String)> {
    idx.entries
        .iter()
        .map(|e| {
            (
                String::from_utf8_lossy(e.path.as_bytes()).into_owned(),
                e.oid.to_string(),
            )
        })
        .collect()
}

fn expect(files: &[(&str, &[u8])]) -> Vec<(String, String)> {
    let mut v: Vec<_> = files
        .iter()
        .map(|(p, c)| (p.to_string(), blob_oid(c).to_string()))
        .collect();
    v.sort();
    v
}

fn staged(path: &str, content: &[u8]) -> StageRow {
    let got = ingest(Cursor::new(content)).unwrap();
    StageRow {
        path: path.as_bytes().to_vec(),
        stage: 0,
        kind: Kind::File,
        file: Some(got.file),
        size: got.size,
        git: Some(got.git),
        intent_to_add: false,
    }
}

#[tokio::test]
async fn commit_head_move_and_staging_keep_the_export_in_step() {
    let w = World::new(Mode::Plain).await;
    let sink = w.sink();
    assert!(w.index().is_none());

    let c1 = w
        .commit(None, &[("a.txt", b"one"), ("src/b.txt", b"two")])
        .await;
    w.checkout_index(c1).await;
    sink.on_head_moved(w.ws, None, Some(c1)).await.unwrap();
    let idx = w.index().expect("exported");
    assert_eq!(
        listing(&idx),
        expect(&[("a.txt", b"one"), ("src/b.txt", b"two")])
    );

    // Clean plain workspace: every entry's stat equals the file's, so a stat-trusting reader
    // hashes nothing, and no entry is racy.
    for e in &idx.entries {
        let name = String::from_utf8_lossy(e.path.as_bytes()).into_owned();
        let meta = std::fs::metadata(w.root.join(name)).unwrap();
        assert_eq!(u64::from(e.size), meta.len());
        assert_eq!(
            e.mtime,
            StatTime::from_system_time(meta.modified().unwrap())
        );
        assert!(idx.mtime.unwrap() >= e.mtime.to_system_time() + Duration::from_secs(2));
        assert_eq!(e.mode, FileMode::File);
    }

    // A staged new file, a staged delete and a staged modify.
    let rows = [
        staged("n.txt", b"new"),
        StageRow::from_leaf(b"src/b.txt".to_vec(), None),
        staged("a.txt", b"one changed"),
    ];
    stage::upsert(&w.db, w.ws, StageSet::Default, &rows)
        .await
        .unwrap();
    let paths: Vec<RepoPath> = ["n.txt", "src/b.txt", "a.txt"]
        .iter()
        .map(|p| RepoPath::new(*p).unwrap())
        .collect();
    sink.on_stage_changed(w.ws, &paths).await.unwrap();
    assert_eq!(
        listing(&w.index().unwrap()),
        expect(&[("a.txt", b"one changed"), ("n.txt", b"new")])
    );

    // Commit the staged state; `src/b.txt` stays in HEAD because the commit never deleted it.
    let c2 = w
        .commit(Some(c1), &[("a.txt", b"one changed"), ("n.txt", b"new")])
        .await;
    stage::clear(&w.db, w.ws, StageSet::Default).await.unwrap();
    sink.on_commit(w.ws, c2).await.unwrap();
    assert_eq!(
        listing(&w.index().unwrap()),
        expect(&[
            ("a.txt", b"one changed"),
            ("n.txt", b"new"),
            ("src/b.txt", b"two")
        ])
    );
    let root2 = w.engine.get_changeset(c2).await.unwrap().root;
    assert_eq!(index_export::read_base(&w.gitdir).map(|b| b.0), Some(root2));

    // Back to the first commit.
    sink.on_head_moved(w.ws, Some(c2), Some(c1)).await.unwrap();
    assert_eq!(
        listing(&w.index().unwrap()),
        expect(&[("a.txt", b"one"), ("src/b.txt", b"two")])
    );
}

#[tokio::test]
async fn a_virtual_workspace_gets_an_index_only_with_turbo_json() {
    let w = World::new(Mode::Virtual).await;
    let sink = w.sink();
    let c1 = w.commit(None, &[("a.txt", b"one")]).await;
    sink.on_commit(w.ws, c1).await.unwrap();
    assert!(
        !w.gitdir.join("index").exists(),
        "no turbo.json: no index file"
    );

    let c2 = w.commit(Some(c1), &[("turbo.json", b"{}")]).await;
    sink.on_commit(w.ws, c2).await.unwrap();
    let idx = w.index().expect("turbo.json turns it on");
    assert_eq!(
        listing(&idx),
        expect(&[("a.txt", b"one"), ("turbo.json", b"{}")])
    );
    assert!(
        !w.gitdir.join("shallow").exists(),
        "no shallow file is ever written"
    );

    sink.on_head_moved(w.ws, Some(c2), Some(c1)).await.unwrap();
    assert!(
        !w.gitdir.join("index").exists(),
        "off again once turbo.json is gone"
    );
}

#[tokio::test]
async fn an_unverified_file_is_never_recorded_as_stat_clean() {
    let w = World::new(Mode::Plain).await;
    let sink = w.sink();
    let c1 = w
        .commit(None, &[("a.txt", b"one"), ("b.txt", b"two")])
        .await;
    // The file now differs from HEAD with the same size and nothing has hashed it.
    std::fs::write(w.root.join("b.txt"), b"twp").unwrap();
    sink.on_head_moved(w.ws, None, Some(c1)).await.unwrap();
    let idx = w.index().expect("exported");
    assert_eq!(
        listing(&idx),
        expect(&[("a.txt", b"one"), ("b.txt", b"two")])
    );
    assert!(
        idx.entries.iter().all(|e| e.mtime == StatTime::default()),
        "no entry may claim a stat match for content asc has not seen"
    );
}
