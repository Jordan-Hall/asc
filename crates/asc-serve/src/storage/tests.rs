//! Bring-your-own storage against an in-process S3 (`fake_s3`): the service, the routing into the
//! registry, migrations, health, the guard rules and the secrecy of credentials.

mod rpc_e2e;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use asc_rpc::auth::Principal;
use asc_rpc::server::RepoName;
use asc_store::key::ObjKey;
use asc_store::objects::Objects;
use bytes::Bytes;
use sqlx::AnyPool;

use super::audit::MemoryAudit;
use super::authority::{Caller, fake::Table};
use super::fake_s3::FakeS3;
use super::live::Step;
use super::migrate::{Event, state};
use super::model::{Addressing, Credentials, Owner, OwnerKind, State};
use super::service::{NewFields, Patch, Service, StorageConfig, StorageError, View};
use super::{Byos, Parts, register};
use crate::netguard::{BlockReason, Guard, NetGuardConfig, StaticResolver};
use crate::registry::{Extensions, NewRepo, Registry, RepoEntry};
use crate::vault::Vault;

const AK: &str = "AKIATESTKEY0123456789";
const SK: &str = "s3cr3t-PLANTED-9f3a7c1e5b";
const BUCKET: &str = "byos-test";

fn caller(id: &str) -> Caller {
    Caller {
        principal: Principal { id: id.into() },
        credential: None,
        forwarded: Default::default(),
    }
}

fn alice() -> Caller {
    caller("7:alice")
}

fn bob() -> Caller {
    caller("8:bob")
}

fn owner_of(name: &str) -> Owner {
    Owner::new(OwnerKind::User, name).unwrap()
}

#[derive(Clone)]
struct Opts {
    loopback: bool,
    settle: u64,
    repin: u64,
    max_targets: u32,
}

impl Default for Opts {
    fn default() -> Self {
        Self {
            loopback: true,
            settle: 0,
            repin: 300,
            max_targets: 64,
        }
    }
}

struct Fx {
    pool: AnyPool,
    registry: Arc<Registry>,
    byos: Arc<Byos>,
    svc: Arc<Service>,
    vault: Arc<Vault>,
    s3: FakeS3,
    audit: Arc<MemoryAudit>,
    table: Arc<Table>,
    resolver: Arc<StaticResolver>,
    _dir: tempfile::TempDir,
}

async fn fx() -> Fx {
    fx_with(Opts::default()).await
}

async fn fx_with(o: Opts) -> Fx {
    let pool = crate::migrate::connect("sqlite::memory:", 1).await.unwrap();
    crate::migrate::migrate(&pool).await.unwrap();
    let dir = tempfile::tempdir().unwrap();
    let vault = Vault::open_file(dir.path().join("vault.keys"), pool.clone());
    let registry = Arc::new(
        Registry::new(pool.clone(), "memory:///")
            .unwrap()
            .with_entry_ttl(Duration::ZERO),
    );
    let resolver = Arc::new(StaticResolver::default());
    let guard = Guard::with_resolver(
        NetGuardConfig {
            allow_loopback_http: o.loopback,
            ..NetGuardConfig::default()
        },
        resolver.clone(),
    );
    let audit = Arc::new(MemoryAudit::default());
    let table = Arc::new(Table::default());
    table.grant_owner("7:alice", &owner_of("alice"));
    table.grant_owner("8:bob", &owner_of("bob"));
    let ext = Extensions::new();
    let byos = register(
        &ext,
        &registry,
        Parts {
            vault: vault.clone(),
            guard,
            cfg: StorageConfig {
                max_targets_per_owner: o.max_targets,
                migration_settle_secs: o.settle,
                repin_secs: o.repin,
                health_interval_secs: 0,
                ..StorageConfig::default()
            },
            authorizer: Arc::new(crate::DenyAll),
            audit: audit.clone(),
            authority: Some(table.clone()),
        },
    );
    let s3 = FakeS3::start(BUCKET, AK, SK).await;
    Fx {
        pool,
        registry,
        svc: byos.service.clone(),
        byos,
        vault,
        s3,
        audit,
        table,
        resolver,
        _dir: dir,
    }
}

impl Fx {
    fn fields(&self, name: &str) -> NewFields {
        NewFields {
            owner: owner_of("alice"),
            name: name.into(),
            endpoint: self.s3.endpoint(),
            region: "us-east-1".into(),
            bucket: BUCKET.into(),
            prefix: "pre/".into(),
            addressing: Addressing::Path,
            credentials: Credentials::new(AK, SK, "").unwrap(),
        }
    }

    async fn create(&self, name: &str) -> View {
        self.svc
            .create_target(&alice(), self.fields(name))
            .await
            .unwrap()
    }

    async fn verified(&self, name: &str) -> View {
        let v = self.create(name).await;
        let (report, v) = self
            .svc
            .verify_target(&alice(), &v.target.id)
            .await
            .unwrap();
        assert!(report.ok(), "{:?}", report.checks);
        v
    }

    async fn enabled(&self, name: &str) -> View {
        let v = self.verified(name).await;
        self.svc
            .set_enabled(&alice(), &v.target.id, true)
            .await
            .unwrap()
    }

    /// An enabled target under its own prefix, so two of them are two places.
    async fn enabled_at(&self, name: &str, prefix: &str) -> View {
        let mut x = self.fields(name);
        x.prefix = prefix.into();
        let v = self.svc.create_target(&alice(), x).await.unwrap();
        let (report, v) = self
            .svc
            .verify_target(&alice(), &v.target.id)
            .await
            .unwrap();
        assert!(report.ok(), "{:?}", report.checks);
        self.svc
            .set_enabled(&alice(), &v.target.id, true)
            .await
            .unwrap()
    }

    async fn repo(&self, name: &str) -> Arc<RepoEntry> {
        let e = self
            .registry
            .create(&RepoName::parse(name).unwrap(), &NewRepo::default())
            .await
            .unwrap();
        self.table.grant_repo("7:alice", name);
        e
    }

    async fn open(&self, name: &str) -> Result<Arc<RepoEntry>, crate::registry::RegistryError> {
        self.registry.open(&RepoName::parse(name).unwrap()).await
    }

    /// Runs the migration engine until nothing is active.
    async fn drain(&self) {
        for _ in 0..600 {
            self.byos.engine.tick().await;
            self.byos.engine.wait_idle().await;
            if self.svc.db.count_active_migrations().await.unwrap() == 0 {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("migrations did not finish");
    }
}

async fn put_obj(e: &RepoEntry, body: &[u8]) -> ObjKey {
    let key = ObjKey::File(asc_core::id::FileId::of(body));
    e.objects
        .put(&key, asc_store::codec::encode_stored(body))
        .await
        .unwrap();
    key
}

fn body(i: usize) -> Vec<u8> {
    format!(
        "object number {i} of the migration test, padded {}",
        "x".repeat(i % 17)
    )
    .into_bytes()
}

// ------------------------------------------------------------------------------ targets

#[tokio::test]
async fn create_returns_a_pending_target_that_shows_a_hint_and_never_the_secret() {
    let f = fx().await;
    let v = f.create("main").await;
    let t = &v.target;
    assert!(super::model::is_id("st_", &t.id));
    assert_eq!(
        (t.state, t.name.as_str(), t.prefix.as_str()),
        (State::Pending, "main", "pre")
    );
    assert_eq!(t.access_key_hint, "AKIA...6789");
    assert_eq!(t.endpoint, f.s3.endpoint());
    assert_eq!((v.repos, t.verified_at), (0, 0));
    let shown = format!("{v:?}");
    assert!(!shown.contains(SK) && !shown.contains(AK));
    // At rest the credentials are sealed: no plaintext in the row, and it opens only for this row.
    let sealed = f.svc.db.sealed(&t.id).await.unwrap().unwrap();
    for needle in [SK.as_bytes(), AK.as_bytes()] {
        assert!(!sealed.windows(needle.len()).any(|w| w == needle));
    }
    assert_eq!(crate::vault::crypto::generation_of(&sealed), Some(1));
    let scope = t.owner.scope();
    assert!(
        f.vault
            .open(&super::model::secret_context(&scope, &t.id), &sealed)
            .is_ok()
    );
    assert!(
        f.vault
            .open(&super::model::secret_context(&scope, "st_other"), &sealed)
            .is_err()
    );
    assert!(
        f.vault
            .open(&super::model::secret_context("user:bob", &t.id), &sealed)
            .is_err()
    );
    assert_eq!(f.audit.actions(), vec!["storage.target.created"]);
    assert_eq!(f.audit.events()[0].actor, "7:alice");
}

#[tokio::test]
async fn field_rules_are_enforced_before_anything_is_stored() {
    let f = fx().await;
    let bad = |mutate: &dyn Fn(&mut NewFields)| {
        let mut x = f.fields("ok");
        mutate(&mut x);
        x
    };
    for x in [
        bad(&|x| x.name = "Bad_Name".into()),
        bad(&|x| x.name = String::new()),
        bad(&|x| x.bucket = "a".into()),
        bad(&|x| x.region = "no spaces".into()),
        bad(&|x| x.prefix = "/abs".into()),
        bad(&|x| x.prefix = "a/../b".into()),
        bad(&|x| x.prefix = ".asc-probe".into()),
    ] {
        let err = f.svc.create_target(&alice(), x).await.unwrap_err();
        assert!(matches!(err, StorageError::InvalidArgument(_)), "{err:?}");
    }
    assert_eq!(f.svc.db.count_targets(&owner_of("alice")).await.unwrap(), 0);
    // An empty region means us-east-1; a trailing slash on the prefix is dropped.
    let v = f
        .svc
        .create_target(
            &alice(),
            NewFields {
                region: String::new(),
                prefix: "a/b/".into(),
                ..f.fields("fine")
            },
        )
        .await
        .unwrap();
    assert_eq!(
        (v.target.region.as_str(), v.target.prefix.as_str()),
        ("us-east-1", "a/b")
    );
    // A name is unique per owner.
    let dup = f
        .svc
        .create_target(&alice(), f.fields("fine"))
        .await
        .unwrap_err();
    assert!(matches!(dup, StorageError::AlreadyExists(_)));
    // A repository owner has to exist.
    let ghost = NewFields {
        owner: Owner::new(OwnerKind::Repo, "acme/ghost").unwrap(),
        ..f.fields("r")
    };
    f.table.grant_repo("7:alice", "acme/ghost");
    assert!(matches!(
        f.svc.create_target(&alice(), ghost).await.unwrap_err(),
        StorageError::NotFound(_)
    ));
}

#[tokio::test]
async fn an_owner_may_hold_only_so_many_targets() {
    let f = fx_with(Opts {
        max_targets: 2,
        ..Opts::default()
    })
    .await;
    f.create("one").await;
    f.create("two").await;
    let err = f
        .svc
        .create_target(&alice(), f.fields("three"))
        .await
        .unwrap_err();
    assert!(matches!(err, StorageError::Quota(_)), "{err:?}");
    // Another owner has its own allowance.
    let mut b = f.fields("one");
    b.owner = owner_of("bob");
    f.svc.create_target(&bob(), b).await.unwrap();
}

#[tokio::test]
async fn user_owned_targets_can_be_switched_off() {
    let f = fx().await;
    let svc = Service {
        cfg: StorageConfig {
            allow_user_targets: false,
            ..f.svc.cfg.clone()
        },
        db: f.svc.db.clone(),
        vault: f.svc.vault.clone(),
        guard: f.svc.guard.clone(),
        router: f.svc.router.clone(),
        engine: f.svc.engine.clone(),
        audit: f.svc.audit.clone(),
        authority: f.svc.authority.clone(),
    };
    let err = svc
        .create_target(&alice(), f.fields("x"))
        .await
        .unwrap_err();
    assert!(matches!(err, StorageError::PermissionDenied(_)), "{err:?}");
    // An organisation still can: the host vouches for it.
    f.table
        .grant_owner("7:alice", &Owner::new(OwnerKind::Org, "acme").unwrap());
    let mut org = f.fields("org");
    org.owner = Owner::new(OwnerKind::Org, "acme").unwrap();
    assert!(svc.create_target(&alice(), org).await.is_ok());
}

#[tokio::test]
async fn other_callers_get_not_found_for_ids_and_denied_for_owners() {
    let f = fx().await;
    let v = f.create("main").await;
    let id = v.target.id.clone();
    // bob names alice as the owner.
    let mut x = f.fields("sneaky");
    x.owner = owner_of("alice");
    assert!(matches!(
        f.svc.create_target(&bob(), x).await.unwrap_err(),
        StorageError::PermissionDenied(_)
    ));
    assert!(matches!(
        f.svc
            .list_targets(&bob(), &owner_of("alice"))
            .await
            .unwrap_err(),
        StorageError::PermissionDenied(_)
    ));
    let nf = |e: StorageError| assert!(matches!(e, StorageError::NotFound(_)), "{e:?}");
    nf(f.svc.get_target(&bob(), &id).await.unwrap_err());
    nf(f.svc
        .update_target(&bob(), &id, Patch::default())
        .await
        .unwrap_err());
    nf(f.svc.delete_target(&bob(), &id).await.unwrap_err());
    nf(f.svc
        .verify_target(&bob(), &id)
        .await
        .map(|_| ())
        .unwrap_err());
    nf(f.svc.set_enabled(&bob(), &id, true).await.unwrap_err());
    nf(f.svc
        .get_target(&alice(), "st_doesnotexistdoesnot")
        .await
        .unwrap_err());
    nf(f.svc.get_target(&alice(), "garbage").await.unwrap_err());
    // The same answer for a real id and a made-up one: ids cannot be probed.
    assert_eq!(
        format!("{:?}", f.svc.get_target(&bob(), &id).await.unwrap_err()),
        format!(
            "{:?}",
            f.svc
                .get_target(&bob(), "st_aaaaaaaaaaaaaaaaaaaa")
                .await
                .unwrap_err()
        )
    );
    assert_eq!(
        f.svc
            .list_targets(&alice(), &owner_of("alice"))
            .await
            .unwrap()
            .len(),
        1
    );
}

#[tokio::test]
async fn verification_writes_reads_and_deletes_a_probe_with_the_targets_own_key() {
    let f = fx().await;
    let v = f.create("main").await;
    let (report, after) = f.svc.verify_target(&alice(), &v.target.id).await.unwrap();
    assert!(report.ok(), "{:?}", report.checks);
    let steps: Vec<Step> = report.checks.iter().map(|c| c.step).collect();
    assert_eq!(
        steps,
        vec![
            Step::Resolve,
            Step::Connect,
            Step::Put,
            Step::Get,
            Step::Delete
        ]
    );
    assert_eq!(after.target.state, State::Verified);
    assert!(after.target.verified_at > 0 && after.target.last_error.is_empty());
    assert!(
        f.s3.keys("pre/.asc-probe/").is_empty(),
        "the probe object is deleted"
    );
    let seen = f.s3.requests();
    let methods: Vec<&str> = seen
        .iter()
        .filter(|r| r.path.contains(".asc-probe"))
        .map(|r| r.method.as_str())
        .collect();
    assert_eq!(methods, vec!["PUT", "GET"]);
    // The delete goes out as the provider's bulk-delete call.
    assert_eq!(seen.last().map(|r| r.method.as_str()), Some("POST"));
    // Every signed request used the target's key and no other.
    assert!(
        seen.iter()
            .filter(|r| r.key_id.is_some())
            .all(|r| r.key_id.as_deref() == Some(AK))
    );
    // Enabling needs the verification.
    let fresh = f.create("second").await;
    assert!(matches!(
        f.svc
            .set_enabled(&alice(), &fresh.target.id, true)
            .await
            .unwrap_err(),
        StorageError::VerifyFailed(_)
    ));
    let on = f
        .svc
        .set_enabled(&alice(), &v.target.id, true)
        .await
        .unwrap();
    assert_eq!(on.target.state, State::Enabled);
    let again = f
        .svc
        .set_enabled(&alice(), &v.target.id, true)
        .await
        .unwrap();
    assert_eq!(again.target.state, State::Enabled, "idempotent");
    let off = f
        .svc
        .set_enabled(&alice(), &v.target.id, false)
        .await
        .unwrap();
    assert_eq!(off.target.state, State::Disabled);
    let back = f
        .svc
        .set_enabled(&alice(), &v.target.id, true)
        .await
        .unwrap();
    assert_eq!(
        back.target.state,
        State::Enabled,
        "a verified target comes back without a new verification"
    );
}

async fn failing_step(f: &Fx, id: &str) -> (Step, String, View) {
    let (report, v) = f.svc.verify_target(&alice(), id).await.unwrap();
    assert!(!report.ok());
    let bad = report.checks.iter().find(|c| !c.ok).unwrap().clone();
    // Steps after the failing one did not run (except the cleanup delete after a put).
    assert!(report.checks.last().unwrap().step == bad.step || bad.step == Step::Get);
    assert_eq!(v.target.state, State::Failed);
    assert_eq!(
        v.target.last_error,
        format!("{}: {}", super::live::step_name(bad.step), bad.message)
    );
    (bad.step, bad.message, v)
}

#[tokio::test]
async fn each_failing_step_of_the_handshake_is_reported_with_a_fixed_phrase() {
    // Wrong secret: the provider rejects the signature at the first write.
    let f = fx().await;
    let mut x = f.fields("badkey");
    x.credentials = Credentials::new(AK, "not-the-secret", "").unwrap();
    let v = f.svc.create_target(&alice(), x).await.unwrap();
    let (step, msg, v) = failing_step(&f, &v.target.id).await;
    assert_eq!(step, Step::Put);
    assert_eq!(msg, "the provider rejected the credentials");
    assert!(
        !v.target.last_error.contains("not-the-secret")
            && !v.target.last_error.contains("Signature")
    );

    // Unknown key id.
    let mut x = f.fields("badid");
    x.credentials = Credentials::new("AKIAUNKNOWN000000000", SK, "").unwrap();
    let v = f.svc.create_target(&alice(), x).await.unwrap();
    assert_eq!(
        failing_step(&f, &v.target.id).await.1,
        "the provider rejected the credentials"
    );

    // Wrong bucket.
    let mut x = f.fields("badbucket");
    x.bucket = "no-such-bucket".into();
    let v = f.svc.create_target(&alice(), x).await.unwrap();
    let (step, msg, _) = failing_step(&f, &v.target.id).await;
    assert_eq!(
        (step, msg.as_str()),
        (Step::Put, "the bucket or object was not found")
    );

    // Permission lost after the key was created: access denied.
    let ok = f.create("denied").await;
    f.s3.deny();
    let (step, msg, _) = failing_step(&f, &ok.target.id).await;
    assert_eq!(
        (step, msg.as_str()),
        (
            Step::Put,
            "access denied: check the key's permissions on the bucket"
        )
    );
    f.s3.allow();

    // The read-back fails: the probe is still deleted.
    let g = f.create("getfails").await;
    f.s3.fail_next("GET", 1, 403);
    let (step, _, _) = failing_step(&f, &g.target.id).await;
    assert_eq!(step, Step::Get);
    assert!(f.s3.keys("pre/.asc-probe/").is_empty());

    // The delete fails.
    let d = f.create("delfails").await;
    f.s3.fail_next("POST", 1, 403);
    let (step, _, _) = failing_step(&f, &d.target.id).await;
    assert_eq!(step, Step::Delete);

    // Nothing listens at the endpoint.
    let mut x = f.fields("closed");
    x.endpoint = "http://127.0.0.1:1".into();
    let c = f.svc.create_target(&alice(), x).await.unwrap();
    let (step, msg, _) = failing_step(&f, &c.target.id).await;
    assert_eq!(step, Step::Connect);
    assert!(
        msg.contains("refused") || msg.contains("unreachable") || msg.contains("timed out"),
        "{msg}"
    );
}

#[tokio::test]
async fn a_failed_verification_blocks_enabling_and_a_later_success_clears_the_error() {
    let f = fx().await;
    let v = f.create("main").await;
    f.s3.fail_next("PUT", 1, 403);
    let (step, _, v) = failing_step(&f, &v.target.id).await;
    assert_eq!(step, Step::Put);
    assert!(matches!(
        f.svc
            .set_enabled(&alice(), &v.target.id, true)
            .await
            .unwrap_err(),
        StorageError::VerifyFailed(_)
    ));
    let (report, v) = f.svc.verify_target(&alice(), &v.target.id).await.unwrap();
    assert!(report.ok());
    assert_eq!(
        (v.target.state, v.target.last_error.as_str()),
        (State::Verified, "")
    );
}

#[tokio::test]
async fn verification_is_limited_to_six_an_hour() {
    let f = fx().await;
    let v = f.create("main").await;
    for _ in 0..6 {
        f.svc.verify_target(&alice(), &v.target.id).await.unwrap();
    }
    assert_eq!(
        f.svc
            .verify_target(&alice(), &v.target.id)
            .await
            .map(|_| ())
            .unwrap_err(),
        StorageError::RateLimited
    );
    // Another target has its own allowance.
    let w = f.create("other").await;
    assert!(f.svc.verify_target(&alice(), &w.target.id).await.is_ok());
    // A new window opens after an hour.
    sqlx::query("UPDATE storage_targets SET verify_window = verify_window - 4000")
        .execute(&f.pool)
        .await
        .unwrap();
    assert!(f.svc.verify_target(&alice(), &v.target.id).await.is_ok());
}

#[tokio::test]
async fn changing_the_connection_sends_a_target_back_to_pending() {
    let f = fx().await;
    let v = f.enabled("main").await;
    let id = v.target.id.clone();
    // A new name alone keeps it enabled.
    let renamed = f
        .svc
        .update_target(
            &alice(),
            &id,
            Patch {
                name: Some("renamed".into()),
                ..Patch::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(
        (renamed.target.state, renamed.target.name.as_str()),
        (State::Enabled, "renamed")
    );
    // A new prefix needs a new verification and a new enabling.
    let moved = f
        .svc
        .update_target(
            &alice(),
            &id,
            Patch {
                prefix: Some("other/".into()),
                ..Patch::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(
        (
            moved.target.state,
            moved.target.prefix.as_str(),
            moved.target.verified_at
        ),
        (State::Pending, "other", 0)
    );
    assert!(matches!(
        f.svc.set_enabled(&alice(), &id, true).await.unwrap_err(),
        StorageError::VerifyFailed(_)
    ));
    // New credentials replace the pair as a whole and change the hint.
    let c = Credentials::new("AKIANEWKEY0123456789", SK, "").unwrap();
    let re = f
        .svc
        .update_target(
            &alice(),
            &id,
            Patch {
                credentials: Some(c),
                ..Patch::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(re.target.access_key_hint, "AKIA...6789");
    assert_eq!(re.target.key_gen, 1);
    let (report, _) = f.svc.verify_target(&alice(), &id).await.unwrap();
    assert!(!report.ok(), "the provider does not know the new key id");
    f.s3.set_credentials("AKIANEWKEY0123456789", SK);
    let (report, v) = f.svc.verify_target(&alice(), &id).await.unwrap();
    assert!(report.ok(), "{:?}", report.checks);
    assert_eq!(v.target.state, State::Verified);
    // A blocked endpoint is refused and changes nothing.
    let err = f
        .svc
        .update_target(
            &alice(),
            &id,
            Patch {
                endpoint: Some("http://10.0.0.5:9000".into()),
                credentials: Some(Credentials::new("AKIANEWKEY0123456789", SK, "").unwrap()),
                ..Patch::default()
            },
        )
        .await
        .unwrap_err();
    assert_eq!(err, StorageError::Blocked(BlockReason::Private));
    assert_eq!(
        f.svc
            .get_target(&alice(), &id)
            .await
            .unwrap()
            .target
            .endpoint,
        f.s3.endpoint()
    );
    // Two names cannot collide.
    f.create("taken").await;
    assert!(
        f.svc
            .update_target(
                &alice(),
                &id,
                Patch {
                    name: Some("taken".into()),
                    ..Patch::default()
                }
            )
            .await
            .is_err()
    );
}

#[tokio::test]
async fn a_target_in_use_cannot_be_deleted_or_disabled() {
    let f = fx().await;
    let v = f.enabled("main").await;
    let id = v.target.id.clone();
    f.repo("acme/app").await;
    f.svc.assign_repo(&alice(), "acme/app", &id).await.unwrap();
    assert_eq!(f.svc.get_target(&alice(), &id).await.unwrap().repos, 1);
    assert!(matches!(
        f.svc.delete_target(&alice(), &id).await.unwrap_err(),
        StorageError::InUse(_)
    ));
    assert!(matches!(
        f.svc.set_enabled(&alice(), &id, false).await.unwrap_err(),
        StorageError::InUse(_)
    ));
    f.svc.assign_repo(&alice(), "acme/app", "").await.unwrap();
    f.svc.delete_target(&alice(), &id).await.unwrap();
    assert!(matches!(
        f.svc.get_target(&alice(), &id).await.unwrap_err(),
        StorageError::NotFound(_)
    ));
    assert!(
        f.audit
            .actions()
            .contains(&"storage.target.deleted".to_owned())
    );
}

// ------------------------------------------------------------------------------ the guard

#[tokio::test]
async fn every_guard_rule_is_reached_through_the_service() {
    let f = fx_with(Opts {
        loopback: false,
        ..Opts::default()
    })
    .await;
    f.resolver.script(
        "public.example.com",
        vec![vec!["93.184.216.34".parse().unwrap()]],
    );
    f.resolver.script(
        "inward.example.com",
        vec![vec![
            "93.184.216.34".parse().unwrap(),
            "10.1.2.3".parse().unwrap(),
        ]],
    );
    f.resolver
        .script("loop.example.com", vec![vec!["127.0.0.1".parse().unwrap()]]);
    f.resolver.script(
        "meta.example.com",
        vec![vec!["169.254.169.254".parse().unwrap()]],
    );
    f.resolver.script(
        "mapped.example.com",
        vec![vec!["::ffff:10.0.0.1".parse().unwrap()]],
    );
    f.resolver
        .script("ll.example.com", vec![vec!["fe80::1".parse().unwrap()]]);
    f.resolver
        .script("mc.example.com", vec![vec!["224.0.0.1".parse().unwrap()]]);
    let cases: &[(&str, BlockReason)] = &[
        ("http://public.example.com", BlockReason::NotHttps),
        ("ftp://public.example.com", BlockReason::NotHttps),
        ("http://127.0.0.1:9000", BlockReason::NotHttps),
        ("https://127.0.0.1", BlockReason::Loopback),
        ("https://[::1]", BlockReason::Loopback),
        ("https://localhost.example.com", BlockReason::Unresolvable),
        ("https://loop.example.com", BlockReason::Loopback),
        ("https://10.0.0.1", BlockReason::Private),
        ("https://192.168.1.1", BlockReason::Private),
        ("https://172.16.0.9", BlockReason::Private),
        ("https://100.64.0.1", BlockReason::Private),
        ("https://inward.example.com", BlockReason::Private),
        ("https://mapped.example.com", BlockReason::Private),
        ("https://169.254.169.254", BlockReason::Metadata),
        ("https://meta.example.com", BlockReason::Metadata),
        ("https://metadata.google.internal", BlockReason::Metadata),
        ("https://[fd00:ec2::254]", BlockReason::Metadata),
        ("https://ll.example.com", BlockReason::LinkLocal),
        ("https://mc.example.com", BlockReason::Reserved),
        ("https://0.0.0.0", BlockReason::Reserved),
        (
            "https://user:pw@public.example.com",
            BlockReason::CredentialsInUrl,
        ),
        (
            "https://public.example.com/?X-Amz-Signature=1",
            BlockReason::CredentialsInUrl,
        ),
        (
            "https://public.example.com/#frag",
            BlockReason::CredentialsInUrl,
        ),
        ("https://public.example.com:22", BlockReason::Port),
        ("https://public.example.com:6379", BlockReason::Port),
        ("https://public.example.com:0", BlockReason::Port),
        ("not a url", BlockReason::Malformed),
        ("https://2130706433", BlockReason::Loopback),
    ];
    for (endpoint, reason) in cases {
        let mut x = f.fields("t");
        x.endpoint = (*endpoint).into();
        let err = f.svc.create_target(&alice(), x).await.unwrap_err();
        assert_eq!(err, StorageError::Blocked(*reason), "{endpoint}");
    }
    // Virtual-host addressing resolves the bucket's own host name, which can differ.
    f.resolver.script(
        "bkt.vh.example.com",
        vec![vec!["10.0.0.9".parse().unwrap()]],
    );
    f.resolver.script(
        "vh.example.com",
        vec![vec!["93.184.216.34".parse().unwrap()]],
    );
    let mut x = f.fields("vh");
    x.endpoint = "https://vh.example.com".into();
    x.bucket = "bkt".into();
    x.addressing = Addressing::VirtualHost;
    assert_eq!(
        f.svc.create_target(&alice(), x).await.unwrap_err(),
        StorageError::Blocked(BlockReason::Private)
    );
    // And a good one passes.
    let mut x = f.fields("good");
    x.endpoint = "https://public.example.com:9000/".into();
    let v = f.svc.create_target(&alice(), x).await.unwrap();
    assert_eq!(v.target.endpoint, "https://public.example.com:9000");
    assert_eq!(
        f.svc.db.count_targets(&owner_of("alice")).await.unwrap(),
        1,
        "only the good one was stored"
    );
}

#[tokio::test]
async fn a_name_that_starts_pointing_inwards_is_judged_again_and_refused() {
    let f = fx_with(Opts {
        repin: 0,
        ..Opts::default()
    })
    .await;
    // The name answers with the fake server (loopback, allowed by the opt-in) the first time.
    f.resolver.script(
        "s3.rebind.example.com",
        vec![vec!["127.0.0.1".parse().unwrap()]],
    );
    let mut x = f.fields("main");
    x.endpoint = format!(
        "http://s3.rebind.example.com:{}",
        f.s3.endpoint().rsplit(':').next().unwrap()
    );
    let v = f.svc.create_target(&alice(), x).await.unwrap();
    let (report, _) = f.svc.verify_target(&alice(), &v.target.id).await.unwrap();
    assert!(report.ok(), "{:?}", report.checks);
    f.svc
        .set_enabled(&alice(), &v.target.id, true)
        .await
        .unwrap();
    f.repo("acme/app").await;
    f.svc
        .assign_repo(&alice(), "acme/app", &v.target.id)
        .await
        .unwrap();
    let calls_before = f.resolver.calls();
    let e = f.open("acme/app").await.unwrap();
    put_obj(&e, b"first").await;
    // The same answer keeps the same store (and the registry's caches).
    let stamp = f.open("acme/app").await.unwrap().route_stamp;
    assert_eq!(stamp, f.open("acme/app").await.unwrap().route_stamp);
    assert!(
        f.resolver.calls() > calls_before,
        "repin 0: every lookup re-resolves"
    );
    // DNS now answers with a private address: the next lookup refuses the repository's storage.
    f.resolver.script(
        "s3.rebind.example.com",
        vec![vec!["10.0.0.1".parse().unwrap()]],
    );
    let err = f.open("acme/app").await.err().unwrap();
    assert!(
        err.unavailable().is_some_and(|m| m.contains("blocked")),
        "{err}"
    );
    // Nothing reached the private address: the fake server saw no request after the switch.
    let n = f.s3.requests().len();
    assert!(f.open("acme/app").await.is_err());
    assert_eq!(f.s3.requests().len(), n);
}

// ------------------------------------------------------------------------------ secrecy

/// A log writer the tests can read back.
#[derive(Clone, Default)]
struct Capture(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Capture {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Capture {
    type Writer = Capture;
    fn make_writer(&'a self) -> Capture {
        self.clone()
    }
}

#[tokio::test]
async fn credentials_never_reach_logs_errors_audit_or_responses() {
    let capture = Capture::default();
    let _g = tracing::subscriber::set_default(
        tracing_subscriber::fmt()
            .with_writer(capture.clone())
            .with_max_level(tracing::Level::TRACE)
            .with_ansi(false)
            .finish(),
    );
    let f = fx().await;
    let mut seen = String::new();
    let v = f.create("main").await;
    seen += &format!("{v:?}");
    // Successful and failing verifications, an update, a blocked endpoint, a wrong key.
    let (r, v2) = f.svc.verify_target(&alice(), &v.target.id).await.unwrap();
    seen += &format!("{r:?}{v2:?}");
    f.s3.fail_next("PUT", 1, 403);
    let (r, v3) = f.svc.verify_target(&alice(), &v.target.id).await.unwrap();
    seen += &format!("{r:?}{v3:?}");
    let c = Credentials::new(
        "AKIANEWKEY0123456789",
        "another-PLANTED-secret",
        "tok-PLANTED-session",
    )
    .unwrap();
    seen += &format!("{c:?}");
    let v4 = f
        .svc
        .update_target(
            &alice(),
            &v.target.id,
            Patch {
                credentials: Some(c),
                ..Patch::default()
            },
        )
        .await
        .unwrap();
    seen += &format!("{v4:?}");
    let (r, _) = f.svc.verify_target(&alice(), &v.target.id).await.unwrap();
    seen += &format!("{r:?}");
    let mut x = f.fields("blocked");
    x.endpoint = "https://10.0.0.1".into();
    seen += &format!("{:?}", f.svc.create_target(&alice(), x).await.unwrap_err());
    seen += &format!("{:?}", f.audit.events());
    seen += &format!(
        "{:?}",
        super::rpc::status_of(StorageError::Blocked(BlockReason::Private))
    );
    let wire = super::rpc::target_pb(&v4);
    seen += &format!("{wire:?}");
    let logged = String::from_utf8_lossy(&capture.0.lock().unwrap()).into_owned();
    for planted in [
        SK,
        AK,
        "another-PLANTED-secret",
        "tok-PLANTED-session",
        "AKIANEWKEY0123456789",
    ] {
        assert!(!seen.contains(planted), "{planted} leaked into a result");
        assert!(!logged.contains(planted), "{planted} leaked into a log");
    }
    // The wire form has the hint and the flag, nothing else.
    assert_eq!(wire.access_key_hint, "AKIA...6789");
    assert!(wire.has_secret);
}

#[tokio::test]
async fn rotating_the_master_key_keeps_credentials_working() {
    let f = fx().await;
    let v = f.enabled("main").await;
    f.repo("acme/app").await;
    f.svc
        .assign_repo(&alice(), "acme/app", &v.target.id)
        .await
        .unwrap();
    let before = f.svc.db.sealed(&v.target.id).await.unwrap().unwrap();
    let r = f.vault.rotate(true).await.unwrap();
    f.vault.wait_idle().await;
    let done = f.vault.rotation(&r.id).await.unwrap().unwrap();
    assert!(done.finished && done.error.is_empty(), "{done:?}");
    assert_eq!(done.done, 1);
    let after = f.svc.db.sealed(&v.target.id).await.unwrap().unwrap();
    assert_ne!(before, after);
    assert_eq!(crate::vault::crypto::generation_of(&after), Some(2));
    assert_eq!(
        f.svc
            .get_target(&alice(), &v.target.id)
            .await
            .unwrap()
            .target
            .key_gen,
        2
    );
    let status = f.vault.status().await;
    assert_eq!(
        status.generations.len(),
        1,
        "generation 1 is dropped, nothing is sealed under it"
    );
    // The target still verifies and serves the repository.
    f.svc.router.invalidate(&v.target.id);
    let (report, _) = f.svc.verify_target(&alice(), &v.target.id).await.unwrap();
    assert!(report.ok());
    let e = f.open("acme/app").await.unwrap();
    put_obj(&e, b"after the rotation").await;
}

#[tokio::test]
async fn a_locked_vault_refuses_to_seal_and_says_why() {
    let f = fx().await;
    let locked = Vault::locked("set [vault] key_file", f.pool.clone());
    let svc = Service {
        vault: locked,
        db: f.svc.db.clone(),
        guard: f.svc.guard.clone(),
        router: f.svc.router.clone(),
        engine: f.svc.engine.clone(),
        audit: f.svc.audit.clone(),
        authority: f.svc.authority.clone(),
        cfg: f.svc.cfg.clone(),
    };
    let err = svc
        .create_target(&alice(), f.fields("x"))
        .await
        .unwrap_err();
    assert_eq!(
        err,
        StorageError::VaultLocked("set [vault] key_file".into())
    );
    let status = super::rpc::status_of(err);
    assert_eq!(status.code(), tonic::Code::Unavailable);
    assert_eq!(
        asc_rpc::server::status_rpc_error(&status)
            .map(|e| e.code().to_owned())
            .as_deref(),
        Some("VAULT_LOCKED")
    );
}

// ------------------------------------------------------------------------------ routing

#[tokio::test]
async fn an_assigned_repository_writes_to_its_target_and_still_reads_what_it_had() {
    let f = fx().await;
    let v = f.enabled("main").await;
    let e = f.repo("acme/app").await;
    let old = put_obj(&e, b"stored before the assignment").await;
    f.svc
        .assign_repo(&alice(), "acme/app", &v.target.id)
        .await
        .unwrap();
    let e = f.open("acme/app").await.unwrap();
    assert!(e.route_stamp > 0);
    // The old object is read through the fallback.
    assert_eq!(
        &e.objects.get(&old).await.unwrap().unwrap()[..],
        b"stored before the assignment"
    );
    // A new one lands in the target's bucket under the prefix and the namespace.
    let new = put_obj(&e, b"stored after the assignment").await;
    let ns = e.ns.hex();
    let keys = f.s3.keys(&format!("pre/ns/{ns}/"));
    assert_eq!(keys.len(), 1, "{keys:?}");
    assert!(keys[0].ends_with(&new.path()));
    assert_eq!(
        &e.objects.get(&new).await.unwrap().unwrap()[..],
        b"stored after the assignment"
    );
    // The default bucket did not get it, and the registry's namespace lookup finds the route.
    let (default_store, default_root) = f.registry.default_place();
    let in_default = futures::TryStreamExt::try_collect::<Vec<_>>(
        default_store.list(Some(&default_root.clone().join("ns").join(ns.clone()))),
    )
    .await
    .unwrap();
    assert_eq!(
        in_default.len(),
        1,
        "only the object from before the assignment"
    );
    let (store, prefix) = f.registry.namespace(e.ns);
    let listed: Vec<_> = futures::TryStreamExt::try_collect::<Vec<_>>(store.list(Some(&prefix)))
        .await
        .unwrap();
    assert_eq!(listed.len(), 2, "the namespace lists both places");
    assert!(
        f.audit
            .actions()
            .contains(&"storage.repo.assigned".to_owned())
    );
}

#[tokio::test]
async fn a_fresh_repository_has_no_fallback_and_can_be_reassigned() {
    let f = fx().await;
    let a = f.enabled("a").await;
    let b = f.enabled("b").await;
    f.repo("acme/app").await;
    f.svc
        .assign_repo(&alice(), "acme/app", &a.target.id)
        .await
        .unwrap();
    // Nothing was stored yet, so there is nothing to keep reachable.
    let route = f
        .svc
        .db
        .route(f.svc.db.repo("acme/app").await.unwrap().unwrap().0)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        (route.target_id.as_str(), route.fallback),
        (a.target.id.as_str(), None)
    );
    f.svc
        .assign_repo(&alice(), "acme/app", &b.target.id)
        .await
        .unwrap();
    // With data in the first target, a second assignment would split the repository three ways.
    let e = f.open("acme/app").await.unwrap();
    put_obj(&e, b"in b").await;
    f.svc
        .assign_repo(&alice(), "acme/app", &a.target.id)
        .await
        .unwrap();
    let e = f.open("acme/app").await.unwrap();
    put_obj(&e, b"in a").await;
    let err = f
        .svc
        .assign_repo(&alice(), "acme/app", &b.target.id)
        .await
        .unwrap_err();
    assert!(
        matches!(err, StorageError::FailedPrecondition(_)),
        "{err:?}"
    );
    // The same target again is a no-op.
    assert_eq!(
        f.svc
            .assign_repo(&alice(), "acme/app", &a.target.id)
            .await
            .unwrap(),
        a.target.id
    );
}

#[tokio::test]
async fn assignment_is_scoped_to_what_the_caller_and_the_target_allow() {
    let f = fx().await;
    let mine = f.enabled("mine").await;
    f.repo("acme/app").await;
    f.repo("acme/other").await;
    // bob's target, bob's repository.
    let mut x = f.fields("bobs");
    x.owner = owner_of("bob");
    let bobs = f.svc.create_target(&bob(), x).await.unwrap();
    let (_, bobs) = f.svc.verify_target(&bob(), &bobs.target.id).await.unwrap();
    f.svc
        .set_enabled(&bob(), &bobs.target.id, true)
        .await
        .unwrap();
    // alice cannot use bob's target: it does not exist for her.
    assert!(matches!(
        f.svc
            .assign_repo(&alice(), "acme/app", &bobs.target.id)
            .await
            .unwrap_err(),
        StorageError::NotFound(_)
    ));
    // bob cannot assign alice's repository, with his target or without.
    assert!(matches!(
        f.svc
            .assign_repo(&bob(), "acme/app", &bobs.target.id)
            .await
            .unwrap_err(),
        StorageError::PermissionDenied(_)
    ));
    assert!(matches!(
        f.svc
            .start_migration(&bob(), "acme/app", &bobs.target.id, false)
            .await
            .unwrap_err(),
        StorageError::PermissionDenied(_)
    ));
    // A repository that does not exist.
    assert!(matches!(
        f.svc
            .assign_repo(&alice(), "acme/none", &mine.target.id)
            .await
            .unwrap_err(),
        StorageError::NotFound(_)
    ));
    // A target that is not enabled cannot be assigned.
    let pending = f.create("pending").await;
    assert!(matches!(
        f.svc
            .assign_repo(&alice(), "acme/app", &pending.target.id)
            .await
            .unwrap_err(),
        StorageError::FailedPrecondition(_)
    ));
    // A repository-owned target serves that repository only.
    f.table.grant_repo("7:alice", "acme/app");
    let mut x = f.fields("repo-owned");
    x.owner = Owner::new(OwnerKind::Repo, "acme/app").unwrap();
    let own = f.svc.create_target(&alice(), x).await.unwrap();
    let (_, own) = f.svc.verify_target(&alice(), &own.target.id).await.unwrap();
    f.svc
        .set_enabled(&alice(), &own.target.id, true)
        .await
        .unwrap();
    f.svc
        .assign_repo(&alice(), "acme/app", &own.target.id)
        .await
        .unwrap();
    assert!(matches!(
        f.svc
            .assign_repo(&alice(), "acme/other", &own.target.id)
            .await
            .unwrap_err(),
        StorageError::PermissionDenied(_)
    ));
}

#[tokio::test]
async fn a_target_that_fails_its_health_checks_makes_its_repositories_unavailable_with_a_reason() {
    let f = fx().await;
    let v = f.enabled("main").await;
    f.repo("acme/app").await;
    f.svc
        .assign_repo(&alice(), "acme/app", &v.target.id)
        .await
        .unwrap();
    assert!(f.open("acme/app").await.is_ok());
    let t = f.svc.db.target(&v.target.id).await.unwrap().unwrap();
    f.s3.deny();
    // One or two failures are noise; the third marks it failed.
    f.byos.health.check(&t).await;
    f.byos.health.check(&t).await;
    assert_eq!(
        f.svc.db.target(&v.target.id).await.unwrap().unwrap().state,
        State::Enabled
    );
    f.byos.health.check(&t).await;
    let failed = f.svc.db.target(&v.target.id).await.unwrap().unwrap();
    assert_eq!(failed.state, State::Failed);
    assert!(failed.was_enabled);
    assert_eq!(
        failed.last_error,
        "put: access denied: check the key's permissions on the bucket"
    );
    f.svc.router.invalidate(&v.target.id);
    let err = f.open("acme/app").await.err().unwrap();
    assert!(
        err.unavailable().is_some_and(|m| m.contains("failed")),
        "{err}"
    );
    let status = crate::commit_graph_rpc::registry_status(err);
    assert_eq!(status.code(), tonic::Code::Unavailable);
    // It recovers by itself.
    f.s3.allow();
    let t = f.svc.db.target(&v.target.id).await.unwrap().unwrap();
    f.byos.health.check(&t).await;
    let healed = f.svc.db.target(&v.target.id).await.unwrap().unwrap();
    assert_eq!(
        (healed.state, healed.last_error.as_str()),
        (State::Enabled, "")
    );
    f.svc.router.invalidate(&v.target.id);
    assert!(f.open("acme/app").await.is_ok());
    // A pass looks at every enabled target.
    f.s3.deny();
    f.byos.health.pass().await;
    assert_eq!(
        f.svc.db.target(&v.target.id).await.unwrap().unwrap().state,
        State::Enabled
    );
}

// ------------------------------------------------------------------------------ migration

async fn seed(f: &Fx, e: &RepoEntry, n: usize) -> Vec<(ObjKey, Vec<u8>)> {
    let mut out = Vec::new();
    for i in 0..n {
        let b = body(i);
        out.push((put_obj(e, &b).await, b));
    }
    let _ = f;
    out
}

async fn assert_all_in_target(f: &Fx, e: &RepoEntry, objs: &[(ObjKey, Vec<u8>)]) {
    let prefix = format!("pre/ns/{}/", e.ns.hex());
    for (k, b) in objs {
        let stored =
            f.s3.get(&format!("{prefix}{}", k.path()))
                .unwrap_or_else(|| panic!("{k} missing in the target"));
        assert_eq!(stored, asc_store::codec::encode_stored(b).to_vec(), "{k}");
    }
    assert_eq!(f.s3.keys(&prefix).len(), objs.len());
}

#[tokio::test]
async fn a_migration_copies_verifies_switches_and_leaves_the_source_alone_unless_asked() {
    let f = fx().await;
    let v = f.enabled("main").await;
    let e = f.repo("acme/app").await;
    let objs = seed(&f, &e, 150).await;
    let mut events = f.byos.engine.subscribe();
    let m = f
        .svc
        .start_migration(&alice(), "acme/app", &v.target.id, false)
        .await
        .unwrap();
    assert_eq!(
        (m.state.as_str(), m.from_target.as_str()),
        (state::QUEUED, "")
    );
    f.drain().await;
    let m = f.svc.get_migration(&alice(), &m.id).await.unwrap();
    assert_eq!(m.state, state::DONE, "{m:?}");
    assert_eq!((m.objects_total, m.error.as_str()), (150, ""));
    assert!(m.objects_copied >= 150 && m.bytes_copied > 0);
    assert_all_in_target(&f, &e, &objs).await;
    // The route points at the target; reads come from it.
    let e2 = f.open("acme/app").await.unwrap();
    let route = f.svc.db.route(e2.key.0).await.unwrap().unwrap();
    assert_eq!(
        (route.target_id.as_str(), route.fallback, route.mirror_to),
        (v.target.id.as_str(), None, None)
    );
    for (k, b) in objs.iter().step_by(37) {
        assert_eq!(&e2.objects.get(k).await.unwrap().unwrap()[..], &b[..]);
    }
    // The default bucket still has the copies (delete_source was false).
    assert!(e.objects.get(&objs[0].0).await.unwrap().is_some());
    let (default_store, default_root) = f.registry.default_place();
    let n = futures::TryStreamExt::try_collect::<Vec<_>>(
        default_store.list(Some(&default_root.clone().join("ns").join(e.ns.hex()))),
    )
    .await
    .unwrap()
    .len();
    assert_eq!(n, 150);
    // The notices a subscriber saw, in order.
    let mut states = Vec::new();
    while let Ok(ev) = events.try_recv() {
        let Event { state, .. } = ev;
        if states.last() != Some(&state) {
            states.push(state);
        }
    }
    assert_eq!(states, vec!["copying", "verifying", "switching", "done"]);
    let actions = f.audit.actions();
    assert!(actions.contains(&"storage.migration.started".to_owned()));
    assert!(actions.contains(&"storage.migration.finished".to_owned()));
    // The finished migration is listed and the target can now not be deleted.
    let (rows, next) = f
        .svc
        .list_migrations(&alice(), "acme/app", None, 0)
        .await
        .unwrap();
    assert_eq!((rows.len(), next), (1, None));
    assert!(matches!(
        f.svc
            .delete_target(&alice(), &v.target.id)
            .await
            .unwrap_err(),
        StorageError::InUse(_)
    ));
}

#[tokio::test]
async fn with_delete_source_the_old_objects_go_after_the_switch() {
    let f = fx().await;
    let v = f.enabled("main").await;
    let e = f.repo("acme/app").await;
    let objs = seed(&f, &e, 20).await;
    let other = f.repo("acme/other").await;
    let other_obj = put_obj(&other, b"another repository").await;
    f.svc
        .start_migration(&alice(), "acme/app", &v.target.id, true)
        .await
        .unwrap();
    f.drain().await;
    assert_all_in_target(&f, &e, &objs).await;
    let (default_store, default_root) = f.registry.default_place();
    let left = futures::TryStreamExt::try_collect::<Vec<_>>(
        default_store.list(Some(&default_root.clone().join("ns").join(e.ns.hex()))),
    )
    .await
    .unwrap();
    assert!(left.is_empty(), "{} objects left in the source", left.len());
    // The other repository was not touched.
    assert!(other.objects.get(&other_obj).await.unwrap().is_some());
    let e2 = f.open("acme/app").await.unwrap();
    assert!(e2.objects.get(&objs[3].0).await.unwrap().is_some());
}

#[tokio::test]
async fn a_failed_migration_leaves_the_source_authoritative_and_a_second_try_continues() {
    let f = fx().await;
    let v = f.enabled("main").await;
    let e = f.repo("acme/app").await;
    let objs = seed(&f, &e, 100).await;
    // The provider starts refusing writes after 20 objects.
    f.s3.fail_after("PUT", 20, 10_000, 403);
    let m = f
        .svc
        .start_migration(&alice(), "acme/app", &v.target.id, true)
        .await
        .unwrap();
    f.drain().await;
    let failed = f.svc.get_migration(&alice(), &m.id).await.unwrap();
    assert_eq!(failed.state, state::FAILED);
    assert_eq!(
        failed.error,
        "access denied: check the key's permissions on the bucket"
    );
    // The source is untouched and still serves; the mirror is off, so writes work again.
    let e2 = f.open("acme/app").await.unwrap();
    let route = f.svc.db.route(e2.key.0).await.unwrap();
    assert!(route.is_none_or(|r| r.mirror_to.is_none() && r.target_id.is_empty()));
    for (k, b) in objs.iter().step_by(11) {
        assert_eq!(&e2.objects.get(k).await.unwrap().unwrap()[..], &b[..]);
    }
    put_obj(&e2, b"written while it was broken").await;
    let copied_first = f.s3.keys(&format!("pre/ns/{}/", e.ns.hex())).len();
    assert!(copied_first > 0 && copied_first < 100, "{copied_first}");
    // The provider is back: a new migration skips what is there.
    f.s3.allow();
    f.s3.clear_requests();
    let m2 = f
        .svc
        .start_migration(&alice(), "acme/app", &v.target.id, true)
        .await
        .unwrap();
    f.drain().await;
    assert_eq!(
        f.svc.get_migration(&alice(), &m2.id).await.unwrap().state,
        state::DONE
    );
    let puts = f.s3.requests().iter().filter(|r| r.method == "PUT").count();
    assert!(
        puts <= 101 - copied_first + 2,
        "copied again: {puts} puts, {copied_first} were there"
    );
    assert_eq!(f.s3.keys(&format!("pre/ns/{}/", e.ns.hex())).len(), 101);
    let (rows, _) = f
        .svc
        .list_migrations(&alice(), "acme/app", None, 10)
        .await
        .unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].id, m2.id, "newest first");
}

#[tokio::test]
async fn a_migration_a_stopped_server_left_half_done_is_resumed_from_its_cursor() {
    let f = fx().await;
    let v = f.enabled("main").await;
    let e = f.repo("acme/app").await;
    let objs = seed(&f, &e, 200).await;
    f.s3.set_delay("PUT", 15);
    let m = f
        .svc
        .start_migration(&alice(), "acme/app", &v.target.id, false)
        .await
        .unwrap();
    // The first server starts it and "crashes" after the first chunks.
    let row = f.svc.db.migration(&m.id).await.unwrap().unwrap();
    assert!(
        f.svc
            .db
            .claim_migration(
                &m.id,
                super::migrate::now() + 120,
                super::migrate::now(),
                false
            )
            .await
            .unwrap()
    );
    let first = tokio::spawn(f.byos.engine.clone().run(row));
    loop {
        let cur = f.svc.db.migration(&m.id).await.unwrap().unwrap();
        if cur.objects_copied >= 64 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    first.abort();
    let _ = first.await;
    let mid = f.svc.db.migration(&m.id).await.unwrap().unwrap();
    assert!(
        !mid.cursor.is_empty() && mid.state == state::COPYING,
        "{mid:?}"
    );
    assert!(mid.objects_copied < 200);
    // While it is down, the lease is still held: nobody takes it over early.
    let second = Engine2::new(&f);
    second.tick().await;
    second.wait_idle().await;
    assert_eq!(
        f.svc.db.migration(&m.id).await.unwrap().unwrap().state,
        state::COPYING
    );
    // The lease runs out; a restarted server resumes.
    sqlx::query("UPDATE storage_migrations SET lease_until = 0")
        .execute(&f.pool)
        .await
        .unwrap();
    f.s3.clear_requests();
    f.s3.allow();
    second.tick().await;
    second.wait_idle().await;
    let done = f.svc.db.migration(&m.id).await.unwrap().unwrap();
    assert_eq!(done.state, state::DONE, "{done:?}");
    assert_all_in_target(&f, &e, &objs).await;
    // The resumed run did not start over: fewer puts than objects.
    let puts = f.s3.requests().iter().filter(|r| r.method == "PUT").count();
    assert!(puts < 200, "{puts} puts after the restart");
}

/// A second engine over the same database: "the restarted server".
struct Engine2(Arc<super::migrate::Engine>);

impl Engine2 {
    fn new(f: &Fx) -> Self {
        Self(super::migrate::Engine::new(
            f.svc.db.clone(),
            f.svc.router.clone(),
            f.audit.clone(),
            super::migrate::Settings {
                settle: Duration::ZERO,
                workers: 2,
                poll: Duration::from_millis(50),
            },
        ))
    }
    async fn tick(&self) {
        self.0.tick().await;
    }
    async fn wait_idle(&self) {
        self.0.wait_idle().await;
    }
}

#[tokio::test]
async fn cancelling_stops_the_copy_clears_the_mirror_and_is_idempotent() {
    let f = fx().await;
    let v = f.enabled("main").await;
    let e = f.repo("acme/app").await;
    let objs = seed(&f, &e, 300).await;
    f.s3.set_delay("PUT", 10);
    let m = f
        .svc
        .start_migration(&alice(), "acme/app", &v.target.id, true)
        .await
        .unwrap();
    let engine = f.byos.engine.clone();
    let runner = tokio::spawn(async move {
        engine.tick().await;
        engine.wait_idle().await;
    });
    loop {
        let cur = f.svc.db.migration(&m.id).await.unwrap().unwrap();
        if cur.objects_copied >= 64 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    // The mirror is on while it copies.
    let mid = f.svc.db.route(e.key.0).await.unwrap().unwrap();
    assert_eq!(mid.mirror_to.as_deref(), Some(v.target.id.as_str()));
    let c = f.svc.cancel_migration(&alice(), &m.id).await.unwrap();
    assert!(c.cancel);
    runner.await.unwrap();
    let after = f.svc.get_migration(&alice(), &m.id).await.unwrap();
    assert_eq!(after.state, state::CANCELLED);
    assert!(
        f.svc.db.route(e.key.0).await.unwrap().is_none(),
        "back to the plain default route"
    );
    // Nothing was deleted from the source, and the repository works.
    let e2 = f.open("acme/app").await.unwrap();
    assert_eq!(
        &e2.objects.get(&objs[299].0).await.unwrap().unwrap()[..],
        &objs[299].1[..]
    );
    // Cancelling again, or a migration that ended, changes nothing.
    let again = f.svc.cancel_migration(&alice(), &m.id).await.unwrap();
    assert_eq!(again.state, state::CANCELLED);
    // Another caller does not see it.
    assert!(matches!(
        f.svc.cancel_migration(&bob(), &m.id).await.unwrap_err(),
        StorageError::NotFound(_)
    ));
    assert!(matches!(
        f.svc.get_migration(&bob(), &m.id).await.unwrap_err(),
        StorageError::NotFound(_)
    ));
    // And the repository can be migrated afterwards.
    f.s3.allow();
    let f2 = f
        .svc
        .start_migration(&alice(), "acme/app", &v.target.id, false)
        .await
        .unwrap();
    f.drain().await;
    assert_eq!(
        f.svc.get_migration(&alice(), &f2.id).await.unwrap().state,
        state::DONE
    );
}

#[tokio::test]
async fn only_one_migration_runs_per_repository_and_the_target_is_pinned_by_it() {
    let f = fx().await;
    let v = f.enabled("main").await;
    let w = f.enabled_at("second", "second/").await;
    f.repo("acme/app").await;
    let m = f
        .svc
        .start_migration(&alice(), "acme/app", &v.target.id, false)
        .await
        .unwrap();
    assert_eq!(
        f.svc
            .start_migration(&alice(), "acme/app", &w.target.id, false)
            .await
            .unwrap_err(),
        StorageError::MigrationActive
    );
    assert_eq!(
        f.svc
            .assign_repo(&alice(), "acme/app", &w.target.id)
            .await
            .unwrap_err(),
        StorageError::MigrationActive
    );
    assert!(matches!(
        f.svc
            .delete_target(&alice(), &v.target.id)
            .await
            .unwrap_err(),
        StorageError::InUse(_)
    ));
    f.drain().await;
    assert_eq!(
        f.svc.get_migration(&alice(), &m.id).await.unwrap().state,
        state::DONE
    );
    // Migrating to where it already is is refused; to the other target is fine.
    assert!(matches!(
        f.svc
            .start_migration(&alice(), "acme/app", &v.target.id, false)
            .await
            .unwrap_err(),
        StorageError::InvalidArgument(_)
    ));
    f.svc
        .start_migration(&alice(), "acme/app", &w.target.id, true)
        .await
        .unwrap();
    f.drain().await;
}

#[tokio::test]
async fn two_targets_on_the_same_bucket_and_prefix_are_not_a_migration() {
    let f = fx().await;
    let a = f.enabled("a").await;
    let b = f.enabled("b").await;
    f.repo("acme/app").await;
    f.svc
        .assign_repo(&alice(), "acme/app", &a.target.id)
        .await
        .unwrap();
    let err = f
        .svc
        .start_migration(&alice(), "acme/app", &b.target.id, true)
        .await
        .unwrap_err();
    assert!(
        matches!(err, StorageError::InvalidArgument(ref m) if m.contains("same bucket")),
        "{err:?}"
    );
}

#[tokio::test]
async fn writes_and_deletes_during_the_copy_reach_both_places() {
    let f = fx().await;
    let v = f.enabled("main").await;
    let e = f.repo("acme/app").await;
    let objs = seed(&f, &e, 120).await;
    f.s3.set_delay("PUT", 10);
    let m = f
        .svc
        .start_migration(&alice(), "acme/app", &v.target.id, false)
        .await
        .unwrap();
    let engine = f.byos.engine.clone();
    let runner = tokio::spawn(async move {
        engine.tick().await;
        engine.wait_idle().await;
    });
    // Wait for the mirror to be on, then write and delete through the registry.
    loop {
        if f.svc
            .db
            .route(e.key.0)
            .await
            .unwrap()
            .is_some_and(|r| r.mirror_to.is_some())
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let during = f.open("acme/app").await.unwrap();
    let new = put_obj(&during, b"written during the copy").await;
    let gone = &objs[0].0;
    during.objects.delete(gone).await.unwrap();
    let prefix = format!("pre/ns/{}/", e.ns.hex());
    assert!(
        f.s3.get(&format!("{prefix}{}", new.path())).is_some(),
        "the write went to the destination at once"
    );
    runner.await.unwrap();
    assert_eq!(
        f.svc.get_migration(&alice(), &m.id).await.unwrap().state,
        state::DONE
    );
    assert!(f.s3.get(&format!("{prefix}{}", new.path())).is_some());
    assert!(
        f.s3.get(&format!("{prefix}{}", gone.path())).is_none(),
        "the delete reached the destination"
    );
    let after = f.open("acme/app").await.unwrap();
    assert!(after.objects.get(&new).await.unwrap().is_some());
}

#[tokio::test]
async fn verification_repairs_a_copy_that_differs() {
    let f = fx_with(Opts {
        settle: 2,
        ..Opts::default()
    })
    .await;
    let v = f.enabled("main").await;
    let e = f.repo("acme/app").await;
    let objs = seed(&f, &e, 30).await;
    let mut events = f.byos.engine.subscribe();
    let m = f
        .svc
        .start_migration(&alice(), "acme/app", &v.target.id, false)
        .await
        .unwrap();
    let engine = f.byos.engine.clone();
    let runner = tokio::spawn(async move {
        engine.tick().await;
        engine.wait_idle().await;
    });
    // While the engine waits for the replicas to settle, a copy is damaged and another is lost.
    let prefix = format!("pre/ns/{}/", e.ns.hex());
    loop {
        let ev = events.recv().await.unwrap();
        if ev.state == state::VERIFYING {
            break;
        }
    }
    f.s3.put(
        &format!("{prefix}{}", objs[4].0.path()),
        b"damaged on the provider's side",
    );
    f.s3.remove(&format!("{prefix}{}", objs[9].0.path()));
    runner.await.unwrap();
    assert_eq!(
        f.svc.get_migration(&alice(), &m.id).await.unwrap().state,
        state::DONE
    );
    assert_all_in_target(&f, &e, &objs).await;
}

#[tokio::test]
async fn a_repository_can_move_back_to_the_instance_bucket() {
    let f = fx().await;
    let v = f.enabled("main").await;
    let e = f.repo("acme/app").await;
    f.svc
        .assign_repo(&alice(), "acme/app", &v.target.id)
        .await
        .unwrap();
    let e = f.open("acme/app").await.unwrap_or(e);
    let objs = seed(&f, &e, 10).await;
    f.svc
        .start_migration(&alice(), "acme/app", "", true)
        .await
        .unwrap();
    f.drain().await;
    assert!(
        f.s3.keys("pre/ns/").is_empty(),
        "the target's copies were deleted"
    );
    let back = f.open("acme/app").await.unwrap();
    assert_eq!(back.route_stamp, 0);
    assert!(back.objects.get(&objs[2].0).await.unwrap().is_some());
    assert!(f.svc.db.route(back.key.0).await.unwrap().is_none());
    f.svc.delete_target(&alice(), &v.target.id).await.unwrap();
}

#[tokio::test]
async fn listing_the_target_follows_the_providers_continuation_pages() {
    let f = fx().await;
    let v = f.enabled("main").await;
    let e = f.repo("acme/app").await;
    f.svc
        .assign_repo(&alice(), "acme/app", &v.target.id)
        .await
        .unwrap();
    let e = f.open("acme/app").await.unwrap_or(e);
    let objs = seed(&f, &e, 23).await;
    assert_eq!(f.s3.keys("pre/ns/").len(), 23);
    // Four keys a page: moving back has to list the target six times to see every object.
    f.s3.set_page_size(4);
    let m = f
        .svc
        .start_migration(&alice(), "acme/app", "", true)
        .await
        .unwrap();
    f.drain().await;
    let m = f.svc.get_migration(&alice(), &m.id).await.unwrap();
    assert_eq!(
        (m.state.as_str(), m.objects_total, m.error.as_str()),
        (state::DONE, 23, ""),
        "{m:?}"
    );
    assert!(
        f.s3.keys("pre/ns/").is_empty(),
        "every page of the source was deleted"
    );
    let back = f.open("acme/app").await.unwrap();
    for (k, b) in &objs {
        assert_eq!(&back.objects.get(k).await.unwrap().unwrap()[..], &b[..]);
    }
}

#[tokio::test]
async fn a_request_signed_for_another_region_is_not_accepted() {
    let f = fx().await;
    let mut x = f.fields("eu");
    x.region = "eu-west-1".into();
    let v = f.svc.create_target(&alice(), x).await.unwrap();
    let (step, msg, _) = failing_step(&f, &v.target.id).await;
    assert_eq!(
        (step, msg.as_str()),
        (Step::Put, "the provider rejected the credentials")
    );
}

#[tokio::test]
async fn page_tokens_and_limits() {
    let f = fx().await;
    let v = f.enabled("main").await;
    f.repo("acme/app").await;
    for _ in 0..3 {
        let a = f
            .svc
            .start_migration(&alice(), "acme/app", &v.target.id, false)
            .await;
        if a.is_ok() {
            f.drain().await;
            // Go back so the next one has somewhere to go.
            f.svc
                .start_migration(&alice(), "acme/app", "", false)
                .await
                .unwrap();
            f.drain().await;
        }
    }
    let (page, next) = f
        .svc
        .list_migrations(&alice(), "acme/app", None, 2)
        .await
        .unwrap();
    assert_eq!(page.len(), 2);
    let next = next.expect("more rows");
    let (rest, end) = f
        .svc
        .list_migrations(&alice(), "acme/app", Some(next), 2)
        .await
        .unwrap();
    assert_eq!(rest.len(), 2);
    assert!(end.is_some());
    let (all, _) = f
        .svc
        .list_migrations(&alice(), "acme/app", None, 1000)
        .await
        .unwrap();
    assert_eq!(all.len(), 6, "limit is capped, not refused");
    let _ = Bytes::new();
}

// ------------------------------------------------------------------------------ re-binding

#[tokio::test]
async fn moving_a_target_needs_credentials_in_the_same_request() {
    let f = fx().await;
    let v = f.enabled("main").await;
    let id = v.target.id.clone();
    let patches: [(&str, Patch); 4] = [
        (
            "endpoint",
            Patch {
                endpoint: Some("http://attacker.example:9000".into()),
                ..Patch::default()
            },
        ),
        (
            "bucket",
            Patch {
                bucket: Some("other-bucket".into()),
                ..Patch::default()
            },
        ),
        (
            "region",
            Patch {
                region: Some("eu-west-1".into()),
                ..Patch::default()
            },
        ),
        (
            "addressing",
            Patch {
                addressing: Some(Addressing::VirtualHost),
                ..Patch::default()
            },
        ),
    ];
    for (field, patch) in patches {
        let err = f.svc.update_target(&alice(), &id, patch).await.unwrap_err();
        assert_eq!(err, StorageError::CredentialsRequired { field }, "{field}");
        let status = super::rpc::status_of(err);
        assert_eq!(status.code(), tonic::Code::InvalidArgument);
        assert!(status.message().starts_with(field), "{}", status.message());
    }
    // Nothing moved: same destination, same sealed pair, still enabled.
    let after = f.svc.get_target(&alice(), &id).await.unwrap().target;
    assert_eq!(
        (
            after.endpoint.as_str(),
            after.bucket.as_str(),
            after.region.as_str()
        ),
        (f.s3.endpoint().as_str(), BUCKET, "us-east-1")
    );
    assert_eq!(
        (after.state, after.key_gen),
        (State::Enabled, v.target.key_gen)
    );
    // Restating the destination is no move, and a name or prefix change needs no new pair.
    f.svc
        .update_target(
            &alice(),
            &id,
            Patch {
                endpoint: Some(f.s3.endpoint()),
                region: Some("us-east-1".into()),
                name: Some("renamed".into()),
                ..Patch::default()
            },
        )
        .await
        .unwrap();
    // With a new pair in the same request the move is allowed, and the target must be verified again.
    let moved = f
        .svc
        .update_target(
            &alice(),
            &id,
            Patch {
                region: Some("eu-west-1".into()),
                credentials: Some(Credentials::new("AKIANEWKEY0123456789", SK, "").unwrap()),
                ..Patch::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(
        (moved.target.region.as_str(), moved.target.state),
        ("eu-west-1", State::Pending)
    );
}

fn storage_service(f: &Fx) -> super::rpc::StorageService {
    use asc_rpc::auth::{Access, StaticTokens};
    super::rpc::StorageService::new(
        f.svc.clone(),
        Arc::new(
            StaticTokens::new()
                .with_token("alice-token", "7:alice", Access::Admin)
                .with_token("bob-token", "8:bob", Access::Admin),
        ),
    )
}

fn call<T>(msg: T, token: &str, op: &str) -> tonic::Request<T> {
    use asc_rpc::auth::{BearerToken, Credential};
    let mut r = tonic::Request::new(msg);
    r.extensions_mut()
        .insert(Credential::bearer(BearerToken::new(token).unwrap()));
    r.metadata_mut().insert("x-asc-op-id", op.parse().unwrap());
    r
}

fn alice_ref() -> Option<asc_rpc::server::wire::OwnerRef> {
    Some(asc_rpc::server::wire::OwnerRef {
        kind: asc_rpc::server::wire::OwnerKind::User as i32,
        name: "alice".into(),
    })
}

#[tokio::test]
async fn a_mutating_answer_is_for_the_owner_that_asked_and_carries_the_owner() {
    use asc_rpc::server::wire;
    use asc_rpc::services::storage_server::Storage;
    const OP_A: &str = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
    const OP_B: &str = "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb";
    let f = fx().await;
    let svc = storage_service(&f);
    let create = || wire::CreateStorageTargetRequest {
        owner: alice_ref(),
        name: "main".into(),
        endpoint: f.s3.endpoint(),
        region: "us-east-1".into(),
        bucket: BUCKET.into(),
        prefix: "pre/".into(),
        addressing: wire::StorageAddressing::Path as i32,
        credentials: Some(wire::StorageCredentials {
            access_key_id: AK.into(),
            secret_access_key: SK.into(),
            session_token: String::new(),
        }),
    };
    // The owner's answer names the owner, and a retry returns it.
    let made = svc
        .create_target(call(create(), "alice-token", OP_A))
        .await
        .unwrap()
        .into_inner()
        .target
        .unwrap();
    assert_eq!(made.owner, alice_ref());
    let again = svc
        .create_target(call(create(), "alice-token", OP_A))
        .await
        .unwrap()
        .into_inner()
        .target
        .unwrap();
    assert_eq!(again.id, made.id);
    // Another user cannot create in alice's name, and cannot read her answer back with her op id.
    let denied = svc
        .create_target(call(create(), "bob-token", OP_B))
        .await
        .unwrap_err();
    assert_eq!(denied.code(), tonic::Code::PermissionDenied);
    let stolen = svc
        .create_target(call(create(), "bob-token", OP_A))
        .await
        .unwrap_err();
    assert_eq!(stolen.code(), tonic::Code::PermissionDenied);
    assert!(!format!("{stolen:?}").contains(&made.id));
    // An update: the owner's answer carries the owner; bob gets NotFound for the target...
    let rename = |name: &str| wire::UpdateStorageTargetRequest {
        id: made.id.clone(),
        name: Some(name.into()),
        ..Default::default()
    };
    let updated = svc
        .update_target(call(rename("renamed"), "alice-token", OP_B))
        .await
        .unwrap()
        .into_inner()
        .target
        .unwrap();
    assert_eq!(
        (updated.owner.clone(), updated.name.as_str()),
        (alice_ref(), "renamed")
    );
    let denied = svc
        .update_target(call(
            rename("evil"),
            "bob-token",
            "cccccccc-cccc-4ccc-8ccc-cccccccccccc",
        ))
        .await
        .unwrap_err();
    assert_eq!(denied.code(), tonic::Code::NotFound);
    // ...and cannot fetch that answer by replaying alice's operation id.
    let stolen = svc
        .update_target(call(rename("evil"), "bob-token", OP_B))
        .await
        .unwrap_err();
    assert_eq!(stolen.code(), tonic::Code::NotFound);
    assert_eq!(
        f.svc
            .get_target(&alice(), &made.id)
            .await
            .unwrap()
            .target
            .name,
        "renamed"
    );
    // Even if the op id were presented against a target bob does administer, it is not his answer.
    f.table.grant_owner("8:bob", &owner_of("bob"));
    let bobs = f
        .svc
        .create_target(
            &bob(),
            NewFields {
                owner: owner_of("bob"),
                ..f.fields("bobs")
            },
        )
        .await
        .unwrap();
    let replayed = svc
        .update_target(call(
            wire::UpdateStorageTargetRequest {
                id: bobs.target.id,
                name: Some("x".into()),
                ..Default::default()
            },
            "bob-token",
            OP_B,
        ))
        .await
        .unwrap_err();
    assert_eq!(replayed.code(), tonic::Code::InvalidArgument);
}

#[tokio::test]
async fn vault_calls_are_for_site_administrators_and_their_answers_stay_with_them() {
    use asc_rpc::auth::{Access, StaticTokens};
    use asc_rpc::server::wire;
    use asc_rpc::services::vault_server::Vault as VaultApi;
    const OP: &str = "dddddddd-dddd-4ddd-8ddd-dddddddddddd";
    let f = fx().await;
    f.table.grant_admin("7:alice");
    f.table.grant_admin("9:root2");
    let svc = super::rpc::VaultService::new(
        f.vault.clone(),
        f.table.clone(),
        Arc::new(
            StaticTokens::new()
                .with_token("alice-token", "7:alice", Access::Admin)
                .with_token("bob-token", "8:bob", Access::Admin)
                .with_token("root2-token", "9:root2", Access::Admin),
        ),
        f.audit.clone(),
        f.pool.clone(),
    );
    let rotate = || wire::RotateVaultRequest { retire_old: false };
    // A user who is not a site administrator is refused before anything is read or done.
    let denied = svc
        .rotate(call(rotate(), "bob-token", OP))
        .await
        .unwrap_err();
    assert_eq!(denied.code(), tonic::Code::PermissionDenied);
    let denied = svc
        .status(call(wire::VaultStatusRequest {}, "bob-token", OP))
        .await
        .unwrap_err();
    assert_eq!(denied.code(), tonic::Code::PermissionDenied);
    // An administrator's retry gets the first answer; another administrator presenting the id does not.
    let first = svc
        .rotate(call(rotate(), "alice-token", OP))
        .await
        .unwrap()
        .into_inner();
    let again = svc
        .rotate(call(rotate(), "alice-token", OP))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(first.rotation_id, again.rotation_id);
    let other = svc
        .rotate(call(rotate(), "root2-token", OP))
        .await
        .unwrap_err();
    assert_eq!(other.code(), tonic::Code::InvalidArgument);
    assert!(!format!("{other:?}").contains(&first.rotation_id));
    // A non-administrator replaying the id learns nothing either.
    let denied = svc
        .rotate(call(rotate(), "bob-token", OP))
        .await
        .unwrap_err();
    assert_eq!(denied.code(), tonic::Code::PermissionDenied);
}
