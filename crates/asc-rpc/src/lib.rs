//! asc-rpc: the `asc.v1` wire types, their conversions to domain types, and (in later units) the
//! gRPC server and client.
//!
//! Generated prost messages stay inside this crate (`pb`). The public surface is the domain
//! conversions in [`convert`], [`errors`], and the generated tonic services in [`services`].

pub mod changes;
pub mod convert;
pub mod errors;
pub mod json;

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
    pub use crate::pb::changes_client;
    pub use crate::pb::changes_server;
    pub use crate::pb::projects_client;
    pub use crate::pb::projects_server;
    pub use crate::pb::repo_client;
    pub use crate::pb::repo_server;
    pub use crate::pb::workspace_client;
    pub use crate::pb::workspace_server;
}

pub use convert::{ConvertError, IdMode, ModeId, WireId};
pub use errors::RpcError;

/// The encoded `FileDescriptorSet` of every `asc.v1` proto (with imports and comments).
pub const DESCRIPTOR_SET: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/asc_descriptor.bin"));
