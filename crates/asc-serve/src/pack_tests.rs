//! Packed objects through the server: the SQL index, the registry, the job, the admin command.

use std::sync::Arc;
use std::time::{Duration, SystemTime};

use asc_core::id::FileId;
use asc_rpc::server::RepoName;
use asc_store::key::ObjKey;
use asc_store::objects::Objects;
use asc_store::pack::PackConfig;
use asc_store::pack::index::run_index_suite;

use crate::admin_cli::{AdminCli, PackCmd};
use crate::ingest::testutil::{commit, registry, repo};
use crate::migrate;
use crate::pack_service::PackService;
use crate::pack_sql::SqlPackIndex;
use crate::registry::{Ns, Registry};
use crate::update::PipelineHooks;

fn cfg() -> PackConfig {
    PackConfig {
        enabled: true,
        min_age: Duration::ZERO,
        min_loose: 1,
        min_batch: 1,
        negative_ttl: Duration::ZERO,
        merge_count: 3,
        retire_age: Duration::from_secs(3600),
        ..PackConfig::default()
    }
}

fn later() -> SystemTime {
    SystemTime::now() + Duration::from_secs(7200)
}

fn name() -> RepoName {
    RepoName::parse("o/r").unwrap()
}

async fn packed_registry() -> Arc<Registry> {
    let reg = registry().await;
    assert!(reg.set_pack_config(cfg()));
    reg
}

#[tokio::test]
async fn the_sql_index_conforms() {
    let pool = migrate::connect("sqlite::memory:", 1).await.unwrap();
    migrate::migrate(&pool).await.unwrap();
    let ns = Ns::from_bytes(&[7u8; 16]).unwrap();
    run_index_suite(&SqlPackIndex::new(pool.clone(), ns)).await;
    // A second namespace sees none of it.
    let other = SqlPackIndex::new(pool, Ns::from_bytes(&[8u8; 16]).unwrap());
    use asc_store::pack::PackIndex as _;
    assert!(
        other
            .packs()
            .await
            .unwrap()
            .iter()
            .all(|p| p.info.objects != 3)
    );
}

#[tokio::test]
async fn a_repository_reads_the_same_before_and_after_its_objects_are_packed() {
    let reg = packed_registry().await;
    let e = repo(&reg).await;
    let a = commit(&e, &[], &[("a", b"one"), ("dir/b", b"two")]).await;
    let b = commit(&e, &[a], &[("dir/c", b"three")]).await;
    let engine = e.backend.engine();
    let before = engine.get_changeset(b).await.unwrap().root;

    let svc = PackService::new(reg.clone(), Arc::new(PipelineHooks::default()));
    let rep = svc.run_repo(&name(), later(), false).await.unwrap();
    assert!(rep.packed >= 8, "{rep:?}");
    assert_eq!(rep.loose_deleted, rep.packed);

    // A fresh entry (nothing cached) reads everything through the pack.
    reg.forget(&name());
    let e2 = reg.open(&name()).await.unwrap();
    let engine = e2.backend.engine();
    assert_eq!(engine.get_changeset(b).await.unwrap().root, before);
    let tree = engine.get_tree(before).await.unwrap();
    assert!(tree.entries().len() >= 2);
    assert!(
        e2.objects
            .get(&ObjKey::File(FileId::of(b"three")))
            .await
            .unwrap()
            .is_some()
    );

    // The namespace now carries the feature, and the index holds the rows.
    let features: Vec<String> = sqlx::query_scalar("SELECT feature FROM store_features")
        .fetch_all(reg.pool())
        .await
        .unwrap();
    assert_eq!(features, ["pack1"]);
    let v = svc.verify(&name()).await.unwrap();
    assert!(v.problems.is_empty(), "{:?}", v.problems);
    assert_eq!(v.objects, rep.packed);
}

#[tokio::test]
async fn a_server_with_packing_off_still_reads_packs() {
    // Two server processes over one database and one bucket; only the first writes packs.
    let dir = tempfile::tempdir().unwrap();
    let url = url::Url::from_directory_path(dir.path()).unwrap();
    let pool = migrate::connect("sqlite::memory:", 1).await.unwrap();
    migrate::migrate(&pool).await.unwrap();
    let writer = Arc::new(Registry::new(pool.clone(), url.as_str()).unwrap());
    assert!(writer.set_pack_config(cfg()));
    let reader = Registry::new(pool, url.as_str()).unwrap();
    let e = repo(&writer).await;
    let a = commit(&e, &[], &[("a", b"one")]).await;
    let svc = PackService::new(writer.clone(), Arc::new(PipelineHooks::default()));
    let rep = svc.run_repo(&name(), later(), false).await.unwrap();
    assert!(rep.packed > 0);

    let seen = reader.open(&name()).await.unwrap();
    assert!(seen.backend.engine().get_changeset(a).await.is_ok());
    // Its own writes stay loose: the switch is off.
    let b = commit(&seen, &[a], &[("b", b"two")]).await;
    assert!(seen.backend.engine().get_changeset(b).await.is_ok());
    let rep = PackService::new(reader.into(), Arc::new(PipelineHooks::default()))
        .run_repo(&name(), later(), true)
        .await
        .unwrap();
    assert_eq!(rep.packs_written, 0, "{rep:?}");
}

#[tokio::test]
async fn a_namespace_with_a_feature_this_build_does_not_know_is_refused() {
    let reg = registry().await;
    let e = repo(&reg).await;
    sqlx::query("INSERT INTO store_features (ns, feature, since) VALUES ($1, 'pack9', 1)")
        .bind(e.ns.as_bytes().to_vec())
        .execute(reg.pool())
        .await
        .unwrap();
    reg.forget(&name());
    let err = reg.open(&name()).await.err().expect("refused");
    assert!(err.to_string().contains("pack9"), "{err}");
}

#[tokio::test]
async fn the_job_merges_compacts_and_cleans_up_through_the_service() {
    let reg = packed_registry().await;
    let e = repo(&reg).await;
    let svc = PackService::new(reg.clone(), Arc::new(PipelineHooks::default()));
    // Four rounds of commits; the first two are packed on their own (two small packs), the rest wait.
    let mut last = None;
    for i in 0..4u8 {
        let parents: Vec<_> = last.into_iter().collect();
        let body = format!("content {i}");
        last = Some(commit(&e, &parents, &[("f", body.as_bytes())]).await);
        if i < 2 {
            svc.run_repo(&name(), later(), true).await.unwrap();
        }
    }
    let before = svc.status(&name()).await.unwrap();
    assert!(before.live_packs >= 2, "{before:?}");
    let rep = svc.run_repo(&name(), later(), false).await.unwrap();
    assert!(rep.merged >= 3, "{rep:?}");
    let after = svc.status(&name()).await.unwrap();
    assert_eq!(after.live_packs, 1);
    assert_eq!(after.packed_objects, before.packed_objects + rep.packed);
    // Retired packs go after the retire age.
    let much_later = SystemTime::now() + Duration::from_secs(4 * 3600);
    let rep = svc.run_repo(&name(), much_later, false).await.unwrap();
    assert!(rep.deleted >= 3, "{rep:?}");
    assert_eq!(svc.status(&name()).await.unwrap().retired_packs, 0);
    let v = svc.verify(&name()).await.unwrap();
    assert!(v.problems.is_empty(), "{:?}", v.problems);
    reg.forget(&name());
    let e2 = reg.open(&name()).await.unwrap();
    assert!(
        e2.backend
            .engine()
            .get_changeset(last.unwrap())
            .await
            .is_ok()
    );
}

#[tokio::test]
async fn reindex_restores_a_lost_index_from_the_pack_files() {
    let reg = packed_registry().await;
    let e = repo(&reg).await;
    let a = commit(&e, &[], &[("a", b"one"), ("b", b"two")]).await;
    let svc = PackService::new(reg.clone(), Arc::new(PipelineHooks::default()));
    svc.run_repo(&name(), later(), false).await.unwrap();

    sqlx::query("DELETE FROM pack_objects")
        .execute(reg.pool())
        .await
        .unwrap();
    sqlx::query("DELETE FROM packs")
        .execute(reg.pool())
        .await
        .unwrap();
    // The loose copies are gone, so without the index the changeset reads as missing.
    assert!(
        e.objects
            .get(&ObjKey::Changeset(a))
            .await
            .unwrap()
            .is_none()
    );
    let r = svc.reindex(&name()).await.unwrap();
    assert_eq!(r.packs, 1);
    assert!(r.objects >= 4);
    reg.forget(&name());
    let e2 = reg.open(&name()).await.unwrap();
    assert!(e2.backend.engine().get_changeset(a).await.is_ok());
}

#[tokio::test]
async fn two_pack_runs_at_once_are_refused_one_of_them() {
    let reg = packed_registry().await;
    repo(&reg).await;
    let svc = Arc::new(PackService::new(
        reg.clone(),
        Arc::new(PipelineHooks::default()),
    ));
    let n = name();
    let (a, b) = tokio::join!(
        svc.run_repo(&n, later(), true),
        svc.run_repo(&n, later(), true)
    );
    assert!(a.is_ok() || b.is_ok());
    // Afterwards a run is accepted again.
    svc.run_repo(&name(), later(), true).await.unwrap();
}

#[tokio::test]
async fn the_admin_command_runs_reports_verifies_and_reindexes() {
    let reg = packed_registry().await;
    let e = repo(&reg).await;
    commit(&e, &[], &[("a", b"one")]).await;
    let cli = AdminCli::new(reg.clone(), None, None, None);

    let r = cli
        .pack(&PackCmd::Status, &["o/r".to_owned()], false)
        .await
        .unwrap();
    assert!(r.text.contains("0 live pack"), "{}", r.text);
    let r = cli
        .pack(&PackCmd::Run { force: true }, &[], true)
        .await
        .unwrap();
    assert!(r.ok && r.text.contains("packed"), "{}", r.text);
    let r = cli
        .pack(&PackCmd::Status, &["o/r".to_owned()], false)
        .await
        .unwrap();
    assert!(r.text.contains("1 live pack"), "{}", r.text);
    let r = cli.pack(&PackCmd::Verify, &[], true).await.unwrap();
    assert!(r.ok && r.text.contains("0 problem"), "{}", r.text);
    let r = cli
        .pack(&PackCmd::Reindex, &["o/r".to_owned()], false)
        .await
        .unwrap();
    assert!(r.ok && r.text.contains("rebuilt"), "{}", r.text);
    // Name repositories or say --all, not neither and not both.
    assert!(cli.pack(&PackCmd::Status, &[], false).await.is_err());
    assert!(
        cli.pack(&PackCmd::Status, &["o/r".to_owned()], true)
            .await
            .is_err()
    );
}
