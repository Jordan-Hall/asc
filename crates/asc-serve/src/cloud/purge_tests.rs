use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, SystemTime};

use asc_core::id::FileId;
use asc_rpc::IdMode;
use asc_rpc::auth::{Access, BearerToken, Credential, StaticTokens};
use asc_rpc::server::wire::{self, RepoRef};
use asc_rpc::server::{RepoName, ServerInfo};
use asc_rpc::services::cloud_server::Cloud;
use asc_store::gc::GcConfig;
use bytes::Bytes;
use sqlx::Row;
use tonic::Request;

use super::*;
use crate::cloud::Limits;
use crate::cloud::rpc::{CloudRpc, CloudService, register};
use crate::cloud::testutil::{Fx, add_head, cid, count, fx, set_ref, submit};
use crate::gc::GcService;
use crate::http::Metrics;
use crate::ingest::testutil::commit;
use crate::outbox::{Outbox, OutboxConfig};
use crate::registry::Extensions;
use crate::update::PipelineHooks;

const ALICE: &str = "alice-token";
const BOB: &str = "bob-token";
const ADMIN: &str = "admin-token";
const SERVICE_ADMIN: &str = "service-admin-token";
const SERVICE_READ: &str = "service-read-token";

/// A tier that records what it was sent and fails its first `fail` calls.
struct Tier {
    name: &'static str,
    fail: usize,
    calls: AtomicUsize,
    seen: Mutex<Vec<(String, Vec<String>)>>,
}

impl Tier {
    fn new(name: &'static str, fail: usize) -> Arc<Self> {
        Arc::new(Self {
            name,
            fail,
            calls: AtomicUsize::new(0),
            seen: Mutex::default(),
        })
    }
}

#[async_trait]
impl CacheTier for Tier {
    fn name(&self) -> &str {
        self.name
    }

    async fn evict(&self, ns: &str, ids: &[String]) -> Result<(), String> {
        let n = self.calls.fetch_add(1, Ordering::SeqCst);
        if n < self.fail {
            return Err("tier is down".into());
        }
        self.seen
            .lock()
            .unwrap()
            .push((ns.to_owned(), ids.to_vec()));
        Ok(())
    }
}

struct Svc {
    f: Fx,
    svc: CloudService,
    rpc: Arc<CloudRpc>,
}

async fn svc() -> Svc {
    let f = fx().await;
    let ext = Extensions::new();
    register(&ext, &f.reg, Limits::default());
    let rpc = ext.cloud.get().unwrap().clone();
    let auth = StaticTokens::new()
        .with_token(ALICE, "42:alice", Access::Read)
        .with_token(BOB, "43:bob", Access::Read)
        .with_token(ADMIN, "1:root", Access::Admin)
        .with_token(SERVICE_ADMIN, "ci-service", Access::Admin)
        .with_token(SERVICE_READ, "ci-reader", Access::Read);
    let svc = CloudService::new(rpc.clone(), Arc::new(auth), ServerInfo::default());
    Svc { f, svc, rpc }
}

fn req(cs: &[ChangesetId], token: &str) -> Request<wire::PurgeRequest> {
    let mut r = Request::new(wire::PurgeRequest {
        repo: Some(RepoRef {
            name: "o/r".into(),
            id_mode: IdMode::Asc.to_wire(),
        }),
        cs: cs
            .iter()
            .map(|c| Bytes::copy_from_slice(c.as_bytes()))
            .collect(),
    });
    r.extensions_mut()
        .insert(Credential::bearer(BearerToken::new(token).unwrap()));
    r
}

async fn purge(
    s: &Svc,
    cs: &[ChangesetId],
    token: &str,
) -> Result<wire::PurgeResponse, tonic::Status> {
    s.svc.purge(req(cs, token)).await.map(|r| r.into_inner())
}

fn refused_ids(r: &wire::PurgeResponse) -> Vec<Vec<u8>> {
    r.refused.iter().map(|b| b.to_vec()).collect()
}

async fn heads(f: &Fx) -> i64 {
    count(f, "SELECT COUNT(*) FROM cloud_heads").await
}

async fn nothing_was_written(f: &Fx) {
    assert_eq!(count(f, "SELECT COUNT(*) FROM gc_purge").await, 0);
    assert_eq!(count(f, "SELECT COUNT(*) FROM outbox").await, 0);
}

#[tokio::test]
async fn an_owner_purges_their_own_unsubmitted_draft() {
    let s = svc().await;
    let tier = Tier::new("ci-cache", 0);
    s.rpc.cache_tiers().register(tier.clone());
    let cs = s.f.css[4];
    add_head(&s.f, 42, "w", cs, 5, None).await;
    add_head(&s.f, 42, "w", s.f.css[3], 5, None).await;
    let r = purge(&s, &[cs], ALICE).await.unwrap();
    assert_eq!(r.purged, 1);
    assert!(r.refused.is_empty());
    assert_eq!(r.unacked_tiers, vec!["ci-cache".to_owned()]);
    // The head is gone, the workspace count follows and its version moved.
    assert_eq!(heads(&s.f).await, 1);
    let (n, version): (i64, i64) = sqlx::query_as("SELECT heads, version FROM cloud_ws")
        .fetch_one(s.f.reg.pool())
        .await
        .unwrap();
    assert_eq!((n, version), (1, 2));
    // The changeset, its file and its trees are listed for the next sweep.
    let keys: Vec<String> = sqlx::query_scalar("SELECT key FROM gc_purge")
        .fetch_all(s.f.reg.pool())
        .await
        .unwrap();
    for k in [ObjKey::Changeset(cs), ObjKey::File(FileId::of(&[5, 5, 5]))] {
        assert!(keys.contains(&k.path()), "{k:?} in {keys:?}");
    }
    assert!(
        !keys.contains(&ObjKey::Changeset(s.f.css[3]).path()),
        "only the named changeset"
    );
    // The eviction is queued in the same transaction, in its own lane.
    let row = sqlx::query("SELECT repo_id, effects FROM outbox")
        .fetch_one(s.f.reg.pool())
        .await
        .unwrap();
    assert_eq!(row.get::<i64, _>("repo_id"), -s.f.e.key.0);
    let doc: serde_json::Value = serde_json::from_slice(&row.get::<Vec<u8>, _>("effects")).unwrap();
    assert_eq!(doc["effects"][0], EFFECT);
    assert_eq!(doc["ns"], s.f.e.ns.hex());
    assert!(
        doc["ids"]
            .as_array()
            .unwrap()
            .iter()
            .any(|i| *i == ObjKey::Changeset(cs).path())
    );
}

#[tokio::test]
async fn an_owner_cannot_purge_a_submitted_draft() {
    let s = svc().await;
    let cs = s.f.css[2];
    add_head(&s.f, 42, "w", cs, 5, None).await;
    // A version of the same change was submitted under another changeset id.
    submit(&s.f, cid(3), s.f.css[0]).await;
    let r = purge(&s, &[cs], ALICE).await.unwrap();
    assert_eq!(r.purged, 0);
    assert_eq!(refused_ids(&r), vec![cs.as_bytes().to_vec()]);
    assert_eq!(heads(&s.f).await, 1);
    nothing_was_written(&s.f).await;

    // A row naming the changeset itself refuses too.
    let other = svc().await;
    add_head(&other.f, 42, "w", other.f.css[2], 5, None).await;
    submit(&other.f, cid(9), other.f.css[2]).await;
    let r = purge(&other, &[other.f.css[2]], ALICE).await.unwrap();
    assert_eq!(r.purged, 0);
    nothing_was_written(&other.f).await;
}

#[tokio::test]
async fn an_owner_cannot_purge_another_users_draft_and_cannot_tell_it_exists() {
    let s = svc().await;
    let cs = s.f.css[2];
    add_head(&s.f, 42, "w", cs, 5, None).await;
    let theirs = purge(&s, &[cs], BOB).await.unwrap();
    let nothing = purge(&s, &[s.f.css[4]], BOB).await.unwrap();
    assert_eq!((theirs.purged, nothing.purged), (0, 0));
    assert_eq!(theirs.refused.len(), 1);
    assert_eq!(
        nothing.refused.len(),
        1,
        "the same answer for a draft nobody holds"
    );
    assert_eq!(heads(&s.f).await, 1);
    nothing_was_written(&s.f).await;
}

#[tokio::test]
async fn an_owner_cannot_purge_a_draft_a_ref_reaches() {
    let s = svc().await;
    add_head(&s.f, 42, "w", s.f.css[1], 5, None).await;
    add_head(&s.f, 42, "w", s.f.css[4], 5, None).await;
    set_ref(&s.f, "refs/heads/master", Some(s.f.css[4])).await;
    // css[4] is a ref's tip and css[1] is its ancestor: both are shared history.
    let r = purge(&s, &[s.f.css[1], s.f.css[4]], ALICE).await.unwrap();
    assert_eq!(r.purged, 0);
    assert_eq!(r.refused.len(), 2);
    assert_eq!(heads(&s.f).await, 2);
    nothing_was_written(&s.f).await;
    // Once nothing reaches them, the same request works.
    set_ref(&s.f, "refs/heads/master", None).await;
    let r = purge(&s, &[s.f.css[1], s.f.css[4]], ALICE).await.unwrap();
    assert_eq!(r.purged, 2);
    assert_eq!(heads(&s.f).await, 0);
}

#[tokio::test]
async fn an_admin_purges_any_draft_and_a_service_without_admin_or_identity_cannot() {
    let s = svc().await;
    add_head(&s.f, 42, "w", s.f.css[2], 5, None).await;
    add_head(&s.f, 43, "w", s.f.css[2], 5, None).await;
    add_head(&s.f, 42, "w", s.f.css[3], 5, None).await;
    submit(&s.f, cid(3), s.f.css[2]).await;
    set_ref(&s.f, "refs/heads/master", Some(s.f.css[3])).await;

    for t in [SERVICE_READ] {
        let e = purge(&s, &[s.f.css[2]], t).await.unwrap_err();
        assert_eq!(e.code(), tonic::Code::PermissionDenied);
    }
    assert_eq!(heads(&s.f).await, 3);
    nothing_was_written(&s.f).await;

    // An admin identity, and an admin credential with no user identity, purge every owner's
    // heads of a submitted, ref-reachable changeset.
    let r = purge(&s, &[s.f.css[2]], SERVICE_ADMIN).await.unwrap();
    assert_eq!(r.purged, 1);
    assert_eq!(
        heads(&s.f).await,
        1,
        "both owners' heads of css[2] are gone"
    );
    let r = purge(&s, &[s.f.css[3]], ADMIN).await.unwrap();
    assert_eq!(r.purged, 1);
    assert_eq!(heads(&s.f).await, 0);
    // An unknown changeset is refused, not listed.
    let unknown = ChangesetId::from_bytes([7; 32]);
    let r = purge(&s, &[unknown], ADMIN).await.unwrap();
    assert_eq!((r.purged, r.refused.len()), (0, 1));
}

#[tokio::test]
async fn the_request_is_bounded_and_validated() {
    let s = svc().await;
    let many: Vec<ChangesetId> = (0..=MAX_PURGE_CS as u8)
        .map(|i| ChangesetId::from_bytes([i; 32]))
        .collect();
    let e = purge(&s, &many, ADMIN).await.unwrap_err();
    assert_eq!(e.code(), tonic::Code::InvalidArgument);
    let e = purge(&s, &[], ADMIN).await.unwrap_err();
    assert_eq!(e.code(), tonic::Code::InvalidArgument);
    let mut bad = req(&[], ADMIN);
    bad.get_mut().cs = vec![Bytes::from_static(b"short")];
    let e = s.svc.purge(bad).await.unwrap_err();
    assert_eq!(e.code(), tonic::Code::InvalidArgument);
    nothing_was_written(&s.f).await;

    // A closure past the object cap fails the whole request.
    let mut total = MAX_PURGE_KEYS;
    let e = closure(&*s.f.e.objects, s.f.css[0], &mut total)
        .await
        .unwrap_err();
    assert_eq!(e.code(), tonic::Code::ResourceExhausted);
}

#[tokio::test]
async fn a_purged_object_is_swept_at_the_next_run_unless_a_ref_reaches_it() {
    let s = svc().await;
    let f = &s.f;
    let hooks = Arc::new(PipelineHooks::default());
    hooks
        .gc_roots()
        .register(Arc::new(crate::cloud::expiry::CloudRoots::new(
            f.reg.pool().clone(),
        )));
    let gc = GcService::new(f.reg.clone(), hooks, GcConfig::default());
    // master holds the same file content as the draft css[4].
    let shared = commit(&f.e, &[], &[("d/f", &[5, 5, 5])]).await;
    set_ref(f, "refs/heads/master", Some(shared)).await;
    add_head(f, 42, "w", f.css[4], 5, None).await;
    let has = |k: ObjKey| async move { f.e.objects.has_many(&[k]).await.unwrap()[0] };

    purge(&s, &[f.css[4]], ALICE).await.unwrap();
    // One run, no age rule, no second mark.
    let t = SystemTime::now() + Duration::from_secs(60);
    let report = gc
        .run_repo(&RepoName::parse("o/r").unwrap(), t, false)
        .await
        .unwrap();
    assert!(report.sweep.purged >= 1, "{report}");
    assert!(
        !has(ObjKey::Changeset(f.css[4])).await,
        "the purged changeset"
    );
    assert!(
        has(ObjKey::File(FileId::of(&[5, 5, 5]))).await,
        "master still reaches this file"
    );
    assert!(
        has(ObjKey::Changeset(f.css[3])).await,
        "an unpurged parent waits for the two-mark rule"
    );
}

#[tokio::test]
async fn a_finished_sweep_forgets_the_purge_rows_except_those_a_ref_still_reaches() {
    let s = svc().await;
    let f = &s.f;
    let hooks = Arc::new(PipelineHooks::default());
    hooks
        .gc_roots()
        .register(Arc::new(crate::cloud::expiry::CloudRoots::new(
            f.reg.pool().clone(),
        )));
    let gc = GcService::new(f.reg.clone(), hooks, GcConfig::default());
    let name = RepoName::parse("o/r").unwrap();
    let shared = commit(&f.e, &[], &[("d/f", &[5, 5, 5])]).await;
    set_ref(f, "refs/heads/master", Some(shared)).await;
    add_head(f, 42, "w", f.css[4], 5, None).await;
    purge(&s, &[f.css[4]], ALICE).await.unwrap();
    let rows = || async {
        sqlx::query_scalar::<_, String>("SELECT key FROM gc_purge")
            .fetch_all(f.reg.pool())
            .await
            .unwrap()
    };
    let queued = rows().await;
    let file = ObjKey::File(FileId::of(&[5, 5, 5])).path();
    assert!(queued.contains(&file) && queued.contains(&ObjKey::Changeset(f.css[4]).path()));

    // A dry run deletes nothing, so it forgets nothing.
    let t = SystemTime::now() + Duration::from_secs(60);
    gc.run_repo(&name, t, true).await.unwrap();
    assert_eq!(rows().await.len(), queued.len());

    // The first real sweep deletes the changeset and drops its row; the file master reaches
    // stays listed.
    gc.run_repo(&name, t + Duration::from_secs(5), false)
        .await
        .unwrap();
    let left = rows().await;
    assert!(
        !left.contains(&ObjKey::Changeset(f.css[4]).path()),
        "swept: {left:?}"
    );
    assert!(left.contains(&file), "still reached: {left:?}");
    assert!(left.len() < queued.len());
}

#[tokio::test]
async fn an_owner_purge_does_not_list_an_object_another_draft_holds() {
    let s = svc().await;
    let f = &s.f;
    // Bob's draft holds the same file content as Alice's css[4] but no ref reaches it.
    let bobs = commit(&f.e, &[], &[("other/name", &[5, 5, 5])]).await;
    add_head(f, 43, "w", bobs, 5, None).await;
    add_head(f, 42, "w", f.css[4], 5, None).await;
    purge(&s, &[f.css[4]], ALICE).await.unwrap();
    let keys: Vec<String> = sqlx::query_scalar("SELECT key FROM gc_purge")
        .fetch_all(f.reg.pool())
        .await
        .unwrap();
    assert!(
        keys.contains(&ObjKey::Changeset(f.css[4]).path()),
        "{keys:?}"
    );
    assert!(
        !keys.contains(&ObjKey::File(FileId::of(&[5, 5, 5])).path()),
        "the shared file must not be queued: {keys:?}"
    );
    // A GC run keeps it.
    let hooks = Arc::new(PipelineHooks::default());
    hooks
        .gc_roots()
        .register(Arc::new(crate::cloud::expiry::CloudRoots::new(
            f.reg.pool().clone(),
        )));
    let gc = GcService::new(f.reg.clone(), hooks, GcConfig::default());
    let t = SystemTime::now() + Duration::from_secs(60);
    gc.run_repo(&RepoName::parse("o/r").unwrap(), t, false)
        .await
        .unwrap();
    assert!(
        f.e.objects
            .has_many(&[ObjKey::File(FileId::of(&[5, 5, 5]))])
            .await
            .unwrap()[0]
    );
}

#[tokio::test]
async fn cache_evict_is_retried_until_every_tier_acknowledges() {
    let s = svc().await;
    let f = &s.f;
    let flaky = Tier::new("cacheproxy", 2);
    let steady = Tier::new("ci-cache", 0);
    s.rpc.cache_tiers().register(flaky.clone());
    s.rpc.cache_tiers().register(steady.clone());
    let outbox = Outbox::new(
        f.reg.pool().clone(),
        Arc::new(Metrics::new()),
        OutboxConfig::default(),
    );
    outbox.effects().register(s.rpc.cache_evict_effect());
    // An ordinary row of the same repository, queued after the purge.
    add_head(f, 42, "w", f.css[4], 5, None).await;
    let r = purge(&s, &[f.css[4]], ALICE).await.unwrap();
    assert_eq!(
        r.unacked_tiers,
        vec!["cacheproxy".to_owned(), "ci-cache".to_owned()]
    );
    sqlx::query("INSERT INTO outbox (repo_id, op_id, effects, next_at) VALUES ($1, $2, $3, 0)")
        .bind(f.e.key.0)
        .bind(vec![1u8])
        .bind(br#"{"effects":["post_receive"]}"#.to_vec())
        .execute(f.reg.pool())
        .await
        .unwrap();
    let pending = || async {
        count(
            f,
            "SELECT COUNT(*) FROM outbox WHERE done_at IS NULL AND repo_id < 0",
        )
        .await
    };

    // The eviction row is due at the time of the purge (the wall clock).
    let mut at = i64::try_from(
        SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_secs(),
    )
    .unwrap()
        + 1;
    outbox.deliver_due(at).await.unwrap();
    assert_eq!(pending().await, 1, "the down tier has not acknowledged");
    assert_eq!(
        count(
            f,
            "SELECT COUNT(*) FROM outbox WHERE done_at IS NOT NULL AND repo_id > 0"
        )
        .await,
        1,
        "the repository's own rows are not held back by a tier that is down"
    );
    at += 1_000;
    outbox.deliver_due(at).await.unwrap();
    assert_eq!(pending().await, 1, "still down on the second try");
    at += 1_000;
    outbox.deliver_due(at).await.unwrap();
    assert_eq!(pending().await, 0, "acknowledged by every tier");

    assert_eq!(flaky.calls.load(Ordering::SeqCst), 3);
    // Every try reached the healthy tier too; it was sent the same ids each time.
    let seen = steady.seen.lock().unwrap().clone();
    assert_eq!(seen.len(), 3);
    assert!(seen.iter().all(|(ns, ids)| {
        *ns == f.e.ns.hex() && ids.contains(&ObjKey::Changeset(f.css[4]).path())
    }));
    // Nothing is sent once the row is done.
    outbox.deliver_due(at + 10_000).await.unwrap();
    assert_eq!(flaky.calls.load(Ordering::SeqCst), 3);
}

#[tokio::test]
async fn an_eviction_row_with_anything_but_object_paths_reaches_no_tier() {
    let tiers = Arc::new(CacheTiers::default());
    let tier = Tier::new("ci-cache", 0);
    tiers.register(tier.clone());
    let effect = CacheEvict::new(tiers);
    let ns = "00".repeat(16);
    let good = ObjKey::File(FileId::of(b"x")).path();
    for doc in [
        serde_json::json!({ "ns": ns, "ids": ["../../etc/passwd"] }),
        serde_json::json!({ "ns": ns, "ids": [good.clone(), "f/ab/not-hex"] }),
        serde_json::json!({ "ns": "not-hex", "ids": [good.clone()] }),
        serde_json::json!({ "ns": ns, "ids": [] }),
        serde_json::json!({ "ids": [good.clone()] }),
        serde_json::json!({ "ns": ns, "ids": vec![good.clone(); EVICT_IDS_PER_ROW + 1] }),
    ] {
        let job = Job {
            id: 1,
            repo_id: -1,
            op_id: vec![],
            doc,
        };
        assert!(effect.run(&job).await.is_err());
    }
    assert_eq!(tier.calls.load(Ordering::SeqCst), 0);
}
