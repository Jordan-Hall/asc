//! CI insights tests over SQLite: grouping across runs, replay, new versus trunk, stale results,
//! flaky detection, quarantine and its authorisation, bounds, secret masking and retention.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use asc_core::id::RefTarget;
use asc_core::path::RefName;
use asc_rpc::IdMode;
use asc_rpc::auth::{Access, Principal, StaticTokens, parse_authorization};
use asc_rpc::server::{CallCtx, RepoName, ServerInfo, wire};
use asc_rpc::services::ci_insights_server::CiInsights;
use asc_store::refs::{Actor, Expect, RefTxn, RefUpdate, Refs};
use sqlx::Row;
use tonic::{Code, Request};
use uuid::Uuid;

use super::rpc::CiInsightsService;
use super::{Insights, InsightsConfig, retention, store};
use crate::ingest::testutil::{commit, registry, repo};
use crate::registry::{Registry, RepoEntry};

const POLICY_ON: &str = "version = 1\n[roles]\nmaintainer = [\"user:alice\"]\n[ci]\nallow_quarantine = true\nflaky_threshold = 0.05\n";
const POLICY_OFF: &str = "version = 1\n[roles]\nmaintainer = [\"user:alice\"]\n";

struct Fx {
    reg: Arc<Registry>,
    entry: Arc<RepoEntry>,
    hub: Arc<Insights>,
    next_run: i64,
}

async fn fx() -> Fx {
    let reg = registry().await;
    let entry = repo(&reg).await;
    let hub = Arc::new(Insights::new(reg.clone(), InsightsConfig::default()));
    Fx {
        reg,
        entry,
        hub,
        next_run: 0,
    }
}

fn ctx(principal: &str) -> CallCtx {
    CallCtx {
        repo: RepoName::parse("o/r").unwrap(),
        id_mode: IdMode::Asc,
        principal: Principal {
            id: principal.into(),
        },
        op_id: Some(Uuid::new_v4()),
        client: None,
        admin: Default::default(),
    }
}

fn case(name: &str, outcome: wire::TestOutcome, message: &str) -> wire::TestCase {
    wire::TestCase {
        name: name.into(),
        outcome: outcome as i32,
        attempt: 1,
        duration_ms: 5,
        message: message.into(),
    }
}

fn fail(name: &str, message: &str) -> wire::TestCase {
    case(name, wire::TestOutcome::Failed, message)
}

fn pass(name: &str) -> wire::TestCase {
    case(name, wire::TestOutcome::Passed, "")
}

impl Fx {
    /// A run with one job `unit`, for `change` at `version` (empty and 0 for a trunk run).
    async fn run(&mut self, change: &[u8], version: i64) -> (u64, u64) {
        self.next_run += 1;
        let id = self.next_run;
        let ns = self.entry.ns.as_bytes().to_vec();
        let pool = self.reg.pool();
        sqlx::query(
            "INSERT INTO check_runs (ns, id, source, external_id, name, change, version, status, created) \
             VALUES ($1, $2, 'ci', $3, 'ci', $4, $5, 2, 1)",
        )
        .bind(ns.clone())
        .bind(id)
        .bind(format!("ext-{id}"))
        .bind(change.to_vec())
        .bind(version)
        .execute(pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO check_jobs (ns, id, run, name, status) VALUES ($1, $2, $2, 'unit', 2)",
        )
        .bind(ns)
        .bind(id)
        .execute(pool)
        .await
        .unwrap();
        (id as u64, id as u64)
    }

    async fn report(
        &self,
        run: (u64, u64),
        head: &[u8],
        cases: Vec<wire::TestCase>,
    ) -> Result<wire::ReportTestCasesResponse, tonic::Status> {
        self.hub
            .report(
                &ctx("7:ci"),
                wire::ReportTestCasesRequest {
                    repo: None,
                    run_id: run.0,
                    job_id: run.1,
                    cases,
                    head: head.to_vec().into(),
                },
            )
            .await
    }

    async fn groups(&self, req: wire::ListFailureGroupsRequest) -> Vec<wire::FailureGroup> {
        self.hub
            .list_groups(&ctx("7:alice"), req)
            .await
            .unwrap()
            .groups
    }

    async fn all_groups(&self) -> Vec<wire::FailureGroup> {
        self.groups(wire::ListFailureGroupsRequest::default()).await
    }

    /// Commits a trunk holding `policy` as `.asc/policy.toml` and points the default branch at it.
    async fn trunk_with_policy(&self, policy: &str) {
        let cs = commit(&self.entry, &[], &[(".asc/policy.toml", policy.as_bytes())]).await;
        self.reg
            .refs()
            .update(
                self.entry.key,
                &RefTxn {
                    updates: vec![RefUpdate {
                        name: RefName::new(b"refs/heads/master".as_slice()).unwrap(),
                        expect: Expect::Absent,
                        new: Some(RefTarget::Changeset(cs)),
                        expires_at: None,
                    }],
                    actor: Actor {
                        id: "7:alice".into(),
                        via: "test".into(),
                    },
                    reason: "test".into(),
                    op_id: Uuid::new_v4(),
                },
            )
            .await
            .unwrap();
    }

    async fn add_version(&self, change: &[u8], v: i64) {
        sqlx::query(
            "INSERT INTO change_versions (ns, change, v, cs, at, actor, op_id) VALUES ($1, $2, $3, $4, 1, 'a', $5)",
        )
        .bind(self.entry.ns.as_bytes().to_vec())
        .bind(change.to_vec())
        .bind(v)
        .bind(vec![v as u8; 32])
        .bind(Uuid::new_v4().as_bytes().to_vec())
        .execute(self.reg.pool())
        .await
        .unwrap();
    }
}

const CHANGE: [u8; 16] = [7; 16];

#[tokio::test]
async fn one_failure_in_two_runs_is_one_group() {
    let mut f = fx().await;
    let a = f.run(&CHANGE, 1).await;
    let b = f.run(&CHANGE, 1).await;
    f.report(
        a,
        b"h1",
        vec![
            fail(
                "t::adds",
                "thread 'x' panicked at src/lib.rs:10:5:\nassertion failed: got 3 want 4\n",
            ),
            pass("t::ok"),
        ],
    )
    .await
    .unwrap();
    let out = f
        .report(
            b,
            b"h1",
            vec![fail(
                "t::adds",
                "thread 'x' panicked at src/lib.rs:99:5:\nassertion failed: got 7 want 8\n",
            )],
        )
        .await
        .unwrap();
    assert_eq!((out.stored, out.groups_touched), (1, 1));
    let groups = f.all_groups().await;
    assert_eq!(groups.len(), 1);
    let g = &groups[0];
    assert_eq!((g.occurrences, g.changes_affected), (2, 1));
    assert_eq!(g.test_name, "t::adds");
    assert_eq!(g.job_name, "unit");
    assert_eq!(g.title, "assertion failed: got <n> want <n>");
    assert_eq!(g.state, wire::FailureGroupState::Open as i32);
    assert_eq!(g.signature.len(), 64);
    // A different failure of the same test is another group; the same message in another test too.
    let c = f.run(&CHANGE, 1).await;
    f.report(
        c,
        b"h1",
        vec![
            fail("t::adds", "timeout after 5s"),
            fail("t::other", "assertion failed: got 1 want 2"),
        ],
    )
    .await
    .unwrap();
    assert_eq!(f.all_groups().await.len(), 3);
}

#[tokio::test]
async fn a_replayed_report_replaces_instead_of_doubling() {
    let mut f = fx().await;
    let r = f.run(&CHANGE, 1).await;
    let cases = vec![fail("t::a", "boom 1"), fail("t::b", "bang 2"), pass("t::c")];
    f.report(r, b"h", cases.clone()).await.unwrap();
    f.report(r, b"h", cases).await.unwrap();
    let groups = f.all_groups().await;
    assert_eq!(groups.len(), 2);
    assert!(groups.iter().all(|g| g.occurrences == 1));
    let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM test_cases")
        .fetch_one(f.reg.pool())
        .await
        .unwrap();
    assert_eq!(n, 3);
    // A second report with other results drops the groups that lost their only occurrence.
    f.report(r, b"h", vec![fail("t::a", "boom 1")])
        .await
        .unwrap();
    let groups = f.all_groups().await;
    assert_eq!(groups.len(), 1);
    assert_eq!(groups[0].test_name, "t::a");
    // A report with no cases clears the run.
    f.report(r, b"h", vec![]).await.unwrap();
    assert!(f.all_groups().await.is_empty());
}

#[tokio::test]
async fn a_failure_is_new_until_the_trunk_has_it() {
    let mut f = fx().await;
    let pr = f.run(&CHANGE, 1).await;
    f.report(pr, b"h", vec![fail("t::a", "boom")])
        .await
        .unwrap();
    assert!(f.all_groups().await[0].new_vs_main);
    let trunk = f.run(b"", 0).await;
    f.report(trunk, b"m1", vec![fail("t::a", "boom")])
        .await
        .unwrap();
    let g = &f.all_groups().await[0];
    assert!(!g.new_vs_main, "the trunk fails the same way");
    assert_eq!((g.occurrences, g.changes_affected), (2, 1));
}

#[tokio::test]
async fn results_for_an_older_version_are_stale() {
    let mut f = fx().await;
    f.add_version(&CHANGE, 1).await;
    f.add_version(&CHANGE, 2).await;
    let old = f.run(&CHANGE, 1).await;
    let new = f.run(&CHANGE, 2).await;
    f.report(old, b"", vec![fail("t::a", "boom")])
        .await
        .unwrap();
    f.report(new, b"", vec![fail("t::a", "boom")])
        .await
        .unwrap();
    let id = f.all_groups().await[0].id;
    let got = f
        .hub
        .get_group(
            &ctx("7:alice"),
            wire::GetFailureGroupRequest {
                repo: None,
                id,
                limit: 0,
                page_token: String::new(),
            },
        )
        .await
        .unwrap();
    assert_eq!(got.occurrences.len(), 2);
    // Newest first: the run for version 2 is current, the one for version 1 is stale.
    let by_version: Vec<(u32, bool)> = got
        .occurrences
        .iter()
        .map(|o| (o.version, o.stale))
        .collect();
    assert_eq!(by_version, vec![(2, false), (1, true)]);
    // The head of each occurrence is the changeset of its version.
    assert_eq!(got.occurrences[0].head.as_ref(), &[2u8; 32]);
    assert_eq!(got.occurrences[1].head.as_ref(), &[1u8; 32]);
    // The same rule marks check runs.
    let mut conn = f.reg.pool().acquire().await.unwrap();
    let mut runs = vec![
        wire::CiRun {
            change: CHANGE.to_vec().into(),
            version: 1,
            ..Default::default()
        },
        wire::CiRun {
            change: CHANGE.to_vec().into(),
            version: 2,
            ..Default::default()
        },
        wire::CiRun {
            change: Default::default(),
            version: 0,
            ..Default::default()
        },
        wire::CiRun {
            change: vec![9; 16].into(),
            version: 3,
            ..Default::default()
        },
    ];
    store::mark_stale(&mut conn, f.entry.ns, &mut runs)
        .await
        .unwrap();
    assert_eq!(
        runs.iter().map(|r| r.stale).collect::<Vec<_>>(),
        vec![true, false, false, false]
    );
}

#[tokio::test]
async fn occurrences_page_newest_first() {
    let mut f = fx().await;
    for _ in 0..5 {
        let r = f.run(&CHANGE, 1).await;
        f.report(r, b"", vec![fail("t::a", "boom")]).await.unwrap();
    }
    let id = f.all_groups().await[0].id;
    let mut seen = Vec::new();
    let mut token = String::new();
    loop {
        let page = f
            .hub
            .get_group(
                &ctx("7:alice"),
                wire::GetFailureGroupRequest {
                    repo: None,
                    id,
                    limit: 2,
                    page_token: token.clone(),
                },
            )
            .await
            .unwrap();
        seen.extend(page.occurrences.iter().map(|o| o.run_id));
        if page.next_page_token.is_empty() {
            break;
        }
        token = page.next_page_token;
    }
    assert_eq!(seen, vec![5, 4, 3, 2, 1]);
    for bad in ["x", "1:2", "1:2:3:x", &"9".repeat(40)] {
        let e = f
            .hub
            .get_group(
                &ctx("7:alice"),
                wire::GetFailureGroupRequest {
                    repo: None,
                    id,
                    limit: 2,
                    page_token: bad.into(),
                },
            )
            .await
            .unwrap_err();
        assert_eq!(e.code(), Code::InvalidArgument, "{bad}");
    }
    let missing = f
        .hub
        .get_group(
            &ctx("7:alice"),
            wire::GetFailureGroupRequest {
                repo: None,
                id: 999,
                limit: 0,
                page_token: String::new(),
            },
        )
        .await
        .unwrap_err();
    assert_eq!(missing.code(), Code::NotFound);
}

#[tokio::test]
async fn groups_filter_and_page() {
    let mut f = fx().await;
    for i in 0..7 {
        let r = f.run(&CHANGE, 1).await;
        f.report(
            r,
            b"",
            vec![fail(
                &format!("t::n{i}"),
                &format!("distinct failure {i} here{}", "x".repeat(i)),
            )],
        )
        .await
        .unwrap();
    }
    let mut seen = Vec::new();
    let mut token = String::new();
    loop {
        let page = f
            .hub
            .list_groups(
                &ctx("7:alice"),
                wire::ListFailureGroupsRequest {
                    limit: 3,
                    page_token: token.clone(),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert!(page.groups.len() <= 3);
        seen.extend(page.groups.iter().map(|g| g.id));
        if page.next_page_token.is_empty() {
            break;
        }
        token = page.next_page_token;
    }
    seen.sort_unstable();
    assert_eq!(seen, (1..=7).collect::<Vec<_>>());
    let one = f
        .groups(wire::ListFailureGroupsRequest {
            test_name: "t::n3".into(),
            ..Default::default()
        })
        .await;
    assert_eq!(one.len(), 1);
    assert!(
        f.groups(wire::ListFailureGroupsRequest {
            job_name: "other".into(),
            ..Default::default()
        })
        .await
        .is_empty()
    );
    assert!(
        f.groups(wire::ListFailureGroupsRequest {
            states: vec![wire::FailureGroupState::Resolved as i32],
            ..Default::default()
        })
        .await
        .is_empty()
    );
    assert_eq!(
        f.groups(wire::ListFailureGroupsRequest {
            states: vec![wire::FailureGroupState::Open as i32],
            ..Default::default()
        })
        .await
        .len(),
        7
    );
    assert!(
        f.groups(wire::ListFailureGroupsRequest {
            since: i64::MAX,
            ..Default::default()
        })
        .await
        .is_empty()
    );
    let e = f
        .hub
        .list_groups(
            &ctx("7:alice"),
            wire::ListFailureGroupsRequest {
                states: vec![99],
                ..Default::default()
            },
        )
        .await
        .unwrap_err();
    assert_eq!(e.code(), Code::InvalidArgument);
}

#[tokio::test]
async fn a_test_that_flips_on_one_head_is_flaky() {
    let mut f = fx().await;
    // pass, fail, pass on the same head.
    for outcome in [
        wire::TestOutcome::Passed,
        wire::TestOutcome::Failed,
        wire::TestOutcome::Passed,
    ] {
        let r = f.run(&CHANGE, 1).await;
        f.report(
            r,
            b"same-head",
            vec![
                case("t::flaky", outcome, "timeout waiting for 3s"),
                pass("t::steady"),
            ],
        )
        .await
        .unwrap();
    }
    let flaky = f
        .hub
        .list_flaky(&ctx("7:alice"), wire::ListFlakyTestsRequest::default())
        .await
        .unwrap();
    assert_eq!(flaky.tests.len(), 1);
    let t = &flaky.tests[0];
    assert_eq!(
        (t.test_name.as_str(), t.job_name.as_str()),
        ("t::flaky", "unit")
    );
    assert_eq!((t.runs, t.flips), (3, 2));
    assert!((t.flake_rate - 2.0 / 3.0).abs() < 1e-9);
    assert_ne!(t.group_id, 0, "the failure group of the test");
    // The group says so, and `flaky_only` finds it.
    let groups = f
        .groups(wire::ListFailureGroupsRequest {
            flaky_only: true,
            ..Default::default()
        })
        .await;
    assert_eq!(groups.len(), 1);
    assert!(groups[0].flaky);
    // History by day.
    let h = f
        .hub
        .flaky_history(
            &ctx("7:alice"),
            wire::FlakyHistoryRequest {
                repo: None,
                test_name: "t::flaky".into(),
                job_name: String::new(),
                window_days: 0,
            },
        )
        .await
        .unwrap();
    assert_eq!(h.buckets.len(), 1);
    assert_eq!(
        (
            h.buckets[0].runs,
            h.buckets[0].failures,
            h.buckets[0].flaky_flips
        ),
        (3, 1, 2)
    );
    assert!(h.flaky);
    assert!(h.first_flaky > 0);
    // The steady test has none.
    let steady = f
        .hub
        .flaky_history(
            &ctx("7:alice"),
            wire::FlakyHistoryRequest {
                repo: None,
                test_name: "t::steady".into(),
                job_name: String::new(),
                window_days: 30,
            },
        )
        .await
        .unwrap();
    assert!(steady.buckets.is_empty() && !steady.flaky);
    // Neither name is an error.
    let e = f
        .hub
        .flaky_history(&ctx("7:alice"), wire::FlakyHistoryRequest::default())
        .await
        .unwrap_err();
    assert_eq!(e.code(), Code::InvalidArgument);
}

#[tokio::test]
async fn a_fix_on_a_new_head_is_not_flaky() {
    let mut f = fx().await;
    let a = f.run(&CHANGE, 1).await;
    f.report(a, b"head-a", vec![fail("t::a", "boom")])
        .await
        .unwrap();
    let b = f.run(&CHANGE, 2).await;
    f.report(b, b"head-b", vec![pass("t::a")]).await.unwrap();
    let flaky = f
        .hub
        .list_flaky(&ctx("7:alice"), wire::ListFlakyTestsRequest::default())
        .await
        .unwrap();
    assert!(flaky.tests.is_empty());
}

#[tokio::test]
async fn a_retry_inside_one_run_is_a_flip() {
    let mut f = fx().await;
    let r = f.run(&CHANGE, 1).await;
    let mut retry = pass("t::retried");
    retry.attempt = 2;
    f.report(r, b"", vec![fail("t::retried", "connection reset"), retry])
        .await
        .unwrap();
    let flaky = f
        .hub
        .list_flaky(&ctx("7:alice"), wire::ListFlakyTestsRequest::default())
        .await
        .unwrap();
    assert_eq!(flaky.tests.len(), 1);
    assert_eq!(flaky.tests[0].flips, 1);
}

#[tokio::test]
async fn the_policy_threshold_decides_what_is_flaky() {
    let mut f = fx().await;
    f.trunk_with_policy(POLICY_ON).await; // flaky_threshold = 0.05
    // Two flips (one failure) in 100 runs is a rate of 0.02: below 0.05.
    for i in 0..100 {
        let r = f.run(&CHANGE, 1).await;
        let c = if i == 50 {
            fail("t::rare", "boom")
        } else {
            pass("t::rare")
        };
        f.report(r, b"h", vec![c]).await.unwrap();
    }
    let flaky = f
        .hub
        .list_flaky(&ctx("7:alice"), wire::ListFlakyTestsRequest::default())
        .await
        .unwrap();
    assert!(flaky.tests.is_empty(), "{:?}", flaky.tests);
    let h = f
        .hub
        .flaky_history(
            &ctx("7:alice"),
            wire::FlakyHistoryRequest {
                repo: None,
                test_name: "t::rare".into(),
                job_name: String::new(),
                window_days: 30,
            },
        )
        .await
        .unwrap();
    assert!((h.flake_rate - 0.02).abs() < 1e-9);
    assert!(!h.flaky);
}

#[tokio::test]
async fn quarantine_needs_the_policy_and_a_maintainer() {
    let mut f = fx().await;
    let r = f.run(&CHANGE, 1).await;
    f.report(r, b"h", vec![fail("t::a", "boom")]).await.unwrap();
    let id = f.all_groups().await[0].id;
    let set = |maintainer: bool, state: wire::FailureGroupState| {
        let hub = f.hub.clone();
        async move {
            hub.set_state(
                &ctx("7:bob"),
                maintainer,
                wire::SetFailureGroupStateRequest {
                    repo: None,
                    id,
                    state: state as i32,
                    note: "known issue".into(),
                },
            )
            .await
        }
    };
    // No policy: quarantine is off, whoever asks.
    assert_eq!(
        set(true, wire::FailureGroupState::Quarantined)
            .await
            .unwrap_err()
            .code(),
        Code::FailedPrecondition
    );
    // Another state needs no maintainer.
    let ack = set(false, wire::FailureGroupState::Acknowledged)
        .await
        .unwrap();
    assert_eq!(
        ack.group.unwrap().state,
        wire::FailureGroupState::Acknowledged as i32
    );
    // A policy that does not allow it.
    f.trunk_with_policy(POLICY_OFF).await;
    assert_eq!(
        set(true, wire::FailureGroupState::Quarantined)
            .await
            .unwrap_err()
            .code(),
        Code::FailedPrecondition
    );
}

#[tokio::test]
async fn a_maintainer_can_quarantine_when_the_policy_allows_it() {
    let mut f = fx().await;
    f.trunk_with_policy(POLICY_ON).await;
    let r = f.run(&CHANGE, 1).await;
    f.report(r, b"h", vec![fail("t::a", "boom"), fail("t::b", "bang")])
        .await
        .unwrap();
    let groups = f.all_groups().await;
    let a = groups.iter().find(|g| g.test_name == "t::a").unwrap().id;
    let b = groups.iter().find(|g| g.test_name == "t::b").unwrap().id;
    let denied = f
        .hub
        .set_state(
            &ctx("7:bob"),
            false,
            wire::SetFailureGroupStateRequest {
                repo: None,
                id: a,
                state: wire::FailureGroupState::Quarantined as i32,
                note: String::new(),
            },
        )
        .await
        .unwrap_err();
    assert_eq!(denied.code(), Code::PermissionDenied);
    let c = ctx("7:alice");
    let ok = f
        .hub
        .set_state(
            &c,
            true,
            wire::SetFailureGroupStateRequest {
                repo: None,
                id: a,
                state: wire::FailureGroupState::Quarantined as i32,
                note: "flaky network".into(),
            },
        )
        .await
        .unwrap();
    let g = ok.group.unwrap();
    assert_eq!(
        (g.state, g.note.as_str()),
        (wire::FailureGroupState::Quarantined as i32, "flaky network")
    );
    // The same op id again answers the same, and does not write a second audit event.
    let again = f
        .hub
        .set_state(
            &c,
            true,
            wire::SetFailureGroupStateRequest {
                repo: None,
                id: a,
                state: wire::FailureGroupState::Quarantined as i32,
                note: "flaky network".into(),
            },
        )
        .await
        .unwrap();
    assert_eq!(again.group.unwrap().id, a);
    let events: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM audit_events WHERE scope_kind = 'repo' AND action = 'ci.failure_group.state'")
        .fetch_one(f.reg.pool())
        .await
        .unwrap();
    assert_eq!(events, 1);
    // The quarantine list is what a landing check reads: `t::a` no longer blocks, `t::b` does.
    {
        let mut conn = f.reg.pool().acquire().await.unwrap();
        let q = store::quarantine(&mut conn, f.entry.ns).await.unwrap();
        assert_eq!(q.len(), 1);
        let blocking = store::blocking_failures(&mut conn, f.entry.ns, r.0 as i64)
            .await
            .unwrap();
        assert_eq!(blocking.len(), 1);
        assert_eq!(
            (blocking[0].test.as_str(), blocking[0].group),
            ("t::b", b as i64)
        );
    }
    // Lifting the quarantine makes it block again.
    f.hub
        .set_state(
            &ctx("7:alice"),
            true,
            wire::SetFailureGroupStateRequest {
                repo: None,
                id: a,
                state: wire::FailureGroupState::Open as i32,
                note: String::new(),
            },
        )
        .await
        .unwrap();
    let mut conn = f.reg.pool().acquire().await.unwrap();
    assert_eq!(
        store::blocking_failures(&mut conn, f.entry.ns, r.0 as i64)
            .await
            .unwrap()
            .len(),
        2
    );
}

#[tokio::test]
async fn a_different_failure_of_a_quarantined_test_still_blocks() {
    let mut f = fx().await;
    f.trunk_with_policy(POLICY_ON).await;
    let r1 = f.run(&CHANGE, 1).await;
    f.report(r1, b"h", vec![fail("t::a", "timeout after 5s")])
        .await
        .unwrap();
    let id = f.all_groups().await[0].id;
    f.hub
        .set_state(
            &ctx("7:alice"),
            true,
            wire::SetFailureGroupStateRequest {
                repo: None,
                id,
                state: wire::FailureGroupState::Quarantined as i32,
                note: String::new(),
            },
        )
        .await
        .unwrap();
    let r2 = f.run(&CHANGE, 1).await;
    f.report(r2, b"h", vec![fail("t::a", "timeout after 9s")])
        .await
        .unwrap();
    let r3 = f.run(&CHANGE, 1).await;
    f.report(
        r3,
        b"h",
        vec![fail("t::a", "a totally different assertion")],
    )
    .await
    .unwrap();
    let mut conn = f.reg.pool().acquire().await.unwrap();
    assert!(
        store::blocking_failures(&mut conn, f.entry.ns, r2.0 as i64)
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        store::blocking_failures(&mut conn, f.entry.ns, r3.0 as i64)
            .await
            .unwrap()
            .len(),
        1
    );
}

#[tokio::test]
async fn a_failure_that_passes_on_retry_does_not_block() {
    let mut f = fx().await;
    let r = f.run(&CHANGE, 1).await;
    let mut retry = pass("t::a");
    retry.attempt = 2;
    f.report(
        r,
        b"h",
        vec![fail("t::a", "boom"), retry, fail("t::b", "bang")],
    )
    .await
    .unwrap();
    let mut conn = f.reg.pool().acquire().await.unwrap();
    let blocking = store::blocking_failures(&mut conn, f.entry.ns, r.0 as i64)
        .await
        .unwrap();
    assert_eq!(
        blocking.iter().map(|b| b.test.as_str()).collect::<Vec<_>>(),
        vec!["t::b"]
    );
}

#[tokio::test]
async fn a_resolved_failure_that_returns_reopens() {
    let mut f = fx().await;
    let r = f.run(&CHANGE, 1).await;
    f.report(r, b"h", vec![fail("t::a", "boom")]).await.unwrap();
    let id = f.all_groups().await[0].id;
    f.hub
        .set_state(
            &ctx("7:bob"),
            false,
            wire::SetFailureGroupStateRequest {
                repo: None,
                id,
                state: wire::FailureGroupState::Resolved as i32,
                note: "fixed".into(),
            },
        )
        .await
        .unwrap();
    let r2 = f.run(&CHANGE, 1).await;
    f.report(r2, b"h", vec![fail("t::a", "boom")])
        .await
        .unwrap();
    let g = &f.all_groups().await[0];
    assert_eq!(g.state, wire::FailureGroupState::Open as i32);
    assert_eq!(g.note, "fixed", "the note stays");
}

#[tokio::test]
async fn state_changes_are_validated_and_audited() {
    let mut f = fx().await;
    let r = f.run(&CHANGE, 1).await;
    f.report(r, b"h", vec![fail("t::a", "boom")]).await.unwrap();
    let id = f.all_groups().await[0].id;
    let set = |id: u64, state: i32, note: String, op: bool| {
        let hub = f.hub.clone();
        async move {
            let mut c = ctx("7:bob");
            if !op {
                c.op_id = None;
            }
            hub.set_state(
                &c,
                false,
                wire::SetFailureGroupStateRequest {
                    repo: None,
                    id,
                    state,
                    note,
                },
            )
            .await
        }
    };
    let open = wire::FailureGroupState::Open as i32;
    assert_eq!(
        set(id, 0, String::new(), true).await.unwrap_err().code(),
        Code::InvalidArgument
    );
    assert_eq!(
        set(id, 77, String::new(), true).await.unwrap_err().code(),
        Code::InvalidArgument
    );
    assert_eq!(
        set(0, open, String::new(), true).await.unwrap_err().code(),
        Code::InvalidArgument
    );
    assert_eq!(
        set(id, open, "x".repeat(2000), true)
            .await
            .unwrap_err()
            .code(),
        Code::InvalidArgument
    );
    assert_eq!(
        set(id, open, "a\u{7}b".into(), true)
            .await
            .unwrap_err()
            .code(),
        Code::InvalidArgument
    );
    assert_eq!(
        set(id, open, String::new(), false)
            .await
            .unwrap_err()
            .code(),
        Code::InvalidArgument,
        "needs an op id"
    );
    assert_eq!(
        set(999, open, String::new(), true)
            .await
            .unwrap_err()
            .code(),
        Code::NotFound
    );
    set(
        id,
        wire::FailureGroupState::Acknowledged as i32,
        "seen".into(),
        true,
    )
    .await
    .unwrap();
    let row = sqlx::query("SELECT actor_name, details_json, action FROM audit_events WHERE scope_kind = 'repo' AND action = 'ci.failure_group.state'")
        .fetch_one(f.reg.pool())
        .await
        .unwrap();
    let who: String = row.try_get("actor_name").unwrap();
    let details: String = row.try_get("details_json").unwrap();
    assert_eq!(who, "bob");
    assert!(
        details.contains("acknowledged") && details.contains("seen") && details.contains("t::a"),
        "{details}"
    );
}

#[tokio::test]
async fn bounds_are_enforced() {
    let mut f = fx().await;
    let r = f.run(&CHANGE, 1).await;
    let many = |n: usize| (0..n).map(|i| pass(&format!("t::{i}"))).collect::<Vec<_>>();
    assert_eq!(
        f.report(r, b"", many(5001)).await.unwrap_err().code(),
        Code::InvalidArgument
    );
    let out = f.report(r, b"", many(5000)).await.unwrap();
    assert_eq!(out.stored, 5000);
    assert_eq!(
        f.report((0, 1), b"", vec![]).await.unwrap_err().code(),
        Code::InvalidArgument
    );
    assert_eq!(
        f.report((1, 0), b"", vec![]).await.unwrap_err().code(),
        Code::InvalidArgument
    );
    assert_eq!(
        f.report((99, 99), b"", vec![]).await.unwrap_err().code(),
        Code::NotFound
    );
    assert_eq!(
        f.report((1, 2), b"", vec![]).await.unwrap_err().code(),
        Code::NotFound,
        "a job of another run"
    );
    assert_eq!(
        f.report(r, &[1u8; 65], vec![]).await.unwrap_err().code(),
        Code::InvalidArgument
    );
    assert_eq!(
        f.report(r, b"", vec![pass(&"n".repeat(513))])
            .await
            .unwrap_err()
            .code(),
        Code::InvalidArgument
    );
    assert_eq!(
        f.report(r, b"", vec![pass("")]).await.unwrap_err().code(),
        Code::InvalidArgument
    );
    assert_eq!(
        f.report(r, b"", vec![pass("a\nb")])
            .await
            .unwrap_err()
            .code(),
        Code::InvalidArgument
    );
    let mut unspecified = pass("x");
    unspecified.outcome = 0;
    assert_eq!(
        f.report(r, b"", vec![unspecified])
            .await
            .unwrap_err()
            .code(),
        Code::InvalidArgument
    );
    // A failure with no name is a job-level failure and is accepted.
    f.report(r, b"", vec![fail("", "the job crashed")])
        .await
        .unwrap();
    // The failed report did not touch what the good one stored.
    let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM test_cases")
        .fetch_one(f.reg.pool())
        .await
        .unwrap();
    assert_eq!(n, 1, "the last good report replaced the run");
}

#[tokio::test]
async fn messages_are_masked_and_cut_before_they_are_stored() {
    let mut f = fx().await;
    let r = f.run(&CHANGE, 1).await;
    let secret = "ghp_0123456789abcdefghijklmnopqrstuvwxyzAB";
    let long = format!("auth failed with token {secret}\n{}", "z".repeat(20_000));
    f.report(r, b"h", vec![fail("t::a", &long)]).await.unwrap();
    let rows = sqlx::query("SELECT message FROM test_cases")
        .fetch_all(f.reg.pool())
        .await
        .unwrap();
    let stored: String = rows[0].try_get("message").unwrap();
    assert!(!stored.contains(secret), "{stored}");
    assert!(stored.contains("***"));
    assert!(stored.len() <= super::MAX_MESSAGE);
    // It is not in the group either.
    let all = f.all_groups().await;
    assert!(!format!("{all:?}").contains(secret));
    let id = all[0].id;
    let got = f
        .hub
        .get_group(
            &ctx("7:alice"),
            wire::GetFailureGroupRequest {
                repo: None,
                id,
                limit: 0,
                page_token: String::new(),
            },
        )
        .await
        .unwrap();
    assert!(!format!("{got:?}").contains(secret));
}

#[tokio::test]
async fn the_repository_secret_rules_mask_too() {
    let mut f = fx().await;
    f.trunk_with_policy(
        "version = 1\n[roles]\nmaintainer = [\"user:alice\"]\n[secrets.rules.corp]\npattern = 'CORP-[0-9]{6}'\nseverity = \"high\"\ndescription = \"internal id\"\n",
    )
    .await;
    let r = f.run(&CHANGE, 1).await;
    f.report(r, b"h", vec![fail("t::a", "denied for CORP-123456 today")])
        .await
        .unwrap();
    let stored: String = sqlx::query_scalar("SELECT message FROM test_cases")
        .fetch_one(f.reg.pool())
        .await
        .unwrap();
    assert!(!stored.contains("CORP-123456"), "{stored}");
}

#[tokio::test]
async fn retention_deletes_old_cases_and_forgotten_groups() {
    let mut f = fx().await;
    let r = f.run(&CHANGE, 1).await;
    f.report(r, b"h", vec![fail("t::a", "boom"), pass("t::b")])
        .await
        .unwrap();
    let cfg = InsightsConfig {
        retention_days: 30,
        ..InsightsConfig::default()
    };
    // Within retention nothing goes.
    let d = retention::run_once(f.reg.pool(), &cfg, crate::review::now() + 86_400)
        .await
        .unwrap();
    assert_eq!((d.cases, d.groups), (0, 0));
    // A year later the cases go, then the group nobody refers to.
    let d = retention::run_once(f.reg.pool(), &cfg, crate::review::now() + 400 * 86_400)
        .await
        .unwrap();
    assert_eq!(d.cases, 2);
    assert_eq!(d.groups, 1);
    assert!(f.all_groups().await.is_empty());
    // Zero keeps cases; the daily totals still age out.
    let r2 = f.run(&CHANGE, 1).await;
    f.report(r2, b"h", vec![fail("t::a", "boom")])
        .await
        .unwrap();
    let keep = InsightsConfig {
        retention_days: 0,
        ..InsightsConfig::default()
    };
    let d = retention::run_once(f.reg.pool(), &keep, crate::review::now() + 400 * 86_400)
        .await
        .unwrap();
    assert_eq!((d.cases, d.groups), (0, 0));
}

#[tokio::test]
async fn a_triaged_group_survives_a_replacing_report() {
    let mut f = fx().await;
    let r = f.run(&CHANGE, 1).await;
    f.report(r, b"h", vec![fail("t::a", "boom")]).await.unwrap();
    let id = f.all_groups().await[0].id;
    f.hub
        .set_state(
            &ctx("7:bob"),
            false,
            wire::SetFailureGroupStateRequest {
                repo: None,
                id,
                state: wire::FailureGroupState::Acknowledged as i32,
                note: "known".into(),
            },
        )
        .await
        .unwrap();
    f.report(r, b"h", vec![pass("t::a")]).await.unwrap();
    let groups = f.all_groups().await;
    assert_eq!(groups.len(), 1, "an acknowledged group is kept");
    assert_eq!(groups[0].occurrences, 0);
}

#[tokio::test]
async fn config_bounds() {
    assert!(InsightsConfig::default().validate().is_ok());
    for bad in [
        InsightsConfig {
            retention_days: 3,
            ..Default::default()
        },
        InsightsConfig {
            retention_days: 4000,
            ..Default::default()
        },
        InsightsConfig {
            flaky_window_runs: 5,
            ..Default::default()
        },
        InsightsConfig {
            flaky_window_runs: 100_000,
            ..Default::default()
        },
    ] {
        assert!(bad.validate().is_err(), "{bad:?}");
    }
    assert!(
        InsightsConfig {
            retention_days: 0,
            ..Default::default()
        }
        .validate()
        .is_ok()
    );
}

// ------------------------------------------------------------------------------ access

fn req<T>(msg: T, token: Option<&str>) -> Request<T> {
    let mut r = Request::new(msg);
    if let Some(t) = token {
        let header = format!("Bearer {t}");
        r.extensions_mut()
            .insert(parse_authorization([header.as_bytes()]).unwrap().unwrap());
    }
    r
}

fn repo_ref() -> Option<wire::RepoRef> {
    Some(wire::RepoRef {
        name: "o/r".into(),
        id_mode: 0,
    })
}

#[tokio::test]
async fn each_call_checks_the_access_it_needs() {
    let mut f = fx().await;
    let r = f.run(&CHANGE, 1).await;
    let auth = Arc::new(
        StaticTokens::new()
            .with_token("reader-token", "7:rita", Access::Read)
            .with_token("writer-token", "8:walt", Access::Write)
            .with_token("admin-token", "9:ada", Access::Admin),
    );
    let svc = CiInsightsService::new(f.hub.clone(), auth, ServerInfo::default());
    let report = |token: Option<&str>| {
        req(
            wire::ReportTestCasesRequest {
                repo: repo_ref(),
                run_id: r.0,
                job_id: r.1,
                cases: vec![fail("t::a", "boom")],
                head: Default::default(),
            },
            token,
        )
    };
    // Only repository admin (the runner's service credential) reports.
    for token in [
        Some("reader-token"),
        Some("writer-token"),
        None,
        Some("nope"),
    ] {
        let e = svc.report_test_cases(report(token)).await.unwrap_err();
        assert!(
            matches!(e.code(), Code::PermissionDenied | Code::Unauthenticated),
            "{token:?}: {e}"
        );
    }
    svc.report_test_cases(report(Some("admin-token")))
        .await
        .unwrap();

    let list = |token: Option<&str>| {
        req(
            wire::ListFailureGroupsRequest {
                repo: repo_ref(),
                ..Default::default()
            },
            token,
        )
    };
    assert_eq!(
        svc.list_failure_groups(list(Some("reader-token")))
            .await
            .unwrap()
            .into_inner()
            .groups
            .len(),
        1
    );
    assert_eq!(
        svc.list_failure_groups(list(None))
            .await
            .unwrap_err()
            .code(),
        Code::Unauthenticated
    );
    assert_eq!(
        svc.list_flaky_tests(req(
            wire::ListFlakyTestsRequest {
                repo: repo_ref(),
                ..Default::default()
            },
            Some("reader-token")
        ))
        .await
        .unwrap()
        .into_inner()
        .tests
        .len(),
        0
    );
    assert_eq!(
        svc.flaky_history(req(
            wire::FlakyHistoryRequest {
                repo: repo_ref(),
                test_name: "t::a".into(),
                ..Default::default()
            },
            None
        ))
        .await
        .unwrap_err()
        .code(),
        Code::Unauthenticated
    );

    let id = svc
        .list_failure_groups(list(Some("reader-token")))
        .await
        .unwrap()
        .into_inner()
        .groups[0]
        .id;
    let set = |token: Option<&str>, state: wire::FailureGroupState| {
        let mut r = req(
            wire::SetFailureGroupStateRequest {
                repo: repo_ref(),
                id,
                state: state as i32,
                note: String::new(),
            },
            token,
        );
        r.metadata_mut()
            .insert("x-asc-op-id", Uuid::new_v4().to_string().parse().unwrap());
        r
    };
    let e = svc
        .set_failure_group_state(set(
            Some("reader-token"),
            wire::FailureGroupState::Acknowledged,
        ))
        .await
        .unwrap_err();
    assert_eq!(
        e.code(),
        Code::PermissionDenied,
        "a reader cannot change a group"
    );
    svc.set_failure_group_state(set(
        Some("writer-token"),
        wire::FailureGroupState::Acknowledged,
    ))
    .await
    .unwrap();
    // A writer is not a maintainer; no policy allows quarantine either way.
    let e = svc
        .set_failure_group_state(set(
            Some("writer-token"),
            wire::FailureGroupState::Quarantined,
        ))
        .await
        .unwrap_err();
    assert_eq!(e.code(), Code::FailedPrecondition);
    // A call without an op id is refused before anything changes.
    let mut no_op = set(Some("writer-token"), wire::FailureGroupState::Open);
    no_op.metadata_mut().remove("x-asc-op-id");
    assert_eq!(
        svc.set_failure_group_state(no_op).await.unwrap_err().code(),
        Code::InvalidArgument
    );
}

#[tokio::test]
async fn the_policy_maintainer_role_can_quarantine_without_repository_admin() {
    let mut f = fx().await;
    f.trunk_with_policy(POLICY_ON).await; // maintainer = user:alice
    let r = f.run(&CHANGE, 1).await;
    f.report(r, b"h", vec![fail("t::a", "boom")]).await.unwrap();
    let auth = Arc::new(
        StaticTokens::new()
            .with_token("alice-token", "7:alice", Access::Write)
            .with_token("walt-token", "8:walt", Access::Write),
    );
    let svc = CiInsightsService::new(f.hub.clone(), auth, ServerInfo::default());
    let id = f.all_groups().await[0].id;
    let quarantine = |token: &str| {
        let mut r = req(
            wire::SetFailureGroupStateRequest {
                repo: repo_ref(),
                id,
                state: wire::FailureGroupState::Quarantined as i32,
                note: String::new(),
            },
            Some(token),
        );
        r.metadata_mut()
            .insert("x-asc-op-id", Uuid::new_v4().to_string().parse().unwrap());
        r
    };
    assert_eq!(
        svc.set_failure_group_state(quarantine("walt-token"))
            .await
            .unwrap_err()
            .code(),
        Code::PermissionDenied
    );
    let ok = svc
        .set_failure_group_state(quarantine("alice-token"))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(
        ok.group.unwrap().state,
        wire::FailureGroupState::Quarantined as i32
    );
}
