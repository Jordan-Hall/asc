//! Temporary scaffolds for the build-out services (docs/BUILDOUT.md, "Scaffolds").
//!
//! Unit 0 of the build-out lands the whole wire contract at once. Every new service gets a [`Scaffold`]
//! implementation here that answers each method `UNIMPLEMENTED`, so the workspace compiles while the
//! units that own the services write them. A unit that implements a service deletes its group from the
//! `scaffold!` call below and its lines from the scaffold list in `docs/BUILDOUT.md`; the two must agree
//! (`tests/scaffold_registry.rs`). Nothing registers a [`Scaffold`] on a listener: a method that is not
//! served is already `UNIMPLEMENTED` from the router, and this type exists to keep the trait impls
//! checked by the compiler until the real ones replace it. When the list is empty this module is deleted.

use async_trait::async_trait;
use futures::stream::Empty;
use tonic::metadata::MetadataValue;
use tonic::{Request, Response, Status};

use crate::pb;

/// The metadata key that names the method a scaffold refused, so a client can tell "this server does
/// not carry the feature yet" from any other `UNIMPLEMENTED`.
pub const UNIMPLEMENTED_HEADER: &str = "x-asc-unimplemented";

/// The status a scaffolded method answers: `UNIMPLEMENTED`, the method named in the message and in
/// [`UNIMPLEMENTED_HEADER`]. `rpc` is `asc.v1.Service/Method`.
pub fn not_available(rpc: &str) -> Status {
    let mut status = Status::unimplemented(format!("{rpc} is not available on this server"));
    if let Ok(v) = MetadataValue::try_from(rpc) {
        status.metadata_mut().insert(UNIMPLEMENTED_HEADER, v);
    }
    status
}

/// A response stream that is never produced: a scaffolded streaming method fails before it starts.
pub type ScaffoldStream<T> = Empty<Result<T, Status>>;

/// Answers every method of every scaffolded service `UNIMPLEMENTED`.
#[derive(Clone, Copy, Debug, Default)]
pub struct Scaffold;

macro_rules! scaffold {
    (
        $(
            $module:ident :: $service:ident = $full:literal {
                unary { $( $uname:literal $ufn:ident($ureq:ident) -> $uresp:ident; )* }
                stream { $( $sname:literal $sfn:ident($sreq:ident) -> $sresp:ident as $sty:ident; )* }
            }
        )+
    ) => {
        $(
            #[async_trait]
            impl crate::services::$module::$service for Scaffold {
                $( type $sty = ScaffoldStream<pb::$sresp>; )*
                $(
                    async fn $ufn(
                        &self,
                        _request: Request<pb::$ureq>,
                    ) -> Result<Response<pb::$uresp>, Status> {
                        Err(not_available(concat!($full, "/", $uname)))
                    }
                )*
                $(
                    async fn $sfn(
                        &self,
                        _request: Request<pb::$sreq>,
                    ) -> Result<Response<Self::$sty>, Status> {
                        Err(not_available(concat!($full, "/", $sname)))
                    }
                )*
            }
        )+

        /// Every method this module scaffolds, as `asc.v1.Service/Method`.
        pub const SCAFFOLDED_RPCS: &[&str] = &[
            $(
                $( concat!($full, "/", $uname), )*
                $( concat!($full, "/", $sname), )*
            )+
        ];
    };
}

scaffold! {
    policy_server::Policy = "asc.v1.Policy" {
        unary {
            "GetSource" get_source(GetPolicySourceRequest) -> GetPolicySourceResponse;
            "Validate" validate(ValidatePolicyRequest) -> ValidatePolicyResponse;
            "Propose" propose(ProposePolicyRequest) -> ProposePolicyResponse;
            "Evaluate" evaluate(EvaluatePolicyRequest) -> EvaluatePolicyResponse;
        }
        stream {
        }
    }
    secret_scan_server::SecretScan = "asc.v1.SecretScan" {
        unary {
            "ListRules" list_rules(ListSecretRulesRequest) -> ListSecretRulesResponse;
            "Scan" scan(ScanSecretsRequest) -> ScanSecretsResponse;
            "ListFindings" list_findings(ListSecretFindingsRequest) -> ListSecretFindingsResponse;
            "UpdateFinding" update_finding(UpdateSecretFindingRequest) -> UpdateSecretFindingResponse;
        }
        stream {
        }
    }
    signing_server::Signing = "asc.v1.Signing" {
        unary {
            "ListKeys" list_keys(ListSigningKeysRequest) -> ListSigningKeysResponse;
            "AddKey" add_key(AddSigningKeyRequest) -> AddSigningKeyResponse;
            "RevokeKey" revoke_key(RevokeSigningKeyRequest) -> RevokeSigningKeyResponse;
            "SubmitSignature" submit_signature(SubmitSignatureRequest) -> SubmitSignatureResponse;
            "Verify" verify(VerifySignaturesRequest) -> VerifySignaturesResponse;
            "GetServerKey" get_server_key(GetServerKeyRequest) -> GetServerKeyResponse;
        }
        stream {
        }
    }
    audit_server::Audit = "asc.v1.Audit" {
        unary {
            "List" list(ListAuditRequest) -> ListAuditResponse;
            "VerifyChain" verify_chain(VerifyAuditChainRequest) -> VerifyAuditChainResponse;
            "ListActions" list_actions(ListAuditActionsRequest) -> ListAuditActionsResponse;
        }
        stream {
            "Export" export(ExportAuditRequest) -> ExportAuditResponse as ExportStream;
        }
    }
    stacks_server::Stacks = "asc.v1.Stacks" {
        unary {
            "Restack" restack(RestackStackRequest) -> RestackStackResponse;
            "RevertLanded" revert_landed(RevertLandedRequest) -> RevertLandedResponse;
            "ListRefLog" list_ref_log(ListRefLogRequest) -> ListRefLogResponse;
            "UndoRef" undo_ref(UndoRefRequest) -> UndoRefResponse;
        }
        stream {
        }
    }
    merge_queue_server::MergeQueue = "asc.v1.MergeQueue" {
        unary {
            "Enqueue" enqueue(EnqueueRequest) -> EnqueueResponse;
            "Dequeue" dequeue(DequeueRequest) -> DequeueResponse;
            "List" list(ListQueueRequest) -> ListQueueResponse;
            "Get" get(GetQueueEntryRequest) -> GetQueueEntryResponse;
            "Revalidate" revalidate(RevalidateRequest) -> RevalidateResponse;
            "SetPaused" set_paused(SetQueuePausedRequest) -> SetQueuePausedResponse;
        }
        stream {
            "Watch" watch(WatchQueueRequest) -> QueueEvent as WatchStream;
        }
    }
    ci_insights_server::CiInsights = "asc.v1.CiInsights" {
        unary {
            "ReportTestCases" report_test_cases(ReportTestCasesRequest) -> ReportTestCasesResponse;
            "ListFailureGroups" list_failure_groups(ListFailureGroupsRequest) -> ListFailureGroupsResponse;
            "GetFailureGroup" get_failure_group(GetFailureGroupRequest) -> GetFailureGroupResponse;
            "SetFailureGroupState" set_failure_group_state(SetFailureGroupStateRequest) -> SetFailureGroupStateResponse;
            "FlakyHistory" flaky_history(FlakyHistoryRequest) -> FlakyHistoryResponse;
            "ListFlakyTests" list_flaky_tests(ListFlakyTestsRequest) -> ListFlakyTestsResponse;
        }
        stream {
        }
    }
    ai_providers_server::AiProviders = "asc.v1.AiProviders" {
        unary {
            "CreateKey" create_key(CreateAiKeyRequest) -> CreateAiKeyResponse;
            "ListKeys" list_keys(ListAiKeysRequest) -> ListAiKeysResponse;
            "GetKey" get_key(GetAiKeyRequest) -> GetAiKeyResponse;
            "UpdateKey" update_key(UpdateAiKeyRequest) -> UpdateAiKeyResponse;
            "DeleteKey" delete_key(DeleteAiKeyRequest) -> DeleteAiKeyResponse;
            "TestKey" test_key(TestAiKeyRequest) -> TestAiKeyResponse;
            "Usage" usage(AiUsageRequest) -> AiUsageResponse;
            "ListModels" list_models(ListAiModelsRequest) -> ListAiModelsResponse;
        }
        stream {
        }
    }
    agents_server::Agents = "asc.v1.Agents" {
        unary {
            "CreateRun" create_run(CreateAgentRunRequest) -> CreateAgentRunResponse;
            "ListRuns" list_runs(ListAgentRunsRequest) -> ListAgentRunsResponse;
            "GetRun" get_run(GetAgentRunRequest) -> GetAgentRunResponse;
            "CancelRun" cancel_run(CancelAgentRunRequest) -> CancelAgentRunResponse;
            "SendInput" send_input(SendAgentInputRequest) -> SendAgentInputResponse;
        }
        stream {
            "WatchRun" watch_run(WatchAgentRunRequest) -> AgentRunEvent as WatchRunStream;
        }
    }
    agent_tokens_server::AgentTokens = "asc.v1.AgentTokens" {
        unary {
            "CreateToken" create_token(CreateAgentTokenRequest) -> CreateAgentTokenResponse;
            "ListTokens" list_tokens(ListAgentTokensRequest) -> ListAgentTokensResponse;
            "GetToken" get_token(GetAgentTokenRequest) -> GetAgentTokenResponse;
            "RevokeToken" revoke_token(RevokeAgentTokenRequest) -> RevokeAgentTokenResponse;
            "SetBudget" set_budget(SetAgentTokenBudgetRequest) -> SetAgentTokenBudgetResponse;
            "ReportUsage" report_usage(ReportAgentUsageRequest) -> ReportAgentUsageResponse;
        }
        stream {
        }
    }
    workflows_server::Workflows = "asc.v1.Workflows" {
        unary {
            "ListDefinitions" list_definitions(ListWorkflowDefinitionsRequest) -> ListWorkflowDefinitionsResponse;
            "GetDefinition" get_definition(GetWorkflowDefinitionRequest) -> GetWorkflowDefinitionResponse;
            "Validate" validate(ValidateWorkflowRequest) -> ValidateWorkflowResponse;
            "SetEnabled" set_enabled(SetWorkflowEnabledRequest) -> SetWorkflowEnabledResponse;
            "Trigger" trigger(TriggerWorkflowRequest) -> TriggerWorkflowResponse;
            "ListRuns" list_runs(ListWorkflowRunsRequest) -> ListWorkflowRunsResponse;
            "GetRun" get_run(GetWorkflowRunRequest) -> GetWorkflowRunResponse;
            "CancelRun" cancel_run(CancelWorkflowRunRequest) -> CancelWorkflowRunResponse;
            "DecideApproval" decide_approval(DecideWorkflowApprovalRequest) -> DecideWorkflowApprovalResponse;
        }
        stream {
            "WatchRun" watch_run(WatchWorkflowRunRequest) -> WorkflowRunEvent as WatchRunStream;
        }
    }
    cross_search_server::CrossSearch = "asc.v1.CrossSearch" {
        unary {
            "LookupSymbol" lookup_symbol(LookupSymbolRequest) -> LookupSymbolResponse;
            "SymbolPanel" symbol_panel(SymbolPanelRequest) -> SymbolPanelResponse;
        }
        stream {
            "Search" search(CrossSearchRequest) -> CrossSearchResponse as SearchStream;
        }
    }
}
