//! The refusal a method gives when this server does not carry its feature.
//!
//! A method no handler serves is `UNIMPLEMENTED` from the router already; a service that is wired
//! but whose backing part is absent (for example a stack operation without a configured executor)
//! answers with [`not_available`], which names the method in [`UNIMPLEMENTED_HEADER`] so a client
//! can tell "this server does not carry the feature" from any other `UNIMPLEMENTED`.

use tonic::Status;
use tonic::metadata::MetadataValue;

/// The metadata key that names the method that was refused.
pub const UNIMPLEMENTED_HEADER: &str = "x-asc-unimplemented";

/// The status of a method this server does not carry: `UNIMPLEMENTED`, the method named in the
/// message and in [`UNIMPLEMENTED_HEADER`]. `rpc` is `asc.v1.Service/Method`.
pub fn not_available(rpc: &str) -> Status {
    let mut status = Status::unimplemented(format!("{rpc} is not available on this server"));
    if let Ok(v) = MetadataValue::try_from(rpc) {
        status.metadata_mut().insert(UNIMPLEMENTED_HEADER, v);
    }
    status
}
