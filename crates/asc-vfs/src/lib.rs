//! asc-vfs: the OS-neutral core of the virtual file system providers (SPEC §10).
//!
//! The ProjFS and FUSE backends implement [`core::Provider`] and share [`core::CoreFs`] for path
//! resolution, content streaming, name filtering, the hydration policy and the counters.

pub mod compose;
pub mod core;
pub mod policy;
pub mod stats;

/// What the Windows backends share: config, journal sink, path rules, the switch engine.
#[cfg(all(windows, any(feature = "projfs", feature = "cloudfiles")))]
pub mod win;

/// Windows ProjFS backend (SPEC §10.2).
#[cfg(all(feature = "projfs", windows))]
#[allow(unsafe_code)] // the ProjFS FFI; every call is confined to this module tree.
pub mod projfs;

/// Windows Cloud Files backend (SPEC §10.2b): lazy files where ProjFS is not enabled.
#[cfg(all(feature = "cloudfiles", windows))]
#[allow(unsafe_code)] // the Cloud Files FFI; every call is confined to this module tree.
pub mod cloudfiles;

/// The Unix merged file system (SPEC §10.3, §10.4) the FUSE and NFS backends serve.
#[cfg(all(unix, any(all(feature = "fuse", target_os = "linux"), feature = "nfs")))]
pub mod unixfs;

/// Linux FUSE backend (SPEC §10.3).
#[cfg(all(feature = "fuse", target_os = "linux"))]
pub mod fuse;
