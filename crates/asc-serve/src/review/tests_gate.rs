//! Gate history, statistics and maintainer overrides: positive, permission-denied, unknown change,
//! and another repository's claim.

use asc_rpc::forwarded::Role;
use asc_rpc::server::wire;
use asc_rpc::services::review_server::Review;
use tonic::Code;

use super::tests::{ADMIN, ALICE, CAROL, DAVE, Fx, cbytes, fx, repo_ref, req};
use super::tests_acting::claimkit::{self, host};

fn rules(held: &[bool]) -> Vec<wire::GateRuleResult> {
    ["two approvals", "checks green", "no open threads"]
        .iter()
        .zip(held)
        .map(|(r, h)| wire::GateRuleResult { rule: (*r).to_owned(), held: *h })
        .collect()
}

async fn record(f: &Fx, token: &str, change: u8, version: u32, held: &[bool]) -> Result<wire::RecordGateDecisionResponse, tonic::Status> {
    f.review
        .record_gate_decision(req(
            wire::RecordGateDecisionRequest { repo: repo_ref(), change: cbytes(change), version, rules: rules(held) },
            token,
            false,
        ))
        .await
        .map(tonic::Response::into_inner)
}

async fn stats(f: &Fx, token: &str) -> wire::GetGateStatsResponse {
    f.review
        .get_gate_stats(req(wire::GetGateStatsRequest { repo: repo_ref(), days: 7 }, token, false))
        .await
        .unwrap()
        .into_inner()
}

#[tokio::test]
async fn the_gate_history_counts_checked_held_cleared_per_day_and_per_rule() {
    let f = fx().await;
    for who in [DAVE, CAROL] {
        assert_eq!(record(&f, who, 1, 2, &[false, false, false]).await.unwrap_err().code(), Code::PermissionDenied, "reporting needs an administrator");
    }
    assert_eq!(record(&f, ADMIN, 9, 1, &[false]).await.unwrap_err().code(), Code::NotFound);
    assert_eq!(record(&f, ADMIN, 1, 7, &[false]).await.unwrap_err().code(), Code::InvalidArgument, "unknown version");
    let long = wire::RecordGateDecisionRequest {
        repo: repo_ref(),
        change: cbytes(1),
        version: 0,
        rules: vec![wire::GateRuleResult { rule: "x".repeat(121), held: false }],
    };
    assert_eq!(f.review.record_gate_decision(req(long, ADMIN, false)).await.unwrap_err().code(), Code::InvalidArgument);
    // Change 1 passes at once; change 2 is held by two rules and clears later; change 3 stays held.
    let r = record(&f, ADMIN, 1, 0, &[false, false, false]).await.unwrap();
    assert!(r.passed && r.first_time && !r.cleared);
    let r = record(&f, ADMIN, 2, 1, &[true, false, true]).await.unwrap();
    assert!(!r.passed && !r.first_time);
    record(&f, ADMIN, 3, 1, &[false, true, false]).await.unwrap();
    let r = record(&f, ADMIN, 2, 1, &[false, false, false]).await.unwrap();
    assert!(r.passed && !r.first_time && r.cleared);
    let s = stats(&f, DAVE).await;
    assert_eq!((s.checked, s.passed_first_time, s.held, s.held_open, s.held_cleared), (3, 1, 2, 1, 1));
    assert_eq!(s.per_day.len(), 8, "every day of the window, zero days included");
    let today = s.per_day.last().unwrap();
    assert_eq!((today.checked, today.held), (3, 2));
    let rule = |n: &str| s.per_rule.iter().find(|r| r.rule == n).map(|r| (r.checked, r.held));
    assert_eq!(rule("checks green"), Some((3, 1)));
    assert_eq!(rule("two approvals"), Some((3, 0)), "the latest report of change 2 replaced the first");
    // Another tenant's claim reads nothing here.
    let (a, v) = claimkit::authorizer();
    let svc = super::rpc::ReviewService::new(
        std::sync::Arc::new(super::Reviews::new(f.reg.clone())),
        a,
        asc_rpc::server::ServerInfo::default(),
    );
    let m = || wire::GetGateStatsRequest { repo: repo_ref(), days: 0 };
    assert_eq!(svc.get_gate_stats(host(&v, m(), "42:alice", Role::Admin, "o/other", false)).await.unwrap_err().code(), Code::PermissionDenied);
    assert!(svc.get_gate_stats(host(&v, m(), "42:alice", Role::Read, "o/r", false)).await.is_ok());
}

#[tokio::test]
async fn an_override_needs_an_administrator_and_a_reason_and_is_chained_and_audited() {
    let f = fx().await;
    let grant = |token: &str, change: u8, rule: &str, reason: &str| {
        let r = req(
            wire::GrantOverrideRequest { repo: repo_ref(), change: cbytes(change), rule: rule.into(), reason: reason.into() },
            token,
            true,
        );
        let svc = &f.review;
        async move { svc.grant_override(r).await.map(tonic::Response::into_inner) }
    };
    for token in [DAVE, CAROL, ALICE] {
        assert_eq!(grant(token, 1, "two approvals", "urgent").await.unwrap_err().code(), Code::PermissionDenied, "only an administrator");
    }
    assert_eq!(grant(ADMIN, 1, "two approvals", "  ").await.unwrap_err().code(), Code::InvalidArgument, "a reason is required");
    assert_eq!(grant(ADMIN, 1, "", "urgent").await.unwrap_err().code(), Code::InvalidArgument);
    assert_eq!(grant(ADMIN, 1, "two approvals", &"x".repeat(501)).await.unwrap_err().code(), Code::InvalidArgument);
    assert_eq!(grant(ADMIN, 9, "two approvals", "urgent").await.unwrap_err().code(), Code::NotFound);
    let a = grant(ADMIN, 1, "two approvals", "the release is blocked").await.unwrap().r#override.unwrap();
    let b = grant(ADMIN, 2, "checks green", "flaky runner").await.unwrap().r#override.unwrap();
    assert_eq!((a.granted_by.as_str(), a.reason.as_str()), ("root", "the release is blocked"));
    assert_eq!(a.prev.len(), 32);
    assert_eq!(b.prev, a.proof, "each proof chains to the one before");
    assert_ne!(a.proof, b.proof);
    let list = |token: &str, change: Vec<u8>, active: bool| {
        let r = req(wire::ListOverridesRequest { repo: repo_ref(), change: change.into(), active_only: active, ..Default::default() }, token, false);
        let svc = &f.review;
        async move { svc.list_overrides(r).await.map(|r| r.into_inner().overrides) }
    };
    assert_eq!(list(DAVE, vec![], false).await.unwrap().len(), 2);
    assert_eq!(list(DAVE, cbytes(2).to_vec(), false).await.unwrap()[0].rule, "checks green");
    // Revoking is for administrators too; a revoked override is no longer active.
    let revoke = |token: &str, id: u64| {
        let r = req(wire::RevokeOverrideRequest { repo: repo_ref(), id }, token, false);
        let svc = &f.review;
        async move { svc.revoke_override(r).await.map(tonic::Response::into_inner) }
    };
    assert_eq!(revoke(CAROL, a.id).await.unwrap_err().code(), Code::PermissionDenied);
    assert_eq!(revoke(ADMIN, 999).await.unwrap_err().code(), Code::NotFound);
    let r = revoke(ADMIN, a.id).await.unwrap().r#override.unwrap();
    assert!(r.revoked_at > 0 && r.revoked_by == "root");
    assert_eq!(list(DAVE, vec![], true).await.unwrap().len(), 1);
    assert_eq!(stats(&f, DAVE).await.overrides_active, 1);
    assert!(claimkit::audited(f.reg.pool(), "review.override.granted").await.len() >= 2);
    // The hub's admin claim grants, as the person, with the host as relay; a write claim does not.
    let (auth, v) = claimkit::authorizer();
    let svc = super::rpc::ReviewService::new(
        std::sync::Arc::new(super::Reviews::new(f.reg.clone())),
        auth,
        asc_rpc::server::ServerInfo::default(),
    );
    let m = |rule: &str| wire::GrantOverrideRequest { repo: repo_ref(), change: cbytes(3), rule: rule.into(), reason: "from the hub".into() };
    assert_eq!(svc.grant_override(host(&v, m("r1"), "45:root", Role::Write, "o/r", true)).await.unwrap_err().code(), Code::PermissionDenied);
    assert_eq!(svc.grant_override(host(&v, m("r1"), "45:root", Role::Admin, "o/other", true)).await.unwrap_err().code(), Code::PermissionDenied);
    assert!(svc.grant_override(host(&v, m("r1"), "45:root", Role::Admin, "o/r", true)).await.is_ok());
    assert!(claimkit::audited(f.reg.pool(), "review.override.granted").await.contains(&("root".to_owned(), "host".to_owned())));
}
