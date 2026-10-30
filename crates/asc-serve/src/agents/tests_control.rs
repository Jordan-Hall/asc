//! Control of live runs and the run boards: requests and decisions, plans, pause, checkpoints and
//! rollback, limits, continue, estimate, recent tasks, run log, agents, spend and monthly budgets.
//! Every call is tried as the starter, as an administrator, as a writer who did not start the run,
//! as a reader, on another repository's claim and (for runs) on a run of another tenant.

use std::sync::Arc;
use std::time::Duration;

use asc_rpc::forwarded::{ForwardedVerifier, Role};
use asc_rpc::server::{ServerInfo, wire};
use asc_rpc::services::agents_server::Agents;
use serde_json::json;
use tonic::{Code, Request};

use super::rpc::AgentService;
use super::runs::RunState;
use super::tests::{World, done, repo_ref, static_auth, tools};
use crate::ai::fake::Reply;
use crate::ai::owner::Owner;
use crate::authz::forwarded::ForwardedAuthorizer;

fn alice() -> Owner {
    Owner::User("alice".into())
}

fn op() -> String {
    uuid::Uuid::new_v4().to_string()
}

async fn until<F, Fut>(what: &str, mut f: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let end = std::time::Instant::now() + Duration::from_secs(60);
    while !f().await {
        assert!(
            std::time::Instant::now() < end,
            "gave up waiting for: {what}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn pending(w: &World, run: u64) -> Vec<wire::AgentRequest> {
    w.svc
        .list_requests(w.request(
            wire::ListAgentRequestsRequest {
                repo: repo_ref(),
                run_id: run,
                states: vec![wire::AgentRequestState::Pending as i32],
                ..Default::default()
            },
            "alice-token",
            None,
        ))
        .await
        .unwrap()
        .into_inner()
        .requests
}

fn decide(
    w: &World,
    who: &str,
    id: u64,
    d: wire::AgentDecision,
    reason: &str,
) -> impl std::future::Future<Output = Result<wire::DecideAgentRequestResponse, tonic::Status>> {
    let r = w.request(
        wire::DecideAgentRequestRequest {
            repo: repo_ref(),
            request_id: id,
            decision: d as i32,
            reason: reason.into(),
        },
        who,
        Some(&op()),
    );
    let svc = w.svc.clone();
    async move { svc.decide_request(r).await.map(tonic::Response::into_inner) }
}

fn plan_msg() -> wire::CreateAgentRunRequest {
    wire::CreateAgentRunRequest {
        repo: repo_ref(),
        agent: "fixer".into(),
        title: "Plan it".into(),
        task: "Add a greeting".into(),
        plan_first: true,
        ..Default::default()
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_plan_first_run_waits_for_a_decision_and_the_decision_is_authorised_and_logged() {
    let w = World::new().await;
    w.key(alice(), "mine").await;
    w.script(vec![
        // A tool before the plan is refused and the model is told so.
        tools(vec![("list_files", json!({"path": "src"}))]),
        tools(vec![(
            "propose_plan",
            json!({"steps": ["Write the file", "Commit it"]}),
        )]),
        tools(vec![(
            "propose_plan",
            json!({"steps": ["Write the greeting", "Commit it", "Finish"]}),
        )]),
        tools(vec![
            (
                "write_file",
                json!({"path": "src/greeting.rs", "content": "pub const G: &str = \"hi\";\n"}),
            ),
            ("commit", json!({"message": "Add the greeting"})),
            ("complete_step", json!({"step": 1})),
            ("complete_step", json!({"step": 2})),
        ]),
        done("done"),
    ]);
    let run = w.start_run_with("alice-token", plan_msg()).await.unwrap();
    assert_eq!(run.task, "Add a greeting");
    assert!(run.progress.as_ref().unwrap().plan_first);
    w.wait_state(run.id, RunState::WaitingInput).await;
    let req = pending(&w, run.id).await;
    assert_eq!(req.len(), 1);
    assert_eq!(req[0].kind, wire::AgentRequestKind::Plan as i32);
    assert_eq!(req[0].plan.len(), 2);
    let got = w
        .svc
        .get_run(w.request(
            wire::GetAgentRunRequest {
                repo: repo_ref(),
                id: run.id,
            },
            "carol-token",
            None,
        ))
        .await
        .unwrap()
        .into_inner()
        .run
        .unwrap();
    assert_eq!(got.pending_requests, 1);
    // Reader, a writer who did not start the run, and a decision that does not fit the request.
    for who in ["carol-token", "bob-token"] {
        let e = decide(&w, who, req[0].id, wire::AgentDecision::RejectPlan, "no")
            .await
            .unwrap_err();
        assert_eq!(e.code(), Code::PermissionDenied, "{who}");
    }
    let e = decide(
        &w,
        "alice-token",
        req[0].id,
        wire::AgentDecision::AllowOnce,
        "",
    )
    .await
    .unwrap_err();
    assert_eq!(e.code(), Code::InvalidArgument);
    let e = decide(
        &w,
        "alice-token",
        req[0].id,
        wire::AgentDecision::Unspecified,
        "",
    )
    .await
    .unwrap_err();
    assert_eq!(e.code(), Code::InvalidArgument);
    let e = decide(
        &w,
        "alice-token",
        req[0].id,
        wire::AgentDecision::RejectPlan,
        &"x".repeat(513),
    )
    .await
    .unwrap_err();
    assert_eq!(e.code(), Code::InvalidArgument);
    // Reject with a reason: the model proposes again.
    let r = decide(
        &w,
        "alice-token",
        req[0].id,
        wire::AgentDecision::RejectPlan,
        "too small",
    )
    .await
    .unwrap();
    assert_eq!(
        r.request.unwrap().state,
        wire::AgentRequestState::Rejected as i32
    );
    until("the second plan", || async {
        pending(&w, run.id).await.iter().any(|p| p.plan.len() == 3)
    })
    .await;
    let second = pending(&w, run.id).await.remove(0);
    // A decided request answers with the stored decision when decided again.
    let again = decide(
        &w,
        "alice-token",
        req[0].id,
        wire::AgentDecision::ApprovePlan,
        "",
    )
    .await
    .unwrap();
    assert_eq!(
        again.request.unwrap().state,
        wire::AgentRequestState::Rejected as i32
    );
    decide(
        &w,
        "alice-token",
        second.id,
        wire::AgentDecision::ApprovePlan,
        "go",
    )
    .await
    .unwrap();
    let row = w.wait(run.id).await;
    assert_eq!(row.state, RunState::Succeeded, "{row:?}");
    let got = w
        .svc
        .get_run(w.request(
            wire::GetAgentRunRequest {
                repo: repo_ref(),
                id: run.id,
            },
            "alice-token",
            None,
        ))
        .await
        .unwrap()
        .into_inner()
        .run
        .unwrap();
    let p = got.progress.unwrap();
    assert_eq!(
        (p.steps_total, p.steps_done),
        (3, 2),
        "two of three steps were marked done"
    );
    // The approvals log keeps both decisions with the decider and the reason.
    let log = w
        .svc
        .list_requests(w.request(
            wire::ListAgentRequestsRequest {
                repo: repo_ref(),
                run_id: run.id,
                ..Default::default()
            },
            "carol-token",
            None,
        ))
        .await
        .unwrap()
        .into_inner()
        .requests;
    assert_eq!(log.len(), 2);
    let rejected = log.iter().find(|r| r.id == req[0].id).unwrap();
    assert_eq!(
        (rejected.decided_by.as_str(), rejected.reason.as_str()),
        ("alice", "too small")
    );
    assert_eq!(
        log.iter()
            .filter(|r| r.state == wire::AgentRequestState::Approved as i32)
            .count(),
        1
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_permission_is_allowed_once_or_denied_and_the_model_hears_which() {
    let w = World::new().await;
    w.key(alice(), "mine").await;
    w.script(vec![
        tools(vec![(
            "request_permission",
            json!({"action": "delete the build cache", "reason": "it is stale"}),
        )]),
        tools(vec![(
            "request_permission",
            json!({"action": "delete the build cache"}),
        )]),
        done("done"),
    ]);
    let run = w
        .start_run_with(
            "alice-token",
            wire::CreateAgentRunRequest {
                repo: repo_ref(),
                agent: "fixer".into(),
                task: "tidy".into(),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    until("the first request", || async {
        !pending(&w, run.id).await.is_empty()
    })
    .await;
    let first = pending(&w, run.id).await.remove(0);
    assert_eq!(first.kind, wire::AgentRequestKind::Permission as i32);
    assert_eq!(first.summary, "delete the build cache");
    assert_eq!(first.detail, "it is stale");
    decide(
        &w,
        "alice-token",
        first.id,
        wire::AgentDecision::Deny,
        "keep it",
    )
    .await
    .unwrap();
    until("the second request", || async {
        pending(&w, run.id).await.iter().any(|p| p.id != first.id)
    })
    .await;
    let second = pending(&w, run.id).await.remove(0);
    decide(
        &w,
        "alice-token",
        second.id,
        wire::AgentDecision::AllowOnce,
        "",
    )
    .await
    .unwrap();
    let row = w.wait(run.id).await;
    assert_eq!(row.state, RunState::Succeeded);
    let results: Vec<String> = w
        .events(run.id)
        .await
        .into_iter()
        .filter(|e| e.kind == super::runs::EventKind::ToolResult as i64)
        .map(|e| e.text)
        .collect();
    assert!(
        results[0].contains("permission denied: keep it"),
        "{results:?}"
    );
    assert!(results[1].contains("granted once"), "{results:?}");
    // The starter was told of both questions and of the end, each once.
    until("the notices", || async {
        crate::notify::tests::notes(&w.hub.core.pool).await.len() >= 3
    })
    .await;
    let notes = crate::notify::tests::notes(&w.hub.core.pool).await;
    let kinds: Vec<&str> = notes.iter().map(|n| n.0.as_str()).collect();
    assert_eq!(
        kinds,
        ["agent_question", "agent_question", "agent_finished"],
        "{notes:?}"
    );
    assert!(notes.iter().all(|n| n.1 == ["alice"]), "{notes:?}");
    assert!(
        notes[0].2.contains("delete the build cache"),
        "{}",
        notes[0].2
    );
    assert_eq!(notes[0].3, "it is stale");
    assert_eq!(notes[0].4, format!("/acme/app/agents/runs/{}", run.id));
    assert!(notes[2].2.contains("finished"), "{}", notes[2].2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_paused_run_holds_at_its_next_tool_call_and_a_checkpoint_can_be_rolled_back_to() {
    let w = World::new().await;
    w.key(alice(), "mine").await;
    let write = |n: u32| {
        tools(vec![
            (
                "write_file",
                json!({"path": format!("src/f{n}.rs"), "content": format!("pub const N: u32 = {n};\n")}),
            ),
            ("commit", json!({"message": format!("Change {n}")})),
        ])
    };
    // The third model call waits for the test to open this gate, not for a clock: however slow
    // the machine, the run cannot reach its next tool call (and so cannot finish) while the test
    // is still pausing, resuming and asking for the rollback.
    let gate = Arc::new(tokio::sync::Notify::new());
    w.script(vec![
        write(1),
        write(2),
        Reply::Gate(gate.clone(), Box::new(tools(vec![("status", json!({}))]))),
        done("done"),
    ]);
    let run = w
        .start_run_with(
            "alice-token",
            wire::CreateAgentRunRequest {
                repo: repo_ref(),
                agent: "fixer".into(),
                task: "two changes".into(),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let cps = |who: &'static str| {
        let (w2, id) = (&w, run.id);
        async move {
            w2.svc
                .list_checkpoints(w2.request(
                    wire::ListAgentCheckpointsRequest {
                        repo: repo_ref(),
                        run_id: id,
                    },
                    who,
                    None,
                ))
                .await
                .map(|r| r.into_inner().checkpoints)
        }
    };
    until("two checkpoints", || async {
        cps("carol-token").await.unwrap().len() == 2
    })
    .await;
    let pause = |who: &str, paused: bool| {
        let r = w.request(
            wire::PauseAgentRunRequest {
                repo: repo_ref(),
                id: run.id,
                paused,
                reason: "look".into(),
            },
            who,
            None,
        );
        let svc = w.svc.clone();
        async move { svc.pause_run(r).await.map(tonic::Response::into_inner) }
    };
    for who in ["carol-token", "bob-token"] {
        assert_eq!(
            pause(who, true).await.unwrap_err().code(),
            Code::PermissionDenied,
            "{who}"
        );
    }
    let paused = pause("alice-token", true).await.unwrap().run.unwrap();
    assert!(paused.paused);
    let cps = cps("carol-token").await.unwrap();
    assert_eq!(
        (cps[0].seq, cps[0].label.as_str(), cps[0].changes.len()),
        (1, "Change 1", 1)
    );
    assert_eq!(cps[1].changes.len(), 2);
    // Rolling back needs the permission and the pause; a checkpoint of nothing is not found.
    let rollback = |who: &str, cp: u64| {
        let r = w.request(
            wire::RollbackAgentRunRequest {
                repo: repo_ref(),
                run_id: run.id,
                checkpoint_id: cp,
                reason: "wrong turn".into(),
            },
            who,
            Some(&op()),
        );
        let svc = w.svc.clone();
        async move { svc.rollback_run(r).await.map(tonic::Response::into_inner) }
    };
    assert_eq!(
        rollback("bob-token", cps[0].id).await.unwrap_err().code(),
        Code::PermissionDenied
    );
    assert_eq!(
        rollback("alice-token", 999_999).await.unwrap_err().code(),
        Code::NotFound
    );
    pause("alice-token", false).await.unwrap();
    assert_eq!(
        rollback("alice-token", cps[0].id).await.unwrap_err().code(),
        Code::FailedPrecondition,
        "only a paused run"
    );
    pause("alice-token", true).await.unwrap();
    rollback("alice-token", cps[0].id).await.unwrap();
    // Now the run may reach its next tool call: it holds there, takes the rollback, and goes on
    // only when it is resumed.
    gate.notify_one();
    // The run changes first and the checkpoints are marked after it: wait for the mark.
    until("the rollback", || async {
        let r = w.hub.core.runs.get(run.id).await.unwrap().unwrap();
        let marked = w
            .hub
            .checkpoints(run.id)
            .await
            .unwrap()
            .iter()
            .any(|c| c.rolled_back_at > 0);
        r.changes.len() == 1 && marked
    })
    .await;
    let after = w
        .svc
        .list_checkpoints(w.request(
            wire::ListAgentCheckpointsRequest {
                repo: repo_ref(),
                run_id: run.id,
            },
            "alice-token",
            None,
        ))
        .await
        .unwrap()
        .into_inner()
        .checkpoints;
    assert_eq!(after[0].rolled_back_at, 0);
    assert!(
        after[1].rolled_back_at > 0 && after[1].rolled_back_by == "alice",
        "{after:?}"
    );
    let events = w.events(run.id).await;
    assert!(
        events
            .iter()
            .any(|e| e.tool == "rollback" && e.text.contains("rolled back to checkpoint 1")),
        "{events:?}"
    );
    pause("alice-token", false).await.unwrap();
    let row = w.wait(run.id).await;
    assert_eq!(row.state, RunState::Succeeded, "{row:?}");
    assert_eq!(row.changes.len(), 1, "the later change left the run");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn limits_are_edited_on_a_live_run_never_above_the_policy_cap() {
    let w = World::new().await;
    w.key(alice(), "mine").await;
    w.fake.set_default(Reply::Delay(
        Duration::from_secs(30),
        Box::new(done("late")),
    ));
    let run = w
        .start_run_with(
            "alice-token",
            wire::CreateAgentRunRequest {
                repo: repo_ref(),
                agent: "fixer".into(),
                task: "slow".into(),
                limits: Some(wire::AgentBudgetLimits {
                    max_changes: 50,
                    ..Default::default()
                }),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    // The run's token is made when it starts; edit the limits only after, so the edit reaches it.
    for _ in 0..200 {
        let row = w.hub.core.runs.get(run.id).await.unwrap().unwrap();
        if !row.token_id.is_empty() || !row.state.is_active() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    let set = |who: &str, l: wire::AgentBudgetLimits| {
        let r = w.request(
            wire::SetAgentRunLimitsRequest {
                repo: repo_ref(),
                id: run.id,
                limits: Some(l),
            },
            who,
            None,
        );
        let svc = w.svc.clone();
        async move { svc.set_run_limits(r).await.map(tonic::Response::into_inner) }
    };
    let l = |tokens, changes| wire::AgentBudgetLimits {
        max_tokens: tokens,
        max_changes: changes,
        ..Default::default()
    };
    for who in ["carol-token", "bob-token"] {
        assert_eq!(
            set(who, l(1000, 0)).await.unwrap_err().code(),
            Code::PermissionDenied
        );
    }
    let r = set("alice-token", l(100_000, 0)).await.unwrap();
    let limits = r.run.unwrap().limits.unwrap();
    assert_eq!(limits.max_tokens, 100_000);
    assert!(r.lowered.is_empty());
    // The policy caps changes at 5: asking for 50 is lowered, and the answer says so.
    let r = set("alice-token", l(0, 50)).await.unwrap();
    assert_eq!(r.lowered, ["changes"]);
    assert_eq!(r.run.unwrap().limits.unwrap().max_changes, 5);
    let token = w.hub.core.runs.get(run.id).await.unwrap().unwrap().token_id;
    if !token.is_empty() {
        let t = w.hub.core.tokens.get(&token).await.unwrap().unwrap();
        assert_eq!(t.limits.tokens, 100_000);
    }
    w.hub.cancel_run(run.id, "done", "7:alice").await.unwrap();
    assert_eq!(
        set("alice-token", l(1, 0)).await.unwrap_err().code(),
        Code::FailedPrecondition
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_create_that_asks_for_more_than_the_policy_allows_says_what_was_lowered() {
    let w = World::new().await;
    w.key(alice(), "mine").await;
    w.fake.set_default(done("ok"));
    let r = w
        .svc
        .create_run(w.request(
            wire::CreateAgentRunRequest {
                repo: repo_ref(),
                agent: "fixer".into(),
                task: "x".into(),
                limits: Some(wire::AgentBudgetLimits {
                    max_changes: 40,
                    ..Default::default()
                }),
                ..Default::default()
            },
            "alice-token",
            Some(&op()),
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(r.lowered, ["changes"]);
    assert_eq!(r.run.unwrap().limits.unwrap().max_changes, 5);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_cancelled_run_continues_as_a_new_run_with_raised_limits() {
    let w = World::new().await;
    w.key(alice(), "mine").await;
    w.fake.set_default(Reply::Delay(
        Duration::from_secs(30),
        Box::new(done("late")),
    ));
    let run = w.start_run("alice-token", "long task").await.unwrap();
    let cont = |who: &str, id: u64| {
        let r = w.request(
            wire::ContinueAgentRunRequest {
                repo: repo_ref(),
                id,
                limits: Some(wire::AgentBudgetLimits {
                    max_tokens: 500_000,
                    ..Default::default()
                }),
            },
            who,
            Some(&op()),
        );
        let svc = w.svc.clone();
        async move { svc.continue_run(r).await.map(tonic::Response::into_inner) }
    };
    assert_eq!(
        cont("alice-token", run.id).await.unwrap_err().code(),
        Code::FailedPrecondition,
        "still running"
    );
    w.hub.cancel_run(run.id, "stop", "7:alice").await.unwrap();
    w.wait(run.id).await;
    for who in ["carol-token", "bob-token"] {
        assert_eq!(
            cont(who, run.id).await.unwrap_err().code(),
            Code::PermissionDenied,
            "{who}"
        );
    }
    assert_eq!(
        cont("alice-token", 987_654).await.unwrap_err().code(),
        Code::NotFound
    );
    let r = cont("alice-token", run.id).await.unwrap();
    let new = r.run.unwrap();
    assert_ne!(new.id, run.id);
    assert_eq!(new.continues_run, run.id);
    assert!(new.task.starts_with("long task"));
    assert!(new.task.contains(&format!("continues run #{}", run.id)));
    assert_eq!(new.limits.unwrap().max_tokens, 500_000);
    w.hub.cancel_run(new.id, "stop", "7:alice").await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_boards_report_estimates_recent_tasks_logs_agents_and_spend() {
    let w = World::new().await;
    w.key(alice(), "mine").await;
    w.fake.set_default(done("ok"));
    let run = w
        .start_run("alice-token", "Rename the helper")
        .await
        .unwrap();
    let row = w.wait(run.id).await;
    assert_eq!(row.state, RunState::Succeeded);
    let call = |who: &str| who.to_owned();
    // Estimate: from finished runs, readable by a reader.
    let est = w
        .svc
        .get_estimate(w.request(
            wire::GetAgentEstimateRequest {
                repo: repo_ref(),
                agent: "fixer".into(),
                task_bytes: 0,
            },
            &call("carol-token"),
            None,
        ))
        .await
        .unwrap()
        .into_inner();
    assert!(est.samples <= 1);
    // Recent tasks: the caller's own; an administrator sees everyone's.
    let tasks = |who: &str| {
        let r = w.request(
            wire::RecentAgentTasksRequest {
                repo: repo_ref(),
                limit: 0,
            },
            who,
            None,
        );
        let svc = w.svc.clone();
        async move { svc.recent_tasks(r).await.unwrap().into_inner().tasks }
    };
    assert_eq!(tasks("alice-token").await[0].task, "Rename the helper");
    assert!(tasks("bob-token").await.is_empty(), "bob started nothing");
    assert!(tasks("carol-token").await.is_empty());
    // The log of a run.
    let log = w
        .svc
        .get_run_log(w.request(
            wire::GetAgentRunLogRequest {
                repo: repo_ref(),
                id: run.id,
            },
            "carol-token",
            None,
        ))
        .await
        .unwrap()
        .into_inner();
    let text = String::from_utf8(log.jsonl.to_vec()).unwrap();
    assert!(
        text.lines()
            .all(|l| serde_json::from_str::<serde_json::Value>(l).is_ok())
    );
    assert!(text.contains("\"type\":\"event\""));
    assert!(log.retained_until > row.finished);
    let missing = w
        .svc
        .get_run_log(w.request(
            wire::GetAgentRunLogRequest {
                repo: repo_ref(),
                id: 424_242,
            },
            "carol-token",
            None,
        ))
        .await
        .unwrap_err();
    assert_eq!(missing.code(), Code::NotFound);
    // The agents of the repository.
    let agents = w
        .svc
        .list_agents(w.request(
            wire::ListRepoAgentsRequest { repo: repo_ref() },
            "carol-token",
            None,
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(agents.agents.len(), 1);
    assert_eq!(
        (agents.agents[0].agent.as_str(), agents.agents[0].runs),
        ("fixer", 1)
    );
    // Spend: the model call is on the board and a monthly cap stops the next run.
    let spend = |who: &str, month: &str| {
        let r = w.request(
            wire::GetAgentSpendRequest {
                repo: repo_ref(),
                month: month.into(),
            },
            who,
            None,
        );
        let svc = w.svc.clone();
        async move { svc.get_spend(r).await.map(tonic::Response::into_inner) }
    };
    assert_eq!(
        spend("carol-token", "").await.unwrap_err().code(),
        Code::PermissionDenied,
        "money is for writers"
    );
    let s = spend("bob-token", "").await.unwrap();
    assert!(
        s.tokens > 0 && !s.days.is_empty() && s.agents[0].agent == "fixer",
        "{s:?}"
    );
    assert_eq!(
        spend("bob-token", "2026-13").await.unwrap_err().code(),
        Code::InvalidArgument
    );
    assert_eq!(spend("bob-token", "2020-01").await.unwrap().tokens, 0);
    let cap = |who: &str, agent: &str, micro: u64| {
        let r = w.request(
            wire::SetMonthlyBudgetRequest {
                repo: repo_ref(),
                agent: agent.into(),
                cap_micro_usd: micro,
            },
            who,
            Some(&op()),
        );
        let svc = w.svc.clone();
        async move {
            svc.set_monthly_budget(r)
                .await
                .map(tonic::Response::into_inner)
        }
    };
    for who in ["carol-token", "bob-token"] {
        assert_eq!(
            cap(who, "", 5).await.unwrap_err().code(),
            Code::PermissionDenied,
            "{who}"
        );
    }
    assert_eq!(
        cap("alice-token", "BAD NAME", 5).await.unwrap_err().code(),
        Code::InvalidArgument
    );
    // Make the spend of the month visible in money, then cap it below that.
    w.hub.record_spend("acme/app", "fixer", 10, 7_000, 1).await;
    cap("alice-token", "fixer", 1_000).await.unwrap();
    let e = w
        .svc
        .create_run(w.request(
            wire::CreateAgentRunRequest {
                repo: repo_ref(),
                agent: "fixer".into(),
                task: "more".into(),
                ..Default::default()
            },
            "alice-token",
            Some(&op()),
        ))
        .await
        .unwrap_err();
    assert_eq!(e.code(), Code::ResourceExhausted, "{e:?}");
    let s = spend("bob-token", "").await.unwrap();
    assert_eq!(s.agents[0].monthly_cap_micro_usd, 1_000);
    cap("alice-token", "fixer", 0).await.unwrap();
    assert_eq!(
        spend("bob-token", "").await.unwrap().agents[0].monthly_cap_micro_usd,
        0
    );
}

fn claim_service(w: &World) -> (AgentService, ForwardedVerifier) {
    let v = ForwardedVerifier::new(vec![5u8; 32]).unwrap();
    let auth: Arc<dyn asc_rpc::auth::Authorizer> =
        Arc::new(ForwardedAuthorizer::new(static_auth(), v.clone()));
    (
        AgentService::new(w.hub.clone(), auth, ServerInfo::default()),
        v,
    )
}

fn host<T>(v: &ForwardedVerifier, m: T, user: &str, role: Role, subject: &str) -> Request<T> {
    use asc_rpc::auth::{BearerToken, Credential};
    let mut r = Request::new(m);
    r.extensions_mut()
        .insert(Credential::bearer(BearerToken::new("host-token").unwrap()));
    let now = i64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs(),
    )
    .unwrap();
    for (k, val) in v.sign(user, role, subject, now) {
        r.metadata_mut().append(k, val.parse().unwrap());
    }
    r.metadata_mut()
        .insert("x-asc-op-id", op().parse().unwrap());
    r
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_hubs_claim_is_the_whole_authority_for_control_calls_and_the_relay_is_audited() {
    let w = World::new().await;
    w.key(alice(), "mine").await;
    w.script(vec![
        tools(vec![(
            "request_permission",
            json!({"action": "push to a branch"}),
        )]),
        done("done"),
    ]);
    let run = w
        .start_run_with(
            "alice-token",
            wire::CreateAgentRunRequest {
                repo: repo_ref(),
                agent: "fixer".into(),
                task: "t".into(),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    until("the request", || async {
        !pending(&w, run.id).await.is_empty()
    })
    .await;
    let id = pending(&w, run.id).await[0].id;
    let (svc, v) = claim_service(&w);
    let msg = || wire::DecideAgentRequestRequest {
        repo: repo_ref(),
        request_id: id,
        decision: wire::AgentDecision::AllowOnce as i32,
        reason: String::new(),
    };
    // A read claim does not decide; another repository's claim is no claim here; a forged one fails.
    let e = svc
        .decide_request(host(&v, msg(), "7:alice", Role::Read, "acme/app"))
        .await
        .unwrap_err();
    assert_eq!(e.code(), Code::PermissionDenied);
    let e = svc
        .decide_request(host(&v, msg(), "7:alice", Role::Admin, "acme/other"))
        .await
        .unwrap_err();
    assert_eq!(e.code(), Code::PermissionDenied);
    let forged = ForwardedVerifier::new(vec![9u8; 32]).unwrap();
    let e = svc
        .decide_request(host(&forged, msg(), "7:alice", Role::Admin, "acme/app"))
        .await
        .unwrap_err();
    assert_eq!(e.code(), Code::PermissionDenied);
    // A write claim of a person who is neither the starter nor an administrator is refused...
    let e = svc
        .decide_request(host(&v, msg(), "8:bob", Role::Write, "acme/app"))
        .await
        .unwrap_err();
    assert_eq!(e.code(), Code::PermissionDenied);
    // ...and the starter's write claim decides, as the starter, with the host as relay.
    let r = svc
        .decide_request(host(&v, msg(), "7:alice", Role::Write, "acme/app"))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(r.request.unwrap().decided_by, "alice");
    let row: (String, String) = {
        use sqlx::Row;
        let r = sqlx::query(
            "SELECT actor_name, on_behalf_name FROM audit_events WHERE action = 'agent.request.decided' ORDER BY seq DESC LIMIT 1",
        )
        .fetch_one(&w.pool)
        .await
        .unwrap();
        (r.get("actor_name"), r.get("on_behalf_name"))
    };
    assert_eq!(row, ("alice".to_owned(), "host".to_owned()));
    w.wait(run.id).await;
}

fn argv(words: &[&str]) -> Vec<String> {
    words.iter().map(|w| (*w).to_owned()).collect()
}

#[test]
fn ask_before_matches_what_would_run_not_the_text_of_the_command() {
    use super::control::needs_permission;
    let ask = vec![argv(&["git", "push"])];
    for yes in [
        &["git", "push"][..],
        &["/usr/bin/git", "push", "origin"],
        &[r"C:\Git\bin\GIT.EXE", "push"],
        &["git", "-C", "dir", "push"],
        &["git", "--no-pager", "push"],
        &["env", "git", "push"],
        &["env", "-i", "FOO=1", "git", "push"],
        &["timeout", "30", "git", "push"],
        &["nice", "-n", "5", "git", "push"],
        &["sudo", "-u", "root", "git", "push"],
        &["xargs", "-n1", "git", "push"],
        &["env", "nice", "timeout", "30", "git", "push"],
        &["sh", "-c", "git push"],
        &["/bin/bash", "-c", "x"],
        &["python3", "-c", "x"],
        &["python3.12", "x.py"],
        &["env", "sh", "-c", "x"],
        &["make", "deploy"],
        &["npx", "something"],
    ] {
        assert!(needs_permission(&argv(yes), &ask), "{yes:?} must ask");
    }
    for no in [
        &["git", "status"][..],
        &["cargo", "test"],
        &["env", "cargo", "test"],
        &["ls"],
        &[],
    ] {
        assert!(!needs_permission(&argv(no), &ask), "{no:?} must not ask");
    }
    // Nothing configured: nothing asks, interpreters included (the allow list is the gate then).
    assert!(!needs_permission(&argv(&["sh", "-c", "x"]), &[]));
    // Too long, or wrapped too deep to examine: ask.
    let long: Vec<String> = (0..65).map(|_| "a".to_owned()).collect();
    assert!(needs_permission(&long, &ask));
    assert!(needs_permission(&argv(&["env"; 12]), &ask));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_unreadable_control_state_stops_the_run_and_a_budget_that_cannot_be_read_counts_as_reached()
 {
    use super::control::{Flow, Hold};
    use super::runtime::Ended;
    let w = World::new().await;
    w.key(alice(), "mine").await;
    w.fake.set_default(Reply::Delay(
        Duration::from_secs(30),
        Box::new(done("late")),
    ));
    let run = w.start_run("alice-token", "slow").await.unwrap();
    let row = w.hub.core.runs.get(run.id).await.unwrap().unwrap();
    let live = Arc::new(super::Live {
        cancel: tokio_util::sync::CancellationToken::new(),
        reason: std::sync::Mutex::new(None),
    });
    assert!(matches!(w.hub.hold(&row, &live).await, Hold::Go));
    sqlx::query("DROP TABLE agent_run_ctl")
        .execute(&w.pool)
        .await
        .unwrap();
    assert!(matches!(
        w.hub.hold(&row, &live).await,
        Hold::Stop(Ended::Failed { .. })
    ));
    let call = crate::ai::model::ToolCall {
        id: "1".into(),
        name: "write_file".into(),
        input: json!({}),
    };
    assert!(matches!(
        w.hub.control_call(&row, &live, &call).await,
        Flow::Stop(Ended::Failed { .. })
    ));
    assert!(!w.hub.monthly_exceeded("acme/app", "fixer").await);
    sqlx::query("DROP TABLE agent_monthly_budget")
        .execute(&w.pool)
        .await
        .unwrap();
    assert!(
        w.hub.monthly_exceeded("acme/app", "fixer").await,
        "an unreadable cap is a reached cap"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_run_for_a_change_names_a_change_of_this_repository_to_babysit_its_checks() {
    let w = World::new().await;
    w.key(alice(), "mine").await;
    w.script(vec![done("watching")]);
    let start = |change: Vec<u8>| {
        w.start_run_with(
            "alice-token",
            wire::CreateAgentRunRequest {
                repo: repo_ref(),
                agent: "fixer".into(),
                task: "watch the checks of this change and fix what fails".into(),
                for_change: change.into(),
                ..Default::default()
            },
        )
    };
    // Not 16 bytes, and a change nobody published here: refused before anything is queued.
    assert_eq!(
        start(vec![1; 5]).await.unwrap_err().code(),
        tonic::Code::InvalidArgument
    );
    assert_eq!(
        start(vec![7; 16]).await.unwrap_err().code(),
        tonic::Code::NotFound
    );
    // A change of another repository is as unknown as a made-up one.
    let ns = crate::notify::ns_of(&w.hub.core.pool, "acme/app")
        .await
        .unwrap()
        .unwrap();
    sqlx::query(
        "INSERT INTO change_versions (ns, change, v, cs, parent_change, at, actor, op_id) \
         VALUES ($1, $2, 1, $3, NULL, 1, '42:alice', $4)",
    )
    .bind(vec![9u8; 4])
    .bind(vec![7u8; 16])
    .bind(vec![3u8; 32])
    .bind(vec![1u8])
    .execute(&w.hub.core.pool)
    .await
    .unwrap();
    assert_eq!(
        start(vec![7; 16]).await.unwrap_err().code(),
        tonic::Code::NotFound
    );
    sqlx::query("UPDATE change_versions SET ns = $1")
        .bind(ns)
        .execute(&w.hub.core.pool)
        .await
        .unwrap();
    let run = start(vec![7; 16]).await.unwrap();
    assert_eq!(run.for_change.as_ref(), &[7u8; 16]);
    // The run holds the hub's id of the person who made it (alice is user 7 here), beside the name.
    assert_eq!(run.created_by_id, 7);
    assert_eq!(w.wait(run.id).await.state, RunState::Succeeded);
}

struct Hub(std::sync::Mutex<Result<Option<crate::profile::Profile>, String>>);

#[async_trait::async_trait]
impl crate::profile::ProfileSource for Hub {
    async fn profile(&self, login: &str) -> Result<Option<crate::profile::Profile>, String> {
        let _ = login;
        self.0.lock().unwrap().clone()
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_agent_gets_the_profile_of_a_person_only_when_the_hub_says_the_person_allows_it() {
    use sqlx::Row;
    let w = World::new().await;
    let ask = |who: &'static str, login: &'static str| {
        let r = w.request(
            wire::GetAgentPersonRequest {
                repo: repo_ref(),
                login: login.into(),
            },
            who,
            None,
        );
        let svc = w.svc.clone();
        async move { svc.get_person(r).await.map(tonic::Response::into_inner) }
    };
    // No hub is set: everything is refused, for anyone who may read.
    let none = ask("carol-token", "maya").await.unwrap();
    assert!(!none.allowed && none.full_name.is_empty());
    let hub = std::sync::Arc::new(Hub(std::sync::Mutex::new(Ok(Some(
        crate::profile::Profile {
            login: "maya".into(),
            full_name: "Maya Okafor".into(),
            location: "Lagos".into(),
            ..Default::default()
        },
    )))));
    assert!(
        w.hub
            .set_profiles(std::sync::Arc::new(crate::profile::AgentProfiles::new(
                Some(hub.clone())
            )))
    );
    // A reader gets what the hub allows (and nothing else: no email, no avatar exist in the answer).
    let got = ask("carol-token", "maya").await.unwrap();
    assert_eq!(
        (got.allowed, got.full_name.as_str(), got.location.as_str()),
        (true, "Maya Okafor", "Lagos")
    );
    // The person switched it off: nothing, in the same shape as a person who does not exist.
    *hub.0.lock().unwrap() = Ok(None);
    let other = ask("carol-token", "nobody").await.unwrap();
    assert!(!other.allowed && other.login.is_empty() && other.full_name.is_empty());
    // The hub cannot be asked: refused, never guessed.
    *hub.0.lock().unwrap() = Err("unreachable".into());
    assert!(!ask("carol-token", "someone").await.unwrap().allowed);
    // Who may ask, and what is a login.
    assert_eq!(
        ask("no-such-token", "maya").await.unwrap_err().code(),
        Code::Unauthenticated
    );
    assert_eq!(
        ask("carol-token", "").await.unwrap_err().code(),
        Code::InvalidArgument
    );
    assert!(!ask("carol-token", "a b").await.unwrap().allowed);
    // Every ask is audited with the answer, and the value is not in the log.
    let rows = sqlx::query(
        "SELECT target FROM audit_events WHERE action = 'agent.profile.read' ORDER BY seq",
    )
    .fetch_all(&w.pool)
    .await
    .unwrap();
    assert!(rows.len() >= 5, "{}", rows.len());
    let all: String = sqlx::query_scalar("SELECT details_json FROM audit_events WHERE action = 'agent.profile.read' ORDER BY seq DESC LIMIT 1")
        .fetch_one(&w.pool)
        .await
        .unwrap_or_default();
    assert!(
        !all.contains("Maya Okafor") && !all.contains("Lagos"),
        "{all}"
    );
    let _ = rows[0].get::<String, _>("target");
}
