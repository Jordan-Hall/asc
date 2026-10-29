//! The places that raise notification events: reviews, comments, checks, the policy effect and
//! the configurable links. (Agents, the merge queue and restack have their events tested beside
//! their own fixtures.)

use asc_core::id::RefTarget;
use asc_rpc::server::wire;
use asc_rpc::services::ci_server::Ci;
use asc_rpc::services::review_server::Review;
use bytes::Bytes;
use sqlx::Row;

use super::policy::{self, PolicyChanged};
use super::tests::notes;
use super::*;
use crate::agents::repo::PolicyState;
use crate::outbox::Job;
use crate::review::tests::{ADMIN, ALICE, BOB, CAROL, cbytes, fx, repo_ref, req, request_review};

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn change_path(n: u8) -> String {
    format!("/o/r/changes/{}", hex(&[n; 16]))
}

async fn comment(f: &crate::review::tests::Fx, token: &str, change: u8, body: &str) {
    f.review
        .add_comment(req(
            wire::AddCommentRequest {
                repo: repo_ref(),
                change: cbytes(change),
                body: body.into(),
                anchor: Some(wire::ThreadAnchor {
                    path: Bytes::from_static(b"f"),
                    start_line: 1,
                    side: wire::DiffSide::New as i32,
                    ..Default::default()
                }),
                ..Default::default()
            },
            token,
            true,
        ))
        .await
        .unwrap();
}

// ------------------------------------------------------------------------------ review

#[tokio::test]
async fn people_asked_to_review_are_told_once_and_the_asker_is_not() {
    let f = fx().await;
    request_review(&f, CAROL, 1, &["alice", "bob", "carol"], &[])
        .await
        .unwrap();
    let n = notes(f.reg.pool()).await;
    assert_eq!(n.len(), 1, "{n:?}");
    assert_eq!(n[0].0, "review_requested");
    assert_eq!(n[0].1, ["alice", "bob"], "not carol, who asked");
    assert!(n[0].2.contains("carol"), "{}", n[0].2);
    assert_eq!(n[0].4, change_path(1));
    // Asking again names only the people not already asked.
    request_review(&f, CAROL, 1, &["alice", "dave"], &[])
        .await
        .unwrap();
    let n = notes(f.reg.pool()).await;
    assert_eq!(n.len(), 2, "{n:?}");
    assert_eq!(n[1].1, ["dave"]);
    // Taking a request back tells nobody.
    request_review(&f, CAROL, 1, &[], &["dave"]).await.unwrap();
    assert_eq!(notes(f.reg.pool()).await.len(), 2);
}

#[tokio::test]
async fn a_refused_request_tells_nobody() {
    let f = fx().await;
    // A reader cannot ask for reviews.
    assert!(request_review(&f, ALICE, 1, &["bob"], &[]).await.is_err());
    // Nor can anyone ask about a change that does not exist.
    assert!(request_review(&f, CAROL, 9, &["bob"], &[]).await.is_err());
    assert!(notes(f.reg.pool()).await.is_empty());
}

#[tokio::test]
async fn a_mention_tells_the_person_named_once_per_comment_and_not_the_commenter() {
    let f = fx().await;
    comment(&f, CAROL, 1, "@alice and @bob, also @carol and a@b.example").await;
    let n = notes(f.reg.pool()).await;
    assert_eq!(n.len(), 1, "{n:?}");
    assert_eq!(n[0].0, "mentioned");
    assert_eq!(n[0].1, ["alice", "bob"]);
    assert_eq!(n[0].4, change_path(1));
    assert!(n[0].3.contains("@alice"), "the comment is the detail");
    // A comment that names nobody tells nobody.
    comment(&f, CAROL, 1, "looks fine").await;
    assert_eq!(notes(f.reg.pool()).await.len(), 1);
    // A second comment is a second cause.
    comment(&f, CAROL, 2, "ping @dave").await;
    let n = notes(f.reg.pool()).await;
    assert_eq!((n.len(), n[1].1.clone()), (2, vec!["dave".to_owned()]));
}

#[tokio::test]
async fn a_comment_a_reader_may_not_make_tells_nobody() {
    let f = fx().await;
    let denied = f
        .review
        .add_comment(req(
            wire::AddCommentRequest {
                repo: repo_ref(),
                change: cbytes(1),
                body: "@bob".into(),
                anchor: Some(wire::ThreadAnchor {
                    path: Bytes::from_static(b"f"),
                    start_line: 1,
                    side: wire::DiffSide::New as i32,
                    ..Default::default()
                }),
                ..Default::default()
            },
            "no-such-token",
            true,
        ))
        .await;
    assert!(denied.is_err());
    assert!(notes(f.reg.pool()).await.is_empty());
}

// ------------------------------------------------------------------------------ checks

fn run_req(id: &str, change: u8, status: wire::CiStatus) -> wire::ReportRunRequest {
    wire::ReportRunRequest {
        repo: repo_ref(),
        source: "actions".into(),
        external_id: id.into(),
        name: "unit".into(),
        change: if change == 0 {
            Bytes::new()
        } else {
            cbytes(change)
        },
        status: status as i32,
        url: "https://ci.example/runs/1".into(),
        ..Default::default()
    }
}

#[tokio::test]
async fn a_failed_check_tells_the_authors_of_the_change_once_per_run() {
    let f = fx().await;
    let report = |m: wire::ReportRunRequest, token: &str| {
        let r = req(m, token, false);
        async { f.ci.report_run(r).await }
    };
    // A run that has not ended, one that passed, and one for no change are not news.
    report(run_req("a", 1, wire::CiStatus::Running), ADMIN)
        .await
        .unwrap();
    report(run_req("b", 1, wire::CiStatus::Success), ADMIN)
        .await
        .unwrap();
    report(run_req("c", 0, wire::CiStatus::Failure), ADMIN)
        .await
        .unwrap();
    assert!(notes(f.reg.pool()).await.is_empty());
    // A failure of change 1 (alice's) and of change 2 (bob's).
    report(run_req("d", 1, wire::CiStatus::Failure), ADMIN)
        .await
        .unwrap();
    report(run_req("e", 2, wire::CiStatus::Failure), ADMIN)
        .await
        .unwrap();
    // The same failure reported again (CI retries its report) tells nobody twice.
    report(run_req("d", 1, wire::CiStatus::Failure), ADMIN)
        .await
        .unwrap();
    let n = notes(f.reg.pool()).await;
    assert_eq!(n.len(), 2, "{n:?}");
    assert_eq!(
        (n[0].0.as_str(), n[0].1.as_slice()),
        ("checks_failed", &["alice".to_owned()][..])
    );
    assert_eq!(n[1].1, ["bob"]);
    assert_eq!(n[0].2, "Check unit failed");
    assert_eq!(n[0].3, "https://ci.example/runs/1");
    assert_eq!(n[0].4, change_path(1));
}

#[tokio::test]
async fn a_report_nobody_may_make_tells_nobody() {
    let f = fx().await;
    for token in [ALICE, BOB, CAROL] {
        let r =
            f.ci.report_run(req(run_req("x", 1, wire::CiStatus::Failure), token, false))
                .await;
        assert!(r.is_err(), "{token}");
    }
    assert!(notes(f.reg.pool()).await.is_empty());
}

// ------------------------------------------------------------------------------ policy

const POLICY: &str = "version = 1\n[roles]\nmaintainer = [\"user:a\"]\n";
const POLICY_LOOSE: &str =
    "version = 1\n[roles]\nmaintainer = [\"user:a\"]\n[review]\nauthor_may_approve = true\n";

fn target_hex(c: asc_core::id::ChangesetId) -> String {
    hex(&RefTarget::Changeset(c).to_bytes())
}

fn job(
    repo_id: i64,
    branch: &str,
    old: asc_core::id::ChangesetId,
    new: asc_core::id::ChangesetId,
    pusher: &str,
) -> Job {
    Job {
        id: 1,
        repo_id,
        op_id: vec![1],
        doc: serde_json::json!({
            "effects": ["notify.policy"],
            "pusher": pusher,
            "refs": [{"name": branch, "old": target_hex(old), "new": target_hex(new)}],
        }),
    }
}

#[test]
fn a_policy_change_is_told_by_how_it_changed() {
    let valid = |text: &str| {
        PolicyState::Valid(std::sync::Arc::new(
            asc_core::policy::Policy::parse(text.as_bytes()).unwrap(),
        ))
    };
    let invalid = |m: &str| PolicyState::Invalid(m.into());
    assert_eq!(
        policy::change(&PolicyState::Absent, &PolicyState::Absent),
        None
    );
    assert_eq!(policy::change(&valid(POLICY), &valid(POLICY)), None);
    assert!(
        policy::change(&PolicyState::Absent, &valid(POLICY))
            .unwrap()
            .contains("added")
    );
    assert!(
        policy::change(&valid(POLICY), &PolicyState::Absent)
            .unwrap()
            .contains("removed")
    );
    assert!(
        policy::change(&valid(POLICY), &valid(POLICY_LOOSE))
            .unwrap()
            .contains("changed")
    );
    assert!(
        policy::change(&valid(POLICY), &invalid("x"))
            .unwrap()
            .contains("cannot")
    );
    assert!(
        policy::change(&invalid("x"), &valid(POLICY))
            .unwrap()
            .contains("again")
    );
    assert_eq!(policy::change(&invalid("x"), &invalid("x")), None);
}

#[tokio::test]
async fn trunk_moving_over_a_changed_policy_tells_the_contributors_but_not_the_pusher() {
    use crate::ingest::testutil::commit;
    let f = fx().await;
    let e = &f.e;
    let trunk = format!("refs/heads/{}", e.default_branch);
    let base = commit(
        e,
        &[],
        &[("a", b"a\n"), (".asc/policy.toml", POLICY.as_bytes())],
    )
    .await;
    let same = commit(e, &[base], &[("b", b"b\n")]).await;
    let loose = commit(e, &[same], &[(".asc/policy.toml", POLICY_LOOSE.as_bytes())]).await;
    let effect = PolicyChanged::effect(&f.reg);
    let id = e.key.0;
    // Trunk moved and the policy did not: nothing to say.
    effect
        .run(&job(id, &trunk, base, same, "42:alice"))
        .await
        .unwrap();
    assert!(notes(f.reg.pool()).await.is_empty());
    // Another branch moving over a changed policy is not trunk.
    effect
        .run(&job(id, "refs/heads/topic", same, loose, "42:alice"))
        .await
        .unwrap();
    assert!(notes(f.reg.pool()).await.is_empty());
    // Trunk moved over the changed file: the contributors are told (alice pushed, so not alice).
    effect
        .run(&job(id, &trunk, same, loose, "42:alice"))
        .await
        .unwrap();
    let n = notes(f.reg.pool()).await;
    assert_eq!(n.len(), 1, "{n:?}");
    assert_eq!(n[0].0, "policy_changed");
    assert_eq!(
        n[0].1,
        ["bob"],
        "contributors of the fixture are alice and bob"
    );
    assert_eq!(n[0].4, "/o/r/settings/policy");
    assert!(n[0].3.contains("rules"), "{}", n[0].3);
    // Delivered again (at-least-once): one row only.
    effect
        .run(&job(id, &trunk, same, loose, "42:alice"))
        .await
        .unwrap();
    assert_eq!(notes(f.reg.pool()).await.len(), 1);
    // A creation or a deletion of trunk has nothing to compare.
    let mut created = job(id, &trunk, same, loose, "42:alice");
    created.doc["refs"][0]["old"] = serde_json::Value::Null;
    effect.run(&created).await.unwrap();
    // A repository that is gone is not an error: the row is done.
    effect
        .run(&job(9999, &trunk, same, loose, "42:alice"))
        .await
        .unwrap();
    assert_eq!(notes(f.reg.pool()).await.len(), 1);
}

#[tokio::test]
async fn the_contributors_are_the_people_who_made_versions_newest_first_without_the_pusher() {
    let f = fx().await;
    let ns = f.e.ns.as_bytes();
    sqlx::query("UPDATE change_versions SET at = 9 WHERE actor = '43:bob'")
        .execute(f.reg.pool())
        .await
        .unwrap();
    assert_eq!(
        policy::contributors(f.reg.pool(), ns, "").await,
        ["bob", "alice"]
    );
    assert_eq!(
        policy::contributors(f.reg.pool(), ns, "bob").await,
        ["alice"]
    );
    // A token, a deploy key and a CI token are not people with a login.
    sqlx::query("UPDATE change_versions SET actor = 'deploy:o/r' WHERE change = $1")
        .bind(vec![3u8; 16])
        .execute(f.reg.pool())
        .await
        .unwrap();
    assert!(
        !policy::contributors(f.reg.pool(), ns, "")
            .await
            .iter()
            .any(|u| u.contains("deploy"))
    );
}

// ------------------------------------------------------------------------------ the links

#[test]
fn the_links_are_templates_with_a_prefix_and_bad_ones_are_refused() {
    let c = NotifyConfig::default();
    c.validate().unwrap();
    assert_eq!(c.render(&c.change_path, "o/r", "ab", ""), "/o/r/changes/ab");
    let moved = NotifyConfig {
        path_prefix: "/hub".into(),
        run_path: "/{repo}/-/agents/{run}".into(),
        ..NotifyConfig::default()
    };
    moved.validate().unwrap();
    assert_eq!(
        moved.render(&moved.run_path, "o/r", "", "7"),
        "/hub/o/r/-/agents/7"
    );
    for bad in [
        NotifyConfig {
            path_prefix: "hub".into(),
            ..NotifyConfig::default()
        },
        NotifyConfig {
            path_prefix: "/hub/".into(),
            ..NotifyConfig::default()
        },
        NotifyConfig {
            change_path: "//evil/{repo}".into(),
            ..NotifyConfig::default()
        },
        NotifyConfig {
            change_path: "https://evil/{repo}".into(),
            ..NotifyConfig::default()
        },
        NotifyConfig {
            queue_path: "/queue".into(),
            ..NotifyConfig::default()
        },
        NotifyConfig {
            policy_path: "/{repo}/{other}".into(),
            ..NotifyConfig::default()
        },
        NotifyConfig {
            run_path: "/{repo}/a b".into(),
            ..NotifyConfig::default()
        },
        NotifyConfig {
            run_path: "/{repo}/{run".into(),
            ..NotifyConfig::default()
        },
    ] {
        assert!(bad.validate().is_err(), "{bad:?}");
    }
    // Unknown keys are an error, like every other section.
    assert!(toml_like("enabled = true\nfoo = 1").is_err());
    assert!(toml_like("path_prefix = \"/hub\"").is_ok());
}

fn toml_like(text: &str) -> Result<NotifyConfig, String> {
    let v: serde_json::Value = {
        let mut m = serde_json::Map::new();
        for line in text.lines() {
            let (k, v) = line.split_once('=').unwrap();
            m.insert(k.trim().to_owned(), serde_json::from_str(v.trim()).unwrap());
        }
        m.into()
    };
    serde_json::from_value(v).map_err(|e| e.to_string())
}

#[tokio::test]
async fn a_delivery_and_a_refusal_are_audited() {
    use std::sync::Arc;

    use super::worker::{DispatchConfig, Dispatcher, Failure, Sink};
    struct Script(std::sync::Mutex<Vec<Result<u64, Failure>>>);
    #[async_trait::async_trait]
    impl Sink for Script {
        async fn deliver(&self, _id: &str, _e: &super::worker::Outgoing) -> Result<u64, Failure> {
            self.0.lock().unwrap().remove(0)
        }
    }
    let f = fx().await;
    let pool = f.reg.pool();
    for t in ["one", "two"] {
        record(
            pool,
            "o/r",
            Event::new(Kind::MergeQueue, t).users(["alice"]),
        )
        .await
        .unwrap();
    }
    let sink = Arc::new(Script(std::sync::Mutex::new(vec![
        Ok(1),
        Err(Failure {
            permanent: true,
            message: "invalid request".into(),
        }),
    ])));
    Dispatcher::new(pool.clone(), Some(sink), DispatchConfig::default())
        .deliver_due(crate::outbox::now())
        .await
        .unwrap();
    let rows = sqlx::query(
        "SELECT action FROM audit_events WHERE action LIKE 'notify.event.%' ORDER BY action",
    )
    .fetch_all(pool)
    .await
    .unwrap();
    let actions: Vec<String> = rows.iter().map(|r| r.get("action")).collect();
    assert!(
        actions.iter().any(|a| a == "notify.event.delivered")
            && actions.iter().any(|a| a == "notify.event.rejected"),
        "{actions:?}"
    );
}
