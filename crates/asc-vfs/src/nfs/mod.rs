//! The macOS backend (SPEC §10.4): the merged file system served over NFSv3 on a loopback port
//! and mounted with the kernel's own NFS client, so nothing has to be installed. The same server
//! runs on Linux, where it is tested; FUSE is the Linux default.
//!
//! [`xdr`] and [`rpc`] decode the wire, [`server`] answers NFS and MOUNT calls over
//! [`crate::unixfs::MergedFs`], and [`provider`] runs the server and the mount.

pub mod provider;
pub mod rpc;
pub mod server;
pub mod xdr;

pub use provider::NfsProvider;
