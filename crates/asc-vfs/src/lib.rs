//! asc-vfs: the OS-neutral core of the virtual file system providers (SPEC §10).
//!
//! The ProjFS and FUSE backends implement [`core::Provider`] and share [`core::CoreFs`] for path
//! resolution, content streaming, name filtering, the hydration policy and the counters.

pub mod core;
pub mod policy;
pub mod stats;
