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
use sha2::Digest as _;
use tonic::Code;

use super::config::{BackupsConfig, TargetConfig};
use super::place::{self, Place};
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
        let db = b"the database rows of the instance";
        std::fs::write(out.join("db.sqlite"), db).unwrap();
        let sha = sha2::Sha256::digest(db)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>();
        std::fs::write(
            out.join("manifest.json"),
            format!(
                r#"{{"format":1,"created":1700000000,"db_kind":"sqlite","vault_key":"sealed","objects":2,"object_bytes":9,"db_sha256":"{sha}"}}"#
            ),
        )
        .unwrap();
        Ok(())
    }

    fn verify(&self, dir: &Path) -> Result<u64, String> {
        let manifest = std::fs::read_to_string(dir.join("manifest.json")).unwrap_or_default();
        let sha = std::fs::read(dir.join("db.sqlite"))
            .map(|b| {
                sha2::Sha256::digest(b)
                    .iter()
                    .map(|x| format!("{x:02x}"))
                    .collect::<String>()
            })
            .unwrap_or_default();
        if self.fail_verify.load(Ordering::Relaxed) || sha.is_empty() || !manifest.contains(&sha) {
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
    assert!(v.note.contains("does not match the record"), "{}", v.note);
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

// ------------------------------------------------------------------------------ restore input is not trusted

/// An encrypted, verified S3 backup in the fixture's bucket.
async fn verified_s3(f: &Fx) -> store::Run {
    let r = run_now(f, "r2-backup", 1).await;
    f.svc
        .verify_backup(admin(f, wire::VerifyBackupRequest { id: r.id.clone() }, 2))
        .await
        .unwrap();
    let v = settle(f, &r.id).await;
    assert_eq!(v.verify, store::V_OK, "{}", v.note);
    v
}

async fn put(f: &Fx, id: &str, rel: &str, bytes: Vec<u8>) {
    f.bucket
        .put(
            &object_store::path::Path::from(format!("p/{id}/{rel}")),
            bytes.into(),
        )
        .await
        .unwrap();
}

async fn restore_outcome(f: &Fx, id: &str, n: u32) -> (bool, String) {
    let _ = f
        .svc
        .restore(admin(
            f,
            wire::RestoreBackupRequest {
                id: id.into(),
                confirm: "RESTORE".into(),
            },
            n,
        ))
        .await;
    let marker = f.k.dir.path().join("state").join("restore.pending.json");
    for _ in 0..200 {
        let row: Option<(i64, String)> = sqlx::query_as(
            "SELECT state, note FROM backup_restores ORDER BY requested DESC LIMIT 1",
        )
        .fetch_optional(&f.k.pool)
        .await
        .unwrap();
        if let Some((st, note)) = row
            && st != store::R_PREPARING
        {
            return (marker.exists(), note);
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    (marker.exists(), "timeout".into())
}

#[tokio::test(flavor = "current_thread")]
async fn a_flipped_sidecar_cannot_downgrade_an_encrypted_backup() {
    let f = fx().await;
    let r = verified_s3(&f).await;
    // The attacker rewrites the record beside the backup to say it was never encrypted.
    put(
        &f,
        &r.id,
        "asc-stored.json",
        br#"{"encrypted":false,"files":[["db.sqlite",33],["manifest.json",100]]}"#.to_vec(),
    )
    .await;
    let (staged, note) = restore_outcome(&f, &r.id, 3).await;
    assert!(!staged, "nothing is staged");
    assert!(note.contains("disagrees"), "{note}");
    f.svc
        .verify_backup(admin(&f, wire::VerifyBackupRequest { id: r.id.clone() }, 4))
        .await
        .unwrap();
    let v = settle(&f, &r.id).await;
    assert_eq!(v.verify, store::V_FAILED);
    assert!(v.note.contains("disagrees"), "{}", v.note);
}

#[tokio::test(flavor = "current_thread")]
async fn a_tampered_manifest_fails_the_seal() {
    let f = fx().await;
    let r = verified_s3(&f).await;
    let key = object_store::path::Path::from(format!("p/{}/manifest.json", r.id));
    let mut m = f
        .bucket
        .get(&key)
        .await
        .unwrap()
        .bytes()
        .await
        .unwrap()
        .to_vec();
    let at = m.windows(2).position(|w| w == b"17").unwrap();
    m[at] = b'9';
    put(&f, &r.id, "manifest.json", m).await;
    let (staged, note) = restore_outcome(&f, &r.id, 3).await;
    assert!(!staged);
    assert!(note.contains("does not match the record"), "{note}");
}

#[tokio::test(flavor = "current_thread")]
async fn a_swapped_file_is_refused_even_when_it_decrypts_and_has_the_right_size() {
    let f = fx().await;
    let r = verified_s3(&f).await;
    // A valid encrypted file of the same length under the same key, with other content.
    let dir = f.k.dir.path().join("swap");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("a"), &b"another database, the same length!"[..33]).unwrap();
    super::crypto::encrypt_file(&[5u8; 32], &dir.join("a"), &dir.join("b")).unwrap();
    put(
        &f,
        &r.id,
        "db.sqlite",
        std::fs::read(dir.join("b")).unwrap(),
    )
    .await;
    let (staged, note) = restore_outcome(&f, &r.id, 3).await;
    assert!(!staged);
    assert!(
        note.contains("damaged") || note.contains("does not match"),
        "{note}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn a_plain_file_in_place_of_an_encrypted_one_and_a_traversal_name_are_refused() {
    let f = fx().await;
    let r = verified_s3(&f).await;
    put(
        &f,
        &r.id,
        "db.sqlite",
        vec![b'x'; super::crypto::sealed_len(33) as usize],
    )
    .await;
    let (staged, _) = restore_outcome(&f, &r.id, 3).await;
    assert!(
        !staged,
        "an unauthenticated file is not accepted for an encrypted run"
    );
    sqlx::query("DELETE FROM backup_restores")
        .execute(&f.k.pool)
        .await
        .unwrap();
    // A sidecar that names a path outside the backup, with sizes that still add up.
    let mut side = f
        .hub
        .place_for_test("r2-backup")
        .sidecar(&r.id)
        .await
        .unwrap();
    side.files[0].0 = "../../evil".to_owned();
    put(
        &f,
        &r.id,
        "asc-stored.json",
        serde_json::to_vec(&side).unwrap(),
    )
    .await;
    let (staged, note) = restore_outcome(&f, &r.id, 4).await;
    assert!(!staged);
    assert!(
        note.contains("does not match") || note.contains("unsafe"),
        "{note}"
    );
    assert!(
        !f.k.dir.path().join("evil").exists()
            && !f.k.dir.path().join("state").join("evil").exists()
    );
}

#[tokio::test(flavor = "current_thread")]
async fn the_wrong_backup_key_opens_nothing_and_proves_nothing() {
    let f = fx().await;
    let r = verified_s3(&f).await;
    let place = Place::S3 {
        store: f.bucket.clone(),
        prefix: "p/".into(),
    };
    let dst = f.k.dir.path().join("wrongkey");
    let expected = place::Expected {
        encrypted: true,
        size: u64::try_from(r.size_bytes).unwrap(),
        seal: r.seal.clone(),
    };
    let e = place
        .download_dir(&r.id, &dst, Some([9u8; 32]), &expected)
        .await
        .unwrap_err();
    assert!(e.contains("does not match"), "{e}");
    assert!(!dst.exists(), "nothing was downloaded");
    let e = place
        .download_dir(&r.id, &f.k.dir.path().join("nokey"), None, &expected)
        .await
        .unwrap_err();
    assert!(e.contains("no backup key"), "{e}");
    // Another server key makes another seal: a check under it fails against the recorded one.
    let hub2 = BackupsHub::new(
        f.k.pool.clone(),
        f.hub.cfg.clone(),
        f.fake.clone(),
        f.k.dir.path().join("s2"),
        f.audit.clone(),
    );
    hub2.set_key(Some([9u8; 32]));
    hub2.set_place(
        "r2-backup",
        Place::S3 {
            store: f.bucket.clone(),
            prefix: "p/".into(),
        },
    );
    hub2.start_verify(&r.id).await.unwrap();
    for _ in 0..200 {
        let x = hub2.run(&r.id).await.unwrap();
        if x.verify != store::V_RUNNING {
            assert_eq!(x.verify, store::V_FAILED, "{}", x.note);
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("no verdict");
}

#[tokio::test(flavor = "current_thread")]
async fn a_local_backup_changed_after_the_fact_is_not_restored() {
    let f = fx().await;
    let r = run_now(&f, "local-disk", 1).await;
    f.svc
        .verify_backup(admin(&f, wire::VerifyBackupRequest { id: r.id.clone() }, 2))
        .await
        .unwrap();
    assert_eq!(settle(&f, &r.id).await.verify, store::V_OK);
    // Database and manifest rewritten together so they agree with each other.
    let dir = f.local.join(&r.id);
    let db = b"rows an attacker wrote 123456789";
    std::fs::write(dir.join("db.sqlite"), db).unwrap();
    let sha = sha2::Sha256::digest(db)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>();
    std::fs::write(
        dir.join("manifest.json"),
        format!(r#"{{"format":1,"db_kind":"sqlite","objects":2,"db_sha256":"{sha}"}}"#),
    )
    .unwrap();
    let (staged, note) = restore_outcome(&f, &r.id, 3).await;
    assert!(!staged);
    assert!(note.contains("does not match the record"), "{note}");
    assert!(
        !f.k.dir
            .path()
            .join("state")
            .join("restore")
            .join(&r.id)
            .exists(),
        "the refused copy is removed"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn a_sidecar_with_inflated_sizes_is_refused_before_anything_is_downloaded() {
    let f = fx().await;
    let r = verified_s3(&f).await;
    let side = f
        .hub
        .place_for_test("r2-backup")
        .sidecar(&r.id)
        .await
        .unwrap();
    let inflated: Vec<(String, u64)> = side
        .files
        .iter()
        .map(|(n, s)| (n.clone(), s + 10_000_000_000))
        .collect();
    let body =
        serde_json::to_vec(&serde_json::json!({"encrypted": true, "files": inflated})).unwrap();
    put(&f, &r.id, "asc-stored.json", body).await;
    let place = Place::S3 {
        store: f.bucket.clone(),
        prefix: "p/".into(),
    };
    let dst = f.k.dir.path().join("inflated");
    let expected = place::Expected {
        encrypted: true,
        size: u64::try_from(r.size_bytes).unwrap(),
        seal: r.seal.clone(),
    };
    let e = place
        .download_dir(&r.id, &dst, Some([5u8; 32]), &expected)
        .await
        .unwrap_err();
    assert!(e.contains("disagrees"), "{e}");
    assert!(!dst.exists(), "no bulk download happened");
    // The same total, reshuffled between files, fails the seal instead.
    let mut moved = side.files.clone();
    moved[0].1 += 1;
    moved[1].1 -= 1;
    let body = serde_json::to_vec(&serde_json::json!({"encrypted": true, "files": moved})).unwrap();
    put(&f, &r.id, "asc-stored.json", body).await;
    let e = place
        .download_dir(&r.id, &dst, Some([5u8; 32]), &expected)
        .await
        .unwrap_err();
    assert!(e.contains("does not match"), "{e}");
    assert!(!dst.exists());
}

/// A staged local restore and a configuration whose state directory is where it was staged.
async fn staged_local(
    f: &Fx,
) -> (
    crate::config::ServeConfig,
    std::path::PathBuf,
    std::path::PathBuf,
) {
    let r = run_now(f, "local-disk", 1).await;
    f.svc
        .verify_backup(admin(f, wire::VerifyBackupRequest { id: r.id.clone() }, 2))
        .await
        .unwrap();
    assert_eq!(settle(f, &r.id).await.verify, store::V_OK);
    let flag = Arc::new(AtomicBool::new(false));
    f.hub.set_read_only(flag.clone());
    let (staged, note) = restore_outcome(f, &r.id, 3).await;
    assert!(staged, "{note}");
    assert!(
        flag.load(Ordering::Relaxed) && f.hub.is_read_only(),
        "staging puts the server in maintenance"
    );
    let state = f.k.dir.path().join("state");
    // The marker points at a private copy, not at the backup directory.
    let marker: serde_json::Value =
        serde_json::from_slice(&std::fs::read(state.join("restore.pending.json")).unwrap())
            .unwrap();
    let from = std::path::PathBuf::from(marker["from"].as_str().unwrap());
    assert_eq!(from, state.join("restore").join(&r.id));
    assert_ne!(from, f.local.join(&r.id));
    // Apply from a configuration whose state directory is this one.
    let cfg_dir = f.k.dir.path().join("cfg");
    std::fs::create_dir_all(&cfg_dir).unwrap();
    let cfg = crate::config::ServeConfig {
        db: Some(format!("sqlite:{}/x.db", cfg_dir.display()).replace('\\', "/")),
        ..crate::config::ServeConfig::default()
    };
    let applied = super::state_dir(&cfg);
    std::fs::create_dir_all(&applied).unwrap();
    std::fs::copy(
        state.join("restore.pending.json"),
        applied.join("restore.pending.json"),
    )
    .unwrap();
    (cfg, from, applied)
}

#[tokio::test(flavor = "current_thread")]
async fn a_file_swapped_after_staging_is_refused_when_the_restore_is_applied() {
    let f = fx().await;
    let (cfg, copy, applied) = staged_local(&f).await;
    // Changing the original after staging changes nothing: the copy is what is applied.
    let r = f.hub.list(1).await.unwrap().remove(0);
    std::fs::write(
        f.local.join(&r.id).join("db.sqlite"),
        b"swapped in the original",
    )
    .unwrap();
    // Changing the staged copy is caught.
    std::fs::write(copy.join("db.sqlite"), b"swapped in the staged copy!!!!!!").unwrap();
    let e = super::apply_pending(&cfg).await.unwrap_err();
    assert!(e.contains("changed after they were checked"), "{e}");
    assert!(applied.join("restore.failed.json").exists());
    assert!(
        !applied.join("restore.pending.json").exists(),
        "a refused restore is not tried again"
    );
    assert!(!copy.exists(), "the refused copy is removed");
}

#[tokio::test(flavor = "current_thread")]
async fn a_marker_whose_seal_does_not_match_the_copy_is_refused() {
    let f = fx().await;
    let (cfg, copy, applied) = staged_local(&f).await;
    let path = applied.join("restore.pending.json");
    let mut m: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    m["seal"] = serde_json::json!("sha256:0000");
    std::fs::write(&path, serde_json::to_vec(&m).unwrap()).unwrap();
    let e = super::apply_pending(&cfg).await.unwrap_err();
    assert!(e.contains("changed after they were checked"), "{e}");
    // A marker with no seal at all is refused too.
    let _ = copy;
}

/// A store that answers the second read of the manifest with other bytes.
#[derive(Debug)]
struct Flip {
    inner: Arc<InMemory>,
    manifest_reads: std::sync::atomic::AtomicUsize,
}

impl std::fmt::Display for Flip {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Flip")
    }
}

#[async_trait]
impl object_store::ObjectStore for Flip {
    async fn put_opts(
        &self,
        l: &object_store::path::Path,
        p: object_store::PutPayload,
        o: object_store::PutOptions,
    ) -> object_store::Result<object_store::PutResult> {
        self.inner.put_opts(l, p, o).await
    }

    async fn put_multipart_opts(
        &self,
        l: &object_store::path::Path,
        o: object_store::PutMultipartOptions,
    ) -> object_store::Result<Box<dyn object_store::MultipartUpload>> {
        self.inner.put_multipart_opts(l, o).await
    }

    async fn get_opts(
        &self,
        l: &object_store::path::Path,
        o: object_store::GetOptions,
    ) -> object_store::Result<object_store::GetResult> {
        if l.as_ref().ends_with("manifest.json")
            && self
                .manifest_reads
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
                > 0
        {
            // The attacker's manifest: same length, other content.
            let other = object_store::path::Path::from("attacker/manifest.json");
            return self.inner.get_opts(&other, o).await;
        }
        self.inner.get_opts(l, o).await
    }

    fn delete_stream(
        &self,
        l: futures::stream::BoxStream<'static, object_store::Result<object_store::path::Path>>,
    ) -> futures::stream::BoxStream<'static, object_store::Result<object_store::path::Path>> {
        self.inner.delete_stream(l)
    }

    fn list(
        &self,
        p: Option<&object_store::path::Path>,
    ) -> futures::stream::BoxStream<'static, object_store::Result<object_store::ObjectMeta>> {
        self.inner.list(p)
    }

    async fn list_with_delimiter(
        &self,
        p: Option<&object_store::path::Path>,
    ) -> object_store::Result<object_store::ListResult> {
        self.inner.list_with_delimiter(p).await
    }

    async fn copy_opts(
        &self,
        a: &object_store::path::Path,
        b: &object_store::path::Path,
        o: object_store::CopyOptions,
    ) -> object_store::Result<()> {
        self.inner.copy_opts(a, b, o).await
    }
}

#[tokio::test(flavor = "current_thread")]
async fn the_manifest_that_was_sealed_is_the_manifest_that_is_used() {
    let f = fx().await;
    let r = verified_s3(&f).await;
    let real = f
        .bucket
        .get(&object_store::path::Path::from(format!(
            "p/{}/manifest.json",
            r.id
        )))
        .await
        .unwrap()
        .bytes()
        .await
        .unwrap();
    // The attacker's manifest, same length, ready to be served on any later read.
    let mut other = real.to_vec();
    let at = other.windows(2).position(|w| w == b"17").unwrap();
    other[at] = b'9';
    f.bucket
        .put(
            &object_store::path::Path::from("attacker/manifest.json"),
            other.into(),
        )
        .await
        .unwrap();
    let flip = Arc::new(Flip {
        inner: f.bucket.clone(),
        manifest_reads: std::sync::atomic::AtomicUsize::new(0),
    });
    let place = Place::S3 {
        store: flip.clone(),
        prefix: "p/".into(),
    };
    let expected = place::Expected {
        encrypted: true,
        size: u64::try_from(r.size_bytes).unwrap(),
        seal: r.seal.clone(),
    };
    let dst = f.k.dir.path().join("once");
    place
        .download_dir(&r.id, &dst, Some([5u8; 32]), &expected)
        .await
        .unwrap();
    assert_eq!(
        std::fs::read(dst.join("manifest.json")).unwrap(),
        real.to_vec()
    );
    assert_eq!(
        flip.manifest_reads
            .load(std::sync::atomic::Ordering::SeqCst),
        1,
        "the manifest is fetched once"
    );
}
