//! asc-vfs: the OS-neutral core of the virtual file system providers (SPEC §10).
//!
//! The ProjFS and FUSE backends implement [`core::Provider`] and share [`core::CoreFs`] for path
//! resolution, content streaming, name filtering, the hydration policy and the counters.

pub mod compose;
pub mod core;
pub mod policy;
pub mod stats;

/// Windows ProjFS backend (SPEC §10.2).
#[cfg(all(feature = "projfs", windows))]
#[allow(unsafe_code)] // the ProjFS FFI; every call is confined to this module tree.
pub mod projfs;

/// Linux FUSE backend (SPEC §10.3).
#[cfg(all(feature = "fuse", target_os = "linux"))]
pub mod fuse;
