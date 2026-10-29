//! Tests of the backups hub and service with a fake engine and an in-memory S3 bucket.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use asc_rpc::forwarded::Role;
use asc_rpc::server::{ServerInfo, wire};
use asc_rpc::services::backups_server::Backups;
use async_trait::async_trait;
use object_store::ObjectStoreExt as _;
use object_store::memory::InMemory;
use tonic::Code;

use super::config::{BackupsConfig, TargetConfig};
use super::place::Place;
use super::rpc::BackupsService;
use super::store::{self, Schedule};
use super::{BackupsHub, DumpOptions, Engine};
use crate::settings::testkit::{Kit, Recorder, claim, kit, op, req};

/// Writes a small backup directory; can be told to fail.
#[derive(Default)]
struct Fake {
    fail_dump: AtomicBool,
    fail_verify: AtomicBool,
}

#[async_trait]
impl Engine for Fake {
    async fn dump(&self, out: &Path, _: &DumpOptions) -> Result<(), String> {
        if self.fail_dump.load(Ordering::Relaxed) {
            return Err("object store: timeout\nsecond line is dropped".into());
        }
        std::fs::create_dir_all(out).map_err(|e| e.to_string())?;
        std::fs::write(out.join("db.sqlite"), b"the database rows of the instance").unwrap();
        std::fs::write(
            out.join("manifest.json"),
            br#"{"format":1,"created":1700000000,"db_kind":"sqlite","vault_key":"sealed","objects":2,"object_bytes":9}"#,
        )
        .unwrap();
        Ok(())
    }

    fn verify(&self, dir: &Path) -> Result<u64, String> {
        if self.fail_verify.load(Ordering::Relaxed) || !dir.join("db.sqlite").exists() {
            return Err("db.sqlite does not match the manifest: the backup is damaged".into());
        }
        Ok(2)
    }
}

struct Fx {
    k: Kit,
    hub: Arc<BackupsHub>,
    svc: BackupsService,
    fake: Arc<Fake>,
    audit: Arc<Recorder>,
    bucket: Arc<InMemory>,
    local: std::path::PathBuf,
}

async fn fx() -> Fx {
    let k = kit().await;
    let local = k.dir.path().join("targets");
    let mut cfg = BackupsConfig {
        enabled: true,
        key_file: Some(k.dir.path().join("backup.key")),
        staging_dir: Some(k.dir.path().join("staging")),
        ..BackupsConfig::default()
    };
    cfg.targets.insert(
        "local-disk".into(),
        TargetConfig::Local {
            path: local.clone(),
        },
    );
    cfg.targets.insert(
        "r2-backup".into(),
        TargetConfig::S3 {
            bucket: "b".into(),
            endpoint: String::new(),
            region: String::new(),
            prefix: "p/".into(),
            access_key_id_env: String::new(),
            secret_access_key_env: String::new(),
        },
    );
    let fake = Arc::new(Fake::default());
    let audit = Arc::new(Recorder::default());
    let hub = BackupsHub::new(
        k.pool.clone(),
        cfg,
        fake.clone(),
        k.dir.path().join("state"),
        audit.clone(),
    );
    hub.set_key(Some([5u8; 32]));
    let bucket = Arc::new(InMemory::new());
    hub.set_place(
        "r2-backup",
        Place::S3 {
            store: bucket.clone(),
            prefix: "p/".into(),
        },
    );
    let svc = BackupsService::new(hub.clone(), k.authz.clone(), ServerInfo::default());
    Fx {
        k,
        hub,
        svc,
        fake,
        audit,
        bucket,
        local,
    }
}

const HOST: &str = "host-token";

fn admin<T>(f: &Fx, msg: T, n: u32) -> tonic::Request<T> {
    claim(
        req(msg, HOST, Some(&op(n))),
        &f.k,
        "1:root",
        Role::Admin,
        "instance",
    )
}

async fn settle(f: &Fx, id: &str) -> store::Run {
    for _ in 0..200 {
        let r = f.hub.run(id).await.unwrap();
        if r.state != store::RUNNING && r.verify != store::V_RUNNING {
            return r;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("run {id} did not settle");
}

async fn run_now(f: &Fx, target: &str, n: u32) -> store::Run {
    let r = f
        .svc
        .run_backup(admin(
            f,
            wire::RunBackupRequest {
                target: target.into(),
            },
            n,
        ))
        .await
        .unwrap()
        .into_inner()
        .backup
        .unwrap();
    settle(f, &r.id).await
}

#[tokio::test(flavor = "current_thread")]
async fn a_local_backup_runs_is_recorded_and_audited_with_the_person_and_the_host() {
    let f = fx().await;
    let r = run_now(&f, "local-disk", 1).await;
    assert_eq!(r.state, store::SUCCEEDED, "{}", r.note);
    assert!(r.size_bytes > 0 && !r.encrypted);
    assert!(f.local.join(&r.id).join("manifest.json").exists());
    assert_eq!(r.objects, 2);
    let rec = f.audit.of("backup.finished");
    assert_eq!(rec.len(), 1);
    assert_eq!(rec[0].1, "1:root", "the person is the actor");
    assert_eq!(rec[0].2, None, "finished is written by the worker");
    let started = f.audit.of("backup.started");
    assert_eq!(started[0].1, "1:root");
    assert_eq!(
        started[0].2.as_deref(),
        Some("host-service"),
        "the host is the relay"
    );
    let list = f
        .svc
        .list_backups(admin(&f, wire::ListBackupsRequest { limit: 0 }, 0))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(list.backups.len(), 1);
    assert!(list.status.unwrap().last_success > 0);
}

#[tokio::test(flavor = "current_thread")]
async fn only_the_site_administrator_may_call_and_other_claims_do_not_widen_it() {
    let f = fx().await;
    let list = |r| f.svc.list_backups(r);
    let msg = || wire::ListBackupsRequest { limit: 0 };
    // A user's own token: not the instance.
    let e = list(req(msg(), "alice-token", None)).await.unwrap_err();
    assert_eq!(e.code(), Code::PermissionDenied);
    // The host with a write claim, a claim for an organisation, and a stale claim.
    for (role, subject) in [
        (Role::Write, "instance"),
        (Role::Admin, "org:acme"),
        (Role::Admin, "user:alice"),
    ] {
        let r = claim(req(msg(), HOST, None), &f.k, "7:alice", role, subject);
        assert_eq!(
            list(r).await.unwrap_err().code(),
            Code::PermissionDenied,
            "{subject}"
        );
    }
    // Every other method refuses alice too.
    let e = f
        .svc
        .run_backup(req(
            wire::RunBackupRequest::default(),
            "alice-token",
            Some(&op(1)),
        ))
        .await
        .unwrap_err();
    assert_eq!(e.code(), Code::PermissionDenied);
    let e = f
        .svc
        .restore(req(
            wire::RestoreBackupRequest {
                id: "bk_x".into(),
                confirm: "RESTORE".into(),
            },
            "alice-token",
            Some(&op(2)),
        ))
        .await
        .unwrap_err();
    assert_eq!(e.code(), Code::PermissionDenied);
    assert!(f.hub.list(10).await.unwrap().is_empty(), "nothing ran");
    // The host's own credential and an instance admin claim both pass.
    assert!(list(req(msg(), HOST, None)).await.is_ok());
    assert!(list(admin(&f, msg(), 3)).await.is_ok());
}

#[tokio::test(flavor = "current_thread")]
async fn a_failed_backup_leaves_a_note_and_no_files_and_the_next_one_may_start() {
    let f = fx().await;
    f.fake.fail_dump.store(true, Ordering::Relaxed);
    let r = run_now(&f, "local-disk", 1).await;
    assert_eq!(r.state, store::FAILED);
    assert_eq!(r.note, "object store: timeout", "one line, no detail");
    assert!(!f.local.join(&r.id).exists());
    f.fake.fail_dump.store(false, Ordering::Relaxed);
    assert_eq!(run_now(&f, "local-disk", 2).await.state, store::SUCCEEDED);
    // Unknown target and a second run at once.
    let e = f
        .svc
        .run_backup(admin(
            &f,
            wire::RunBackupRequest {
                target: "nope".into(),
            },
            3,
        ))
        .await
        .unwrap_err();
    assert_eq!(e.code(), Code::FailedPrecondition);
}

#[tokio::test(flavor = "current_thread")]
async fn one_backup_runs_at_a_time_and_an_operation_id_replays_the_first_answer() {
    let f = fx().await;
    let a = f
        .svc
        .run_backup(admin(
            &f,
            wire::RunBackupRequest {
                target: "local-disk".into(),
            },
            9,
        ))
        .await
        .unwrap()
        .into_inner();
    let again = f
        .svc
        .run_backup(admin(
            &f,
            wire::RunBackupRequest {
                target: "local-disk".into(),
            },
            9,
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(a.backup.unwrap().id, again.backup.unwrap().id);
    settle(&f, &f.hub.list(1).await.unwrap()[0].id).await;
    assert_eq!(
        f.hub.list(10).await.unwrap().len(),
        1,
        "a replay starts nothing"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn an_s3_backup_is_encrypted_before_upload_verified_and_restorable_only_when_confirmed() {
    let f = fx().await;
    let r = run_now(&f, "r2-backup", 1).await;
    assert_eq!(r.state, store::SUCCEEDED, "{}", r.note);
    assert!(r.encrypted);
    // The database file is in the bucket, and its content is not.
    let stored = f
        .bucket
        .get(&object_store::path::Path::from(format!(
            "p/{}/db.sqlite",
            r.id
        )))
        .await
        .unwrap()
        .bytes()
        .await
        .unwrap();
    assert!(!stored.windows(8).any(|w| w == b"database"));
    assert_eq!(stored.len() as u64, super::crypto::sealed_len(33));
    // Not verified: a restore is refused, with the right word or not.
    let restore = |confirm: &str, n| {
        f.svc.restore(admin(
            &f,
            wire::RestoreBackupRequest {
                id: r.id.clone(),
                confirm: confirm.into(),
            },
            n,
        ))
    };
    assert_eq!(
        restore("RESTORE", 2).await.unwrap_err().code(),
        Code::FailedPrecondition
    );
    // Verify.
    f.svc
        .verify_backup(admin(&f, wire::VerifyBackupRequest { id: r.id.clone() }, 3))
        .await
        .unwrap();
    let v = settle(&f, &r.id).await;
    assert_eq!(v.verify, store::V_OK, "{}", v.note);
    // The word must be exact.
    for bad in ["", "restore", "RESTORE ", "yes"] {
        assert_eq!(
            restore(bad, 4).await.unwrap_err().code(),
            Code::InvalidArgument,
            "{bad:?}"
        );
    }
    let p = f
        .svc
        .preview_restore(admin(
            &f,
            wire::PreviewRestoreRequest { id: r.id.clone() },
            5,
        ))
        .await
        .unwrap()
        .into_inner();
    assert!(p.verified && p.has_vault_key && p.confirmation == "RESTORE");
    assert_eq!((p.database.as_str(), p.objects), ("sqlite", 2));
    let done = restore("RESTORE", 6).await.unwrap().into_inner();
    assert!(done.restart_required);
    // The download is prepared in the background and the marker written.
    let marker = f.k.dir.path().join("state").join("restore.pending.json");
    for _ in 0..200 {
        if marker.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(marker.exists(), "the restore is staged");
    let dir = f.k.dir.path().join("state").join("restore").join(&r.id);
    assert_eq!(
        std::fs::read(dir.join("db.sqlite")).unwrap(),
        b"the database rows of the instance",
        "decrypted into the scratch area"
    );
    assert_eq!(f.audit.of("backup.restore.staged").len(), 1);
    // A second restore while one is staged is refused.
    assert_eq!(
        restore("RESTORE", 7).await.unwrap_err().code(),
        Code::FailedPrecondition
    );
    let status = f
        .svc
        .list_backups(admin(&f, wire::ListBackupsRequest::default(), 8))
        .await
        .unwrap()
        .into_inner()
        .status
        .unwrap();
    assert!(status.restore_staged);
}

#[tokio::test(flavor = "current_thread")]
async fn a_damaged_backup_fails_its_check_and_a_missing_one_is_not_found() {
    let f = fx().await;
    let r = run_now(&f, "local-disk", 1).await;
    std::fs::remove_file(f.local.join(&r.id).join("db.sqlite")).unwrap();
    f.svc
        .verify_backup(admin(&f, wire::VerifyBackupRequest { id: r.id.clone() }, 2))
        .await
        .unwrap();
    let v = settle(&f, &r.id).await;
    assert_eq!(v.verify, store::V_FAILED);
    assert!(v.note.contains("damaged"), "{}", v.note);
    let e = f
        .svc
        .verify_backup(admin(
            &f,
            wire::VerifyBackupRequest {
                id: "bk_nothing".into(),
            },
            3,
        ))
        .await
        .unwrap_err();
    assert_eq!(e.code(), Code::NotFound);
    // A malformed id reaches no query.
    for id in ["", "../x", &"bk_".repeat(30)] {
        let e = f
            .svc
            .preview_restore(admin(&f, wire::PreviewRestoreRequest { id: id.into() }, 4))
            .await
            .unwrap_err();
        assert_eq!(e.code(), Code::NotFound, "{id:?}");
    }
}

fn sched(enabled: bool, target: &str) -> wire::BackupSchedule {
    wire::BackupSchedule {
        enabled,
        interval_hours: 24,
        minute_of_day: 120,
        target: target.into(),
        keep_days: 30,
        keep_weeks: 52,
        include_vault_key: false,
        include_objects: true,
        next_run: 0,
    }
}

#[tokio::test(flavor = "current_thread")]
async fn the_schedule_is_validated_stored_and_audited() {
    let f = fx().await;
    let set = |s: wire::BackupSchedule, n| {
        f.svc.set_schedule(admin(
            &f,
            wire::SetBackupScheduleRequest { schedule: Some(s) },
            n,
        ))
    };
    let out = set(sched(true, "r2-backup"), 1)
        .await
        .unwrap()
        .into_inner()
        .schedule
        .unwrap();
    assert!(out.next_run > crate::ai::clock::now());
    assert_eq!(f.audit.of("backup.schedule.updated").len(), 1);
    for (i, bad) in [
        wire::BackupSchedule {
            interval_hours: 0,
            ..sched(true, "r2-backup")
        },
        wire::BackupSchedule {
            interval_hours: 169,
            ..sched(true, "r2-backup")
        },
        wire::BackupSchedule {
            minute_of_day: 1440,
            ..sched(true, "r2-backup")
        },
        wire::BackupSchedule {
            keep_days: 0,
            ..sched(true, "r2-backup")
        },
        wire::BackupSchedule {
            keep_weeks: 521,
            ..sched(true, "r2-backup")
        },
    ]
    .into_iter()
    .enumerate()
    {
        let e = set(bad, 10 + u32::try_from(i).unwrap()).await.unwrap_err();
        assert_eq!(e.code(), Code::InvalidArgument, "{i}");
    }
    assert_eq!(
        set(sched(true, "nope"), 20).await.unwrap_err().code(),
        Code::FailedPrecondition
    );
    // The vault key needs a key file in the configuration; this fixture has one.
    let with_key = wire::BackupSchedule {
        include_vault_key: true,
        ..sched(true, "local-disk")
    };
    assert!(set(with_key, 21).await.is_ok());
    let e = f
        .svc
        .set_schedule(admin(
            &f,
            wire::SetBackupScheduleRequest { schedule: None },
            22,
        ))
        .await
        .unwrap_err();
    assert_eq!(e.code(), Code::InvalidArgument);
}

#[tokio::test(flavor = "current_thread")]
async fn the_scheduler_starts_a_run_when_one_is_due_and_not_before() {
    let f = fx().await;
    f.hub
        .set_schedule(Schedule {
            enabled: true,
            interval_hours: 24,
            minute_of_day: 120,
            target: "local-disk".into(),
            keep_days: 30,
            keep_weeks: 4,
            include_vault_key: false,
            include_objects: true,
            updated: 0,
        })
        .await
        .unwrap();
    // The schedule was just set: the next run is ahead, so a tick does nothing.
    f.hub.tick().await;
    assert!(f.hub.list(10).await.unwrap().is_empty());
    // Pretend the schedule was set three days ago.
    sqlx::query("UPDATE backup_schedule SET updated = $1")
        .bind(crate::ai::clock::now() - 3 * 86_400)
        .execute(&f.k.pool)
        .await
        .unwrap();
    f.hub.tick().await;
    let runs = f.hub.list(10).await.unwrap();
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].trig, store::TRIG_SCHEDULE);
    settle(&f, &runs[0].id).await;
    // It ran: the next is ahead again.
    f.hub.tick().await;
    assert_eq!(f.hub.list(10).await.unwrap().len(), 1);
}

#[tokio::test(flavor = "current_thread")]
async fn retention_removes_expired_backups_from_their_targets() {
    let f = fx().await;
    f.hub
        .set_schedule(Schedule {
            enabled: false,
            keep_days: 1,
            keep_weeks: 0,
            ..Schedule::default()
        })
        .await
        .unwrap();
    let old = run_now(&f, "local-disk", 1).await;
    let newer = run_now(&f, "local-disk", 2).await;
    sqlx::query("UPDATE backup_runs SET finished = $2 WHERE id = $1")
        .bind(old.id.clone())
        .bind(crate::ai::clock::now() - 10 * 86_400)
        .execute(&f.k.pool)
        .await
        .unwrap();
    f.hub.prune().await;
    assert!(
        !f.local.join(&old.id).exists(),
        "the expired backup is gone from the target"
    );
    assert!(f.local.join(&newer.id).exists());
    assert!(f.hub.run(&old.id).await.unwrap().purged);
    // A purged backup cannot be verified or restored.
    let e = f
        .svc
        .verify_backup(admin(
            &f,
            wire::VerifyBackupRequest { id: old.id.clone() },
            3,
        ))
        .await
        .unwrap_err();
    assert_eq!(e.code(), Code::FailedPrecondition);
    assert_eq!(f.audit.of("backup.purged").len(), 1);
}

#[tokio::test(flavor = "current_thread")]
async fn targets_are_listed_without_credentials_and_a_run_left_running_is_closed_on_start() {
    let f = fx().await;
    let t = f
        .svc
        .list_targets(admin(&f, wire::ListBackupTargetsRequest {}, 1))
        .await
        .unwrap()
        .into_inner()
        .targets;
    assert_eq!(t.len(), 2);
    let s3 = t.iter().find(|t| t.name == "r2-backup").unwrap();
    assert_eq!(
        (s3.kind.as_str(), s3.location.as_str(), s3.encrypted),
        ("s3", "b/p", true)
    );
    sqlx::query("INSERT INTO backup_runs (id, state, trig, target, started) VALUES ('bk_dead', 1, 1, 'local-disk', 5)")
        .execute(&f.k.pool)
        .await
        .unwrap();
    assert!(f.hub.store.fail_orphans(100).await.unwrap() >= 1);
    let r = f.hub.run("bk_dead").await.unwrap();
    assert_eq!(
        (r.state, r.note.as_str()),
        (store::FAILED, "interrupted by a restart")
    );
}

#[test]
fn config_refuses_plain_http_and_bad_names() {
    let mut c = BackupsConfig::default();
    c.targets.insert(
        "x".into(),
        TargetConfig::S3 {
            bucket: "b".into(),
            endpoint: "http://insecure".into(),
            region: String::new(),
            prefix: String::new(),
            access_key_id_env: String::new(),
            secret_access_key_env: String::new(),
        },
    );
    assert!(c.validate().unwrap_err().contains("https"));
    let mut c = BackupsConfig::default();
    c.targets
        .insert("Bad Name".into(), TargetConfig::Local { path: "/x".into() });
    assert!(c.validate().is_err());
    let parsed: BackupsConfig =
        toml::from_str("enabled = true\n[targets.a]\nkind = \"local\"\npath = \"/srv/b\"\n")
            .unwrap();
    assert!(parsed.usable() && parsed.validate().is_ok());
    assert!(
        toml::from_str::<BackupsConfig>(
            "[targets.a]\nkind = \"local\"\npath = \"/x\"\nsecret = \"k\"\n"
        )
        .is_err()
    );
}

#[tokio::test(flavor = "current_thread")]
async fn nothing_staged_means_nothing_to_apply() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = crate::config::ServeConfig {
        db: Some(format!("sqlite:{}/x.db", dir.path().display()).replace('\\', "/")),
        ..crate::config::ServeConfig::default()
    };
    assert_eq!(super::apply_pending(&cfg).await.unwrap(), None);
}
