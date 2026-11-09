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
