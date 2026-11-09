//! Tests of the per-key activity log: what each use records, `AiProviders.ListActivity` and the 90-day retention.

use asc_rpc::server::wire;
use asc_rpc::services::ai_providers_server::AiProviders;
use tokio_util::sync::CancellationToken;
use tonic::Code;

use super::fake::Reply;
use super::store::ACTIVITY_KEEP_SECS;
use super::tests::{Fx, active_key, fx, owner_ref, req, request, service, user};
use super::{Activity, CallInfo, KeyPatch, clock};

async fn call(f: &Fx, key: &super::store::KeyRow, activity: Activity<'_>, model: &str) {
    f.hub
        .complete(
            key,
            &CallInfo {
                repo: "acme/app",
                agent: "fixer",
                feature: None,
                activity,
            },
            &request(model),
            false,
            &CancellationToken::new(),
        )
        .await
        .unwrap();
}

fn list(key_id: &str, page_size: u32, page_token: &str) -> wire::ListAiActivityRequest {
    wire::ListAiActivityRequest {
        owner: owner_ref(wire::OwnerKind::User, "alice"),
        key_id: key_id.into(),
        page_size,
        page_token: page_token.into(),
    }
}

#[tokio::test(flavor = "current_thread")]
async fn each_use_is_summed_per_run_change_test_day_and_person() {
    let f = fx().await;
    let svc = service(&f);
    let k = active_key(&f, &user("alice"), "work").await;
    f.fake.set_default(Reply::Text {
        text: "x".into(),
        input: 100,
        output: 40,
    });
    let change = "kkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkz";
    for _ in 0..3 {
        call(
            &f,
            &k,
            Activity::AgentRun {
                run: 7,
                by: "carol",
            },
            "model-x",
        )
        .await;
    }
    call(
        &f,
        &k,
        Activity::ReviewSummary { change, by: "dave" },
        "model-x",
    )
    .await;
    call(
        &f,
        &k,
        Activity::ReviewSummary { change, by: "erin" },
        "model-x",
    )
    .await;
    call(&f, &k, Activity::Other, "mystery").await;
    f.hub
        .update_key(
            &k.id,
            KeyPatch {
                default_model: Some("model-x".into()),
                ..KeyPatch::default()
            },
        )
        .await
        .unwrap();
    // Updating the key untests it; a test with a default model makes one call.
    assert!(f.hub.test_key(&k.id).await.unwrap().ok);

    let r = svc
        .list_activity(req(list(&k.id, 0, ""), "alice-token", None))
        .await
        .unwrap()
        .into_inner();
    assert!(r.next_page_token.is_empty());
    assert_eq!(r.items.len(), 5, "{:?}", r.items);
    let find = |kind: wire::AiActivityKind, actor: &str| {
        r.items
            .iter()
            .find(|a| a.kind == kind as i32 && a.actor == actor)
            .unwrap_or_else(|| panic!("{kind:?} {actor}: {:?}", r.items))
    };
    let run = find(wire::AiActivityKind::AgentRun, "carol");
    assert_eq!(run.subject, "7");
    assert_eq!(
        (run.requests, run.input_tokens, run.output_tokens),
        (3, 300, 120)
    );
    assert_eq!(
        (run.repo.as_str(), run.agent.as_str()),
        ("acme/app", "fixer")
    );
    assert!(run.cost_micro_usd > 0 && run.unpriced_requests == 0);
    assert!(run.first_at <= run.last_at && run.last_at > 0);
    let dave = find(wire::AiActivityKind::ReviewSummary, "dave");
    assert_eq!((dave.subject.as_str(), dave.requests), (change, 1));
    assert_eq!(
        find(wire::AiActivityKind::ReviewSummary, "erin").requests,
        1
    );
    let other = find(wire::AiActivityKind::Other, "");
    assert_eq!(
        (other.model.as_str(), other.unpriced_requests),
        ("mystery", 1)
    );
    let test = find(wire::AiActivityKind::KeyTest, "");
    assert_eq!(test.subject, clock::day(clock::now()));
    assert_eq!(test.requests, 1);
    // Newest first.
    assert!(r.items.windows(2).all(|w| w[0].last_at >= w[1].last_at));
}

#[tokio::test(flavor = "current_thread")]
async fn activity_pages_and_refuses_bad_requests() {
    let f = fx().await;
    let svc = service(&f);
    let k = active_key(&f, &user("alice"), "work").await;
    f.fake.set_default(Reply::Text {
        text: "x".into(),
        input: 1,
        output: 1,
    });
    for run in 1..=5 {
        call(&f, &k, Activity::AgentRun { run, by: "carol" }, "model-x").await;
    }
    let mut seen = Vec::new();
    let mut token = String::new();
    let mut pages = 0;
    loop {
        let r = svc
            .list_activity(req(list(&k.id, 2, &token), "alice-token", None))
            .await
            .unwrap()
            .into_inner();
        assert!(r.items.len() <= 2);
        seen.extend(r.items.into_iter().map(|a| a.subject));
        pages += 1;
        if r.next_page_token.is_empty() {
            break;
        }
        token = r.next_page_token;
    }
    assert_eq!(pages, 3);
    seen.sort();
    assert_eq!(seen, ["1", "2", "3", "4", "5"]);

    // A page size above the cap is cut to it, not refused.
    let r = svc
        .list_activity(req(list(&k.id, 10_000, ""), "alice-token", None))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(r.items.len(), 5);

    for bad in ["x", "o", "o-1", "o1x", "o 1", "o999999999999999999999"] {
        let e = svc
            .list_activity(req(list(&k.id, 0, bad), "alice-token", None))
            .await
            .unwrap_err();
        assert_eq!(e.code(), Code::InvalidArgument, "{bad}");
    }
    let e = svc
        .list_activity(req(list("", 0, ""), "alice-token", None))
        .await
        .unwrap_err();
    assert_eq!(e.code(), Code::InvalidArgument);

    // Another owner's key, a missing key and another person asking all look alike.
    let bob_key = active_key(&f, &user("bob"), "b").await;
    for (key, token) in [
        (bob_key.id.as_str(), "alice-token"),
        ("ak_nope", "alice-token"),
    ] {
        let e = svc
            .list_activity(req(list(key, 0, ""), token, None))
            .await
            .unwrap_err();
        assert_eq!(e.code(), Code::NotFound, "{key}");
    }
    let e = svc
        .list_activity(req(list(&k.id, 0, ""), "bob-token", None))
        .await
        .unwrap_err();
    assert_eq!(
        e.code(),
        Code::NotFound,
        "another person never learns the key exists"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn activity_older_than_ninety_days_is_hidden_and_dropped() {
    let f = fx().await;
    let svc = service(&f);
    let k = active_key(&f, &user("alice"), "work").await;
    let old = clock::now() - ACTIVITY_KEEP_SECS - 3_600;
    sqlx::query(
        "INSERT INTO ai_activity (key_id, owner_kind, owner_name, kind, subject, actor, repo, agent, model, \
         first_at, last_at, requests) VALUES ($1, 1, 'alice', 1, 'old-run', 'carol', 'acme/app', 'fixer', 'm', $2, $2, 1)",
    )
    .bind(k.id.clone())
    .bind(old)
    .execute(&f.pool)
    .await
    .unwrap();
    let r = svc
        .list_activity(req(list(&k.id, 0, ""), "alice-token", None))
        .await
        .unwrap()
        .into_inner();
    assert!(r.items.is_empty(), "an expired row is never listed");

    f.fake.set_default(Reply::Text {
        text: "x".into(),
        input: 1,
        output: 1,
    });
    call(
        &f,
        &k,
        Activity::AgentRun {
            run: 9,
            by: "carol",
        },
        "model-x",
    )
    .await;
    let left: Vec<String> = sqlx::query_scalar("SELECT subject FROM ai_activity WHERE key_id = $1")
        .bind(k.id.clone())
        .fetch_all(&f.pool)
        .await
        .unwrap();
    assert_eq!(left, ["9"], "the next use drops the expired row");
}
