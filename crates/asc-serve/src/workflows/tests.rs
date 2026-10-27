//! Workflow definitions, enablement, validation, secrets, authorisation and command steps,
//! against the real services and pipeline (the agent tests' world, plus the workflow service).

use std::future::Future;
use std::time::{Duration, Instant};

use asc_rpc::server::wire;
use asc_rpc::services::workflows_server::Workflows;
use bytes::Bytes;
use tonic::Code;

use super::config::WorkflowsConfig;
use super::store::{RunState, StepState};
use crate::agents::config::SandboxConfig;
use crate::agents::tests::{MAIN_RS, Opts, POLICY, World, repo_ref, sig};
use crate::agents::tests_tokens::code;

pub(crate) const MAINTAINERS: &str = r#"version = 1

[roles]
maintainer = ["user:alice", "user:bob"]

[agents]
default_scope = ["src/**", "tests/**"]
denied_paths = ["src/secret/**"]

[agents.budgets]
max_changes_per_run = 5
"#;

pub(crate) const VERSION: &str = r#"
description = "print the version"

[[trigger]]
kind = "manual"

[[input]]
name = "ticket"
default = "none"

[[step]]
id = "version"
kind = "command"
command = ["git", "--version"]
"#;

pub(crate) const FAILING: &str = r#"
[[trigger]]
kind = "manual"

[[step]]
id = "bad"
kind = "command"
command = ["git", "no-such-subcommand-for-tests"]

[[step]]
id = "after"
kind = "command"
needs = ["bad"]
command = ["git", "--version"]

[[step]]
id = "independent"
kind = "command"
command = ["git", "--version"]
"#;

pub(crate) const TOLERANT: &str = r#"
[[trigger]]
kind = "manual"

[[step]]
id = "bad"
kind = "command"
continue_on_failure = true
command = ["git", "no-such-subcommand-for-tests"]

[[step]]
id = "after"
kind = "command"
needs = ["bad"]
command = ["git", "--version"]
"#;

pub(crate) const APPROVED: &str = r#"
[[trigger]]
kind = "manual"

[[step]]
id = "gate"
kind = "approval"
approvers = ["user:alice"]

[[step]]
id = "after"
kind = "command"
needs = ["gate"]
command = ["git", "--version"]
"#;

pub(crate) const ROLE_GATE: &str = r#"
[[trigger]]
kind = "manual"

[[step]]
id = "gate"
kind = "approval"
approvers = ["role:maintainer"]
"#;

pub(crate) fn git_dir() -> Option<std::path::PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).find(|d| d.join("git").is_file() || d.join("git.exe").is_file())
}

/// A configuration that runs `git --version` and `git config` in the sandbox, and does not poll.
pub(crate) fn wcfg() -> WorkflowsConfig {
    WorkflowsConfig {
        // Working copies of a test never share a directory with another test or another user's.
        work_dir: Some(
            std::env::temp_dir().join(format!("asc-workflows-test-{}", uuid::Uuid::new_v4())),
        ),
        enabled: true,
        poll_interval_secs: 3600,
        sandbox: SandboxConfig {
            allow: vec![
                vec!["git".into(), "--version".into()],
                vec!["git".into(), "config".into()],
                vec!["git".into(), "no-such-subcommand-for-tests".into()],
            ],
            path_dirs: git_dir().into_iter().collect(),
            allow_unisolated: true,
            // A step that holds secrets needs the restricted wrapper; `env` passes the command on.
            restricted_wrapper: if cfg!(unix) {
                vec!["/usr/bin/env".into()]
            } else {
                Vec::new()
            },
            max_command_seconds: 60,
            ..SandboxConfig::default()
        },
        ..WorkflowsConfig::default()
    }
}

/// A world with the given workflow files on trunk.
pub(crate) async fn world_with(
    defs: &[(&'static str, &'static str)],
    policy: &'static str,
    mut cfg: WorkflowsConfig,
) -> World {
    // Each world keeps its working copies in a directory of its own: the default is shared by
    // every test of the process, and one world's startup sweep would remove another's copies.
    cfg.work_dir.get_or_insert_with(|| {
        std::env::temp_dir().join(format!("asc-workflows-test-{}", uuid::Uuid::new_v4()))
    });
    let mut files: Vec<(&'static str, &'static str)> = vec![
        ("src/main.rs", MAIN_RS),
        (
            "src/lib.rs",
            "pub fn add(a: i32, b: i32) -> i32 {\n    a + b\n}\n",
        ),
        ("docs/README.md", "# App\n"),
    ];
    for (name, text) in defs {
        files.push((name, text));
    }
    World::with(Opts {
        files,
        policy: Some(policy),
        workflows: Some(cfg),
        ..Opts::default()
    })
    .await
}

pub(crate) async fn world(defs: &[(&'static str, &'static str)]) -> World {
    world_with(defs, POLICY, wcfg()).await
}

pub(crate) fn svc(w: &World) -> &super::rpc::WorkflowService {
    w.wsvc.as_ref().expect("a world with workflows")
}

pub(crate) async fn until<F, Fut>(what: &str, mut f: F)
where
    F: FnMut() -> Fut,
    Fut: Future<Output = bool>,
{
    let end = Instant::now() + Duration::from_secs(60);
    while !f().await {
        assert!(Instant::now() < end, "gave up waiting for: {what}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

pub(crate) async fn enable(
    w: &World,
    who: &str,
    name: &str,
    on: bool,
) -> Result<wire::WorkflowDefinition, tonic::Status> {
    let r = svc(w)
        .set_enabled(w.request(
            wire::SetWorkflowEnabledRequest {
                repo: repo_ref(),
                name: name.into(),
                enabled: on,
            },
            who,
            None,
        ))
        .await?;
    Ok(r.into_inner().definition.unwrap())
}

pub(crate) async fn trigger_with(
    w: &World,
    who: &str,
    name: &str,
    inputs: &[(&str, &str)],
    change: &[u8],
) -> Result<wire::WorkflowRun, tonic::Status> {
    let op = uuid::Uuid::new_v4().to_string();
    let r = svc(w)
        .trigger(
            w.request(
                wire::TriggerWorkflowRequest {
                    repo: repo_ref(),
                    name: name.into(),
                    inputs: inputs
                        .iter()
                        .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                        .collect(),
                    change: Bytes::copy_from_slice(change),
                },
                who,
                Some(&op),
            ),
        )
        .await?;
    Ok(r.into_inner().run.unwrap())
}

pub(crate) async fn trigger(
    w: &World,
    who: &str,
    name: &str,
) -> Result<wire::WorkflowRun, tonic::Status> {
    trigger_with(w, who, name, &[], &[]).await
}

/// The stored run once it is over.
pub(crate) async fn finished(w: &World, id: u64) -> super::store::RunRow {
    let hub = w.wf.as_ref().unwrap();
    let end = Instant::now() + Duration::from_secs(90);
    loop {
        let r = hub.store.get_run(id).await.unwrap().unwrap();
        if !r.state.is_active() {
            return r;
        }
        assert!(
            Instant::now() < end,
            "run {id} did not finish: {:?}",
            r.state
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

pub(crate) async fn step_of(w: &World, run: u64, step: &str) -> super::store::StepRow {
    w.wf.as_ref()
        .unwrap()
        .store
        .step(run, step)
        .await
        .unwrap()
        .unwrap()
}

pub(crate) async fn wait_step(w: &World, run: u64, step: &str, state: StepState) {
    until(&format!("step {step} to be {state:?}"), || async {
        step_of(w, run, step).await.state == state
    })
    .await;
}

// ------------------------------------------------------------------------------ definitions

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn definitions_come_from_trunk_and_a_broken_one_says_why() {
    let w = world(&[
        (".asc/workflows/version.toml", VERSION),
        (
            ".asc/workflows/broken.toml",
            "[[step]]\nid = \"a\"\nkind = \"agent\"\nbogus = 1\n",
        ),
        (".asc/workflows/Bad Name.toml", VERSION),
        (".asc/workflows/notes.txt", "not a workflow"),
    ])
    .await;
    let list = svc(&w)
        .list_definitions(w.request(
            wire::ListWorkflowDefinitionsRequest { repo: repo_ref() },
            "carol-token",
            None,
        ))
        .await
        .unwrap()
        .into_inner();
    let names: Vec<&str> = list.definitions.iter().map(|d| d.name.as_str()).collect();
    assert_eq!(names, ["Bad Name", "broken", "version"], "{names:?}");
    let by = |n: &str| list.definitions.iter().find(|d| d.name == n).unwrap();
    assert!(by("version").valid && !by("version").enabled);
    assert_eq!(by("version").steps.len(), 1);
    assert_eq!(
        by("version").steps[0].kind,
        wire::WorkflowStepKind::Command as i32
    );
    assert_eq!(by("version").steps[0].command, ["git", "--version"]);
    assert_eq!(
        by("version").triggers[0].kind,
        wire::WorkflowTriggerKind::Manual as i32
    );
    assert_eq!(by("version").inputs[0].name, "ticket");
    assert_eq!(
        by("version").source.len(),
        32,
        "the trunk changeset it was read from"
    );
    assert!(
        !by("broken").valid && by("broken").error.contains("bogus"),
        "{}",
        by("broken").error
    );
    assert!(
        by("broken").error.starts_with(char::is_numeric),
        "the line is first: {}",
        by("broken").error
    );
    assert!(!by("Bad Name").valid);
    let got = svc(&w)
        .get_definition(w.request(
            wire::GetWorkflowDefinitionRequest {
                repo: repo_ref(),
                name: "version".into(),
            },
            "carol-token",
            None,
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(got.text.as_ref(), VERSION.as_bytes());
    let missing = svc(&w)
        .get_definition(w.request(
            wire::GetWorkflowDefinitionRequest {
                repo: repo_ref(),
                name: "nope".into(),
            },
            "carol-token",
            None,
        ))
        .await
        .unwrap_err();
    assert_eq!(missing.code(), Code::NotFound);
    let anon = svc(&w)
        .list_definitions(tonic::Request::new(wire::ListWorkflowDefinitionsRequest {
            repo: repo_ref(),
        }))
        .await
        .unwrap_err();
    assert_eq!(anon.code(), Code::Unauthenticated);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_workflow_on_a_branch_is_not_a_workflow_until_it_is_on_trunk() {
    let w = world(&[]).await;
    // An agent-style branch carries a workflow file; trunk does not.
    let secret = {
        let tokens = w.svc.tokens();
        let op = uuid::Uuid::new_v4().to_string();
        asc_rpc::services::agent_tokens_server::AgentTokens::create_token(
            &tokens,
            w.request(
                wire::CreateAgentTokenRequest {
                    repo: repo_ref(),
                    name: "n".into(),
                    agent: "fixer".into(),
                    ttl_seconds: 600,
                    ..Default::default()
                },
                "alice-token",
                Some(&op),
            ),
        )
        .await
        .unwrap()
        .into_inner()
        .secret
    };
    // `.asc/` is outside the agent's scope, so the commit is refused: an agent cannot add one at all.
    let refused = w
        .inproc
        .commit(
            &secret,
            wire::CommitRequest {
                repo: repo_ref(),
                parent: w.base.iter().map(|b| format!("{b:02x}")).collect(),
                ops: vec![wire::FileOp {
                    path: Bytes::from_static(b".asc/workflows/evil.toml"),
                    op: Some(wire::file_op::Op::PutContent(Bytes::from_static(
                        VERSION.as_bytes(),
                    ))),
                    mode: 0,
                }],
                message: Bytes::from_static(b"add a workflow"),
                author: Some(sig()),
                committer: Some(sig()),
                update_ref: "refs/heads/agent/fixer/evil".into(),
                expect: None,
                sign: false,
            },
        )
        .await
        .unwrap_err();
    assert_eq!(code(&refused), "AGENT_SCOPE_DENIED");
    // A person's branch with the file is also not read: only trunk counts.
    w.inproc
        .commit(
            "alice-token",
            wire::CommitRequest {
                repo: repo_ref(),
                parent: w.base.iter().map(|b| format!("{b:02x}")).collect(),
                ops: vec![wire::FileOp {
                    path: Bytes::from_static(b".asc/workflows/mine.toml"),
                    op: Some(wire::file_op::Op::PutContent(Bytes::from_static(
                        VERSION.as_bytes(),
                    ))),
                    mode: 0,
                }],
                message: Bytes::from_static(b"propose a workflow"),
                author: Some(sig()),
                committer: Some(sig()),
                update_ref: "refs/heads/proposal".into(),
                expect: None,
                sign: false,
            },
        )
        .await
        .unwrap();
    let list = svc(&w)
        .list_definitions(w.request(
            wire::ListWorkflowDefinitionsRequest { repo: repo_ref() },
            "alice-token",
            None,
        ))
        .await
        .unwrap()
        .into_inner();
    assert!(list.definitions.is_empty(), "{:?}", list.definitions);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn validate_reports_where_a_file_is_wrong_and_keeps_nothing() {
    let w = world(&[]).await;
    let call = |name: &str, text: &str, who: &str| {
        svc(&w).validate(w.request(
            wire::ValidateWorkflowRequest {
                repo: repo_ref(),
                name: name.into(),
                text: Bytes::copy_from_slice(text.as_bytes()),
            },
            who,
            None,
        ))
    };
    let ok = call("version", VERSION, "carol-token")
        .await
        .unwrap()
        .into_inner();
    assert!(ok.valid && ok.diagnostics.is_empty());
    assert_eq!(ok.definition.unwrap().steps.len(), 1);
    // Unknown key: refused with its line.
    let bad = call(
        "w",
        "[[step]]\nid = \"a\"\nkind = \"approval\"\napprovers = [\"role:maintainer\"]\nbogus = 1\n",
        "carol-token",
    )
    .await
    .unwrap()
    .into_inner();
    assert!(!bad.valid);
    assert_eq!(bad.diagnostics[0].code, "WORKFLOW_INVALID");
    assert_eq!(bad.diagnostics[0].line, 5, "{:?}", bad.diagnostics);
    // The server's own rule on schedules.
    let sched = "[[trigger]]\nkind = \"schedule\"\ncron = \"* * * * *\"\n[[step]]\nid = \"a\"\nkind = \"approval\"\napprovers = [\"role:maintainer\"]\n";
    let r = call("w", sched, "carol-token").await.unwrap().into_inner();
    assert!(
        !r.valid && r.diagnostics[0].message.contains("60 seconds"),
        "{r:?}"
    );
    // Input checks.
    assert_eq!(
        call("Bad Name", VERSION, "carol-token")
            .await
            .unwrap_err()
            .code(),
        Code::InvalidArgument
    );
    assert_eq!(
        call("w", &"x".repeat(64 * 1024 + 1), "carol-token")
            .await
            .unwrap_err()
            .code(),
        Code::InvalidArgument
    );
    let anon = svc(&w)
        .validate(tonic::Request::new(wire::ValidateWorkflowRequest {
            repo: repo_ref(),
            name: "w".into(),
            text: Bytes::new(),
        }))
        .await
        .unwrap_err();
    assert_eq!(anon.code(), Code::Unauthenticated);
    // Nothing was stored.
    let n: i64 = sqlx::query_scalar("SELECT CAST(COUNT(*) AS BIGINT) FROM workflow_state")
        .fetch_one(&w.pool)
        .await
        .unwrap();
    assert_eq!(n, 0);
}

// ------------------------------------------------------------------------------ enabling

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn only_a_maintainer_switches_a_workflow_and_only_a_valid_one() {
    let w = world(&[
        (".asc/workflows/version.toml", VERSION),
        (".asc/workflows/broken.toml", "bogus = 1\n"),
        (".asc/workflows/agentic.toml", "[[trigger]]\nkind = \"manual\"\n[[step]]\nid = \"a\"\nkind = \"agent\"\nagent = \"fixer\"\nprompt = \"p\"\n"),
    ])
    .await;
    // A writer who is not a maintainer, a reader and nobody are refused.
    assert_eq!(
        enable(&w, "bob-token", "version", true)
            .await
            .unwrap_err()
            .code(),
        Code::PermissionDenied
    );
    assert_eq!(
        enable(&w, "carol-token", "version", true)
            .await
            .unwrap_err()
            .code(),
        Code::PermissionDenied
    );
    let anon = svc(&w)
        .set_enabled(tonic::Request::new(wire::SetWorkflowEnabledRequest {
            repo: repo_ref(),
            name: "version".into(),
            enabled: true,
        }))
        .await
        .unwrap_err();
    assert_eq!(anon.code(), Code::Unauthenticated);
    // A broken definition cannot be switched on; a missing one does not exist.
    let e = enable(&w, "alice-token", "broken", true).await.unwrap_err();
    assert_eq!(code(&e), "WORKFLOW_INVALID");
    assert_eq!(
        enable(&w, "alice-token", "nope", true)
            .await
            .unwrap_err()
            .code(),
        Code::NotFound
    );
    // A maintainer does it, twice with the same result, and off again.
    assert!(
        enable(&w, "alice-token", "version", true)
            .await
            .unwrap()
            .enabled
    );
    assert!(
        enable(&w, "alice-token", "version", true)
            .await
            .unwrap()
            .enabled
    );
    assert!(
        !enable(&w, "alice-token", "version", false)
            .await
            .unwrap()
            .enabled
    );
    // An agent step needs agents on.
    assert!(
        enable(&w, "alice-token", "agentic", true)
            .await
            .unwrap()
            .enabled
    );
    let off = World::with(Opts {
        files: vec![
            ("src/main.rs", MAIN_RS),
            (".asc/workflows/agentic.toml", "[[trigger]]\nkind = \"manual\"\n[[step]]\nid = \"a\"\nkind = \"agent\"\nagent = \"fixer\"\nprompt = \"p\"\n"),
        ],
        cfg: Box::new(|c| c.enabled = false),
        workflows: Some(wcfg()),
        ..Opts::default()
    })
    .await;
    let e = enable(&off, "alice-token", "agentic", true)
        .await
        .unwrap_err();
    assert_eq!(code(&e), "AGENTS_DISABLED");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn with_workflows_off_on_the_instance_nothing_starts() {
    let w = world_with(
        &[(".asc/workflows/version.toml", VERSION)],
        POLICY,
        WorkflowsConfig {
            enabled: false,
            ..wcfg()
        },
    )
    .await;
    let e = enable(&w, "alice-token", "version", true)
        .await
        .unwrap_err();
    assert_eq!(code(&e), "WORKFLOW_DISABLED");
    let e = trigger(&w, "alice-token", "version").await.unwrap_err();
    assert_eq!(code(&e), "WORKFLOW_DISABLED");
    // Reading definitions still works, so a person can see what is there.
    let list = svc(&w)
        .list_definitions(w.request(
            wire::ListWorkflowDefinitionsRequest { repo: repo_ref() },
            "carol-token",
            None,
        ))
        .await
        .unwrap();
    assert_eq!(list.into_inner().definitions.len(), 1);
}

// ------------------------------------------------------------------------------ manual runs of commands

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_manual_run_checks_its_inputs_its_switch_and_who_asks() {
    let w = world(&[(".asc/workflows/version.toml", VERSION), (".asc/workflows/timer.toml", "[[trigger]]\nkind = \"schedule\"\ncron = \"0 3 * * *\"\n[[step]]\nid = \"a\"\nkind = \"approval\"\napprovers = [\"role:maintainer\"]\n")]).await;
    // Not switched on yet.
    let e = trigger(&w, "alice-token", "version").await.unwrap_err();
    assert_eq!(code(&e), "WORKFLOW_DISABLED");
    enable(&w, "alice-token", "version", true).await.unwrap();
    enable(&w, "alice-token", "timer", true).await.unwrap();
    // A reader and nobody may not; neither may an agent.
    assert_eq!(
        trigger(&w, "carol-token", "version")
            .await
            .unwrap_err()
            .code(),
        Code::PermissionDenied
    );
    let tokens = w.svc.tokens();
    let agent = {
        let op = uuid::Uuid::new_v4().to_string();
        asc_rpc::services::agent_tokens_server::AgentTokens::create_token(
            &tokens,
            w.request(
                wire::CreateAgentTokenRequest {
                    repo: repo_ref(),
                    name: "n".into(),
                    agent: "fixer".into(),
                    ttl_seconds: 600,
                    ..Default::default()
                },
                "alice-token",
                Some(&op),
            ),
        )
        .await
        .unwrap()
        .into_inner()
        .secret
    };
    assert_eq!(
        trigger(&w, &agent, "version").await.unwrap_err().code(),
        Code::PermissionDenied,
        "an agent cannot start workflows"
    );
    // Bad input.
    assert_eq!(
        trigger_with(&w, "bob-token", "version", &[("nope", "x")], &[])
            .await
            .unwrap_err()
            .code(),
        Code::InvalidArgument
    );
    assert_eq!(
        trigger_with(
            &w,
            "bob-token",
            "version",
            &[("ticket", &"x".repeat(4097))],
            &[]
        )
        .await
        .unwrap_err()
        .code(),
        Code::InvalidArgument
    );
    assert_eq!(
        trigger_with(&w, "bob-token", "version", &[], &[1, 2, 3])
            .await
            .unwrap_err()
            .code(),
        Code::InvalidArgument
    );
    assert_eq!(
        trigger_with(&w, "bob-token", "version", &[], &[7u8; 16])
            .await
            .unwrap_err()
            .code(),
        Code::NotFound
    );
    assert_eq!(
        trigger(&w, "bob-token", "nope").await.unwrap_err().code(),
        Code::NotFound
    );
    // A workflow with no manual trigger is not started by hand.
    assert_eq!(
        trigger(&w, "bob-token", "timer").await.unwrap_err().code(),
        Code::FailedPrecondition
    );
    // A writer starts one.
    let run = trigger_with(&w, "bob-token", "version", &[("ticket", "T-1")], &[])
        .await
        .unwrap();
    assert_eq!(run.started_by, "bob");
    assert_eq!(run.inputs.get("ticket").map(String::as_str), Some("T-1"));
    assert_eq!(run.trigger, wire::WorkflowTriggerKind::Manual as i32);
    assert_eq!(run.steps.len(), 1);
    // Nothing was started by the refusals.
    let n: i64 = sqlx::query_scalar("SELECT CAST(COUNT(*) AS BIGINT) FROM workflow_runs")
        .fetch_one(&w.pool)
        .await
        .unwrap();
    assert_eq!(n, 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_command_step_runs_in_the_sandbox_and_its_result_is_a_check() {
    if git_dir().is_none() {
        return;
    }
    let w = world(&[(".asc/workflows/version.toml", VERSION)]).await;
    enable(&w, "alice-token", "version", true).await.unwrap();
    let run = trigger(&w, "bob-token", "version").await.unwrap();
    let row = finished(&w, run.id).await;
    assert_eq!(row.state, RunState::Succeeded, "{row:?}");
    let step = step_of(&w, run.id, "version").await;
    assert_eq!(step.state, StepState::Succeeded);
    assert_eq!(step.exit_code, 0);
    assert!(step.output.contains("git version"), "{}", step.output);
    // The result is a check of the CI service, named for the workflow and step.
    let rows: Vec<(String, String, i64)> =
        sqlx::query_as("SELECT source, name, status FROM check_runs")
            .fetch_all(&w.pool)
            .await
            .unwrap();
    assert_eq!(
        rows,
        [("asc-workflow".to_owned(), "version/version".to_owned(), 3)]
    );
    assert!(step.ci_run_id > 0);
    // GetRun shows the output; an event snapshot leaves it out.
    let got = svc(&w)
        .get_run(w.request(
            wire::GetWorkflowRunRequest {
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
    assert!(got.steps[0].output.contains("git version"));
    assert_eq!(got.state, wire::WorkflowRunState::Succeeded as i32);
    let events =
        w.wf.as_ref()
            .unwrap()
            .store
            .events_after(run.id, 0, 100)
            .await
            .unwrap();
    assert!(events.len() >= 3, "queued, running, step changes, finished");
    for (_, _, _, snap) in &events {
        use prost::Message as _;
        let r = wire::WorkflowRun::decode(snap.as_slice()).unwrap();
        assert!(r.steps.iter().all(|s| s.output.is_empty()));
    }
    // The working copy is gone.
    let left = std::fs::read_dir(w.wf.as_ref().unwrap().work_dir.clone())
        .map(|d| d.count())
        .unwrap_or(0);
    assert_eq!(left, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_failed_step_skips_what_needs_it_and_continue_on_failure_lets_them_run() {
    if git_dir().is_none() {
        return;
    }
    let w = world(&[
        (".asc/workflows/failing.toml", FAILING),
        (".asc/workflows/tolerant.toml", TOLERANT),
    ])
    .await;
    enable(&w, "alice-token", "failing", true).await.unwrap();
    enable(&w, "alice-token", "tolerant", true).await.unwrap();
    let run = trigger(&w, "bob-token", "failing").await.unwrap();
    let row = finished(&w, run.id).await;
    assert_eq!(row.state, RunState::Failed);
    assert!(row.error.contains("bad"), "{}", row.error);
    assert_eq!(step_of(&w, run.id, "bad").await.state, StepState::Failed);
    assert_eq!(step_of(&w, run.id, "after").await.state, StepState::Skipped);
    assert_eq!(
        step_of(&w, run.id, "independent").await.state,
        StepState::Succeeded,
        "steps that do not need it still run"
    );
    let bad = step_of(&w, run.id, "bad").await;
    assert_ne!(bad.exit_code, 0);
    assert!(bad.error.contains("exit code"));
    let statuses: Vec<(String, i64)> =
        sqlx::query_as("SELECT name, status FROM check_runs ORDER BY name")
            .fetch_all(&w.pool)
            .await
            .unwrap();
    assert!(
        statuses.contains(&("failing/bad".to_owned(), 4)),
        "{statuses:?}"
    );

    let run = trigger(&w, "bob-token", "tolerant").await.unwrap();
    let row = finished(&w, run.id).await;
    assert_eq!(row.state, RunState::Succeeded, "{row:?}");
    assert_eq!(step_of(&w, run.id, "bad").await.state, StepState::Failed);
    assert_eq!(
        step_of(&w, run.id, "after").await.state,
        StepState::Succeeded
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn commands_need_the_operator_and_stay_inside_the_allow_list() {
    // The default configuration allows no command at all.
    let w = world_with(
        &[(".asc/workflows/version.toml", VERSION)],
        POLICY,
        WorkflowsConfig {
            sandbox: SandboxConfig::default(),
            ..wcfg()
        },
    )
    .await;
    enable(&w, "alice-token", "version", true).await.unwrap();
    let run = trigger(&w, "bob-token", "version").await.unwrap();
    let row = finished(&w, run.id).await;
    assert_eq!(row.state, RunState::Failed);
    let step = step_of(&w, run.id, "version").await;
    assert!(step.error.contains("not enabled"), "{}", step.error);

    if git_dir().is_none() {
        return;
    }
    // An allow list that does not name the command refuses it, and says what is allowed.
    let cfg = WorkflowsConfig {
        sandbox: SandboxConfig {
            allow: vec![vec!["git".into(), "status".into()]],
            ..wcfg().sandbox
        },
        ..wcfg()
    };
    let w = world_with(&[(".asc/workflows/version.toml", VERSION)], POLICY, cfg).await;
    enable(&w, "alice-token", "version", true).await.unwrap();
    let run = trigger(&w, "bob-token", "version").await.unwrap();
    finished(&w, run.id).await;
    let step = step_of(&w, run.id, "version").await;
    assert_eq!(step.state, StepState::Failed);
    assert!(step.error.contains("git status"), "{}", step.error);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_command_that_runs_too_long_is_killed_and_the_step_says_so() {
    let (program, args): (&str, Vec<&str>) = if cfg!(windows) {
        ("ping", vec!["-n", "30", "127.0.0.1"])
    } else {
        ("sleep", vec!["30"])
    };
    let Some(dir) = std::env::var_os("PATH").and_then(|p| {
        std::env::split_paths(&p)
            .find(|d| d.join(program).is_file() || d.join(format!("{program}.exe")).is_file())
    }) else {
        return;
    };
    let mut argv = vec![program.to_owned()];
    argv.extend(args.iter().map(|a| (*a).to_owned()));
    let text: &'static str = Box::leak(
        format!(
            "[[trigger]]\nkind = \"manual\"\n[[step]]\nid = \"slow\"\nkind = \"command\"\ntimeout_seconds = 1\ncommand = [{}]\n",
            argv.iter().map(|a| format!("{a:?}")).collect::<Vec<_>>().join(", ")
        )
        .into_boxed_str(),
    );
    let cfg = WorkflowsConfig {
        sandbox: SandboxConfig {
            allow: vec![vec![program.into()]],
            path_dirs: vec![dir],
            allow_unisolated: true,
            ..SandboxConfig::default()
        },
        ..wcfg()
    };
    let w = world_with(&[(".asc/workflows/slow.toml", text)], POLICY, cfg).await;
    enable(&w, "alice-token", "slow", true).await.unwrap();
    let started = Instant::now();
    let run = trigger(&w, "bob-token", "slow").await.unwrap();
    let row = finished(&w, run.id).await;
    assert!(
        started.elapsed() < Duration::from_secs(20),
        "{:?}",
        started.elapsed()
    );
    assert_eq!(row.state, RunState::Failed);
    let step = step_of(&w, run.id, "slow").await;
    assert!(
        step.timed_out && step.error.contains("timed out"),
        "{step:?}"
    );
}

// ------------------------------------------------------------------------------ secrets

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_secret_is_sealed_shown_nowhere_and_scrubbed_from_the_output_of_the_step_that_reads_it() {
    if git_dir().is_none() || wcfg().sandbox.restricted_wrapper.is_empty() {
        return;
    }
    // `git config` prints a value set through the environment the secrets provide.
    let def: &'static str = r#"
[[trigger]]
kind = "manual"

[[step]]
id = "show"
kind = "command"
command = ["git", "config", "asc.test"]
secrets = { GIT_CONFIG_COUNT = "count", GIT_CONFIG_KEY_0 = "key", GIT_CONFIG_VALUE_0 = "value" }
"#;
    let w = world(&[(".asc/workflows/show.toml", def)]).await;
    enable(&w, "alice-token", "show", true).await.unwrap();
    let (logs, _g) = crate::ai::tests::capture_logs();
    let planted = "planted-secret-value-0123456789";
    let set = |who: &str, name: &str, value: &str| {
        let op = uuid::Uuid::new_v4().to_string();
        svc(&w).set_secret(w.request(
            wire::SetWorkflowSecretRequest {
                repo: repo_ref(),
                name: name.into(),
                value: value.into(),
            },
            who,
            Some(&op),
        ))
    };
    // Only a repository administrator sets secrets.
    assert_eq!(
        set("bob-token", "count", "1").await.unwrap_err().code(),
        Code::PermissionDenied
    );
    assert_eq!(
        set("carol-token", "count", "1").await.unwrap_err().code(),
        Code::PermissionDenied
    );
    for bad in ["", "a b", "a/b", &"n".repeat(65)] {
        assert_eq!(
            set("alice-token", bad, "x").await.unwrap_err().code(),
            Code::InvalidArgument,
            "{bad:?}"
        );
    }
    assert_eq!(
        set("alice-token", "count", "").await.unwrap_err().code(),
        Code::InvalidArgument
    );
    assert_eq!(
        set("alice-token", "count", &"v".repeat(16 * 1024 + 1))
            .await
            .unwrap_err()
            .code(),
        Code::InvalidArgument
    );
    set("alice-token", "count", "1").await.unwrap();
    set("alice-token", "key", "asc.test").await.unwrap();
    let info = set("alice-token", "value", planted)
        .await
        .unwrap()
        .into_inner()
        .secret
        .unwrap();
    assert_eq!(
        info.hint,
        &planted[planted.len() - 4..],
        "a long value shows its last four characters"
    );
    assert!(!format!("{info:?}").contains(planted));
    let short = set("alice-token", "tiny", "abc")
        .await
        .unwrap()
        .into_inner()
        .secret
        .unwrap();
    assert_eq!(short.hint, "", "a short value shows nothing");
    // Stored sealed, not as text, bound to the repository and the name.
    let sealed: Vec<u8> =
        sqlx::query_scalar("SELECT sealed FROM workflow_secrets WHERE name = 'value'")
            .fetch_one(&w.pool)
            .await
            .unwrap();
    assert!(!String::from_utf8_lossy(&sealed).contains(planted));
    // Listing shows names and hints only, to administrators only.
    let list = svc(&w)
        .list_secrets(w.request(
            wire::ListWorkflowSecretsRequest { repo: repo_ref() },
            "alice-token",
            None,
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(
        list.secrets
            .iter()
            .map(|s| s.name.as_str())
            .collect::<Vec<_>>(),
        ["count", "key", "tiny", "value"]
    );
    assert!(!format!("{list:?}").contains(planted));
    assert_eq!(
        svc(&w)
            .list_secrets(w.request(
                wire::ListWorkflowSecretsRequest { repo: repo_ref() },
                "bob-token",
                None
            ))
            .await
            .unwrap_err()
            .code(),
        Code::PermissionDenied
    );

    // The step reads the secret; the output shows the stars where the value was.
    let run = trigger(&w, "bob-token", "show").await.unwrap();
    let row = finished(&w, run.id).await;
    assert_eq!(row.state, RunState::Succeeded, "{row:?}");
    let step = step_of(&w, run.id, "show").await;
    assert!(
        step.output.contains("***"),
        "the value was printed and scrubbed: {:?}",
        step.output
    );
    assert!(!step.output.contains(planted));
    // It is nowhere else: not in the run, the events, the check log, the logs.
    let everything = format!(
        "{:?}",
        sqlx::query_as::<_, (String,)>("SELECT output FROM workflow_steps")
            .fetch_all(&w.pool)
            .await
            .unwrap()
    );
    assert!(!everything.contains(planted));
    let logged: Vec<(Vec<u8>,)> = sqlx::query_as("SELECT data FROM check_job_logs")
        .fetch_all(&w.pool)
        .await
        .unwrap();
    assert!(
        logged
            .iter()
            .all(|(d,)| !String::from_utf8_lossy(d).contains(planted))
    );
    assert!(!logs.text().contains(planted));
    let events =
        w.wf.as_ref()
            .unwrap()
            .store
            .events_after(run.id, 0, 100)
            .await
            .unwrap();
    assert!(
        events
            .iter()
            .all(|(_, _, _, snap)| !String::from_utf8_lossy(snap).contains(planted))
    );

    // Deleting removes it, and a step that names it then fails clearly.
    let del = |name: &str| {
        svc(&w).delete_secret(w.request(
            wire::DeleteWorkflowSecretRequest {
                repo: repo_ref(),
                name: name.into(),
            },
            "alice-token",
            None,
        ))
    };
    del("value").await.unwrap();
    del("value").await.unwrap();
    let run = trigger(&w, "bob-token", "show").await.unwrap();
    finished(&w, run.id).await;
    let step = step_of(&w, run.id, "show").await;
    assert!(
        step.error.contains("\"value\" is not set"),
        "{}",
        step.error
    );
    assert!(!step.error.contains(planted));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn secrets_survive_a_master_key_rotation() {
    let w = world(&[]).await;
    let op = uuid::Uuid::new_v4().to_string();
    svc(&w)
        .set_secret(w.request(
            wire::SetWorkflowSecretRequest {
                repo: repo_ref(),
                name: "token".into(),
                value: "rotating-secret-0123456789".into(),
            },
            "alice-token",
            Some(&op),
        ))
        .await
        .unwrap();
    let before: i64 = sqlx::query_scalar("SELECT key_gen FROM workflow_secrets")
        .fetch_one(&w.pool)
        .await
        .unwrap();
    w.vault.rotate(false).await.unwrap();
    w.vault.wait_idle().await;
    let after: i64 = sqlx::query_scalar("SELECT key_gen FROM workflow_secrets")
        .fetch_one(&w.pool)
        .await
        .unwrap();
    assert!(after > before, "{before} -> {after}");
    let hub = w.wf.as_ref().unwrap();
    let got = hub
        .secrets
        .open("acme/app", "token")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(got.as_str(), "rotating-secret-0123456789");
    // Another repository's name for the same secret does not open it: the value is bound to its repository.
    assert!(
        hub.secrets
            .open("acme/other", "token")
            .await
            .unwrap()
            .is_none()
    );
}

// ------------------------------------------------------------------------------ approvals

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_approval_waits_for_a_named_person_and_rejection_ends_the_run() {
    if git_dir().is_none() {
        return;
    }
    let w = world(&[(".asc/workflows/approved.toml", APPROVED)]).await;
    enable(&w, "alice-token", "approved", true).await.unwrap();
    let run = trigger(&w, "bob-token", "approved").await.unwrap();
    wait_step(&w, run.id, "gate", StepState::Waiting).await;
    until("the run to wait for approval", || async {
        w.wf.as_ref()
            .unwrap()
            .store
            .get_run(run.id)
            .await
            .unwrap()
            .unwrap()
            .state
            == RunState::WaitingApproval
    })
    .await;
    assert_eq!(
        step_of(&w, run.id, "after").await.state,
        StepState::Pending,
        "nothing after the gate runs yet"
    );
    let decide = |who: &str, approve: bool, comment: &str| {
        let op = uuid::Uuid::new_v4().to_string();
        svc(&w).decide_approval(w.request(
            wire::DecideWorkflowApprovalRequest {
                repo: repo_ref(),
                id: run.id,
                step: "gate".into(),
                approve,
                comment: comment.into(),
            },
            who,
            Some(&op),
        ))
    };
    // The person who started the run is not an approver (the step names alice), nor is a reader.
    assert_eq!(
        decide("bob-token", true, "").await.unwrap_err().code(),
        Code::PermissionDenied
    );
    assert_eq!(
        decide("carol-token", true, "").await.unwrap_err().code(),
        Code::PermissionDenied
    );
    assert_eq!(
        decide("alice-token", true, &"c".repeat(1025))
            .await
            .unwrap_err()
            .code(),
        Code::InvalidArgument
    );
    // Other steps are not approvals.
    let op = uuid::Uuid::new_v4().to_string();
    let wrong = svc(&w)
        .decide_approval(w.request(
            wire::DecideWorkflowApprovalRequest {
                repo: repo_ref(),
                id: run.id,
                step: "after".into(),
                approve: true,
                comment: String::new(),
            },
            "alice-token",
            Some(&op),
        ))
        .await
        .unwrap_err();
    assert_eq!(wrong.code(), Code::FailedPrecondition);
    // The named approver decides.
    let r = decide("alice-token", true, "looks fine")
        .await
        .unwrap()
        .into_inner()
        .run
        .unwrap();
    assert!(
        r.steps
            .iter()
            .any(|s| s.id == "gate" && s.decided_by == "alice" && s.comment == "looks fine")
    );
    let row = finished(&w, run.id).await;
    assert_eq!(row.state, RunState::Succeeded, "{row:?}");
    assert_eq!(
        step_of(&w, run.id, "after").await.state,
        StepState::Succeeded
    );
    // A second decision on a decided step is refused.
    let e = decide("alice-token", false, "").await.unwrap_err();
    assert!(matches!(e.code(), Code::FailedPrecondition), "{e:?}");

    // Rejection fails the run and skips what needs the gate.
    let run2 = trigger(&w, "bob-token", "approved").await.unwrap();
    wait_step(&w, run2.id, "gate", StepState::Waiting).await;
    let op = uuid::Uuid::new_v4().to_string();
    svc(&w)
        .decide_approval(w.request(
            wire::DecideWorkflowApprovalRequest {
                repo: repo_ref(),
                id: run2.id,
                step: "gate".into(),
                approve: false,
                comment: "no".into(),
            },
            "alice-token",
            Some(&op),
        ))
        .await
        .unwrap();
    let row = finished(&w, run2.id).await;
    assert_eq!(row.state, RunState::Failed);
    assert_eq!(step_of(&w, run2.id, "gate").await.state, StepState::Failed);
    assert_eq!(
        step_of(&w, run2.id, "after").await.state,
        StepState::Skipped
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_person_who_started_a_run_does_not_approve_it_through_a_role() {
    let w = world_with(
        &[(".asc/workflows/gated.toml", ROLE_GATE)],
        MAINTAINERS,
        wcfg(),
    )
    .await;
    enable(&w, "alice-token", "gated", true).await.unwrap();
    let run = trigger(&w, "alice-token", "gated").await.unwrap();
    wait_step(&w, run.id, "gate", StepState::Waiting).await;
    let decide = |who: &str| {
        let op = uuid::Uuid::new_v4().to_string();
        svc(&w).decide_approval(w.request(
            wire::DecideWorkflowApprovalRequest {
                repo: repo_ref(),
                id: run.id,
                step: "gate".into(),
                approve: true,
                comment: String::new(),
            },
            who,
            Some(&op),
        ))
    };
    // Alice is a maintainer but started the run: the role does not name her.
    assert_eq!(
        decide("alice-token").await.unwrap_err().code(),
        Code::PermissionDenied
    );
    // Bob is the other maintainer.
    decide("bob-token").await.unwrap();
    assert_eq!(finished(&w, run.id).await.state, RunState::Succeeded);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_agent_cannot_approve_and_an_unanswered_approval_times_out() {
    let timed: &'static str = "[[trigger]]\nkind = \"manual\"\n[[step]]\nid = \"gate\"\nkind = \"approval\"\napprovers = [\"user:*\"]\ntimeout_seconds = 2\n";
    let w = world(&[(".asc/workflows/timed.toml", timed)]).await;
    enable(&w, "alice-token", "timed", true).await.unwrap();
    let run = trigger(&w, "bob-token", "timed").await.unwrap();
    wait_step(&w, run.id, "gate", StepState::Waiting).await;
    let agent = {
        let op = uuid::Uuid::new_v4().to_string();
        asc_rpc::services::agent_tokens_server::AgentTokens::create_token(
            &w.svc.tokens(),
            w.request(
                wire::CreateAgentTokenRequest {
                    repo: repo_ref(),
                    name: "n".into(),
                    agent: "fixer".into(),
                    ttl_seconds: 600,
                    ..Default::default()
                },
                "alice-token",
                Some(&op),
            ),
        )
        .await
        .unwrap()
        .into_inner()
        .secret
    };
    let op = uuid::Uuid::new_v4().to_string();
    let e = svc(&w)
        .decide_approval(w.request(
            wire::DecideWorkflowApprovalRequest {
                repo: repo_ref(),
                id: run.id,
                step: "gate".into(),
                approve: true,
                comment: String::new(),
            },
            &agent,
            Some(&op),
        ))
        .await
        .unwrap_err();
    assert_eq!(e.code(), Code::PermissionDenied, "an agent never decides");
    let row = finished(&w, run.id).await;
    assert_eq!(row.state, RunState::Failed);
    assert!(
        step_of(&w, run.id, "gate")
            .await
            .error
            .contains("nobody decided")
    );
}

// ------------------------------------------------------------------------------ cancelling, listing, watching

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cancelling_stops_a_waiting_run_and_is_for_its_starter_and_maintainers() {
    let w = world(&[(".asc/workflows/approved.toml", APPROVED)]).await;
    enable(&w, "alice-token", "approved", true).await.unwrap();
    let run = trigger(&w, "bob-token", "approved").await.unwrap();
    wait_step(&w, run.id, "gate", StepState::Waiting).await;
    let cancel = |who: &str, reason: &str| {
        svc(&w).cancel_run(w.request(
            wire::CancelWorkflowRunRequest {
                repo: repo_ref(),
                id: run.id,
                reason: reason.into(),
            },
            who,
            None,
        ))
    };
    assert_eq!(
        cancel("carol-token", "").await.unwrap_err().code(),
        Code::PermissionDenied
    );
    assert_eq!(
        cancel("alice-token", &"r".repeat(257))
            .await
            .unwrap_err()
            .code(),
        Code::InvalidArgument
    );
    // The maintainer cancels bob's run.
    let got = cancel("alice-token", "stale")
        .await
        .unwrap()
        .into_inner()
        .run
        .unwrap();
    assert_eq!(got.state, wire::WorkflowRunState::Cancelled as i32);
    assert!(got.error.contains("stale"));
    assert_eq!(
        step_of(&w, run.id, "gate").await.state,
        StepState::Cancelled
    );
    assert_eq!(
        step_of(&w, run.id, "after").await.state,
        StepState::Cancelled
    );
    // Cancelling again is the same answer.
    assert_eq!(
        cancel("alice-token", "")
            .await
            .unwrap()
            .into_inner()
            .run
            .unwrap()
            .state,
        wire::WorkflowRunState::Cancelled as i32
    );
    // A deciding after the cancel is refused.
    let op = uuid::Uuid::new_v4().to_string();
    let e = svc(&w)
        .decide_approval(w.request(
            wire::DecideWorkflowApprovalRequest {
                repo: repo_ref(),
                id: run.id,
                step: "gate".into(),
                approve: true,
                comment: String::new(),
            },
            "alice-token",
            Some(&op),
        ))
        .await
        .unwrap_err();
    assert_eq!(code(&e), "RUN_NOT_ACTIVE");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_second_run_of_a_workflow_waits_its_turn_and_a_queued_run_can_be_cancelled() {
    let w = world(&[(".asc/workflows/approved.toml", APPROVED)]).await;
    enable(&w, "alice-token", "approved", true).await.unwrap();
    let first = trigger(&w, "bob-token", "approved").await.unwrap();
    wait_step(&w, first.id, "gate", StepState::Waiting).await;
    let second = trigger(&w, "bob-token", "approved").await.unwrap();
    tokio::time::sleep(Duration::from_millis(600)).await;
    let hub = w.wf.as_ref().unwrap();
    assert_eq!(
        hub.store.get_run(second.id).await.unwrap().unwrap().state,
        RunState::Queued,
        "max_parallel_runs is one"
    );
    // Cancelling a queued run ends it at once.
    let got = svc(&w)
        .cancel_run(w.request(
            wire::CancelWorkflowRunRequest {
                repo: repo_ref(),
                id: second.id,
                reason: "no".into(),
            },
            "bob-token",
            None,
        ))
        .await
        .unwrap()
        .into_inner()
        .run
        .unwrap();
    assert_eq!(got.state, wire::WorkflowRunState::Cancelled as i32);
    // A third waits; when the first ends the third starts.
    let third = trigger(&w, "bob-token", "approved").await.unwrap();
    hub.cancel_run(first.id, "done", "7:alice").await.unwrap();
    wait_step(&w, third.id, "gate", StepState::Waiting).await;
    hub.cancel_run(third.id, "done", "7:alice").await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn runs_are_listed_newest_first_filtered_and_paged_for_readers_only() {
    let w = world(&[
        (".asc/workflows/approved.toml", APPROVED),
        (".asc/workflows/gated.toml", ROLE_GATE),
    ])
    .await;
    enable(&w, "alice-token", "approved", true).await.unwrap();
    enable(&w, "alice-token", "gated", true).await.unwrap();
    let hub = w.wf.as_ref().unwrap();
    let mut ids = Vec::new();
    for name in ["approved", "gated", "approved"] {
        let r = trigger(&w, "bob-token", name).await.unwrap();
        hub.cancel_run(r.id, "x", "7:alice").await.unwrap();
        ids.push(r.id);
    }
    let list = |who: &str, msg: wire::ListWorkflowRunsRequest| {
        svc(&w).list_runs(w.request(msg, who, None))
    };
    let all = list(
        "carol-token",
        wire::ListWorkflowRunsRequest {
            repo: repo_ref(),
            ..Default::default()
        },
    )
    .await
    .unwrap()
    .into_inner();
    assert_eq!(
        all.runs.iter().map(|r| r.id).collect::<Vec<_>>(),
        [ids[2], ids[1], ids[0]]
    );
    let one = list(
        "carol-token",
        wire::ListWorkflowRunsRequest {
            repo: repo_ref(),
            name: "gated".into(),
            ..Default::default()
        },
    )
    .await
    .unwrap()
    .into_inner();
    assert_eq!(one.runs.iter().map(|r| r.id).collect::<Vec<_>>(), [ids[1]]);
    let page = list(
        "carol-token",
        wire::ListWorkflowRunsRequest {
            repo: repo_ref(),
            limit: 2,
            ..Default::default()
        },
    )
    .await
    .unwrap()
    .into_inner();
    assert_eq!(page.runs.len(), 2);
    let next = list(
        "carol-token",
        wire::ListWorkflowRunsRequest {
            repo: repo_ref(),
            limit: 2,
            page_token: page.next_page_token,
            ..Default::default()
        },
    )
    .await
    .unwrap()
    .into_inner();
    assert_eq!(next.runs.iter().map(|r| r.id).collect::<Vec<_>>(), [ids[0]]);
    let none = list(
        "carol-token",
        wire::ListWorkflowRunsRequest {
            repo: repo_ref(),
            states: vec![wire::WorkflowRunState::Succeeded as i32],
            ..Default::default()
        },
    )
    .await
    .unwrap()
    .into_inner();
    assert!(none.runs.is_empty());
    for bad in [
        wire::ListWorkflowRunsRequest {
            repo: repo_ref(),
            page_token: "abc".into(),
            ..Default::default()
        },
        wire::ListWorkflowRunsRequest {
            repo: repo_ref(),
            states: vec![99],
            ..Default::default()
        },
        wire::ListWorkflowRunsRequest {
            repo: repo_ref(),
            name: "Bad Name".into(),
            ..Default::default()
        },
    ] {
        assert_eq!(
            list("carol-token", bad).await.unwrap_err().code(),
            Code::InvalidArgument
        );
    }
    let anon = svc(&w)
        .list_runs(tonic::Request::new(wire::ListWorkflowRunsRequest {
            repo: repo_ref(),
            ..Default::default()
        }))
        .await
        .unwrap_err();
    assert_eq!(anon.code(), Code::Unauthenticated);
    // A run of another repository is not found here.
    w.registry
        .create(
            &asc_rpc::server::RepoName::parse("acme/other").unwrap(),
            &crate::registry::NewRepo::default(),
        )
        .await
        .unwrap();
    let other = Some(wire::RepoRef {
        name: "acme/other".into(),
        id_mode: asc_rpc::IdMode::Asc as i32,
    });
    let e = svc(&w)
        .get_run(w.request(
            wire::GetWorkflowRunRequest {
                repo: other,
                id: ids[0],
            },
            "alice-token",
            None,
        ))
        .await
        .unwrap_err();
    assert_eq!(e.code(), Code::NotFound);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_watch_streams_the_run_as_it_changes_ends_with_it_and_resumes() {
    use futures::StreamExt as _;
    let w = world(&[(".asc/workflows/approved.toml", APPROVED)]).await;
    enable(&w, "alice-token", "approved", true).await.unwrap();
    let run = trigger(&w, "bob-token", "approved").await.unwrap();
    wait_step(&w, run.id, "gate", StepState::Waiting).await;
    let hub = w.wf.as_ref().unwrap().clone();
    let id = run.id;
    let stopper = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(500)).await;
        hub.cancel_run(id, "enough", "7:alice").await.unwrap();
    });
    let watch = |who: &str, after: u64| {
        svc(&w).watch_run(w.request(
            wire::WatchWorkflowRunRequest {
                repo: repo_ref(),
                id: run.id,
                after_seq: after,
            },
            who,
            None,
        ))
    };
    let mut stream = watch("carol-token", 0).await.unwrap().into_inner();
    let mut seen = Vec::new();
    let ended = tokio::time::timeout(Duration::from_secs(60), async {
        while let Some(ev) = stream.next().await {
            seen.push(ev.unwrap());
        }
    })
    .await;
    assert!(ended.is_ok(), "the stream ends with the run");
    stopper.await.unwrap();
    let seqs: Vec<u64> = seen.iter().map(|e| e.seq).collect();
    assert_eq!(seqs, (1..=seqs.len() as u64).collect::<Vec<_>>());
    assert_eq!(
        seen.last().unwrap().run.as_ref().unwrap().state,
        wire::WorkflowRunState::Cancelled as i32
    );
    assert!(seen.iter().any(|e| e.step == "gate"));
    let k = seqs[1];
    let mut rest = watch("carol-token", k).await.unwrap().into_inner();
    let mut got = Vec::new();
    while let Some(ev) = rest.next().await {
        got.push(ev.unwrap().seq);
    }
    assert_eq!(
        got,
        seqs.iter().copied().filter(|s| *s > k).collect::<Vec<_>>()
    );
    let anon = svc(&w)
        .watch_run(tonic::Request::new(wire::WatchWorkflowRunRequest {
            repo: repo_ref(),
            id: run.id,
            after_seq: 0,
        }))
        .await;
    assert_eq!(anon.err().map(|e| e.code()), Some(Code::Unauthenticated));
}

#[allow(dead_code)]
fn _unused(_: &str) {
    let _ = MAINTAINERS;
}
