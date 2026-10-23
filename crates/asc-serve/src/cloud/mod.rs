//! The commit cloud on the server (SPEC §21.3, PLAN B09): per-user workspaces of draft heads,
//! the `Cloud` RPCs, and the quota ledger that ingest and `PutObjects` charge.
//!
//! Later branchless units add their own `pub mod` lines here (`expiry`, `purge`).

pub mod quota;
pub mod rpc;

/// The server-side bounds of SPEC §21.3.2 (`[cloud]`). The defaults are the SPEC values;
/// `max_user_bytes` has no SPEC default, so it is 10 GiB per user per repository.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    /// Draft heads one workspace holds (`max_heads_per_workspace`).
    pub max_heads_per_workspace: u32,
    /// Workspaces one user holds in one repository (`max_workspaces_per_user`).
    pub max_workspaces_per_user: u32,
    /// Bytes one user may have charged in one repository (`max_user_bytes`).
    pub max_user_bytes: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_heads_per_workspace: 10_000,
            max_workspaces_per_user: 16,
            max_user_bytes: 10 * 1024 * 1024 * 1024,
        }
    }
}
