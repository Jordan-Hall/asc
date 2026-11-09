//! Tests of the maintenance switch: validation, the stored row, the flag, the audit event and
//! the service's permissions.

use std::sync::Arc;

use asc_rpc::forwarded::Role;
use asc_rpc::server::{ServerInfo, wire};
use asc_rpc::services::maintenance_server::Maintenance;
use tonic::{Code, Request};

use super::rpc::MaintenanceService;
use super::{
    MaintenanceError, MaintenanceHub, Notice, ReadOnly, Setting, Switch, load, store, validate,
};
use crate::settings::testkit::{Kit, Recorder, claim, kit, op, req};

const NOW: i64 = 1_800_000_000;

struct Fx {
    k: Kit,
    hub: Arc<MaintenanceHub>,
    svc: MaintenanceService,
    audit: Arc<Recorder>,
}

async fn fx() -> Fx {
    let k = kit().await;
    let audit = Arc::new(Recorder::default());
    let hub = MaintenanceHub::new(k.pool.clone(), Arc::new(ReadOnly::default()), audit.clone());
    let svc = MaintenanceService::new(hub.clone(), k.authz.clone(), ServerInfo::default());
    Fx { k, hub, svc, audit }
}

fn on(message: &str, until: i64, version: &str) -> Switch {
    Switch {
        on: true,
        message: message.into(),
        until,
        version: version.into(),
    }
}

fn root<T>(f: &Fx, msg: T, n: Option<u32>) -> Request<T> {
    let op = n.map(op);
    claim(
        req(msg, "host-token", op.as_deref()),
        &f.k,
        "1:root",
        Role::Admin,
        "instance",
    )
}

fn set(on: bool, message: &str) -> wire::SetMaintenanceRequest {
    wire::SetMaintenanceRequest {
        on,
        message: message.into(),
        until: 0,
        version: String::new(),
    }
}

#[test]
fn a_switch_is_bounded_printable_and_not_in_the_past() {
    validate(&on("upgrading", NOW + 60, "2.4.0-rc.1+b7"), NOW).unwrap();
    validate(&on("", 0, ""), NOW).unwrap();
    // Off may carry any stored end; only switching on checks it against now.
    validate(
        &Switch {
            on: false,
            until: NOW - 60,
            ..Switch::default()
        },
        NOW,
    )
    .unwrap();
    let bad = [
        on(&"x".repeat(501), 0, ""),
        on("line\nbreak", 0, ""),
        on("bell\u{7}", 0, ""),
        on("", 0, "2.4 beta"),
        on("", 0, "v/1"),
        on("", 0, &"1".repeat(65)),
        on("", NOW - 1, ""),
        on("", -5, ""),
    ];
    for s in bad {
        assert!(
            matches!(validate(&s, NOW), Err(MaintenanceError::Invalid(_))),
            "{s:?}"
        );
    }
    // 500 characters, counted as characters and not bytes, are allowed.
    validate(&on(&"é".repeat(500), 0, ""), NOW).unwrap();
}

#[tokio::test]
async fn a_server_that_never_switched_is_off_and_a_switch_returns_the_old_row() {
    let k = kit().await;
    assert_eq!(load(&k.pool).await.unwrap(), Setting::default());
    let old = store(&k.pool, &on("disks", NOW + 600, "2.4.0"), "1:root", NOW)
        .await
        .unwrap();
    assert_eq!(old, Setting::default());
    let s = load(&k.pool).await.unwrap();
    assert_eq!(
        s,
        Setting {
            on: true,
            message: "disks".into(),
            until: NOW + 600,
            version: "2.4.0".into(),
            updated: NOW,
            updated_by: "1:root".into(),
        }
    );
    let old = store(&k.pool, &Switch::default(), super::OPERATOR, NOW + 1)
        .await
        .unwrap();
    assert_eq!(old, s);
    let s = load(&k.pool).await.unwrap();
    assert!(!s.on);
    assert_eq!(s.updated_by, "operator");
    // A refused switch leaves the row alone.
    assert!(
        store(&k.pool, &on("", NOW - 9, ""), "x", NOW + 2)
            .await
            .is_err()
    );
    assert_eq!(load(&k.pool).await.unwrap(), s);
}

#[test]
fn the_refusal_names_the_version_the_end_and_the_message() {
    let f = ReadOnly::default();
    assert!(f.refusal().is_none() && !f.is_read_only());
    f.set_notice(Some(Notice::default()));
    let r = f.refusal().unwrap();
    assert_eq!(r.code(), Code::Unavailable);
    assert_eq!(r.message(), "the server is read-only for maintenance");
    f.set_notice(Some(Notice {
        message: "new disks".into(),
        until: 1_767_225_600,
        version: "2.4.0".into(),
    }));
    let m = f.refusal().unwrap().message().to_owned();
    assert!(
        m.starts_with(
            "the server is read-only for maintenance (upgrading to 2.4.0) until 2026-01-01T00:00:00"
        ) && m.ends_with(": new disks"),
        "{m}"
    );
    // A staged restore wins: it cannot be undone without a restart.
    f.stage_restore();
    f.set_notice(None);
    assert!(f.is_read_only());
    assert!(
        f.refusal()
            .unwrap()
            .message()
            .contains("a restore is staged")
    );
}

#[tokio::test]
async fn every_node_follows_the_row_and_a_failed_read_keeps_the_flag() {
    let f = fx().await;
    // A second node shares the database, not the flag.
    let other = MaintenanceHub::new(
        f.k.pool.clone(),
        Arc::new(ReadOnly::default()),
        Arc::new(Recorder::default()),
    );
    f.hub
        .switch(&on("disks", 0, ""), "1:root", None, NOW)
        .await
        .unwrap();
    assert!(f.hub.flag().is_read_only(), "the switching node at once");
    assert!(!other.flag().is_read_only());
    other.refresh().await;
    assert_eq!(other.flag().notice().unwrap().message, "disks");
    f.k.pool.close().await;
    other.refresh().await;
    assert!(
        other.flag().is_read_only(),
        "an unreadable row ends nothing"
    );
}

#[tokio::test]
async fn a_switch_is_audited_with_the_old_and_new_state() {
    let f = fx().await;
    f.hub
        .switch(&on("disks", NOW + 60, "2.4.0"), "1:root", Some("hub"), NOW)
        .await
        .unwrap();
    let ev = f.audit.of("server.maintenance.updated");
    assert_eq!(ev.len(), 1);
    let (_, actor, relay, target, details) = &ev[0];
    assert_eq!(actor, "1:root");
    assert_eq!(relay.as_deref(), Some("hub"));
    assert_eq!(target, "maintenance");
    let d = |k: &str| details.iter().find(|(n, _)| n == k).unwrap().1.clone();
    assert_eq!(d("enabled"), "true");
    assert_eq!(d("old_enabled"), "false");
    assert_eq!(d("message"), "disks");
    assert_eq!(d("until"), (NOW + 60).to_string());
    assert_eq!(d("version"), "2.4.0");
    // A refused switch writes nothing.
    assert!(
        f.hub
            .switch(&on("", NOW - 1, ""), "1:root", None, NOW)
            .await
            .is_err()
    );
    assert_eq!(f.audit.of("server.maintenance.updated").len(), 1);
}

#[tokio::test]
async fn only_a_site_administrator_reads_or_switches_maintenance() {
    let f = fx().await;
    let denied = [
        req(wire::GetMaintenanceRequest {}, "alice-token", None),
        claim(
            req(wire::GetMaintenanceRequest {}, "host-token", None),
            &f.k,
            "2:alice",
            Role::Write,
            "instance",
        ),
    ];
    for r in denied {
        assert_eq!(
            f.svc.get_maintenance(r).await.unwrap_err().code(),
            Code::PermissionDenied
        );
    }
    let e = f
        .svc
        .set_maintenance(req(set(true, "x"), "alice-token", Some(&op(1))))
        .await
        .unwrap_err();
    assert_eq!(e.code(), Code::PermissionDenied);
    assert!(!f.hub.flag().is_read_only());
    assert!(f.audit.all().is_empty());
}

#[tokio::test]
async fn an_administrator_switches_maintenance_once_per_operation() {
    let f = fx().await;
    let e = f
        .svc
        .set_maintenance(root(&f, set(true, "disks"), None))
        .await
        .unwrap_err();
    assert_eq!(e.code(), Code::InvalidArgument, "a switch needs an op id");
    let s = f
        .svc
        .set_maintenance(root(&f, set(true, "  disks  "), Some(1)))
        .await
        .unwrap()
        .into_inner()
        .state
        .unwrap();
    assert!(s.on && !s.restore_staged);
    assert_eq!(s.message, "disks", "trimmed");
    assert_eq!(s.updated_by, "1:root");
    assert!(f.hub.flag().is_read_only());
    // A retry of the same operation is answered without a second switch.
    f.svc
        .set_maintenance(root(&f, set(true, "disks"), Some(1)))
        .await
        .unwrap();
    assert_eq!(f.audit.of("server.maintenance.updated").len(), 1);
    let bad = f
        .svc
        .set_maintenance(root(
            &f,
            wire::SetMaintenanceRequest {
                version: "not a version".into(),
                ..set(true, "")
            },
            Some(2),
        ))
        .await
        .unwrap_err();
    assert_eq!(bad.code(), Code::InvalidArgument);
    let got = f
        .svc
        .get_maintenance(root(&f, wire::GetMaintenanceRequest {}, None))
        .await
        .unwrap()
        .into_inner()
        .state
        .unwrap();
    assert!(got.on);
    assert_eq!(got.message, "disks");
    f.svc
        .set_maintenance(root(&f, set(false, ""), Some(3)))
        .await
        .unwrap();
    assert!(!f.hub.flag().is_read_only());
    f.hub.flag().stage_restore();
    let got = f
        .svc
        .get_maintenance(root(&f, wire::GetMaintenanceRequest {}, None))
        .await
        .unwrap()
        .into_inner()
        .state
        .unwrap();
    assert!(!got.on && got.restore_staged);
}

mod health {
    use std::sync::Arc;

    use asc_rpc::forwarded::Role;
    use asc_rpc::server::wire;
    use asc_rpc::services::maintenance_server::Maintenance;
    use futures::TryStreamExt;
    use object_store::ObjectStore;
    use object_store::local::LocalFileSystem;
    use object_store::memory::InMemory;
    use object_store::path::Path;
    use tonic::Code;

    use super::super::health::HealthProbes;
    use super::{fx, root};
    use crate::settings::testkit::{claim, req};
    use crate::signing::tests::{ALICE_BODY, server_signer};

    #[tokio::test]
    async fn the_site_administrator_probes_the_object_store_and_the_server_key() {
        let f = fx().await;
        // Before the probes are installed the server says it cannot answer.
        let e = f
            .svc
            .check_health(root(&f, wire::CheckHealthRequest {}, None))
            .await
            .unwrap_err();
        assert_eq!(e.code(), Code::Unimplemented);

        let dir = tempfile::tempdir().unwrap();
        let signer = server_signer(&dir, "server_key", ALICE_BODY);
        let store = Arc::new(InMemory::new());
        assert!(f.hub.set_health(HealthProbes::new(
            store.clone(),
            Path::from("root"),
            Some(signer.clone()),
        )));
        let r = f
            .svc
            .check_health(root(&f, wire::CheckHealthRequest {}, None))
            .await
            .unwrap()
            .into_inner();
        let ok = wire::HealthState::Ok as i32;
        assert_eq!(r.object_store.as_ref().unwrap().state, ok, "{r:?}");
        assert_eq!(r.server_key.as_ref().unwrap().state, ok, "{r:?}");
        assert_eq!(r.server_key_fingerprint, signer.public_key().fingerprint());
        assert!(r.checked_at_ms > 0 && r.now_ms >= r.checked_at_ms);
        let left: Vec<_> = store.list(None).try_collect().await.unwrap();
        assert!(left.is_empty(), "the probe object is deleted: {left:?}");

        // A second call within five seconds gets the same probes, and writes nothing more.
        let again = f
            .svc
            .check_health(root(&f, wire::CheckHealthRequest {}, None))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(again.checked_at_ms, r.checked_at_ms);

        // Only the site administrator.
        let denied = [
            req(wire::CheckHealthRequest {}, "alice-token", None),
            claim(
                req(wire::CheckHealthRequest {}, "host-token", None),
                &f.k,
                "2:alice",
                Role::Write,
                "instance",
            ),
        ];
        for d in denied {
            assert_eq!(
                f.svc.check_health(d).await.unwrap_err().code(),
                Code::PermissionDenied
            );
        }
    }

    #[tokio::test]
    async fn a_store_that_refuses_writes_fails_and_a_server_without_a_key_is_not_configured() {
        let dir = tempfile::tempdir().unwrap();
        let bucket = dir.path().join("bucket");
        std::fs::create_dir(&bucket).unwrap();
        let store = Arc::new(LocalFileSystem::new_with_prefix(&bucket).unwrap());
        // The place of the probe objects is a file, so no object can be written under it.
        std::fs::write(bucket.join("root"), b"not a directory").unwrap();
        let probes = HealthProbes::new(store, Path::from("root"), None);
        let h = probes.check(1).await;
        assert_eq!(h.object_store.state, wire::HealthState::Failed);
        assert!(h.object_store.detail.starts_with("write: "), "{h:?}");
        assert!(!h.object_store.detail.contains("bucket"), "no path: {h:?}");
        assert_eq!(h.server_key.state, wire::HealthState::NotConfigured);
        assert!(h.fingerprint.is_empty());
        assert_eq!(h.checked_at_ms, 1);
    }
}

mod runners {
    use std::sync::Arc;

    use asc_rpc::forwarded::Role;
    use asc_rpc::server::wire;
    use asc_rpc::services::maintenance_server::Maintenance;
    use tonic::Code;

    use super::{fx, root};
    use crate::agents::runners::{KEEP_SECS, Runners, name_of, valid_name};
    use crate::agents::runs::RunStore;
    use crate::ai::clock;
    use crate::settings::testkit::{claim, req};

    async fn add_run(pool: &sqlx::AnyPool, id: i64, state: i64, runner: &str) {
        sqlx::query(
            "INSERT INTO agent_runs (id, repo, agent, title, state, target, base, task, created_by, created, started, runner) \
             VALUES ($1, 'acme/api', 'fixer', $2, $3, 'main', $4, 'task', '1:root', $5, $5, $6)",
        )
        .bind(id)
        .bind(format!("Run {id}"))
        .bind(state)
        .bind(Vec::<u8>::new())
        .bind(clock::now())
        .bind(runner)
        .execute(pool)
        .await
        .unwrap();
    }

    async fn list(
        f: &super::Fx,
    ) -> Result<tonic::Response<wire::ListAgentRunnersResponse>, tonic::Status> {
        f.svc
            .list_agent_runners(root(f, wire::ListAgentRunnersRequest {}, None))
            .await
    }

    #[tokio::test]
    async fn the_site_administrator_sees_each_runner_with_its_slots_cpus_and_live_runs() {
        let f = fx().await;
        let pool = f.k.pool.clone();
        // A server with agents off has no runner table.
        assert_eq!(list(&f).await.unwrap_err().code(), Code::Unimplemented);

        let now = clock::now();
        let box1 = Arc::new(Runners::new(pool.clone(), "agent-box-1".into(), 4));
        let box2 = Runners::new(pool.clone(), "agent-box-2".into(), 2);
        let gone = Runners::new(pool.clone(), "agent-box-0".into(), 2);
        gone.beat(now - KEEP_SECS - 60).await.unwrap();
        box2.beat(now - 200).await.unwrap();
        box1.beat(now).await.unwrap();
        assert!(f.hub.set_runners(box1.clone()));

        // A queued run taken by box 1 carries its name; finished runs and runs of no runner are not listed.
        add_run(&pool, 214, 1, "").await;
        assert!(
            RunStore::new(pool.clone())
                .claim(214, box1.name())
                .await
                .unwrap()
        );
        assert!(
            !RunStore::new(pool.clone())
                .claim(214, "agent-box-2")
                .await
                .unwrap(),
            "taken once"
        );
        add_run(&pool, 215, 4, "agent-box-1").await;
        add_run(&pool, 216, 3, "agent-box-2").await;
        add_run(&pool, 217, 1, "").await;

        let r = list(&f).await.unwrap().into_inner();
        assert!(r.now >= now);
        let names: Vec<&str> = r.runners.iter().map(|x| x.name.as_str()).collect();
        assert_eq!(
            names,
            ["agent-box-1", "agent-box-2"],
            "online first; a runner offline for 7 days is removed"
        );
        let b1 = &r.runners[0];
        assert!(b1.online);
        assert_eq!(b1.slots, 4);
        assert!(b1.vcpu >= 1);
        assert_eq!(b1.version, env!("CARGO_PKG_VERSION"));
        assert!(b1.last_seen >= now && b1.started > 0);
        let ids: Vec<u64> = b1.runs.iter().map(|x| x.id).collect();
        assert_eq!(ids, [214]);
        assert_eq!(b1.runs[0].repo, "acme/api");
        assert_eq!(b1.runs[0].title, "Run 214");
        assert_eq!(b1.runs[0].agent, "fixer");
        let b2 = &r.runners[1];
        assert!(!b2.online);
        assert_eq!(b2.last_seen, now - 200);
        assert_eq!(
            b2.runs.iter().map(|x| x.id).collect::<Vec<_>>(),
            [216],
            "a run waiting for input is live"
        );

        // Only the site administrator.
        let denied = [
            req(wire::ListAgentRunnersRequest {}, "alice-token", None),
            claim(
                req(wire::ListAgentRunnersRequest {}, "host-token", None),
                &f.k,
                "2:alice",
                Role::Write,
                "instance",
            ),
        ];
        for d in denied {
            assert_eq!(
                f.svc.list_agent_runners(d).await.unwrap_err().code(),
                Code::PermissionDenied
            );
        }
    }

    #[test]
    fn runner_names_are_short_and_plain() {
        assert!(valid_name("agent-box-1"));
        assert!(valid_name("eu.box_2"));
        assert!(!valid_name(""));
        assert!(!valid_name("box 1"));
        assert!(!valid_name(&"a".repeat(64)));
        assert_eq!(name_of(" Box-1.local\n"), "Box-1.local");
        assert_eq!(name_of("bad name/€"), "badname");
        assert_eq!(name_of("\n"), "asc-serve");
        assert_eq!(name_of(&"x".repeat(80)).len(), 63);
    }

    #[test]
    fn the_runner_name_setting_is_checked() {
        let mut cfg = crate::agents::AgentsConfig {
            runner_name: Some("box 1".into()),
            ..Default::default()
        };
        assert!(cfg.validate().unwrap_err().contains("agents.runner_name"));
        cfg.runner_name = Some("agent-box-1".into());
        assert!(cfg.validate().is_ok());
    }
}
