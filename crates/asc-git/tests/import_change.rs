//! Git import writes `ChangeMeta` (C45, SPEC §20.2 git-import row).

#[path = "fixtures/mod.rs"]
mod fixtures;

use std::sync::Arc;

use asc_core::change::{ChangeId, ChangeMeta, RewriteOp};
use asc_core::codec::decode_envelope;
use asc_core::git::ObjectKind;
use asc_core::id::{ChangesetId, GitOid};
use asc_engine::Engine;
use asc_engine::change::{ChangeRows, MemRows};
use asc_git::import::{ImportPolicy, Importer, Tip};
use asc_git::odb::Odb;
use asc_store::bucket::BucketObjects;
use asc_store::key::ObjKey;
use asc_store::objects::Objects;
use fixtures::{Killer, MemMap, NoRefs, Repo, repo_key};

const JJ: &str = "kxkxkxkxkxkxkxkxkxkxkxkxkxkxkxkx";
const AUTHOR: &[u8] = b"Au Thor <au@example.com> 1700000000 +0000";

struct Env {
    objects: Arc<BucketObjects>,
    engine: Engine<BucketObjects, NoRefs>,
    map: MemMap,
    rows: MemRows,
}

fn env() -> Env {
    let objects = Arc::new(BucketObjects::memory());
    let engine = Engine::new(objects.clone(), Arc::new(NoRefs), repo_key());
    let map = MemMap::new(Arc::new(Killer::new(usize::MAX)));
    Env {
        objects,
        engine,
        map,
        rows: MemRows::default(),
    }
}

async fn import(env: &Env, repo: &Repo, tips: &[Tip]) {
    let odb = Odb::open(repo.path()).expect("open odb");
    let mut importer = Importer::new(
        &env.engine,
        env.objects.clone(),
        &odb,
        &env.map,
        ImportPolicy::default(),
    )
    .with_change_rows(&env.rows);
    importer.import_commits(tips).await.expect("import");
}

fn cs_of(env: &Env, git: GitOid) -> ChangesetId {
    ChangesetId::from_bytes(
        env.map
            .lookup(git, ObjectKind::Commit)
            .expect("commit is in the git map"),
    )
}

fn tip(name: &str, oid: GitOid) -> Tip {
    Tip {
        name: name.as_bytes().to_vec(),
        oid,
    }
}

fn root(repo: &Repo) -> GitOid {
    let blob = repo.blob(b"x\n");
    repo.tree(&[("100644", b"f", blob)])
}

/// The meta indexed for `git`, read back from the object store and checked against its id.
async fn meta_of(env: &Env, git: GitOid) -> Option<ChangeMeta> {
    let cs = cs_of(env, git);
    let row = env.rows.get(cs).await.expect("rows")?;
    let bytes = env
        .objects
        .get(&ObjKey::ChangeMeta(row.meta))
        .await
        .expect("get")
        .expect("meta object is stored");
    let meta: ChangeMeta = decode_envelope(&bytes).expect("decode");
    assert_eq!(meta.id(), row.meta);
    assert_eq!(meta.cs, cs);
    assert_eq!(meta.op, RewriteOp::Import);
    assert!(meta.preds.is_empty());
    Some(meta)
}

#[tokio::test]
async fn jj_header_becomes_the_change_id_and_the_sha_is_unchanged() {
    let repo = Repo::init();
    let t = root(&repo);
    let header = format!("change-id {JJ}\n");
    let c = repo.commit_raw(t, &[], AUTHOR, header.as_bytes(), b"subject\n");
    repo.set_ref("refs/heads/master", c);
    let e = env();
    import(&e, &repo, &repo.tips()).await;

    let meta = meta_of(&e, c).await.expect("meta");
    assert_eq!(meta.change, JJ.parse::<ChangeId>().unwrap());
    let cs = cs_of(&e, c);
    assert_eq!(e.engine.git_commit_of(cs).await.expect("gitcommit"), c);
}

#[tokio::test]
async fn gerrit_trailer_is_scoped_by_the_first_sorted_ref() {
    let repo = Repo::init();
    let t = root(&repo);
    let msg = b"subject\n\nbody\n\nChange-Id: I0123456789abcdef0123456789abcdef01234567\n";
    let c = repo.commit_raw(t, &[], AUTHOR, b"", msg);
    repo.set_ref("refs/heads/b", c);
    repo.set_ref("refs/heads/a", c);
    let e = env();
    // Tips arrive in "b, a" order; the scope is still the sorted-first ref.
    import(&e, &repo, &[tip("refs/heads/b", c), tip("refs/heads/a", c)]).await;

    let meta = meta_of(&e, c).await.expect("meta");
    let want = ChangeId::from_gerrit(
        b"refs/heads/a",
        b"I0123456789abcdef0123456789abcdef01234567",
    );
    assert_eq!(meta.change, want);
}

#[tokio::test]
async fn ancestors_take_the_scope_of_the_first_ref_that_reaches_them() {
    let repo = Repo::init();
    let t = root(&repo);
    let trailer = |n: u8| format!("s\n\nChange-Id: I{n:040}\n");
    let base = repo.commit_raw(t, &[], AUTHOR, b"", trailer(1).as_bytes());
    let top = repo.commit_raw(t, &[base], AUTHOR, b"", trailer(2).as_bytes());
    let e = env();
    import(
        &e,
        &repo,
        &[tip("refs/heads/z", base), tip("refs/heads/a", top)],
    )
    .await;

    // `a` sorts first and reaches both commits.
    let id = |n: u8| ChangeId::from_gerrit(b"refs/heads/a", format!("I{n:040}").as_bytes());
    assert_eq!(meta_of(&e, base).await.expect("base").change, id(1));
    assert_eq!(meta_of(&e, top).await.expect("top").change, id(2));
}

#[tokio::test]
async fn commits_without_an_id_get_no_meta() {
    let repo = Repo::init();
    let t = root(&repo);
    let plain = repo.commit(t, &[], "just a subject\n");
    let subject_only = repo.commit_raw(t, &[plain], AUTHOR, b"", b"Change-Id: Iab\n");
    let bad_jj = repo.commit_raw(t, &[subject_only], AUTHOR, b"change-id zzz\n", b"s\n");
    let e = env();
    import(&e, &repo, &[tip("refs/heads/master", bad_jj)]).await;
    for c in [plain, subject_only, bad_jj] {
        assert!(meta_of(&e, c).await.is_none());
    }
}

#[tokio::test]
async fn rerun_is_idempotent_and_keeps_the_first_scope() {
    let repo = Repo::init();
    let t = root(&repo);
    let c = repo.commit_raw(t, &[], AUTHOR, b"", b"s\n\nChange-Id: Iabc\n");
    let e = env();
    import(&e, &repo, &[tip("refs/heads/m", c)]).await;
    let first = meta_of(&e, c).await.expect("meta");
    // The commit is already in the git map, so a later import under another ref skips it.
    import(&e, &repo, &[tip("refs/heads/a", c)]).await;
    assert_eq!(meta_of(&e, c).await.expect("meta"), first);
    assert_eq!(
        first.change,
        ChangeId::from_gerrit(b"refs/heads/m", b"Iabc")
    );
}
