use std::time::SystemTime;

use asc_core::id::FileId;
use asc_rpc::server::RepoName;
use asc_store::gc::GcConfig;
use asc_store::objects::Objects;

use super::*;
use crate::cloud::testutil::{Fx, add_head, cid, count, fx, land, submit};
use crate::gc::GcService;
use crate::ingest::testutil::commit;
use crate::update::PipelineHooks;

const D: i64 = 86_400;

fn expirer(f: &Fx) -> CloudExpirer {
    CloudExpirer::new(f.reg.pool().clone(), Settings::default())
}

async fn hidden_of(f: &Fx, cs: ChangesetId) -> Option<Option<i64>> {
    sqlx::query_scalar("SELECT hidden_at FROM cloud_heads WHERE ns = $1 AND cs = $2")
        .bind(f.e.ns.as_bytes().to_vec())
        .bind(cs.as_bytes().to_vec())
        .fetch_optional(f.reg.pool())
        .await
        .unwrap()
}

#[tokio::test]
async fn a_hidden_head_is_deleted_only_after_the_grace_period() {
    let f = fx().await;
    let at = 1_000 * D;
    add_head(&f, 42, "w", f.css[0], at - 200 * D, Some(at - 31 * D)).await;
    add_head(&f, 42, "w", f.css[1], at - 200 * D, Some(at - 29 * D)).await;
    add_head(&f, 42, "w", f.css[2], at - 10 * D, None).await;
    let r = expirer(&f).run_once(at).await.unwrap();
    assert_eq!(r.deleted, 1);
    assert_eq!(hidden_of(&f, f.css[0]).await, None, "31 days hidden: gone");
    assert!(hidden_of(&f, f.css[1]).await.is_some(), "29 days: kept");
    assert!(hidden_of(&f, f.css[2]).await.is_some());
    // The workspace counts its rows exactly and moves its version, so clients sync.
    let (heads, version): (i64, i64) =
        sqlx::query_as("SELECT heads, version FROM cloud_ws WHERE owner = 42")
            .fetch_one(f.reg.pool())
            .await
            .unwrap();
    assert_eq!((heads, version), (2, 2));
    // A second run has nothing to do.
    assert_eq!(expirer(&f).run_once(at).await.unwrap().deleted, 0);
}

#[tokio::test]
async fn a_stale_head_with_no_open_pr_is_hidden_and_a_fresh_one_is_not() {
    let f = fx().await;
    let at = 1_000 * D;
    add_head(&f, 42, "w", f.css[0], at - 181 * D, None).await;
    add_head(&f, 42, "w", f.css[1], at - 179 * D, None).await;
    let r = expirer(&f).run_once(at).await.unwrap();
    assert_eq!((r.hidden, r.kept), (1, 0));
    assert_eq!(hidden_of(&f, f.css[0]).await, Some(Some(at)));
    assert_eq!(hidden_of(&f, f.css[1]).await, Some(None));
    // It is deleted only after the grace period that starts now.
    assert_eq!(expirer(&f).run_once(at + 29 * D).await.unwrap().deleted, 0);
    assert_eq!(expirer(&f).run_once(at + 31 * D).await.unwrap().deleted, 1);
}

#[tokio::test]
async fn a_head_with_an_open_pr_anywhere_in_its_stack_is_never_auto_hidden() {
    let f = fx().await;
    let at = 1_000 * D;
    // css[3] is stale; css[1], two parents down, was submitted.
    add_head(&f, 42, "w", f.css[3], at - 400 * D, None).await;
    submit(&f, cid(2), f.css[1]).await;
    let r = expirer(&f).run_once(at).await.unwrap();
    assert_eq!((r.hidden, r.kept), (0, 1));
    assert_eq!(hidden_of(&f, f.css[3]).await, Some(None));
}

#[tokio::test]
async fn the_heads_own_change_with_a_pr_keeps_it_visible() {
    let f = fx().await;
    let at = 1_000 * D;
    add_head(&f, 42, "w", f.css[3], at - 400 * D, None).await;
    submit(&f, cid(4), f.css[3]).await;
    let r = expirer(&f).run_once(at).await.unwrap();
    assert_eq!((r.hidden, r.kept), (0, 1));
}

#[tokio::test]
async fn a_landed_ancestor_is_not_an_open_pr() {
    let f = fx().await;
    let at = 1_000 * D;
    add_head(&f, 42, "w", f.css[3], at - 400 * D, None).await;
    // css[1] was submitted and then landed on master: it ends the stack and holds nothing.
    submit(&f, cid(2), f.css[1]).await;
    land(&f, f.css[1], 0).await;
    let r = expirer(&f).run_once(at).await.unwrap();
    assert_eq!((r.hidden, r.kept), (1, 0));
}

#[tokio::test]
async fn a_rule_set_to_zero_is_off() {
    let f = fx().await;
    let at = 1_000 * D;
    add_head(&f, 42, "w", f.css[0], at - 400 * D, Some(at - 100 * D)).await;
    add_head(&f, 42, "w", f.css[1], at - 400 * D, None).await;
    let off = CloudExpirer::new(
        f.reg.pool().clone(),
        Settings {
            hidden_grace_days: 0,
            abandon_days: 0,
        },
    );
    assert_eq!(off.run_once(at).await.unwrap(), ExpiryReport::default());
    assert_eq!(count(&f, "SELECT COUNT(*) FROM cloud_heads").await, 2);
    assert_eq!(hidden_of(&f, f.css[1]).await, Some(None));
}

#[tokio::test]
async fn old_gc_purge_rows_are_pruned() {
    let f = fx().await;
    let at = 1_000 * D;
    let ns = f.e.ns.as_bytes().to_vec();
    for (k, when) in [("c/aa/old", at - 31 * D), ("c/bb/new", at - 2 * D)] {
        sqlx::query("INSERT INTO gc_purge (ns, key, purged_at) VALUES ($1, $2, $3)")
            .bind(ns.clone())
            .bind(k)
            .bind(when)
            .execute(f.reg.pool())
            .await
            .unwrap();
    }
    assert_eq!(expirer(&f).run_once(at).await.unwrap().pruned, 1);
    assert_eq!(count(&f, "SELECT COUNT(*) FROM gc_purge").await, 1);
}

#[tokio::test]
async fn the_roots_are_every_head_in_grace_and_the_purged_keys() {
    let f = fx().await;
    add_head(&f, 42, "w", f.css[0], 5, None).await;
    add_head(&f, 43, "w", f.css[1], 5, Some(7)).await;
    add_head(&f, 43, "x", f.css[1], 5, None).await;
    let ns = f.e.ns.as_bytes().to_vec();
    for k in [
        ObjKey::File(FileId::of(b"s")).path(),
        "not-a-key".to_owned(),
    ] {
        sqlx::query("INSERT INTO gc_purge (ns, key, purged_at) VALUES ($1, $2, 1)")
            .bind(ns.clone())
            .bind(k)
            .execute(f.reg.pool())
            .await
            .unwrap();
    }
    let roots = CloudRoots::new(f.reg.pool().clone());
    let mut got = roots.roots(&ns).await.unwrap();
    got.sort_by_key(ObjKey::path);
    let mut want = vec![ObjKey::Changeset(f.css[0]), ObjKey::Changeset(f.css[1])];
    want.sort_by_key(ObjKey::path);
    assert_eq!(got, want, "visible and hidden heads, each changeset once");
    assert_eq!(
        roots.purged(&ns).await.unwrap(),
        vec![ObjKey::File(FileId::of(b"s"))],
        "an unparsable key is skipped"
    );
    assert!(
        roots.roots(&[9u8; 16]).await.unwrap().is_empty(),
        "other namespaces"
    );
}

#[tokio::test]
async fn expired_heads_release_their_objects_to_the_two_mark_sweep() {
    let f = fx().await;
    let hooks = Arc::new(PipelineHooks::default());
    hooks
        .gc_roots()
        .register(Arc::new(CloudRoots::new(f.reg.pool().clone())));
    let svc = GcService::new(f.reg.clone(), hooks, GcConfig::default());
    let name = RepoName::parse("o/r").unwrap();
    let wall = now();
    // Two unrelated drafts: one hidden past the grace period, one inside it.
    let old = commit(&f.e, &[], &[("a", b"expired draft")]).await;
    let young = commit(&f.e, &[], &[("b", b"young draft")]).await;
    add_head(&f, 42, "w", old, wall - 200 * D, Some(wall - 31 * D)).await;
    add_head(&f, 42, "w", young, wall - 200 * D, Some(wall - 10 * D)).await;
    let has = |cs: ChangesetId| {
        let f = &f;
        async move {
            f.e.objects
                .has_many(&[ObjKey::Changeset(cs)])
                .await
                .unwrap()[0]
        }
    };
    let hour = Duration::from_secs(25 * 3600);
    let t1 = SystemTime::now() + Duration::from_secs(15 * 86_400);

    // While both rows exist both heads are roots: nothing is collectable.
    svc.run_repo(&name, t1, false).await.unwrap();
    svc.run_repo(&name, t1 + hour, false).await.unwrap();
    assert!(has(old).await && has(young).await);

    // The worker deletes the expired row; its objects then wait for two marks.
    assert_eq!(expirer(&f).run_once(wall).await.unwrap().deleted, 1);
    svc.run_repo(&name, t1 + 2 * hour, false).await.unwrap();
    assert!(has(old).await, "one mark without the root is not enough");
    svc.run_repo(&name, t1 + 3 * hour, false).await.unwrap();
    assert!(!has(old).await, "the expired head's changeset is swept");
    assert!(has(young).await, "a head inside its grace stays a root");
}

#[tokio::test]
async fn deleting_the_roots_is_what_exposes_the_object_a_sweep_without_the_source_would_delete() {
    // Control for the test above: the same draft with no CloudRoots registered is collected
    // although its head row exists, so the roots really are what keeps it.
    let f = fx().await;
    let svc = GcService::new(
        f.reg.clone(),
        Arc::new(PipelineHooks::default()),
        GcConfig::default(),
    );
    let name = RepoName::parse("o/r").unwrap();
    let draft = commit(&f.e, &[], &[("a", b"unrooted draft")]).await;
    add_head(&f, 42, "w", draft, 5, None).await;
    let t1 = SystemTime::now() + Duration::from_secs(15 * 86_400);
    svc.run_repo(&name, t1, false).await.unwrap();
    svc.run_repo(&name, t1 + Duration::from_secs(25 * 3600), false)
        .await
        .unwrap();
    let alive =
        f.e.objects
            .has_many(&[ObjKey::Changeset(draft)])
            .await
            .unwrap()[0];
    assert!(!alive);
}
