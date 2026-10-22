//! Git import against real repositories (U33, SPEC §14.1).

#[path = "fixtures/mod.rs"]
mod fixtures;

use std::collections::BTreeMap;
use std::sync::Arc;

use asc_core::git::ObjectKind;
use asc_core::id::{ChangesetId, FileId, GitOid, TreeId};
use asc_core::object::{AscGitExtra, Kind, SortedMap};
use asc_core::path::RepoPath;
use asc_engine::Engine;
use asc_git::import::{ImportError, ImportPolicy, ImportReport, Importer, SkippedRef, Tip};
use asc_git::odb::Odb;
use asc_store::bucket::BucketObjects;
use asc_store::key::ObjKey;
use asc_store::objects::Objects;
use data_encoding::BASE64;
use fixtures::{Killer, MemMap, NoRefs, Repo, main_fixture, repo_key};

struct Env {
    objects: Arc<BucketObjects>,
    engine: Engine<BucketObjects, NoRefs>,
    map: MemMap,
}

fn env() -> Env {
    let objects = Arc::new(BucketObjects::memory());
    let engine = Engine::new(objects.clone(), Arc::new(NoRefs), repo_key());
    let map = MemMap::new(Arc::new(Killer::new(usize::MAX)));
    Env {
        objects,
        engine,
        map,
    }
}

async fn import(env: &Env, repo: &Repo, tips: &[Tip]) -> Result<ImportReport, ImportError> {
    let odb = Odb::open(repo.path()).expect("open odb");
    let mut importer = Importer::new(
        &env.engine,
        env.objects.clone(),
        &odb,
        &env.map,
        ImportPolicy::default(),
    );
    importer.import_commits(tips).await
}

fn cs_of(env: &Env, git: GitOid) -> ChangesetId {
    let asc = env
        .map
        .lookup(git, ObjectKind::Commit)
        .unwrap_or_else(|| panic!("commit {git} is not in the git map"));
    ChangesetId::from_bytes(asc)
}

/// Every non-directory leaf of an asc tree as `path -> (kind, git oid)`.
async fn flatten(env: &Env, root: TreeId) -> BTreeMap<Vec<u8>, (Kind, GitOid)> {
    let mut out = BTreeMap::new();
    let mut stack = vec![(Vec::new(), root)];
    while let Some((prefix, id)) = stack.pop() {
        let tree = env.engine.get_tree(id).await.expect("tree");
        for e in tree.entries() {
            let mut path = prefix.clone();
            if !path.is_empty() {
                path.push(b'/');
            }
            path.extend_from_slice(e.name.as_bytes());
            if e.kind == Kind::Dir {
                stack.push((path, TreeId::from_bytes(e.id)));
            } else {
                out.insert(path, (e.kind, e.git));
            }
        }
    }
    out
}

#[tokio::test]
async fn full_fixture_round_trips() {
    let fx = main_fixture();
    let e = env();
    let report = import(&e, &fx.repo, &fx.repo.tips()).await.expect("import");

    let commits = fx.repo.rev_list_all();
    assert_eq!(report.commits, commits.len());
    for &oid in &commits {
        let id = cs_of(&e, oid);
        assert_eq!(e.engine.git_commit_of(id).await.expect("gitcommit"), oid);
        let cs = e.engine.get_changeset(id).await.expect("changeset");
        let want: BTreeMap<_, _> = fx.repo.ls_tree(oid).into_iter().collect();
        let got: BTreeMap<_, _> = flatten(&e, cs.root)
            .await
            .into_iter()
            .map(|(p, (_, g))| (p, g))
            .collect();
        assert_eq!(got, want, "tree of {oid}");
    }

    // Two non-canonical trees: the root of one commit and a subtree of another.
    assert_eq!(report.overrides, 2);
    let raw_root = fx.repo.git(
        &["rev-parse", &format!("{}^{{tree}}", fx.unsorted_root)],
        b"",
    );
    let root_oid: GitOid = String::from_utf8(raw_root)
        .expect("utf8")
        .trim()
        .parse()
        .expect("oid");
    let over = e
        .objects
        .get(&ObjKey::GitTreeOverride(root_oid))
        .await
        .expect("get")
        .expect("override stored");
    assert_eq!(
        over.to_vec(),
        fx.repo
            .git(&["cat-file", "tree", &root_oid.to_string()], b"")
    );

    // Signature kept, non-canonical commit kept raw, octopus has three parents.
    let signed = e
        .engine
        .get_changeset(cs_of(&e, fx.signed))
        .await
        .expect("cs");
    assert!(signed.extra.get(&b"git.gpgsig".to_vec()).is_some());
    assert!(signed.extra.get(&b"git.commit".to_vec()).is_none());
    let latin = e
        .engine
        .get_changeset(cs_of(&e, fx.non_utf8))
        .await
        .expect("cs");
    assert!(latin.extra.get(&b"git.commit".to_vec()).is_some());
    let oct = e
        .engine
        .get_changeset(cs_of(&e, fx.octopus))
        .await
        .expect("cs");
    assert_eq!(oct.parents.len(), 3);

    // LFS pointers are plain blobs; the gitlink has no asc object; a big file is chunked.
    let first = e
        .engine
        .get_changeset(cs_of(&e, fx.with_gitlink))
        .await
        .expect("cs");
    let flat = flatten(&e, first.root).await;
    assert_eq!(flat[&b"vendor".to_vec()].0, Kind::Gitlink);
    let tree = e.engine.get_tree(first.root).await.expect("tree");
    let lfs = tree
        .entries()
        .iter()
        .find(|x| x.name.as_bytes() == b"lfs.bin")
        .expect("lfs");
    let pointer = e
        .objects
        .get(&ObjKey::File(FileId::from_bytes(lfs.id)))
        .await
        .expect("get")
        .expect("pointer stored as a file");
    assert!(pointer.starts_with(b"version https://git-lfs.github.com/spec/v1"));
    let head = e
        .engine
        .get_changeset(cs_of(&e, fx.head))
        .await
        .expect("cs");
    let tree = e.engine.get_tree(head.root).await.expect("tree");
    let big = tree
        .entries()
        .iter()
        .find(|x| x.name.as_bytes() == b"big.bin")
        .expect("big");
    assert!(
        e.objects
            .get(&ObjKey::ChunkList(FileId::from_bytes(big.id)))
            .await
            .expect("get")
            .is_some()
    );

    // Tags: one record; tags of a tree, a blob and a tag are skipped with the warning.
    assert_eq!(report.tags.len(), 1);
    assert_eq!(report.tags[0].0, b"refs/tags/v1");
    let mut skipped: Vec<(String, &str)> = report
        .skipped_refs
        .iter()
        .map(|s| (String::from_utf8_lossy(&s.name).into_owned(), s.target))
        .collect();
    skipped.sort();
    assert_eq!(
        skipped,
        vec![
            ("refs/tags/of-blob".to_owned(), "blob"),
            ("refs/tags/of-tag".to_owned(), "tag"),
            ("refs/tags/of-tree".to_owned(), "tree"),
        ]
    );
    let of_tree: &SkippedRef = report
        .skipped_refs
        .iter()
        .find(|s| s.name == b"refs/tags/of-tree")
        .expect("of-tree");
    assert_eq!(
        of_tree.warning(),
        "warning: asc: skipped tag 'refs/tags/of-tree' (points to a tree)"
    );
    assert!(report.rejected_tags.is_empty());
    assert!(
        report
            .refs
            .iter()
            .any(|(n, id)| n == b"refs/heads/master" && *id == cs_of(&e, fx.head))
    );
    assert!(
        report
            .refs
            .iter()
            .any(|(n, id)| n == b"refs/tags/v1" && *id == cs_of(&e, fx.head))
    );
}

#[tokio::test]
async fn second_import_does_nothing() {
    let fx = main_fixture();
    let e = env();
    let tips = fx.repo.tips();
    import(&e, &fx.repo, &tips).await.expect("first");
    let before = (fixtures::dump_store(&*e.objects).await, e.map.dump());
    let again = import(&e, &fx.repo, &tips).await.expect("second");
    assert_eq!((again.commits, again.blobs, again.trees), (0, 0, 0));
    assert!(again.refs.iter().any(|(n, _)| n == b"refs/heads/master"));
    assert_eq!(fixtures::dump_store(&*e.objects).await, before.0);
    assert_eq!(e.map.dump(), before.1);
}

#[tokio::test]
async fn incremental_import_adds_only_new_commits() {
    let fx = main_fixture();
    let e = env();
    let all = fx.repo.tips();
    let side: Vec<Tip> = all
        .iter()
        .filter(|t| t.name == b"refs/heads/side")
        .cloned()
        .collect();
    let first = import(&e, &fx.repo, &side).await.expect("side");
    assert_eq!(first.commits, 2);
    let rest = import(&e, &fx.repo, &all).await.expect("rest");
    assert_eq!(first.commits + rest.commits, fx.repo.rev_list_all().len());
}

#[tokio::test]
async fn unknown_asc_header_version_asks_for_an_upgrade() {
    let r = Repo::init();
    let blob = r.blob(b"x\n");
    let tree = r.tree(&[("100644", b"f", blob)]);
    let mut payload = vec![2u8];
    payload.extend_from_slice(b"whatever");
    let header = format!("asc {}\n", BASE64.encode(&payload));
    let c = r.commit_raw(
        tree,
        &[],
        b"A <a@x> 1700000000 +0000",
        header.as_bytes(),
        b"m\n",
    );
    r.set_ref("refs/heads/master", c);
    let e = env();
    let err = import(&e, &r, &r.tips()).await.expect_err("must fail");
    assert!(matches!(err, ImportError::UpgradeAsc(_, 2)), "{err}");
    assert!(err.to_string().contains("upgrade asc"));
    assert!(e.map.dump().is_empty() || e.map.lookup(c, ObjectKind::Commit).is_none());
}

#[tokio::test]
async fn asc_header_copy_without_a_write_is_rejected() {
    let r = Repo::init();
    let blob = r.blob(b"x\n");
    let tree = r.tree(&[("100644", b"f", blob)]);
    let extra = AscGitExtra {
        copies: SortedMap::new(vec![(
            RepoPath::new("ghost").expect("path"),
            RepoPath::new("f").expect("path"),
        )])
        .expect("sorted"),
        snapshot: false,
    };
    let header = format!("asc {}\n", BASE64.encode(&extra.encode()));
    let c = r.commit_raw(
        tree,
        &[],
        b"A <a@x> 1700000000 +0000",
        header.as_bytes(),
        b"m\n",
    );
    r.set_ref("refs/heads/master", c);
    let e = env();
    let err = import(&e, &r, &r.tips()).await.expect_err("must fail");
    assert!(matches!(err, ImportError::RoundTrip { .. }), "{err}");
    assert!(e.map.lookup(c, ObjectKind::Commit).is_none());
}

#[tokio::test]
async fn empty_directory_is_reported() {
    let r = Repo::init();
    let empty = r.tree(&[]);
    let tree = r.tree(&[("40000", b"e", empty)]);
    let c = r.commit(tree, &[], "m\n");
    r.set_ref("refs/heads/master", c);
    let e = env();
    let err = import(&e, &r, &r.tips()).await.expect_err("must fail");
    assert!(matches!(err, ImportError::EmptyDir(_)), "{err}");
}
