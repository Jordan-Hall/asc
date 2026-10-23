//! U60 acceptance: `submodule status | set-pin | update` through the built binary, over a parent
//! repository whose gitlink is composed from a nested repository registered in `state.db`.

mod support;

use std::fs;
use std::io::Cursor;
use std::process::Output;
use std::sync::Arc;

use asc_core::content::{Body, ingest};
use asc_core::git::ObjectKind;
use asc_core::id::{ChangesetId, FileId, GitOid};
use asc_core::object::{Change, Kind};
use asc_core::path::RepoPath;
use asc_core::sig::Signature;
use asc_engine::Engine;
use asc_engine::commit::{NewChangeset, PathPolicy};
use asc_engine::derive::GitMapRow;
use asc_state::RepoId;
use asc_state::state::git_map;
use asc_state::state::repos::{self, NewRepo, RemoteKind};
use asc_state::state::stage::{Leaf, StageRow};
use asc_store::codec::encode_stored;
use asc_store::key::ObjKey;
use asc_store::objects::Objects;
use asc_store::sqlrefs::SqlRefs;
use asc_store::storeset::StoreSet;
use support::{Fixture, text};

struct Rig {
    f: Fixture,
    engine: Engine<StoreSet, SqlRefs>,
    nested: asc_store::refs::RepoKey,
}

fn sig() -> Signature {
    Signature::new(b"Test".to_vec(), b"test@example.com".to_vec(), 1_700_000_000, 0).unwrap()
}

fn oid(n: u8) -> GitOid {
    GitOid::from_bytes([n; 20])
}

impl Rig {
    async fn new() -> Self {
        let f = Fixture::new().await;
        let nested = repos::insert(
            &f.db,
            &NewRepo {
                id: RepoId::new(),
                name: "asc/soma".into(),
                ns: vec![9; 16],
                remote_url: None,
                remote_kind: RemoteKind::LocalOnly,
                default_branch: "master".into(),
                windows_safe: true,
            },
        )
        .await
        .unwrap();
        let engine = Engine::new(f.objects.clone(), Arc::new(f.db.refs()), nested);
        Self { f, engine, nested }
    }

    /// A commit of the nested repository holding `files`, known to the git map as `git`.
    async fn nested_commit(&self, git: GitOid, files: &[(&str, &[u8])]) -> ChangesetId {
        let mut changes = Vec::new();
        for (path, content) in files {
            let got = ingest(Cursor::new(*content)).unwrap();
            let Body::Inline(bytes) = got.body else { panic!("small files only") };
            self.f
                .objects
                .put(&ObjKey::File(got.file), encode_stored(&bytes))
                .await
                .unwrap();
            changes.push((
                RepoPath::new(*path).unwrap(),
                Change::write(Kind::File, got.file, got.size, got.git, None).unwrap(),
            ));
        }
        let root = self
            .engine
            .apply(Engine::<StoreSet, SqlRefs>::empty_tree_id(), &changes)
            .await
            .unwrap();
        let cs = self
            .engine
            .create_changeset(
                NewChangeset {
                    parents: Vec::new(),
                    author: sig(),
                    committer: sig(),
                    message: b"nested\n".to_vec(),
                    extra: Vec::new(),
                    root,
                    copies: Vec::new(),
                },
                PathPolicy::Native { windows_safe: true },
            )
            .await
            .unwrap();
        git_map::insert_many(
            &self.f.db,
            self.nested,
            &[GitMapRow { git, kind: ObjectKind::Commit, asc: *cs.as_bytes() }],
        )
        .await
        .unwrap();
        cs
    }

    /// Stages a gitlink at `path` pinned to `git`.
    async fn stage_link(&self, path: &str, git: GitOid) {
        let leaf = Leaf { kind: Kind::Gitlink, file: FileId::from_bytes([0; 32]), size: 0, git };
        self.f.stage(&[StageRow::from_leaf(path.as_bytes().to_vec(), Some(&leaf))]).await;
    }

    fn asc(&self, args: &[&str]) -> Output {
        let mut cmd = self.f.asc();
        cmd.args(["-c", "asc.submodule.sdk.repo=asc/soma"]).args(args);
        cmd.output().expect("spawn asc")
    }
}

fn out(o: &Output) -> String {
    text(&o.stdout)
}

#[tokio::test]
async fn status_composes_a_pinned_commit_and_flags_what_is_unavailable() {
    let rig = Rig::new().await;
    rig.nested_commit(oid(1), &[("lib.rs", b"v1")]).await;
    rig.stage_link("sdk", oid(1)).await;
    rig.stage_link("loose", oid(2)).await; // no repository configured for it
    rig.stage_link("gone", oid(3)).await;

    let o = rig.asc(&["-c", "asc.submodule.gone.repo=asc/none", "submodule", "status"]);
    assert!(o.status.success(), "{}", text(&o.stderr));
    let hex1 = "01".repeat(20);
    let hex2 = "02".repeat(20);
    let hex3 = "03".repeat(20);
    assert_eq!(
        out(&o),
        format!("-{hex3} gone (asc/none: commit not available)\n-{hex2} loose\n {hex1} sdk (asc/soma)\n")
    );

    // A pathspec limits the listing; a bare `submodule` is `status`.
    assert_eq!(out(&rig.asc(&["submodule", "status", "sdk"])), format!(" {hex1} sdk (asc/soma)\n"));
    assert!(out(&rig.asc(&["submodule"])).contains("sdk (asc/soma)"));
}

#[tokio::test]
async fn set_pin_stages_a_gitlink_and_reprojects_the_work_tree_read_only() {
    let rig = Rig::new().await;
    rig.nested_commit(oid(1), &[("lib.rs", b"v1")]).await;
    rig.nested_commit(oid(2), &[("lib.rs", b"v2"), ("extra.rs", b"e")]).await;
    rig.stage_link("sdk", oid(1)).await;
    let sdk = rig.f.root.join("sdk");
    fs::create_dir_all(&sdk).unwrap();

    let hex2 = "02".repeat(20);
    let o = rig.asc(&["submodule", "set-pin", "sdk", &hex2]);
    assert!(o.status.success(), "{}", text(&o.stderr));
    assert_eq!(out(&o), format!("{hex2} sdk\n"));
    assert_eq!(fs::read(sdk.join("lib.rs")).unwrap(), b"v2");
    assert!(fs::metadata(sdk.join("lib.rs")).unwrap().permissions().readonly());
    assert!(fs::write(sdk.join("lib.rs"), b"x").is_err(), "a composed file is not writable");
    assert!(out(&rig.asc(&["submodule", "status"])).starts_with(&format!(" {hex2} sdk")));

    // The staged entry is a gitlink carrying the new pin (a `Change::Gitlink` at commit).
    let ws = asc_state::state::workspaces::get(&rig.f.db, rig.f.ws).await.unwrap().unwrap();
    let rows = asc_state::state::stage::overlay(&rig.f.db, ws.ws.id, asc_state::state::stage::StageSet::Default)
        .await
        .unwrap();
    let row = rows.iter().find(|r| r.path == b"sdk").expect("sdk row");
    assert_eq!(row.kind, Kind::Gitlink);
    assert_eq!(row.git, Some(oid(2)));

    // Moving back replaces the composed directory, removing files the old pin lacks.
    let hex1 = "01".repeat(20);
    assert!(rig.asc(&["submodule", "set-pin", "sdk", &hex1]).status.success());
    assert_eq!(fs::read(sdk.join("lib.rs")).unwrap(), b"v1");
    assert!(!sdk.join("extra.rs").exists());
}

#[tokio::test]
async fn set_pin_refuses_what_it_cannot_verify() {
    let rig = Rig::new().await;
    rig.nested_commit(oid(1), &[("lib.rs", b"v1")]).await;
    rig.stage_link("sdk", oid(1)).await;
    let hex9 = "09".repeat(20);

    let unknown = rig.asc(&["submodule", "set-pin", "sdk", &hex9]);
    assert_eq!(unknown.status.code(), Some(128));
    assert!(text(&unknown.stderr).contains("not a commit of asc/soma"));

    let not_link = rig.asc(&["submodule", "set-pin", "README", &"01".repeat(20)]);
    assert_eq!(not_link.status.code(), Some(128));
    assert!(text(&not_link.stderr).contains("no submodule mapping found"));

    let usage = rig.asc(&["submodule", "set-pin", "sdk"]);
    assert_eq!(usage.status.code(), Some(129));

    let name = rig.asc(&["submodule", "set-pin", "sdk", "nosuchbranch"]);
    assert_eq!(name.status.code(), Some(128));

    // Nothing moved.
    assert!(out(&rig.asc(&["submodule", "status"])).starts_with(&format!(" {} sdk", "01".repeat(20))));
}

#[tokio::test]
async fn update_and_init_are_no_ops_with_a_notice() {
    let rig = Rig::new().await;
    rig.stage_link("sdk", oid(1)).await;
    for sub in ["update", "init"] {
        let o = rig.asc(&["submodule", sub, "--init", "--recursive"]);
        assert!(o.status.success(), "{sub}");
        assert!(out(&o).is_empty());
        assert!(text(&o.stderr).contains("nothing to update"));
    }
    assert!(!rig.f.root.join("sdk").exists(), "update must not create or fetch anything");
    let add = rig.asc(&["submodule", "add", "x"]);
    assert_ne!(add.status.code(), Some(0));
}
