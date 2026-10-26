//! `asc.v1.ErrorDetail` as a Rust enum (SPEC §7 error codes).

use bytes::Bytes;

use crate::convert::{Conflict, ConvertError};
use crate::pb;

/// The payload every error code can carry.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ErrorInfo {
    /// Paths the error is about.
    pub paths: Vec<String>,
    /// The actual current value (for example the ref target that defeated a compare-and-swap).
    pub actual: Bytes,
    /// Conflict records for land and rebase failures.
    pub conflicts: Vec<Conflict>,
}

macro_rules! rpc_errors {
    ($($variant:ident => $code:literal),+ $(,)?) => {
        /// A failed call, keyed by `ErrorDetail.code`.
        #[derive(Clone, Debug, PartialEq, Eq)]
        pub enum RpcError {
            $(#[doc = concat!("`", $code, "`.")] $variant(ErrorInfo),)+
            /// A code this version does not know; kept so newer servers stay readable.
            Unknown { code: String, info: ErrorInfo },
        }

        impl RpcError {
            /// The `ErrorDetail.code` string.
            pub fn code(&self) -> &str {
                match self {
                    $(Self::$variant(_) => $code,)+
                    Self::Unknown { code, .. } => code,
                }
            }

            /// The payload.
            pub fn info(&self) -> &ErrorInfo {
                match self {
                    $(Self::$variant(i))|+ | Self::Unknown { info: i, .. } => i,
                }
            }

            fn from_parts(code: &str, info: ErrorInfo) -> Self {
                match code {
                    $($code => Self::$variant(info),)+
                    _ => Self::Unknown { code: code.to_owned(), info },
                }
            }
        }
    };
}

rpc_errors! {
    RefConflict => "REF_CONFLICT",
    NotFastForward => "NOT_FAST_FORWARD",
    LandConflict => "LAND_CONFLICT",
    LandNeedsClientRebase => "LAND_NEEDS_CLIENT_REBASE",
    MissingObjects => "MISSING_OBJECTS",
    InvalidObject => "INVALID_OBJECT",
    HookRejected => "HOOK_REJECTED",
    MirrorMode => "MIRROR_MODE",
    ClientTooOld => "CLIENT_TOO_OLD",
    PathPolicy => "PATH_POLICY",
    ProjectsConfig => "PROJECTS_CONFIG",
    RateLimited => "RATE_LIMITED",
    ChangeDiverged => "CHANGE_DIVERGED",
    ChangeFrozen => "CHANGE_FROZEN",
    ChangeNotAllowed => "CHANGE_NOT_ALLOWED",
    LandNeedsContentMerge => "LAND_NEEDS_CONTENT_MERGE",
    VersionConflict => "VERSION_CONFLICT",
    CloudHeadsLimit => "CLOUD_HEADS_LIMIT",
    CloudWorkspacesLimit => "CLOUD_WORKSPACES_LIMIT",
    UserRefsLimit => "USER_REFS_LIMIT",
    QuotaExceeded => "QUOTA_EXCEEDED",
    RefNotShared => "REF_NOT_SHARED",
    PolicyDenied => "POLICY_DENIED",
    PolicyInvalid => "POLICY_INVALID",
    PolicyLockout => "POLICY_LOCKOUT",
    SecretDetected => "SECRET_DETECTED",
    UnsignedChange => "UNSIGNED_CHANGE",
    SignatureInvalid => "SIGNATURE_INVALID",
    SigningKeyLimit => "SIGNING_KEY_LIMIT",
    StackNotLanded => "STACK_NOT_LANDED",
    RevertConflict => "REVERT_CONFLICT",
    RefUndoProtected => "REF_UNDO_PROTECTED",
    RefLogExpired => "REF_LOG_EXPIRED",
    QueuePaused => "QUEUE_PAUSED",
    QueueEntryExists => "QUEUE_ENTRY_EXISTS",
    RevalidationFailed => "REVALIDATION_FAILED",
    StorageEndpointBlocked => "STORAGE_ENDPOINT_BLOCKED",
    StorageVerifyFailed => "STORAGE_VERIFY_FAILED",
    StorageInUse => "STORAGE_IN_USE",
    MigrationActive => "MIGRATION_ACTIVE",
    VaultLocked => "VAULT_LOCKED",
    RotationActive => "ROTATION_ACTIVE",
    AiEndpointBlocked => "AI_ENDPOINT_BLOCKED",
    AiKeyUntested => "AI_KEY_UNTESTED",
    AiCapReached => "AI_CAP_REACHED",
    AgentsDisabled => "AGENTS_DISABLED",
    AgentTokenExpired => "AGENT_TOKEN_EXPIRED",
    AgentTokenRevoked => "AGENT_TOKEN_REVOKED",
    AgentScopeDenied => "AGENT_SCOPE_DENIED",
    BudgetExceeded => "BUDGET_EXCEEDED",
    RunNotActive => "RUN_NOT_ACTIVE",
    WorkflowInvalid => "WORKFLOW_INVALID",
    WorkflowDisabled => "WORKFLOW_DISABLED",
    SearchTooBroad => "SEARCH_TOO_BROAD",
}

impl std::fmt::Display for RpcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.code())?;
        let paths = &self.info().paths;
        if !paths.is_empty() {
            write!(f, ": {}", paths.join(", "))?;
        }
        Ok(())
    }
}

impl std::error::Error for RpcError {}

impl TryFrom<&pb::ErrorDetail> for RpcError {
    type Error = ConvertError;

    fn try_from(d: &pb::ErrorDetail) -> Result<Self, ConvertError> {
        let conflicts = d
            .conflicts
            .iter()
            .map(Conflict::try_from)
            .collect::<Result<_, _>>()?;
        let info = ErrorInfo {
            paths: d.paths.clone(),
            actual: d.actual.clone(),
            conflicts,
        };
        Ok(Self::from_parts(&d.code, info))
    }
}

impl From<&RpcError> for pb::ErrorDetail {
    fn from(e: &RpcError) -> Self {
        let i = e.info();
        Self {
            code: e.code().to_owned(),
            paths: i.paths.clone(),
            actual: i.actual.clone(),
            conflicts: i.conflicts.iter().map(pb::Conflict::from).collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::convert::{ConflictKind, ModeId};
    use asc_core::id::ChangesetId;
    use asc_core::path::RepoPath;

    fn conflict() -> Conflict {
        Conflict {
            changeset: ModeId::Asc(ChangesetId::from_bytes([1; 32])),
            path: RepoPath::new("bun.lock").unwrap(),
            kind: ConflictKind::Content,
            base: None,
            ours: None,
            theirs: None,
            marked: None,
        }
    }

    #[test]
    fn detail_round_trip_for_every_code() {
        let codes = [
            "REF_CONFLICT",
            "NOT_FAST_FORWARD",
            "LAND_CONFLICT",
            "LAND_NEEDS_CLIENT_REBASE",
            "MISSING_OBJECTS",
            "INVALID_OBJECT",
            "HOOK_REJECTED",
            "MIRROR_MODE",
            "CLIENT_TOO_OLD",
            "PATH_POLICY",
            "PROJECTS_CONFIG",
            "RATE_LIMITED",
            "CHANGE_DIVERGED",
            "CHANGE_FROZEN",
            "CHANGE_NOT_ALLOWED",
            "LAND_NEEDS_CONTENT_MERGE",
            "VERSION_CONFLICT",
            "CLOUD_HEADS_LIMIT",
            "CLOUD_WORKSPACES_LIMIT",
            "USER_REFS_LIMIT",
            "QUOTA_EXCEEDED",
            "REF_NOT_SHARED",
            "POLICY_DENIED",
            "POLICY_INVALID",
            "POLICY_LOCKOUT",
            "SECRET_DETECTED",
            "UNSIGNED_CHANGE",
            "SIGNATURE_INVALID",
            "SIGNING_KEY_LIMIT",
            "STACK_NOT_LANDED",
            "REVERT_CONFLICT",
            "REF_UNDO_PROTECTED",
            "REF_LOG_EXPIRED",
            "QUEUE_PAUSED",
            "QUEUE_ENTRY_EXISTS",
            "REVALIDATION_FAILED",
            "STORAGE_ENDPOINT_BLOCKED",
            "STORAGE_VERIFY_FAILED",
            "STORAGE_IN_USE",
            "MIGRATION_ACTIVE",
            "VAULT_LOCKED",
            "ROTATION_ACTIVE",
            "AI_ENDPOINT_BLOCKED",
            "AI_KEY_UNTESTED",
            "AI_CAP_REACHED",
            "AGENTS_DISABLED",
            "AGENT_TOKEN_EXPIRED",
            "AGENT_TOKEN_REVOKED",
            "AGENT_SCOPE_DENIED",
            "BUDGET_EXCEEDED",
            "RUN_NOT_ACTIVE",
            "WORKFLOW_INVALID",
            "WORKFLOW_DISABLED",
            "SEARCH_TOO_BROAD",
        ];
        for code in codes {
            let d = pb::ErrorDetail {
                code: code.into(),
                paths: vec!["a/b".into()],
                actual: Bytes::from_static(&[7; 33]),
                conflicts: vec![pb::Conflict::from(&conflict())],
            };
            let e = RpcError::try_from(&d).unwrap();
            assert_eq!(e.code(), code);
            assert!(!matches!(e, RpcError::Unknown { .. }), "{code}");
            assert_eq!(e.info().conflicts, vec![conflict()]);
            assert_eq!(pb::ErrorDetail::from(&e), d);
        }
    }

    #[test]
    fn unknown_code_is_preserved() {
        let d = pb::ErrorDetail {
            code: "FUTURE".into(),
            ..Default::default()
        };
        let e = RpcError::try_from(&d).unwrap();
        assert!(matches!(&e, RpcError::Unknown { code, .. } if code == "FUTURE"));
        assert_eq!(pb::ErrorDetail::from(&e), d);
    }

    #[test]
    fn display_names_code_and_paths() {
        let e = RpcError::PathPolicy(ErrorInfo {
            paths: vec!["a".into(), "b".into()],
            ..Default::default()
        });
        assert_eq!(e.to_string(), "PATH_POLICY: a, b");
    }

    #[test]
    fn bad_conflict_is_rejected() {
        let d = pb::ErrorDetail {
            code: "LAND_CONFLICT".into(),
            conflicts: vec![pb::Conflict {
                changeset: Bytes::from_static(&[1; 3]),
                ..Default::default()
            }],
            ..Default::default()
        };
        assert!(RpcError::try_from(&d).is_err());
    }

    #[test]
    fn buildout_codes_map_to_their_grpc_status() {
        use tonic::Code;
        let table = [
            ("POLICY_DENIED", Code::PermissionDenied),
            ("POLICY_INVALID", Code::InvalidArgument),
            ("POLICY_LOCKOUT", Code::FailedPrecondition),
            ("SECRET_DETECTED", Code::FailedPrecondition),
            ("UNSIGNED_CHANGE", Code::FailedPrecondition),
            ("SIGNATURE_INVALID", Code::InvalidArgument),
            ("SIGNING_KEY_LIMIT", Code::ResourceExhausted),
            ("STACK_NOT_LANDED", Code::FailedPrecondition),
            ("REVERT_CONFLICT", Code::Aborted),
            ("REF_UNDO_PROTECTED", Code::PermissionDenied),
            ("REF_LOG_EXPIRED", Code::OutOfRange),
            ("QUEUE_PAUSED", Code::FailedPrecondition),
            ("QUEUE_ENTRY_EXISTS", Code::AlreadyExists),
            ("REVALIDATION_FAILED", Code::FailedPrecondition),
            ("STORAGE_ENDPOINT_BLOCKED", Code::InvalidArgument),
            ("STORAGE_VERIFY_FAILED", Code::FailedPrecondition),
            ("STORAGE_IN_USE", Code::FailedPrecondition),
            ("MIGRATION_ACTIVE", Code::Aborted),
            ("VAULT_LOCKED", Code::Unavailable),
            ("ROTATION_ACTIVE", Code::Aborted),
            ("AI_ENDPOINT_BLOCKED", Code::InvalidArgument),
            ("AI_KEY_UNTESTED", Code::FailedPrecondition),
            ("AI_CAP_REACHED", Code::ResourceExhausted),
            ("AGENTS_DISABLED", Code::FailedPrecondition),
            ("AGENT_TOKEN_EXPIRED", Code::Unauthenticated),
            ("AGENT_TOKEN_REVOKED", Code::Unauthenticated),
            ("AGENT_SCOPE_DENIED", Code::PermissionDenied),
            ("BUDGET_EXCEEDED", Code::ResourceExhausted),
            ("RUN_NOT_ACTIVE", Code::FailedPrecondition),
            ("WORKFLOW_INVALID", Code::InvalidArgument),
            ("WORKFLOW_DISABLED", Code::FailedPrecondition),
            ("SEARCH_TOO_BROAD", Code::InvalidArgument),
        ];
        for (code, want) in table {
            let d = pb::ErrorDetail {
                code: code.into(),
                paths: vec!["a".into()],
                ..Default::default()
            };
            let e = RpcError::try_from(&d).unwrap();
            let status = crate::server::rpc_status(&e);
            assert_eq!(status.code(), want, "{code}");
            let back = crate::server::status_rpc_error(&status).unwrap();
            assert_eq!(back.code(), code);
            assert_eq!(back.info().paths, vec!["a".to_string()]);
        }
    }
}
