//! The macOS FSKit backend (SPEC §10.3b): the merged file system served to the asc FSKit module
//! (`macos/fskit`, a user-space file system extension for macOS 26 and later) over a Unix-domain
//! socket, and mounted with `mount -F -t ascfs`. It replaces the loopback NFS mount where the
//! module is installed: no network stack, no NFS attribute cache, and the extension sees every
//! open and close.
//!
//! [`wire`] is the protocol, [`server`] answers it over [`crate::unixfs::MergedFs`], and
//! [`provider`] runs the server and the mount. The server runs on Linux too, where it is tested
//! with an in-process client; only the mount needs macOS and the installed module.

pub mod provider;
pub mod server;
pub mod wire;

#[cfg(test)]
mod tests;

use std::path::{Path, PathBuf};

pub use provider::FskitProvider;

/// The file system type the module registers (`FSShortName`), as `mount -t` names it.
pub const FS_TYPE: &str = "ascfs";
/// The URL scheme of the volumes the module serves: `ascfs://<work tree id>`.
pub const URL_SCHEME: &str = "ascfs";
/// The suffix of the app group the module and the daemon share; the group is
/// `<team id>.dev.asc.fskit`, set when the module is signed.
pub const GROUP_SUFFIX: &str = ".dev.asc.fskit";
/// Overrides the app group's directory (`ASC_FSKIT_GROUP_DIR`), for a module signed with
/// another group.
pub const GROUP_DIR_ENV: &str = "ASC_FSKIT_GROUP_DIR";
/// The host app that carries the module, in `/Applications` or `~/Applications`.
pub const APP_NAME: &str = "AscFS.app";
/// The longest socket path a Unix-domain address holds on macOS (`sun_path`, less the NUL).
pub const MAX_SOCKET_PATH: usize = 103;

/// The app group directory the sandboxed module can reach: `ASC_FSKIT_GROUP_DIR`, else the one
/// directory in `~/Library/Group Containers` whose name ends in [`GROUP_SUFFIX`].
pub fn group_dir() -> Option<PathBuf> {
    if let Some(d) = std::env::var_os(GROUP_DIR_ENV).filter(|d| !d.is_empty()) {
        return Some(PathBuf::from(d));
    }
    let home = PathBuf::from(std::env::var_os("HOME")?);
    find_group(&home.join("Library/Group Containers"))
}

/// The directory below `containers` whose name ends in [`GROUP_SUFFIX`], when exactly one does.
pub fn find_group(containers: &Path) -> Option<PathBuf> {
    let mut found = std::fs::read_dir(containers)
        .ok()?
        .filter_map(|e| e.ok())
        .filter(|e| {
            e.file_name()
                .to_str()
                .is_some_and(|n| n.ends_with(GROUP_SUFFIX) && n.len() > GROUP_SUFFIX.len())
                && e.file_type().is_ok_and(|t| t.is_dir())
        })
        .map(|e| e.path());
    let one = found.next()?;
    found.next().is_none().then_some(one)
}

/// The socket of the work tree `id` in the group directory `group`.
pub fn socket_path(group: &Path, id: u64) -> PathBuf {
    group.join("s").join(format!("{id:016x}.sock"))
}

/// The volume URL the module is asked to mount for the work tree `id`.
pub fn volume_url(id: u64) -> String {
    format!("{URL_SCHEME}://{id:016x}")
}

/// Whether the host app is installed in `/Applications` or `home/Applications`.
pub fn app_installed(home: Option<&Path>) -> bool {
    let mut places = vec![PathBuf::from("/Applications").join(APP_NAME)];
    if let Some(h) = home {
        places.push(h.join("Applications").join(APP_NAME));
    }
    places.iter().any(|p| p.is_dir())
}

/// Whether a work tree can be mounted through FSKit here: macOS, the module's host app
/// installed, and its app group directory present.
pub fn available() -> bool {
    if !cfg!(target_os = "macos") {
        return false;
    }
    let home = std::env::var_os("HOME").map(PathBuf::from);
    app_installed(home.as_deref()) && group_dir().is_some_and(|d| d.is_dir())
}
