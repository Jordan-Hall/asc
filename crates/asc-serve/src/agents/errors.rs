//! The typed errors of the agent services (`ErrorDetail.code`), built in one place.

use asc_rpc::errors::{ErrorInfo, RpcError};
use asc_rpc::server::rpc_status;
use tonic::Status;

fn info(paths: Vec<String>) -> ErrorInfo {
    ErrorInfo {
        paths,
        ..ErrorInfo::default()
    }
}

/// `AGENTS_DISABLED`: the instance or the repository has agents off.
pub fn disabled(why: &str) -> Status {
    rpc_status(&RpcError::AgentsDisabled(info(vec![why.to_owned()])))
}

/// `AGENT_SCOPE_DENIED` with the refused paths (or refs).
pub fn scope_denied(paths: Vec<String>) -> Status {
    rpc_status(&RpcError::AgentScopeDenied(info(paths)))
}

/// `BUDGET_EXCEEDED`; `dimension` is `changes`, `tokens`, `cost`, `paths` or `runtime`.
pub fn budget_exceeded(dimension: &str) -> Status {
    rpc_status(&RpcError::BudgetExceeded(info(vec![dimension.to_owned()])))
}

/// `RUN_NOT_ACTIVE`: the run is over.
pub fn run_not_active(state: &str) -> Status {
    rpc_status(&RpcError::RunNotActive(info(vec![state.to_owned()])))
}

/// `AGENT_TOKEN_EXPIRED`.
pub fn token_expired() -> Status {
    rpc_status(&RpcError::AgentTokenExpired(ErrorInfo::default()))
}

/// `AGENT_TOKEN_REVOKED`.
pub fn token_revoked() -> Status {
    rpc_status(&RpcError::AgentTokenRevoked(ErrorInfo::default()))
}

/// `POLICY_DENIED` with a reason.
pub fn policy_denied(reason: String, paths: Vec<String>) -> Status {
    let mut v = vec![reason];
    v.extend(paths);
    rpc_status(&RpcError::PolicyDenied(info(v)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_codes_are_the_documented_ones() {
        let code = |s: Status| {
            asc_rpc::server::status_rpc_error(&s)
                .unwrap()
                .code()
                .to_owned()
        };
        assert_eq!(code(disabled("instance")), "AGENTS_DISABLED");
        assert_eq!(code(scope_denied(vec!["a".into()])), "AGENT_SCOPE_DENIED");
        assert_eq!(code(budget_exceeded("tokens")), "BUDGET_EXCEEDED");
        assert_eq!(code(run_not_active("succeeded")), "RUN_NOT_ACTIVE");
        assert_eq!(code(token_expired()), "AGENT_TOKEN_EXPIRED");
        assert_eq!(code(token_revoked()), "AGENT_TOKEN_REVOKED");
        assert_eq!(code(policy_denied("r".into(), vec![])), "POLICY_DENIED");
        assert_eq!(budget_exceeded("x").code(), tonic::Code::ResourceExhausted);
        assert_eq!(scope_denied(vec![]).code(), tonic::Code::PermissionDenied);
        assert_eq!(disabled("x").code(), tonic::Code::FailedPrecondition);
    }
}
