//! `asc agent` and `asc workflow` against a stand-in server on loopback that serves `Agents`,
//! `AgentTokens`, `AiProviders` and `Workflows`. They show the request each command builds, the
//! text it prints, the exit code it ends with, that a secret is read from a file or a variable and
//! never from an argument or printed back, and that text from the server cannot reach the
//! terminal as control sequences.

mod support;

use std::net::SocketAddr;
use std::path::Path;
use std::process::Output;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use asc_rpc::server::wire;
use asc_rpc::services::agent_tokens_server::{AgentTokens, AgentTokensServer};
use asc_rpc::services::agents_server::{Agents, AgentsServer};
use asc_rpc::services::ai_providers_server::{AiProviders, AiProvidersServer};
use asc_rpc::services::workflows_server::{Workflows, WorkflowsServer};
use support::{Sandbox, text};
use tonic::{Request, Response, Status};

const TOKEN: &str = "secret-token-value";
/// What a person pastes as a provider key and as a workflow secret; neither may be printed.
const API_KEY: &str = "sk-test-0123456789abcdefghij";
const SECRET_VALUE: &str = "deploy-secret-value-9876";
/// What the stand-in answers `CreateToken` with.
const AGENT_SECRET: &str = "ascat_0123456789012345678901234567890123456789012";

#[derive(Default)]
struct Seen {
    create_run: Vec<wire::CreateAgentRunRequest>,
    list_runs: Vec<wire::ListAgentRunsRequest>,
    cancel: Vec<wire::CancelAgentRunRequest>,
    input: Vec<wire::SendAgentInputRequest>,
    create_token: Vec<wire::CreateAgentTokenRequest>,
    revoke_token: Vec<wire::RevokeAgentTokenRequest>,
    set_budget: Vec<wire::SetAgentTokenBudgetRequest>,
    create_key: Vec<wire::CreateAiKeyRequest>,
    update_key: Vec<wire::UpdateAiKeyRequest>,
    usage: Vec<wire::AiUsageRequest>,
    deleted_keys: Vec<String>,
    validate: Vec<wire::ValidateWorkflowRequest>,
    enabled: Vec<wire::SetWorkflowEnabledRequest>,
    trigger: Vec<wire::TriggerWorkflowRequest>,
    list_wf_runs: Vec<wire::ListWorkflowRunsRequest>,
    decide: Vec<wire::DecideWorkflowApprovalRequest>,
    set_secret: Vec<wire::SetWorkflowSecretRequest>,
    deleted_secrets: Vec<String>,
    op_ids: Vec<Option<String>>,
    auth: Vec<Option<String>>,
}

#[derive(Clone, Default)]
struct Fake {
    seen: Arc<Mutex<Seen>>,
    /// Refuse `CreateRun` as the scope check of the server would.
    refuse_runs: bool,
    /// Nothing to list.
    empty: bool,
    /// The workflow run ends failed.
    failing: bool,
}

impl Fake {
    fn note<T>(&self, r: &Request<T>) {
        let md = r.metadata();
        let mut s = self.seen.lock().unwrap();
        s.auth.push(
            md.get("authorization")
                .and_then(|v| v.to_str().ok())
                .map(str::to_owned),
        );
        s.op_ids.push(
            md.get("x-asc-op-id")
                .and_then(|v| v.to_str().ok())
                .map(str::to_owned),
        );
    }
}

fn usage(changes: u32, tokens: u64, cost: u64) -> Option<wire::AgentBudgetUsage> {
    Some(wire::AgentBudgetUsage {
        changes,
        tokens,
        cost_micro_usd: cost,
        requests: 3,
        ..Default::default()
    })
}

fn run_of(id: u64, state: wire::AgentRunState) -> wire::AgentRun {
    wire::AgentRun {
        id,
        repo: "acme/app".into(),
        agent: "fixer".into(),
        title: "Fix the \u{1b}[2Jbug".into(),
        state: state as i32,
        target: "master".into(),
        model: "claude-test".into(),
        ai_key_id: "ak_abc".into(),
        created_by: "alice".into(),
        created: 1_700_000_000,
        limits: Some(wire::AgentBudgetLimits {
            max_changes: 5,
            max_cost_micro_usd: 2_500_000,
            ..Default::default()
        }),
        usage: usage(2, 1200, 31_000),
        changes: vec![bytes::Bytes::from_static(&[0xab; 16])],
        ..Default::default()
    }
}

type RunStream = futures::stream::Iter<std::vec::IntoIter<Result<wire::AgentRunEvent, Status>>>;

#[tonic::async_trait]
impl Agents for Fake {
    type WatchRunStream = RunStream;

    // The run controls, estimates and spend belong to other tests; this stand-in serves the commands under test.
    async fn list_requests(
        &self,
        _r: Request<wire::ListAgentRequestsRequest>,
    ) -> Result<Response<wire::ListAgentRequestsResponse>, Status> {
        Err(Status::unimplemented("not served by this stand-in"))
    }

    async fn decide_request(
        &self,
        _r: Request<wire::DecideAgentRequestRequest>,
    ) -> Result<Response<wire::DecideAgentRequestResponse>, Status> {
        Err(Status::unimplemented("not served by this stand-in"))
    }

    async fn pause_run(
        &self,
        _r: Request<wire::PauseAgentRunRequest>,
    ) -> Result<Response<wire::PauseAgentRunResponse>, Status> {
        Err(Status::unimplemented("not served by this stand-in"))
    }

    async fn list_checkpoints(
        &self,
        _r: Request<wire::ListAgentCheckpointsRequest>,
    ) -> Result<Response<wire::ListAgentCheckpointsResponse>, Status> {
        Err(Status::unimplemented("not served by this stand-in"))
    }

    async fn rollback_run(
        &self,
        _r: Request<wire::RollbackAgentRunRequest>,
    ) -> Result<Response<wire::RollbackAgentRunResponse>, Status> {
        Err(Status::unimplemented("not served by this stand-in"))
    }

    async fn set_run_limits(
        &self,
        _r: Request<wire::SetAgentRunLimitsRequest>,
    ) -> Result<Response<wire::SetAgentRunLimitsResponse>, Status> {
        Err(Status::unimplemented("not served by this stand-in"))
    }

    async fn continue_run(
        &self,
        _r: Request<wire::ContinueAgentRunRequest>,
    ) -> Result<Response<wire::ContinueAgentRunResponse>, Status> {
        Err(Status::unimplemented("not served by this stand-in"))
    }

    async fn get_estimate(
        &self,
        _r: Request<wire::GetAgentEstimateRequest>,
    ) -> Result<Response<wire::GetAgentEstimateResponse>, Status> {
        Err(Status::unimplemented("not served by this stand-in"))
    }

    async fn recent_tasks(
        &self,
        _r: Request<wire::RecentAgentTasksRequest>,
    ) -> Result<Response<wire::RecentAgentTasksResponse>, Status> {
        Err(Status::unimplemented("not served by this stand-in"))
    }

    async fn get_run_log(
        &self,
        _r: Request<wire::GetAgentRunLogRequest>,
    ) -> Result<Response<wire::GetAgentRunLogResponse>, Status> {
        Err(Status::unimplemented("not served by this stand-in"))
    }

    async fn list_agents(
        &self,
        _r: Request<wire::ListRepoAgentsRequest>,
    ) -> Result<Response<wire::ListRepoAgentsResponse>, Status> {
        Err(Status::unimplemented("not served by this stand-in"))
    }

    async fn get_spend(
        &self,
        _r: Request<wire::GetAgentSpendRequest>,
    ) -> Result<Response<wire::GetAgentSpendResponse>, Status> {
        Err(Status::unimplemented("not served by this stand-in"))
    }

    async fn set_monthly_budget(
        &self,
        _r: Request<wire::SetMonthlyBudgetRequest>,
    ) -> Result<Response<wire::SetMonthlyBudgetResponse>, Status> {
        Err(Status::unimplemented("not served by this stand-in"))
    }

    async fn create_run(
        &self,
        r: Request<wire::CreateAgentRunRequest>,
    ) -> Result<Response<wire::CreateAgentRunResponse>, Status> {
        self.note(&r);
        if self.refuse_runs {
            let e = asc_rpc::errors::RpcError::AgentScopeDenied(asc_rpc::errors::ErrorInfo {
                paths: vec!["secrets/**".into()],
                ..Default::default()
            });
            return Err(asc_rpc::server::rpc_status(&e));
        }
        self.seen.lock().unwrap().create_run.push(r.into_inner());
        Ok(Response::new(wire::CreateAgentRunResponse {
            run: Some(run_of(41, wire::AgentRunState::Queued)),
            ..Default::default()
        }))
    }

    async fn list_runs(
        &self,
        r: Request<wire::ListAgentRunsRequest>,
    ) -> Result<Response<wire::ListAgentRunsResponse>, Status> {
        self.note(&r);
        self.seen.lock().unwrap().list_runs.push(r.into_inner());
        let runs = if self.empty {
            vec![]
        } else {
            vec![
                run_of(41, wire::AgentRunState::Succeeded),
                run_of(40, wire::AgentRunState::Failed),
            ]
        };
        Ok(Response::new(wire::ListAgentRunsResponse {
            runs,
            next_page_token: if self.empty {
                String::new()
            } else {
                "tok2".into()
            },
        }))
    }

    async fn get_run(
        &self,
        r: Request<wire::GetAgentRunRequest>,
    ) -> Result<Response<wire::GetAgentRunResponse>, Status> {
        self.note(&r);
        Ok(Response::new(wire::GetAgentRunResponse {
            run: Some(run_of(r.into_inner().id, wire::AgentRunState::Succeeded)),
        }))
    }

    async fn cancel_run(
        &self,
        r: Request<wire::CancelAgentRunRequest>,
    ) -> Result<Response<wire::CancelAgentRunResponse>, Status> {
        self.note(&r);
        let id = r.get_ref().id;
        self.seen.lock().unwrap().cancel.push(r.into_inner());
        Ok(Response::new(wire::CancelAgentRunResponse {
            run: Some(run_of(id, wire::AgentRunState::Cancelled)),
        }))
    }

    async fn send_input(
        &self,
        r: Request<wire::SendAgentInputRequest>,
    ) -> Result<Response<wire::SendAgentInputResponse>, Status> {
        self.note(&r);
        let id = r.get_ref().id;
        self.seen.lock().unwrap().input.push(r.into_inner());
        Ok(Response::new(wire::SendAgentInputResponse {
            run: Some(run_of(id, wire::AgentRunState::Running)),
        }))
    }

    async fn watch_run(
        &self,
        r: Request<wire::WatchAgentRunRequest>,
    ) -> Result<Response<Self::WatchRunStream>, Status> {
        self.note(&r);
        let after = r.into_inner().after_seq;
        let ev =
            |seq: u64, kind: wire::AgentRunEventKind, text: &str, tool: &str| wire::AgentRunEvent {
                run_id: 41,
                seq,
                kind: kind as i32,
                text: text.into(),
                tool: tool.into(),
                ..Default::default()
            };
        let all = vec![
            ev(1, wire::AgentRunEventKind::State, "running", ""),
            ev(
                2,
                wire::AgentRunEventKind::ToolCall,
                "{\"path\":\"src/a.rs\"}",
                "read_file",
            ),
            ev(
                3,
                wire::AgentRunEventKind::ToolResult,
                "fn main() {}\n\u{1b}]0;owned\u{7}second line",
                "read_file",
            ),
            ev(4, wire::AgentRunEventKind::State, "succeeded", ""),
        ];
        let items: Vec<_> = all.into_iter().filter(|e| e.seq > after).map(Ok).collect();
        Ok(Response::new(futures::stream::iter(items)))
    }
}

fn token_of(state: wire::AgentTokenState) -> wire::AgentToken {
    wire::AgentToken {
        id: "at_abcdefghij0123456789".into(),
        repo: "acme/app".into(),
        name: "ci".into(),
        agent: "fixer".into(),
        paths: vec!["src/**".into()],
        max_changes_per_run: 3,
        state: state as i32,
        created_by: "alice".into(),
        created: 1_700_000_000,
        expires_at: 1_700_003_600,
        secret_hint: "0123".into(),
        limits: Some(wire::AgentBudgetLimits {
            max_changes: 10,
            ..Default::default()
        }),
        usage: usage(1, 10, 5),
        ..Default::default()
    }
}

#[tonic::async_trait]
impl AgentTokens for Fake {
    async fn create_token(
        &self,
        r: Request<wire::CreateAgentTokenRequest>,
    ) -> Result<Response<wire::CreateAgentTokenResponse>, Status> {
        self.note(&r);
        self.seen.lock().unwrap().create_token.push(r.into_inner());
        Ok(Response::new(wire::CreateAgentTokenResponse {
            token: Some(token_of(wire::AgentTokenState::Active)),
            secret: AGENT_SECRET.into(),
        }))
    }

    async fn list_tokens(
        &self,
        r: Request<wire::ListAgentTokensRequest>,
    ) -> Result<Response<wire::ListAgentTokensResponse>, Status> {
        self.note(&r);
        Ok(Response::new(wire::ListAgentTokensResponse {
            tokens: vec![token_of(wire::AgentTokenState::Active)],
            next_page_token: String::new(),
        }))
    }

    async fn get_token(
        &self,
        r: Request<wire::GetAgentTokenRequest>,
    ) -> Result<Response<wire::GetAgentTokenResponse>, Status> {
        self.note(&r);
        Ok(Response::new(wire::GetAgentTokenResponse {
            token: Some(token_of(wire::AgentTokenState::Active)),
            remaining: usage(9, 0, 0),
        }))
    }

    async fn revoke_token(
        &self,
        r: Request<wire::RevokeAgentTokenRequest>,
    ) -> Result<Response<wire::RevokeAgentTokenResponse>, Status> {
        self.note(&r);
        self.seen.lock().unwrap().revoke_token.push(r.into_inner());
        Ok(Response::new(wire::RevokeAgentTokenResponse {
            token: Some(token_of(wire::AgentTokenState::Revoked)),
        }))
    }

    async fn set_budget(
        &self,
        r: Request<wire::SetAgentTokenBudgetRequest>,
    ) -> Result<Response<wire::SetAgentTokenBudgetResponse>, Status> {
        self.note(&r);
        self.seen.lock().unwrap().set_budget.push(r.into_inner());
        Ok(Response::new(wire::SetAgentTokenBudgetResponse {
            token: Some(token_of(wire::AgentTokenState::Active)),
        }))
    }

    async fn report_usage(
        &self,
        _: Request<wire::ReportAgentUsageRequest>,
    ) -> Result<Response<wire::ReportAgentUsageResponse>, Status> {
        Err(Status::permission_denied("a token reports its own usage"))
    }
}

fn key_of() -> wire::AiKey {
    wire::AiKey {
        id: "ak_abcdefghij0123456789".into(),
        owner: Some(wire::OwnerRef {
            kind: wire::OwnerKind::Repo as i32,
            name: "acme/app".into(),
        }),
        label: "team\u{1b}[31m".into(),
        provider: wire::AiProvider::Anthropic as i32,
        key_hint: "ghij".into(),
        has_key: true,
        state: wire::AiKeyState::Untested as i32,
        monthly_cap_micro_usd: 50_000_000,
        month_spent_micro_usd: 1_500_000,
        ..Default::default()
    }
}

#[tonic::async_trait]
impl AiProviders for Fake {
    async fn create_key(
        &self,
        r: Request<wire::CreateAiKeyRequest>,
    ) -> Result<Response<wire::CreateAiKeyResponse>, Status> {
        self.note(&r);
        self.seen.lock().unwrap().create_key.push(r.into_inner());
        Ok(Response::new(wire::CreateAiKeyResponse {
            key: Some(key_of()),
        }))
    }

    async fn list_keys(
        &self,
        r: Request<wire::ListAiKeysRequest>,
    ) -> Result<Response<wire::ListAiKeysResponse>, Status> {
        self.note(&r);
        Ok(Response::new(wire::ListAiKeysResponse {
            keys: vec![key_of()],
        }))
    }

    async fn get_key(
        &self,
        r: Request<wire::GetAiKeyRequest>,
    ) -> Result<Response<wire::GetAiKeyResponse>, Status> {
        self.note(&r);
        Ok(Response::new(wire::GetAiKeyResponse {
            key: Some(key_of()),
        }))
    }

    async fn update_key(
        &self,
        r: Request<wire::UpdateAiKeyRequest>,
    ) -> Result<Response<wire::UpdateAiKeyResponse>, Status> {
        self.note(&r);
        self.seen.lock().unwrap().update_key.push(r.into_inner());
        Ok(Response::new(wire::UpdateAiKeyResponse {
            key: Some(key_of()),
        }))
    }

    async fn delete_key(
        &self,
        r: Request<wire::DeleteAiKeyRequest>,
    ) -> Result<Response<wire::DeleteAiKeyResponse>, Status> {
        self.note(&r);
        self.seen
            .lock()
            .unwrap()
            .deleted_keys
            .push(r.into_inner().id);
        Ok(Response::new(wire::DeleteAiKeyResponse {}))
    }

    async fn test_key(
        &self,
        r: Request<wire::TestAiKeyRequest>,
    ) -> Result<Response<wire::TestAiKeyResponse>, Status> {
        self.note(&r);
        let bad = r.into_inner().id.ends_with("bad");
        Ok(Response::new(wire::TestAiKeyResponse {
            ok: !bad,
            latency_ms: 120,
            provider_status: if bad {
                "unauthorized".into()
            } else {
                "ok".into()
            },
            message: if bad {
                "the provider refused the key".into()
            } else {
                String::new()
            },
            key: Some(key_of()),
        }))
    }

    async fn usage(
        &self,
        r: Request<wire::AiUsageRequest>,
    ) -> Result<Response<wire::AiUsageResponse>, Status> {
        self.note(&r);
        self.seen.lock().unwrap().usage.push(r.into_inner());
        let row = |g: &str| wire::AiUsageRow {
            group: g.into(),
            requests: 4,
            input_tokens: 1000,
            output_tokens: 200,
            cost_micro_usd: 90_000,
            unpriced_requests: 1,
        };
        Ok(Response::new(wire::AiUsageResponse {
            rows: vec![row("2026-10-01"), row("2026-10-02")],
            total: Some(row("")),
        }))
    }

    async fn list_models(
        &self,
        r: Request<wire::ListAiModelsRequest>,
    ) -> Result<Response<wire::ListAiModelsResponse>, Status> {
        self.note(&r);
        Ok(Response::new(wire::ListAiModelsResponse {
            models: vec![wire::AiModel {
                id: "claude-test".into(),
                display_name: "Claude Test".into(),
                context_tokens: 200_000,
            }],
        }))
    }
}

fn definition() -> wire::WorkflowDefinition {
    wire::WorkflowDefinition {
        name: "fix-ci".into(),
        path: ".asc/workflows/fix-ci.toml".into(),
        description: "Fix a failing check".into(),
        valid: true,
        enabled: true,
        triggers: vec![wire::WorkflowTrigger {
            kind: wire::WorkflowTriggerKind::CheckFailed as i32,
            ..Default::default()
        }],
        steps: vec![
            wire::WorkflowStep {
                id: "test".into(),
                kind: wire::WorkflowStepKind::Command as i32,
                command: vec!["cargo".into(), "test".into()],
                ..Default::default()
            },
            wire::WorkflowStep {
                id: "ok".into(),
                kind: wire::WorkflowStepKind::Approval as i32,
                needs: vec!["test".into()],
                approvers: vec!["role:maintainer".into()],
                ..Default::default()
            },
        ],
        ..Default::default()
    }
}

fn wf_run(id: u64, state: wire::WorkflowRunState) -> wire::WorkflowRun {
    wire::WorkflowRun {
        id,
        repo: "acme/app".into(),
        workflow: "fix-ci".into(),
        state: state as i32,
        trigger: wire::WorkflowTriggerKind::Manual as i32,
        started_by: "alice".into(),
        created: 1_700_000_000,
        steps: vec![
            wire::WorkflowStepRun {
                id: "test".into(),
                state: wire::WorkflowStepState::Failed as i32,
                exit_code: 101,
                ci_run_id: 12,
                output: "running 3 tests\n\u{1b}[31mtest failed\u{1b}[0m\n".into(),
                ..Default::default()
            },
            wire::WorkflowStepRun {
                id: "ok".into(),
                state: wire::WorkflowStepState::Pending as i32,
                ..Default::default()
            },
        ],
        ..Default::default()
    }
}

type WfStream = futures::stream::Iter<std::vec::IntoIter<Result<wire::WorkflowRunEvent, Status>>>;

#[tonic::async_trait]
impl Workflows for Fake {
    type WatchRunStream = WfStream;

    async fn list_definitions(
        &self,
        r: Request<wire::ListWorkflowDefinitionsRequest>,
    ) -> Result<Response<wire::ListWorkflowDefinitionsResponse>, Status> {
        self.note(&r);
        let mut bad = definition();
        bad.name = "broken".into();
        bad.valid = false;
        bad.triggers.clear();
        bad.error = "line 3: unknown key \u{1b}[2J".into();
        Ok(Response::new(wire::ListWorkflowDefinitionsResponse {
            definitions: vec![definition(), bad],
        }))
    }

    async fn get_definition(
        &self,
        r: Request<wire::GetWorkflowDefinitionRequest>,
    ) -> Result<Response<wire::GetWorkflowDefinitionResponse>, Status> {
        self.note(&r);
        Ok(Response::new(wire::GetWorkflowDefinitionResponse {
            definition: Some(definition()),
            text: bytes::Bytes::from_static(b"name = \"fix-ci\"\n# \x1b[2J\nsteps = []\n"),
        }))
    }

    async fn validate(
        &self,
        r: Request<wire::ValidateWorkflowRequest>,
    ) -> Result<Response<wire::ValidateWorkflowResponse>, Status> {
        self.note(&r);
        let req = r.into_inner();
        let bad = req.text.windows(3).any(|w| w == b"bad");
        self.seen.lock().unwrap().validate.push(req);
        Ok(Response::new(wire::ValidateWorkflowResponse {
            valid: !bad,
            diagnostics: if bad {
                vec![wire::WorkflowDiagnostic {
                    code: "x".into(),
                    message: "unknown step kind".into(),
                    line: 4,
                    column: 2,
                }]
            } else {
                vec![]
            },
            definition: None,
        }))
    }

    async fn set_enabled(
        &self,
        r: Request<wire::SetWorkflowEnabledRequest>,
    ) -> Result<Response<wire::SetWorkflowEnabledResponse>, Status> {
        self.note(&r);
        let req = r.into_inner();
        let mut d = definition();
        d.enabled = req.enabled;
        self.seen.lock().unwrap().enabled.push(req);
        Ok(Response::new(wire::SetWorkflowEnabledResponse {
            definition: Some(d),
        }))
    }

    async fn trigger(
        &self,
        r: Request<wire::TriggerWorkflowRequest>,
    ) -> Result<Response<wire::TriggerWorkflowResponse>, Status> {
        self.note(&r);
        self.seen.lock().unwrap().trigger.push(r.into_inner());
        Ok(Response::new(wire::TriggerWorkflowResponse {
            run: Some(wf_run(7, wire::WorkflowRunState::Queued)),
        }))
    }

    async fn list_runs(
        &self,
        r: Request<wire::ListWorkflowRunsRequest>,
    ) -> Result<Response<wire::ListWorkflowRunsResponse>, Status> {
        self.note(&r);
        self.seen.lock().unwrap().list_wf_runs.push(r.into_inner());
        let runs = if self.empty {
            vec![]
        } else {
            vec![wf_run(7, wire::WorkflowRunState::Failed)]
        };
        Ok(Response::new(wire::ListWorkflowRunsResponse {
            runs,
            next_page_token: String::new(),
        }))
    }

    async fn get_run(
        &self,
        r: Request<wire::GetWorkflowRunRequest>,
    ) -> Result<Response<wire::GetWorkflowRunResponse>, Status> {
        self.note(&r);
        let state = if self.failing {
            wire::WorkflowRunState::Failed
        } else {
            wire::WorkflowRunState::Succeeded
        };
        Ok(Response::new(wire::GetWorkflowRunResponse {
            run: Some(wf_run(r.into_inner().id, state)),
        }))
    }

    async fn cancel_run(
        &self,
        r: Request<wire::CancelWorkflowRunRequest>,
    ) -> Result<Response<wire::CancelWorkflowRunResponse>, Status> {
        self.note(&r);
        Ok(Response::new(wire::CancelWorkflowRunResponse {
            run: Some(wf_run(r.into_inner().id, wire::WorkflowRunState::Cancelled)),
        }))
    }

    async fn decide_approval(
        &self,
        r: Request<wire::DecideWorkflowApprovalRequest>,
    ) -> Result<Response<wire::DecideWorkflowApprovalResponse>, Status> {
        self.note(&r);
        let id = r.get_ref().id;
        self.seen.lock().unwrap().decide.push(r.into_inner());
        Ok(Response::new(wire::DecideWorkflowApprovalResponse {
            run: Some(wf_run(id, wire::WorkflowRunState::Running)),
        }))
    }

    async fn watch_run(
        &self,
        r: Request<wire::WatchWorkflowRunRequest>,
    ) -> Result<Response<Self::WatchRunStream>, Status> {
        self.note(&r);
        let after = r.into_inner().after_seq;
        let end = if self.failing {
            wire::WorkflowRunState::Failed
        } else {
            wire::WorkflowRunState::Succeeded
        };
        let all = vec![
            wire::WorkflowRunEvent {
                seq: 1,
                time_ms: 1,
                run: Some(wf_run(7, wire::WorkflowRunState::Running)),
                step: "test".into(),
            },
            wire::WorkflowRunEvent {
                seq: 2,
                time_ms: 2,
                run: Some(wf_run(7, end)),
                step: String::new(),
            },
        ];
        Ok(Response::new(futures::stream::iter(
            all.into_iter()
                .filter(|e| e.seq > after)
                .map(Ok)
                .collect::<Vec<_>>(),
        )))
    }

    async fn list_secrets(
        &self,
        r: Request<wire::ListWorkflowSecretsRequest>,
    ) -> Result<Response<wire::ListWorkflowSecretsResponse>, Status> {
        self.note(&r);
        Ok(Response::new(wire::ListWorkflowSecretsResponse {
            secrets: vec![
                wire::WorkflowSecret {
                    name: "DEPLOY_TOKEN".into(),
                    hint: "9876".into(),
                    updated: 1_700_000_000,
                    updated_by: "alice".into(),
                    ..Default::default()
                },
                wire::WorkflowSecret {
                    name: "SHORT".into(),
                    ..Default::default()
                },
            ],
        }))
    }

    async fn set_secret(
        &self,
        r: Request<wire::SetWorkflowSecretRequest>,
    ) -> Result<Response<wire::SetWorkflowSecretResponse>, Status> {
        self.note(&r);
        let req = r.into_inner();
        let name = req.name.clone();
        self.seen.lock().unwrap().set_secret.push(req);
        Ok(Response::new(wire::SetWorkflowSecretResponse {
            secret: Some(wire::WorkflowSecret {
                name,
                hint: "9876".into(),
                updated: 1_700_000_000,
                updated_by: "alice".into(),
                ..Default::default()
            }),
        }))
    }

    async fn delete_secret(
        &self,
        r: Request<wire::DeleteWorkflowSecretRequest>,
    ) -> Result<Response<wire::DeleteWorkflowSecretResponse>, Status> {
        self.note(&r);
        self.seen
            .lock()
            .unwrap()
            .deleted_secrets
            .push(r.into_inner().name);
        Ok(Response::new(wire::DeleteWorkflowSecretResponse {}))
    }
}

struct Server {
    addr: SocketAddr,
    fake: Fake,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    thread: Option<JoinHandle<()>>,
}

impl Server {
    fn start(fake: Fake) -> Self {
        let svc = fake.clone();
        let (addr_tx, addr_rx) = std::sync::mpsc::channel();
        let (stop, stop_rx) = tokio::sync::oneshot::channel::<()>();
        let thread = std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(1)
                .enable_all()
                .build()
                .expect("runtime");
            rt.block_on(async move {
                let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                    .await
                    .expect("bind");
                addr_tx.send(listener.local_addr().unwrap()).unwrap();
                let incoming = futures::stream::unfold(listener, |l| async move {
                    let r = l.accept().await.map(|(s, _)| s);
                    Some((r, l))
                });
                let _ = tonic::transport::Server::builder()
                    .add_service(AgentsServer::new(svc.clone()))
                    .add_service(AgentTokensServer::new(svc.clone()))
                    .add_service(AiProvidersServer::new(svc.clone()))
                    .add_service(WorkflowsServer::new(svc))
                    .serve_with_incoming_shutdown(incoming, async {
                        let _ = stop_rx.await;
                    })
                    .await;
            });
        });
        Self {
            addr: addr_rx.recv().unwrap(),
            fake,
            stop: Some(stop),
            thread: Some(thread),
        }
    }

    fn url(&self) -> String {
        format!("asc+http://{}/acme/app", self.addr)
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        if let Some(s) = self.stop.take() {
            let _ = s.send(());
        }
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Runs `asc` against the server; `envs` are extra variables (the secrets the commands read).
fn run_with(
    sb: &Sandbox,
    cwd: &Path,
    server: &Server,
    args: &[&str],
    envs: &[(&str, &str)],
) -> Output {
    let mut cmd = sb.asc(cwd);
    cmd.env("ASC_TOKEN", TOKEN).env("ASC_NO_DAEMON", "1");
    for (k, v) in envs {
        cmd.env(k, v);
    }
    cmd.args(args).args(["--remote", &server.url()]);
    cmd.output().expect("spawn asc")
}

fn run(sb: &Sandbox, cwd: &Path, server: &Server, args: &[&str]) -> Output {
    run_with(sb, cwd, server, args, &[])
}

fn no_controls(s: &str) {
    assert!(
        !s.chars().any(|c| c.is_control() && c != '\n' && c != '\t'),
        "control characters reached the output: {s:?}"
    );
}

fn setup(fake: Fake) -> (Server, Sandbox, std::path::PathBuf) {
    let server = Server::start(fake);
    let sb = Sandbox::new();
    let cwd = sb.dir("w");
    (server, sb, cwd)
}

/// Nothing of any secret is in what a command printed.
fn clean_of_secrets(out: &Output) {
    let all = format!("{}{}", text(&out.stdout), text(&out.stderr));
    for secret in [TOKEN, API_KEY, SECRET_VALUE] {
        assert!(!all.contains(secret), "{secret} was printed: {all}");
    }
    no_controls(&all);
}

// ------------------------------------------------------------------------------ agent runs

#[test]
fn a_run_is_asked_for_with_limits_in_plain_units_and_followed_to_its_end() {
    let (server, sb, cwd) = setup(Fake::default());
    let out = run(
        &sb,
        &cwd,
        &server,
        &[
            "agent",
            "run",
            "--agent",
            "fixer",
            "--path",
            "src/**",
            "--max-cost",
            "2.50",
            "--max-seconds",
            "10m",
            "--max-changes",
            "4",
            "--key",
            "ak_abc",
            "-w",
            "fix",
            "the",
            "bug",
        ],
    );
    let (so, se) = (text(&out.stdout), text(&out.stderr));
    assert_eq!(out.status.code(), Some(0), "{se}");
    assert!(so.starts_with("41\n"), "{so:?}");
    assert!(
        so.contains("call   read_file")
            && so.contains("fn main() {} | \\x1b]0;owned\\x07second line"),
        "{so:?}"
    );
    assert!(so.contains("succeeded"));
    clean_of_secrets(&out);
    let seen = server.fake.seen.lock().unwrap();
    let req = &seen.create_run[0];
    assert_eq!(req.task, "fix the bug");
    assert_eq!(req.agent, "fixer");
    assert_eq!(req.paths, ["src/**"]);
    assert_eq!(req.ai_key_id, "ak_abc");
    assert_eq!(req.repo.as_ref().unwrap().name, "acme/app");
    let l = req.limits.unwrap();
    assert_eq!(
        (l.max_cost_micro_usd, l.max_runtime_seconds, l.max_changes),
        (2_500_000, 600, 4)
    );
    assert!(
        seen.op_ids[0].is_some(),
        "a mutating call carries an operation id"
    );
    assert!(seen.auth[0].as_deref().is_some_and(|a| a.contains(TOKEN)));
}

#[test]
fn the_scope_refusal_of_the_server_is_printed_with_its_code_and_fails() {
    let (server, sb, cwd) = setup(Fake {
        refuse_runs: true,
        ..Fake::default()
    });
    let out = run(
        &sb,
        &cwd,
        &server,
        &["agent", "run", "--path", "secrets/**", "touch", "secrets"],
    );
    let se = text(&out.stderr);
    assert_ne!(out.status.code(), Some(0));
    assert!(se.contains("AGENT_SCOPE_DENIED"), "{se}");
    clean_of_secrets(&out);
    assert!(text(&out.stdout).is_empty());
}

#[test]
fn bad_input_fails_before_any_call() {
    let (server, sb, cwd) = setup(Fake::default());
    for args in [
        &["agent", "run"][..],
        &["agent", "run", "--max-cost", "lots", "do it"],
        &["agent", "run", "--max-seconds", "soon", "do it"],
        &["agent", "list", "--state", "sleeping"],
        &["agent", "bogus"],
        &[
            "agent",
            "key",
            "add",
            "--provider",
            "bard",
            "--label",
            "x",
            "--key-env",
            "K",
        ],
        &[
            "agent",
            "key",
            "add",
            "--provider",
            "openai-compatible",
            "--label",
            "x",
            "--key-env",
            "K",
        ],
        &[
            "agent",
            "key",
            "add",
            "--provider",
            "openai",
            "--label",
            "x",
            "--key-env",
            "K",
            "--owner",
            "somebody",
        ],
        &["agent", "key", "show", "../etc"],
        &["agent", "token", "create", "--name", "x", "--ttl", "91d"],
        &["agent", "token", "create", "--name", "x", "--ttl", "0"],
        &["agent", "token", "show", "not an id"],
        &["workflow", "run", "Bad Name"],
        &["workflow", "run", "ok", "--input", "novalue"],
        &["workflow", "run", "ok", "--change", "xyz"],
        &["workflow", "secret", "set", "bad name"],
        &["workflow", "approve", "1", "Bad Step"],
    ] {
        let out = run_with(&sb, &cwd, &server, args, &[("K", API_KEY)]);
        assert!(
            !out.status.success(),
            "{args:?} should fail: {}",
            text(&out.stdout)
        );
        clean_of_secrets(&out);
    }
    let seen = server.fake.seen.lock().unwrap();
    assert!(
        seen.create_run.is_empty()
            && seen.create_key.is_empty()
            && seen.create_token.is_empty()
            && seen.trigger.is_empty()
    );
    assert!(seen.auth.is_empty(), "no call reached the server");
}

#[test]
fn runs_are_listed_shown_cancelled_and_answered() {
    let (server, sb, cwd) = setup(Fake::default());
    let out = run(
        &sb,
        &cwd,
        &server,
        &[
            "agent",
            "list",
            "--state",
            "failed",
            "--state",
            "waiting-input",
            "--by",
            "alice",
            "--limit",
            "5",
        ],
    );
    let (so, se) = (text(&out.stdout), text(&out.stderr));
    assert_eq!(out.status.code(), Some(0), "{se}");
    assert!(
        so.contains("41\tsucceeded\tfixer\t1 changes") && so.contains("40\tfailed"),
        "{so}"
    );
    assert!(se.contains("--page tok2"), "{se}");
    no_controls(&so);
    {
        let seen = server.fake.seen.lock().unwrap();
        let req = &seen.list_runs[0];
        assert_eq!(
            req.states,
            [
                wire::AgentRunState::Failed as i32,
                wire::AgentRunState::WaitingInput as i32
            ]
        );
        assert_eq!((req.created_by.as_str(), req.limit), ("alice", 5));
    }
    let out = run(&sb, &cwd, &server, &["agent", "show", "41"]);
    let so = text(&out.stdout);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    assert!(
        so.contains("state     succeeded")
            && so.contains("limits    5 changes, $2.5")
            && so.contains("(never written by the agent)"),
        "{so}"
    );
    assert!(
        so.contains(&"ab".repeat(16)),
        "the change id is printed in hex: {so}"
    );
    no_controls(&so);
    let out = run(
        &sb,
        &cwd,
        &server,
        &["agent", "cancel", "41", "--reason", "enough"],
    );
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    assert!(text(&out.stdout).contains("cancelled"));
    let out = run(
        &sb,
        &cwd,
        &server,
        &["agent", "say", "41", "use", "the", "other", "file"],
    );
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    let seen = server.fake.seen.lock().unwrap();
    assert_eq!(seen.cancel[0].reason, "enough");
    assert_eq!(seen.input[0].text, "use the other file");
    assert!(seen.op_ids.last().unwrap().is_some());
}

#[test]
fn an_empty_list_exits_one_and_watch_resumes_after_a_number() {
    let (server, sb, cwd) = setup(Fake {
        empty: true,
        ..Fake::default()
    });
    let out = run(&sb, &cwd, &server, &["agent", "list"]);
    assert_eq!(out.status.code(), Some(1));
    let out = run(
        &sb,
        &cwd,
        &server,
        &["agent", "watch", "41", "--after", "3"],
    );
    let so = text(&out.stdout);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    assert!(
        so.contains("succeeded") && !so.contains("read_file"),
        "{so}"
    );
    let json = run(&sb, &cwd, &server, &["agent", "watch", "41", "--json"]);
    let lines: Vec<serde_json::Value> = text(&json.stdout)
        .lines()
        .map(|l| serde_json::from_str(l).expect("a JSON line"))
        .collect();
    assert_eq!(lines.len(), 4);
    assert_eq!(lines[1]["tool"], "read_file");
}

// ------------------------------------------------------------------------------ keys

#[test]
fn a_provider_key_is_read_from_the_environment_sent_once_and_never_printed() {
    let (server, sb, cwd) = setup(Fake::default());
    let out = run_with(
        &sb,
        &cwd,
        &server,
        &[
            "agent",
            "key",
            "add",
            "--provider",
            "anthropic",
            "--label",
            "team",
            "--model",
            "claude-test",
            "--monthly-cap",
            "50",
            "--key-env",
            "MY_KEY",
            "--owner",
            "org:acme",
        ],
        &[("MY_KEY", API_KEY)],
    );
    let (so, se) = (text(&out.stdout), text(&out.stderr));
    assert_eq!(out.status.code(), Some(0), "{se}");
    clean_of_secrets(&out);
    assert!(
        so.contains("stored, ends in ghij") && so.contains("$1.5 of $50"),
        "{so}"
    );
    assert!(se.contains("asc agent key test"), "{se}");
    {
        let seen = server.fake.seen.lock().unwrap();
        let req = &seen.create_key[0];
        assert_eq!(
            req.api_key, API_KEY,
            "the key is sent exactly once, as typed"
        );
        assert_eq!(req.provider, wire::AiProvider::Anthropic as i32);
        assert_eq!(req.monthly_cap_micro_usd, 50_000_000);
        let owner = req.owner.as_ref().unwrap();
        assert_eq!(
            (owner.kind, owner.name.as_str()),
            (wire::OwnerKind::Org as i32, "acme")
        );
    }
    // From a file, the repository of the remote is the default owner, and a flag cannot carry the key.
    let file = cwd.join("key.txt");
    std::fs::write(&file, format!("{API_KEY}\n")).unwrap();
    let out = run(
        &sb,
        &cwd,
        &server,
        &[
            "agent",
            "key",
            "add",
            "--provider",
            "openai",
            "--label",
            "mine",
            "--key-file",
            file.to_str().unwrap(),
        ],
    );
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    clean_of_secrets(&out);
    {
        let seen = server.fake.seen.lock().unwrap();
        let req = &seen.create_key[1];
        assert_eq!(req.api_key, API_KEY, "surrounding whitespace is dropped");
        assert_eq!(req.owner.as_ref().unwrap().name, "acme/app");
    }
    let out = run(
        &sb,
        &cwd,
        &server,
        &[
            "agent",
            "key",
            "add",
            "--provider",
            "openai",
            "--label",
            "x",
            "--api-key",
            API_KEY,
        ],
    );
    assert!(!out.status.success());
    assert_eq!(server.fake.seen.lock().unwrap().create_key.len(), 2);
}

#[test]
fn keys_are_listed_tested_updated_modelled_counted_and_removed() {
    let (server, sb, cwd) = setup(Fake::default());
    let out = run(
        &sb,
        &cwd,
        &server,
        &["agent", "key", "list", "--owner", "user:alice"],
    );
    let so = text(&out.stdout);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    assert!(
        so.contains("ak_abcdefghij0123456789\tanthropic\tuntested") && so.contains("...ghij"),
        "{so}"
    );
    no_controls(&so);
    let out = run(&sb, &cwd, &server, &["agent", "key", "show", "ak_abc"]);
    assert_eq!(out.status.code(), Some(0));
    let out = run(&sb, &cwd, &server, &["agent", "key", "test", "ak_abc"]);
    assert_eq!(out.status.code(), Some(0));
    assert!(text(&out.stdout).starts_with("ok\t120ms"));
    let out = run(&sb, &cwd, &server, &["agent", "key", "test", "ak_bad"]);
    assert_eq!(
        out.status.code(),
        Some(1),
        "a key the provider refused fails the command"
    );
    assert!(text(&out.stdout).contains("unauthorized\t120ms\tthe provider refused the key"));
    let out = run(
        &sb,
        &cwd,
        &server,
        &[
            "agent",
            "key",
            "update",
            "ak_abc",
            "--label",
            "renamed",
            "--monthly-cap",
            "0",
            "--disable",
        ],
    );
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    let out = run(&sb, &cwd, &server, &["agent", "key", "update", "ak_abc"]);
    assert!(!out.status.success(), "nothing to change is a usage error");
    let out = run(&sb, &cwd, &server, &["agent", "key", "models", "ak_abc"]);
    assert!(text(&out.stdout).contains("claude-test\t200000\tClaude Test"));
    let out = run(
        &sb,
        &cwd,
        &server,
        &[
            "agent",
            "key",
            "usage",
            "--group-by",
            "model",
            "--since",
            "2026-10-01",
            "--key",
            "ak_abc",
        ],
    );
    let so = text(&out.stdout);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    assert!(
        so.contains("2026-10-01\t4 requests\t1000 in\t200 out\t$0.09\t1 unpriced")
            && so.contains("total\t"),
        "{so}"
    );
    let out = run(&sb, &cwd, &server, &["agent", "key", "rm", "ak_abc"]);
    assert_eq!(out.status.code(), Some(0));
    let seen = server.fake.seen.lock().unwrap();
    let u = &seen.update_key[0];
    assert_eq!(
        (
            u.label.as_deref(),
            u.monthly_cap_micro_usd,
            u.disabled,
            u.api_key.as_ref()
        ),
        (Some("renamed"), Some(0), Some(true), None)
    );
    assert_eq!(seen.usage[0].group_by, wire::AiUsageGroup::Model as i32);
    assert_eq!(seen.usage[0].since, 1_790_812_800);
    assert_eq!(seen.usage[0].key_id, "ak_abc");
    assert_eq!(seen.deleted_keys, ["ak_abc"]);
}

// ------------------------------------------------------------------------------ tokens

#[test]
fn a_token_secret_is_printed_alone_once_and_the_details_go_to_standard_error() {
    let (server, sb, cwd) = setup(Fake::default());
    let out = run(
        &sb,
        &cwd,
        &server,
        &[
            "agent",
            "token",
            "create",
            "--name",
            "ci",
            "--agent",
            "fixer",
            "--path",
            "src/**",
            "--ttl",
            "2h",
            "--max-changes-per-run",
            "3",
            "--max-cost",
            "5",
        ],
    );
    let (so, se) = (text(&out.stdout), text(&out.stderr));
    assert_eq!(out.status.code(), Some(0), "{se}");
    assert_eq!(
        so,
        format!("{AGENT_SECRET}\n"),
        "the secret alone on standard output"
    );
    assert!(
        se.contains("at_abcdefghij0123456789")
            && se.contains("shown once")
            && !se.contains(AGENT_SECRET),
        "{se}"
    );
    no_controls(&format!("{so}{se}"));
    {
        let seen = server.fake.seen.lock().unwrap();
        let req = &seen.create_token[0];
        assert_eq!(
            (
                req.name.as_str(),
                req.agent.as_str(),
                req.ttl_seconds,
                req.max_changes_per_run
            ),
            ("ci", "fixer", 7200, 3)
        );
        assert_eq!(req.paths, ["src/**"]);
        assert_eq!(req.limits.unwrap().max_cost_micro_usd, 5_000_000);
    }
    // Every later view shows the hint and nothing more.
    for args in [
        &["agent", "token", "list"][..],
        &["agent", "token", "show", "at_abcdefghij0123456789"],
    ] {
        let out = run(&sb, &cwd, &server, args);
        let all = format!("{}{}", text(&out.stdout), text(&out.stderr));
        assert_eq!(out.status.code(), Some(0), "{all}");
        assert!(all.contains("0123") && !all.contains(AGENT_SECRET), "{all}");
    }
    let out = run(
        &sb,
        &cwd,
        &server,
        &["agent", "token", "show", "at_abcdefghij0123456789"],
    );
    assert!(
        text(&out.stdout).contains("left       9 changes"),
        "{}",
        text(&out.stdout)
    );
    let json = run(
        &sb,
        &cwd,
        &server,
        &["agent", "token", "create", "--name", "ci2", "--json"],
    );
    assert!(
        text(&json.stdout).contains(AGENT_SECRET),
        "the JSON answer is the one place the secret is part of the output"
    );
}

#[test]
fn a_token_is_revoked_and_given_a_new_budget() {
    let (server, sb, cwd) = setup(Fake::default());
    let out = run(
        &sb,
        &cwd,
        &server,
        &[
            "agent",
            "token",
            "revoke",
            "at_abcdefghij0123456789",
            "--reason",
            "leaked",
        ],
    );
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    assert!(text(&out.stdout).contains("state      revoked"));
    let out = run(
        &sb,
        &cwd,
        &server,
        &[
            "agent",
            "token",
            "budget",
            "at_abcdefghij0123456789",
            "--max-tokens",
            "100000",
            "--max-cost",
            "1.25",
        ],
    );
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    let seen = server.fake.seen.lock().unwrap();
    assert_eq!(seen.revoke_token[0].reason, "leaked");
    let l = seen.set_budget[0].limits.unwrap();
    assert_eq!((l.max_tokens, l.max_cost_micro_usd), (100_000, 1_250_000));
}

// ------------------------------------------------------------------------------ workflows

#[test]
fn workflows_are_listed_shown_checked_and_switched() {
    let (server, sb, cwd) = setup(Fake::default());
    let out = run(&sb, &cwd, &server, &["workflow", "list"]);
    let so = text(&out.stdout);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    assert!(
        so.contains("fix-ci\tenabled\tcheck_failed\tFix a failing check")
            && so.contains("broken\tinvalid\t-\tline 3: unknown key"),
        "{so}"
    );
    no_controls(&so);
    let out = run(&sb, &cwd, &server, &["workflow", "show", "fix-ci"]);
    let so = text(&out.stdout);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    assert!(
        so.contains("step       test [command] `cargo test`")
            && so.contains("[approval] by role:maintainer after test"),
        "{so}"
    );
    assert!(
        so.contains("---\nname = \"fix-ci\"\n# \\x1b[2J\nsteps = []"),
        "the file is shown line by line and cleaned: {so:?}"
    );
    no_controls(&so);

    let good = cwd.join("good.toml");
    std::fs::write(&good, "name = \"x\"\n").unwrap();
    let bad = cwd.join("bad.toml");
    std::fs::write(&bad, "kind = \"bad\"\n").unwrap();
    let out = run(
        &sb,
        &cwd,
        &server,
        &["workflow", "validate", "x", "-F", good.to_str().unwrap()],
    );
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    assert!(text(&out.stdout).contains("x: valid"));
    let out = run(
        &sb,
        &cwd,
        &server,
        &["workflow", "validate", "x", "-F", bad.to_str().unwrap()],
    );
    assert_eq!(out.status.code(), Some(1));
    assert!(
        text(&out.stdout).contains("x:4:2: unknown step kind"),
        "{}",
        text(&out.stdout)
    );
    let out = run(
        &sb,
        &cwd,
        &server,
        &[
            "workflow",
            "validate",
            "x",
            "-F",
            cwd.join("missing.toml").to_str().unwrap(),
        ],
    );
    assert!(!out.status.success());

    let out = run(&sb, &cwd, &server, &["workflow", "disable", "fix-ci"]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    let out = run(&sb, &cwd, &server, &["workflow", "enable", "fix-ci"]);
    assert_eq!(out.status.code(), Some(0));
    let seen = server.fake.seen.lock().unwrap();
    assert_eq!(seen.validate[0].text.as_ref(), b"name = \"x\"\n");
    assert_eq!(
        seen.enabled.iter().map(|e| e.enabled).collect::<Vec<_>>(),
        [false, true]
    );
}

#[test]
fn a_run_is_started_with_inputs_followed_and_ends_with_the_exit_code_of_its_result() {
    let (server, sb, cwd) = setup(Fake::default());
    let out = run(
        &sb,
        &cwd,
        &server,
        &[
            "workflow",
            "run",
            "fix-ci",
            "--input",
            "branch=main",
            "--input",
            "n=3",
            "--change",
            "0123456789abcdef0123456789abcdef",
            "-w",
        ],
    );
    let (so, se) = (text(&out.stdout), text(&out.stderr));
    assert_eq!(out.status.code(), Some(0), "{se}");
    assert!(so.starts_with("7\n"), "{so}");
    assert!(
        so.contains("   1  running  step test failed") && so.contains("   2  succeeded"),
        "{so}"
    );
    assert!(
        so.contains("step       test") && so.contains("exit 101  check 12"),
        "{so}"
    );
    assert!(
        so.contains("| running 3 tests") && so.contains("| \\x1b[31mtest failed\\x1b[0m"),
        "{so}"
    );
    no_controls(&so);
    {
        let seen = server.fake.seen.lock().unwrap();
        let req = &seen.trigger[0];
        assert_eq!(req.name, "fix-ci");
        assert_eq!(req.inputs.get("branch").map(String::as_str), Some("main"));
        assert_eq!(req.change.len(), 16);
        assert!(seen.op_ids[0].is_some());
    }
    let (server, sb, cwd) = setup(Fake {
        failing: true,
        ..Fake::default()
    });
    let out = run(&sb, &cwd, &server, &["workflow", "run", "fix-ci", "-w"]);
    assert_eq!(
        out.status.code(),
        Some(1),
        "a run that did not succeed ends the command with 1"
    );
}

#[test]
fn runs_are_listed_shown_cancelled_and_decided() {
    let (server, sb, cwd) = setup(Fake::default());
    let out = run(
        &sb,
        &cwd,
        &server,
        &[
            "workflow", "runs", "fix-ci", "--state", "failed", "--limit", "10",
        ],
    );
    let so = text(&out.stdout);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    assert!(so.contains("7\tfailed\tfix-ci\t0/2 steps"), "{so}");
    let out = run(&sb, &cwd, &server, &["workflow", "status", "7"]);
    let so = text(&out.stdout);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    assert!(
        so.contains("state      succeeded") && so.contains("started by alice"),
        "{so}"
    );
    let out = run(
        &sb,
        &cwd,
        &server,
        &["workflow", "cancel", "7", "--reason", "stop"],
    );
    assert!(text(&out.stdout).contains("cancelled"));
    let out = run(
        &sb,
        &cwd,
        &server,
        &["workflow", "approve", "7", "ok", "--comment", "looks good"],
    );
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    let out = run(&sb, &cwd, &server, &["workflow", "reject", "7", "ok"]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    let seen = server.fake.seen.lock().unwrap();
    assert_eq!(seen.list_wf_runs[0].name, "fix-ci");
    assert_eq!(
        seen.list_wf_runs[0].states,
        [wire::WorkflowRunState::Failed as i32]
    );
    let d: Vec<(bool, &str, &str)> = seen
        .decide
        .iter()
        .map(|d| (d.approve, d.step.as_str(), d.comment.as_str()))
        .collect();
    assert_eq!(d, [(true, "ok", "looks good"), (false, "ok", "")]);
}

#[test]
fn a_workflow_secret_is_read_from_the_environment_and_listed_by_name_and_hint() {
    let (server, sb, cwd) = setup(Fake::default());
    let out = run_with(
        &sb,
        &cwd,
        &server,
        &[
            "workflow",
            "secret",
            "set",
            "DEPLOY_TOKEN",
            "--key-env",
            "DEPLOY",
        ],
        &[("DEPLOY", SECRET_VALUE)],
    );
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    clean_of_secrets(&out);
    assert!(text(&out.stdout).starts_with("DEPLOY_TOKEN\t...9876"));
    assert_eq!(
        server.fake.seen.lock().unwrap().set_secret[0].value,
        SECRET_VALUE
    );
    let out = run(&sb, &cwd, &server, &["workflow", "secret", "list"]);
    let so = text(&out.stdout);
    assert!(
        so.contains("DEPLOY_TOKEN\t...9876") && so.contains("SHORT\t-\t"),
        "{so}"
    );
    clean_of_secrets(&out);
    let out = run(
        &sb,
        &cwd,
        &server,
        &["workflow", "secret", "rm", "DEPLOY_TOKEN"],
    );
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(
        server.fake.seen.lock().unwrap().deleted_secrets,
        ["DEPLOY_TOKEN"]
    );
    // The value is never an argument.
    let out = run(
        &sb,
        &cwd,
        &server,
        &["workflow", "secret", "set", "X", "--value", SECRET_VALUE],
    );
    assert!(!out.status.success());
    clean_of_secrets(&out);
    assert_eq!(server.fake.seen.lock().unwrap().set_secret.len(), 1);
}
