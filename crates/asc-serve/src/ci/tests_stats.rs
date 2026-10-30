//! `Ci.Stats` and `Ci.CompareWithMain`: counts, the previous period, failure groups, flaky tests,
//! durations, new failures against main, and the refusals.

use std::sync::Arc;

use asc_rpc::forwarded::Role;
use asc_rpc::server::{ServerInfo, wire};
use asc_rpc::services::ci_server::Ci;
use bytes::Bytes;
use tonic::Code;

use crate::review::Reviews;
use crate::review::tests::{ADMIN, ALICE, Fx, cbytes, fx, repo_ref, req};
use crate::review::tests_acting::claimkit::{self, host};

use wire::CiStatus as C;

async fn run(
    f: &Fx,
    id: &str,
    change: u8,
    status: C,
    jobs: &[(&str, i64)],
    secs: i64,
    age_days: i64,
) -> i64 {
    let r =
        f.ci.report_run(req(
            wire::ReportRunRequest {
                repo: repo_ref(),
                source: "actions".into(),
                external_id: id.into(),
                name: "ci".into(),
                change: if change == 0 {
                    Bytes::new()
                } else {
                    cbytes(change)
                },
                status: status as i32,
                jobs: jobs
                    .iter()
                    .map(|(n, _)| wire::ReportJob {
                        name: (*n).into(),
                        status: status as i32,
                        ..Default::default()
                    })
                    .collect(),
                ..Default::default()
            },
            ADMIN,
            false,
        ))
        .await
        .unwrap()
        .into_inner();
    let rid = i64::try_from(r.run.unwrap().id).unwrap();
    let now = crate::review::now() - age_days * 86_400;
    sqlx::query(
        "UPDATE check_runs SET created = $2, started = $3, finished = $4 WHERE id = $1 AND ns = $5",
    )
    .bind(rid)
    .bind(now)
    .bind(now)
    .bind(now + secs)
    .bind(f.e.ns.as_bytes().to_vec())
    .execute(f.reg.pool())
    .await
    .unwrap();
    for (name, dur) in jobs {
        sqlx::query("UPDATE check_jobs SET started = 1000, finished = $3 WHERE run = $1 AND name = $2 AND ns = $4")
            .bind(rid)
            .bind((*name).to_owned())
            .bind(1000 + dur)
            .bind(f.e.ns.as_bytes().to_vec())
            .execute(f.reg.pool())
            .await
            .unwrap();
    }
    rid
}

async fn test_case(f: &Fx, run: i64, seq: i64, name: &str, outcome: i64, ms: i64) {
    sqlx::query(
        "INSERT INTO test_cases (ns, run_id, job_id, seq, job_name, name, outcome, attempt, duration_ms, group_id, message, change, version, head, at) \
         VALUES ($1, $2, 1, $3, 'unit', $4, $5, 1, $6, 0, '', $7, 1, $7, 1)",
    )
    .bind(f.e.ns.as_bytes().to_vec())
    .bind(run)
    .bind(seq)
    .bind(name.to_owned())
    .bind(outcome)
    .bind(ms)
    .bind(Vec::<u8>::new())
    .execute(f.reg.pool())
    .await
    .unwrap();
}

#[tokio::test]
async fn the_checks_board_counts_runs_against_the_previous_period() {
    let f = fx().await;
    run(&f, "m1", 0, C::Success, &[("unit", 30), ("lint", 5)], 60, 0).await;
    run(&f, "c1", 1, C::Failure, &[("unit", 50)], 120, 1).await;
    run(&f, "c2", 2, C::Success, &[("unit", 40)], 90, 2).await;
    run(&f, "old", 3, C::Failure, &[("unit", 10)], 10, 20).await;
    run(&f, "going", 3, C::Running, &[("unit", 10)], 10, 0).await;
    let ns = f.e.ns.as_bytes().to_vec();
    sqlx::query("INSERT INTO failure_groups (ns, id, signature, title, test_name, job_name, state, first_seen, last_seen) VALUES ($1, 1, 's', 't', 'a', 'unit', 1, 1, 1)")
        .bind(ns.clone())
        .execute(f.reg.pool())
        .await
        .unwrap();
    sqlx::query("INSERT INTO flaky_tests (ns, job_name, test, runs, failures, flips, flaky, updated) VALUES ($1, 'unit', 'a', 10, 3, 4, 1, 1)")
        .bind(ns)
        .execute(f.reg.pool())
        .await
        .unwrap();
    let s =
        f.ci.stats(req(
            wire::CiStatsRequest {
                repo: repo_ref(),
                days: 14,
            },
            ALICE,
            false,
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(
        (s.runs, s.passed, s.failed, s.pass_permille),
        (3, 2, 1, 666),
        "{s:?}"
    );
    assert_eq!((s.previous_runs, s.previous_pass_permille), (1, 0));
    assert_eq!((s.failure_groups_open, s.flaky_tests), (1, 1));
    assert_eq!(s.median_duration_seconds, 90);
    assert_eq!(
        (s.slowest_job.as_str(), s.slowest_job_seconds),
        ("unit", 40)
    );
    // The default window is 14 days; a longer one reaches the old run.
    let wide =
        f.ci.stats(req(
            wire::CiStatsRequest {
                repo: repo_ref(),
                days: 30,
            },
            ALICE,
            false,
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(wide.runs, 4);
    // Another repository's claim reads nothing here.
    let (a, v) = claimkit::authorizer();
    let svc = crate::ci::rpc::CiService::new(
        Arc::new(Reviews::new(f.reg.clone())),
        a,
        ServerInfo::default(),
    );
    let m = || wire::CiStatsRequest {
        repo: repo_ref(),
        days: 0,
    };
    assert_eq!(
        svc.stats(host(&v, m(), "42:alice", Role::Admin, "o/other", false))
            .await
            .unwrap_err()
            .code(),
        Code::PermissionDenied
    );
    assert!(
        svc.stats(host(&v, m(), "42:alice", Role::Read, "o/r", false))
            .await
            .is_ok()
    );
}

#[tokio::test]
async fn a_change_is_compared_with_main_for_new_failures_and_the_slowest_test() {
    let f = fx().await;
    let main = run(&f, "m1", 0, C::Success, &[("unit", 30)], 60, 0).await;
    let mine = run(&f, "c1", 1, C::Failure, &[("unit", 50)], 120, 0).await;
    test_case(&f, main, 1, "t_a", 2, 100).await;
    test_case(&f, main, 2, "t_b", 1, 50).await;
    test_case(&f, mine, 1, "t_a", 2, 120).await;
    test_case(&f, mine, 2, "t_b", 2, 300).await;
    test_case(&f, mine, 3, "t_c", 1, 900).await;
    let cmp = |change: u8| {
        let r = req(
            wire::CompareWithMainRequest {
                repo: repo_ref(),
                change: cbytes(change),
            },
            ALICE,
            false,
        );
        let ci = &f.ci;
        async move {
            ci.compare_with_main(r)
                .await
                .map(tonic::Response::into_inner)
        }
    };
    let c = cmp(1).await.unwrap();
    assert!(c.compared);
    assert_eq!(c.main_status, C::Success as i32);
    assert_eq!((c.failing, c.new_failure_count), (2, 1));
    assert_eq!(
        (
            c.new_failures[0].job.as_str(),
            c.new_failures[0].name.as_str()
        ),
        ("unit", "t_b")
    );
    let slow = c.slowest_test.unwrap();
    assert_eq!(
        (slow.name.as_str(), slow.duration_ms, slow.main_duration_ms),
        ("t_c", 900, 0)
    );
    // A change with no finished run is not compared; an unknown change is not found.
    let none = cmp(2).await.unwrap();
    assert!(!none.compared);
    assert_eq!(none.main_status, C::Success as i32);
    assert_eq!(cmp(9).await.unwrap_err().code(), Code::NotFound);
}
