//! `Insights`: the numbers over a repository with known landings, reviews and an agent run; the
//! windows and the kind filter; and who may ask.

use std::sync::Arc;

use asc_rpc::forwarded::Role;
use asc_rpc::server::wire;
use asc_rpc::services::insights_server::Insights;
use tonic::Code;

use super::rpc::InsightsService;
use crate::review::Reviews;
use crate::review::tests::{DEPLOY, Fx, cid, fx, repo_ref, req};
use crate::review::tests_acting::claimkit::{self, host};

const DAY: i64 = 86_400;
/// A day boundary far from zero.
const U: i64 = 100 * DAY;

fn svc(f: &Fx, authorizer: Arc<dyn asc_rpc::auth::Authorizer>) -> InsightsService {
    InsightsService::new(
        Arc::new(Reviews::new(f.reg.clone())),
        authorizer,
        asc_rpc::server::ServerInfo::default(),
    )
}

async fn opened(f: &Fx, change: u8, at: i64) {
    sqlx::query("UPDATE change_versions SET at = $1 WHERE change = $2 AND v = 1")
        .bind(at)
        .bind(cid(change).as_bytes().to_vec())
        .execute(f.reg.pool())
        .await
        .unwrap();
}

/// One landing of `change` (its first version's changeset) at `at`, on `seq`.
async fn landed(f: &Fx, change: u8, at: i64, seq: i64) {
    let reference = format!("refs/heads/{}", f.e.default_branch);
    sqlx::query(
        "INSERT INTO ref_log (seq, repo_id, name, old, new, actor, via, reason, op_id, at) \
         VALUES ($1, $2, $3, NULL, NULL, 'x', 'land', 't', $4, $5)",
    )
    .bind(seq)
    .bind(f.e.key.0)
    .bind(reference.clone())
    .bind(vec![seq as u8])
    .bind(at)
    .execute(f.reg.pool())
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO land_map_ref (ns, ref, old_cs, new_cs, ref_log_seq, op_id) VALUES ($1, $2, $3, $4, $5, $6)",
    )
    .bind(f.e.ns.as_bytes().to_vec())
    .bind(reference)
    .bind(f.css[usize::from(change) - 1].as_bytes().to_vec())
    .bind(vec![change; 32])
    .bind(seq)
    .bind(vec![seq as u8])
    .execute(f.reg.pool())
    .await
    .unwrap();
}

async fn verdict(f: &Fx, change: u8, reviewer: &str, state: i64, at: i64) {
    sqlx::query(
        "INSERT INTO review_approvals (ns, change, reviewer, version, state, at, reviewer_id) VALUES ($1, $2, $3, 1, $4, $5, 0)",
    )
    .bind(f.e.ns.as_bytes().to_vec())
    .bind(cid(change).as_bytes().to_vec())
    .bind(reviewer.to_owned())
    .bind(state)
    .bind(at)
    .execute(f.reg.pool())
    .await
    .unwrap();
}

/// Change 1 (alice) opened 12 days before `U`, landed 10 days before, approved by carol an hour
/// after opening. Change 2 (bob), opened 41 days before and landed 40 days before (the previous
/// window), approved by dave after two hours. Change 3 is an agent run's (fixer, started by maya),
/// opened 4 days before, landed 3 days before, approved by dave after half an hour.
async fn world() -> Fx {
    let f = fx().await;
    opened(&f, 1, U - 12 * DAY).await;
    opened(&f, 2, U - 41 * DAY).await;
    opened(&f, 3, U - 4 * DAY).await;
    landed(&f, 1, U - 10 * DAY, 1).await;
    landed(&f, 2, U - 40 * DAY, 2).await;
    landed(&f, 3, U - 3 * DAY, 3).await;
    verdict(&f, 1, "carol", 1, U - 12 * DAY + 3600).await;
    verdict(&f, 2, "dave", 1, U - 41 * DAY + 7200).await;
    verdict(&f, 3, "dave", 1, U - 4 * DAY + 1800).await;
    // A verdict that is not a review (a dismissal) counts for nothing.
    verdict(&f, 3, "erin", 3, U - 4 * DAY + 60).await;
    sqlx::query(
        "INSERT INTO agent_runs (id, repo, agent, state, target, base, task, created_by, starter, created, changes_json)          VALUES (1, 'o/r', 'fixer', 4, 'master', $1, 't', '45:root', 'maya', 1, $2)",
    )
    .bind(vec![1u8])
    .bind(format!("[\"{}\"]", "03".repeat(16)))
    .execute(f.reg.pool())
    .await
    .unwrap();
    f
}

fn stats(days: u32, kind: wire::AuthorKind) -> wire::InsightsStatsRequest {
    wire::InsightsStatsRequest {
        repo: repo_ref(),
        days,
        kind: kind as i32,
        until: U,
    }
}

async fn get(f: &Fx, m: wire::InsightsStatsRequest) -> wire::InsightsStatsResponse {
    svc(f, auth(f))
        .get_stats(req(m, DEPLOY, false))
        .await
        .unwrap()
        .into_inner()
}

fn auth(_f: &Fx) -> Arc<dyn asc_rpc::auth::Authorizer> {
    use asc_rpc::auth::{Access, StaticTokens};
    Arc::new(
        StaticTokens::new()
            .with_token(DEPLOY, "deploy:o/r", Access::Read)
            .with_token("write-token", "44:carol", Access::Write),
    )
}

#[tokio::test]
async fn the_tiles_count_landed_changes_review_time_and_the_author_mix() {
    let f = world().await;
    let got = get(&f, stats(30, wire::AuthorKind::Unspecified)).await;
    assert_eq!((got.from, got.until), (U - 30 * DAY, U));
    assert_eq!((got.landed, got.landed_previous), (2, 1));
    // Change 1 and 3 reviewed after 3600 s and 1800 s; change 2 (previous) after 7200 s.
    assert_eq!(got.median_review_seconds, 2700);
    assert_eq!(got.median_review_previous, 7200);
    assert_eq!((got.landed_by_people, got.landed_by_agents), (1, 1));
    assert_eq!(got.agent_changes_reviewed, 1);
    assert_eq!((got.stacks_opened, got.stacks_opened_previous), (2, 1));
    assert!(!got.truncated);
    // A day entry for each of the 30 days, one person and one agent among them.
    assert_eq!(got.per_day.len(), 30);
    assert!(got.per_day.windows(2).all(|w| w[1].day - w[0].day == DAY));
    let (p, a): (u32, u32) = got
        .per_day
        .iter()
        .fold((0, 0), |(p, a), d| (p + d.people, a + d.agents));
    assert_eq!((p, a), (1, 1));
    // Twelve weeks end at `until`; the three landings fall inside them.
    assert_eq!(got.weekly.len(), 12);
    assert_eq!(got.weekly.iter().map(|w| w.landed).sum::<u32>(), 3);
    assert_eq!(got.weekly[11].week_start, U - 7 * DAY);
    // No size is stored in this world.
    let unknown = got.by_size.iter().find(|b| b.label == "unknown").unwrap();
    assert_eq!(unknown.changes, 2);
    assert_eq!(unknown.median_review_seconds, 2700);
}

#[tokio::test]
async fn the_kind_narrows_the_tiles_and_a_longer_window_reaches_further() {
    let f = world().await;
    let people = get(&f, stats(30, wire::AuthorKind::Person)).await;
    assert_eq!((people.landed, people.median_review_seconds), (1, 3600));
    assert_eq!(people.per_day.iter().map(|d| d.agents).sum::<u32>(), 0);
    // The author mix does not follow the filter.
    assert_eq!((people.landed_by_people, people.landed_by_agents), (1, 1));
    let agents = get(&f, stats(30, wire::AuthorKind::Agent)).await;
    assert_eq!((agents.landed, agents.median_review_seconds), (1, 1800));
    let year = get(&f, stats(366, wire::AuthorKind::Unspecified)).await;
    assert_eq!(year.landed, 3);
    assert_eq!(year.per_day.len(), 366);
    // `days` 0 is 30; 367 is refused.
    assert_eq!(
        get(&f, stats(0, wire::AuthorKind::Unspecified))
            .await
            .landed,
        2
    );
    let err = svc(&f, auth(&f))
        .get_stats(req(
            stats(367, wire::AuthorKind::Unspecified),
            DEPLOY,
            false,
        ))
        .await
        .unwrap_err();
    assert_eq!(err.code(), Code::InvalidArgument);
}

#[tokio::test]
async fn a_landing_of_another_repository_or_branch_is_not_counted() {
    let f = world().await;
    // The same changeset landing in another namespace and on another branch of this one.
    sqlx::query(
        "INSERT INTO land_map_ref (ns, ref, old_cs, new_cs, ref_log_seq, op_id) VALUES ($1, 'refs/heads/master', $2, $3, 1, $4)",
    )
    .bind(vec![9u8; 4])
    .bind(f.css[1].as_bytes().to_vec())
    .bind(vec![7u8; 32])
    .bind(vec![9u8])
    .execute(f.reg.pool())
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO land_map_ref (ns, ref, old_cs, new_cs, ref_log_seq, op_id) VALUES ($1, 'refs/heads/release', $2, $3, 1, $4)",
    )
    .bind(f.e.ns.as_bytes().to_vec())
    .bind(f.css[1].as_bytes().to_vec())
    .bind(vec![7u8; 32])
    .bind(vec![8u8])
    .execute(f.reg.pool())
    .await
    .unwrap();
    let got = get(&f, stats(30, wire::AuthorKind::Unspecified)).await;
    assert_eq!((got.landed, got.landed_previous), (2, 1));
}

#[tokio::test]
async fn contributors_have_landed_reviews_run_by_and_a_sparkline() {
    let f = world().await;
    let ask = |kind: wire::AuthorKind| wire::ListContributorsRequest {
        repo: repo_ref(),
        days: 30,
        kind: kind as i32,
        until: U,
    };
    let got = svc(&f, auth(&f))
        .list_contributors(req(ask(wire::AuthorKind::Unspecified), DEPLOY, false))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(got.landed, 2);
    let by_name = |n: &str| got.contributors.iter().find(|c| c.name == n).unwrap();
    let alice = by_name("alice");
    assert_eq!((alice.landed, alice.reviews_given), (1, 0));
    assert_eq!(alice.kind, wire::AuthorKind::Person as i32);
    assert!((alice.share - 0.5).abs() < 1e-9);
    assert_eq!(alice.weekly_landed.len(), 12);
    assert_eq!(alice.weekly_landed.iter().sum::<u32>(), 1);
    let fixer = by_name("fixer");
    assert_eq!(
        (fixer.kind, fixer.run_by.as_str()),
        (wire::AuthorKind::Agent as i32, "maya")
    );
    assert_eq!(fixer.landed, 1);
    // Agents never review; people do: dave gave one verdict in this window (the other was in the previous one).
    let dave = by_name("dave");
    assert_eq!(
        (dave.landed, dave.reviews_given, dave.median_review_seconds),
        (0, 1, 1800)
    );
    assert_eq!(by_name("carol").reviews_given, 1);
    // A dismissal is not a review; bob landed nothing in the window and gave no review.
    assert!(
        got.contributors
            .iter()
            .all(|c| c.name != "erin" && c.name != "bob")
    );
    // Sorted by landed, then reviews.
    assert!(
        got.contributors
            .windows(2)
            .all(|w| w[0].landed >= w[1].landed)
    );
    // Agents only: reviews are not listed.
    let agents = svc(&f, auth(&f))
        .list_contributors(req(ask(wire::AuthorKind::Agent), DEPLOY, false))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(
        agents
            .contributors
            .iter()
            .map(|c| c.name.as_str())
            .collect::<Vec<_>>(),
        ["fixer"]
    );
}

#[tokio::test]
async fn only_a_reader_of_the_repository_may_ask() {
    let f = world().await;
    let s = svc(&f, auth(&f));
    let err = s
        .get_stats(req(
            stats(30, wire::AuthorKind::Unspecified),
            "no-such-token",
            false,
        ))
        .await
        .unwrap_err();
    assert_eq!(err.code(), Code::Unauthenticated);
    let (a, v) = claimkit::authorizer();
    let s = svc(&f, a);
    // The hub's claim for another repository grants nothing here; its read claim here does.
    let err = s
        .get_stats(host(
            &v,
            stats(30, wire::AuthorKind::Unspecified),
            "42:alice",
            Role::Admin,
            "o/other",
            false,
        ))
        .await
        .unwrap_err();
    assert_eq!(err.code(), Code::PermissionDenied);
    let ok = s
        .get_stats(host(
            &v,
            stats(30, wire::AuthorKind::Unspecified),
            "42:alice",
            Role::Read,
            "o/r",
            false,
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(ok.landed, 2);
    let err = s
        .list_contributors(host(
            &v,
            wire::ListContributorsRequest {
                repo: repo_ref(),
                days: 30,
                kind: 0,
                until: U,
            },
            "42:alice",
            Role::Admin,
            "o/other",
            false,
        ))
        .await
        .unwrap_err();
    assert_eq!(err.code(), Code::PermissionDenied);
}

#[test]
fn the_median_is_the_middle_value_or_the_mean_of_the_two_in_the_middle() {
    use super::median;
    assert_eq!(median(vec![]), 0);
    assert_eq!(median(vec![5]), 5);
    assert_eq!(median(vec![9, 1, 5]), 5);
    assert_eq!(median(vec![1, 3, 5, 9]), 4);
}
