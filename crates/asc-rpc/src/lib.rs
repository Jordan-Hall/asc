//! asc-rpc: the `asc.v1` wire types, their conversions to domain types, and (in later units) the
//! gRPC server and client.
//!
//! Generated prost messages stay inside this crate (`pb`). The public surface is the domain
//! conversions in [`convert`], [`errors`], and the generated tonic services in [`services`].

pub mod auth;
pub mod changes;
pub mod client;
pub mod cloud;
pub mod coalesce;
pub mod codeintel;
pub mod convert;
pub mod errors;
pub mod h3;
pub mod ipc;
pub mod json;
pub mod limits;
pub mod log_meta;
pub mod projects;
pub mod retry;
pub mod unavailable;
pub mod server;
pub mod watch_refs;

/// Generated prost messages and tonic services for package `asc.v1`.
#[allow(
    clippy::all,
    clippy::pedantic,
    missing_docs,
    unreachable_pub,
    unused_qualifications
)]
pub(crate) mod pb {
    include!(concat!(env!("OUT_DIR"), "/asc.v1.rs"));
}

/// The generated tonic client and server modules; the only prost-typed public API.
pub mod services {
    pub use crate::pb::agent_tokens_client;
    pub use crate::pb::agent_tokens_server;
    pub use crate::pb::agents_client;
    pub use crate::pb::agents_server;
    pub use crate::pb::ai_providers_client;
    pub use crate::pb::ai_providers_server;
    pub use crate::pb::audit_client;
    pub use crate::pb::audit_server;
    pub use crate::pb::changes_client;
    pub use crate::pb::changes_server;
    pub use crate::pb::ci_client;
    pub use crate::pb::ci_insights_client;
    pub use crate::pb::ci_insights_server;
    pub use crate::pb::ci_server;
    pub use crate::pb::cloud_client;
    pub use crate::pb::cloud_server;
    pub use crate::pb::code_intel_client;
    pub use crate::pb::code_intel_server;
    pub use crate::pb::cross_search_client;
    pub use crate::pb::cross_search_server;
    pub use crate::pb::inbox_client;
    pub use crate::pb::inbox_server;
    pub use crate::pb::merge_queue_client;
    pub use crate::pb::merge_queue_server;
    pub use crate::pb::policy_client;
    pub use crate::pb::policy_server;
    pub use crate::pb::projects_client;
    pub use crate::pb::projects_server;
    pub use crate::pb::repo_client;
    pub use crate::pb::repo_server;
    pub use crate::pb::review_client;
    pub use crate::pb::review_server;
    pub use crate::pb::secret_scan_client;
    pub use crate::pb::secret_scan_server;
    pub use crate::pb::signing_client;
    pub use crate::pb::signing_server;
    pub use crate::pb::stacks_client;
    pub use crate::pb::stacks_server;
    pub use crate::pb::storage_client;
    pub use crate::pb::storage_server;
    pub use crate::pb::vault_client;
    pub use crate::pb::vault_server;
    pub use crate::pb::workflows_client;
    pub use crate::pb::workflows_server;
    pub use crate::pb::workspace_client;
    pub use crate::pb::workspace_server;
}

pub use convert::{ConvertError, IdMode, ModeId, WireId};
pub use errors::RpcError;

/// The encoded `FileDescriptorSet` of every `asc.v1` proto (with imports and comments).
pub const DESCRIPTOR_SET: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/asc_descriptor.bin"));
