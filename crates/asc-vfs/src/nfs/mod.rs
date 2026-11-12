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

#[cfg(test)]
mod client;
#[cfg(test)]
mod tests;

pub use provider::NfsProvider;

/// The NFS mount helpers this system may have; one must exist for a mount to work.
const MOUNT_HELPERS: &[&str] = if cfg!(target_os = "macos") {
    &["/sbin/mount_nfs"]
} else {
    &[
        "/sbin/mount.nfs",
        "/usr/sbin/mount.nfs",
        "/usr/bin/mount.nfs",
    ]
};

/// Whether the kernel's NFS client can mount the server here: its mount helper is installed.
pub fn available() -> bool {
    MOUNT_HELPERS
        .iter()
        .any(|p| std::path::Path::new(p).is_file())
}
