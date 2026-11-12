//! Which Windows backend serves a work tree (`asc.vfs.backend`, SPEC §10.2b).
//!
//! A root stays with the backend that virtualized it: its placeholders mean nothing to the other
//! one. A plain root takes the configured backend; `auto` prefers ProjFS (an optional Windows
//! feature) and falls back to Cloud Files (built into Windows 10 1709 and later).

use std::fmt;
use std::path::Path;
use std::str::FromStr;

/// A Windows backend.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Backend {
    ProjFs,
    CloudFiles,
}

impl Backend {
    /// The name `asc.vfs.backend` and `asc doctor` use.
    pub fn name(self) -> &'static str {
        match self {
            Backend::ProjFs => "projfs",
            Backend::CloudFiles => "cloudfiles",
        }
    }

    fn long(self) -> &'static str {
        match self {
            Backend::ProjFs => "the Windows Projected File System (ProjFS)",
            Backend::CloudFiles => "the Windows Cloud Files API",
        }
    }
}

impl fmt::Display for Backend {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// The value of `asc.vfs.backend`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Preference {
    #[default]
    Auto,
    Only(Backend),
}

impl FromStr for Preference {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, String> {
        match s.trim().to_ascii_lowercase().as_str() {
            "" | "auto" => Ok(Preference::Auto),
            "projfs" => Ok(Preference::Only(Backend::ProjFs)),
            "cloudfiles" => Ok(Preference::Only(Backend::CloudFiles)),
            other => Err(format!(
                "asc.vfs.backend is '{other}'; use auto, projfs or cloudfiles"
            )),
        }
    }
}

/// What the root is now: plain, or virtualized by one backend.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Found {
    Plain,
    Virtual(Backend),
}

/// Which backends load on this machine.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Loadable {
    pub projfs: bool,
    pub cloudfiles: bool,
}

impl Loadable {
    fn has(self, b: Backend) -> bool {
        match b {
            Backend::ProjFs => self.projfs,
            Backend::CloudFiles => self.cloudfiles,
        }
    }
}

/// The backend for a root in state `found`, or why there is none.
pub fn choose(pref: Preference, found: Found, can: Loadable) -> Result<Backend, String> {
    let need = |b: Backend| {
        if can.has(b) {
            Ok(b)
        } else {
            Err(format!(
                "this work tree needs {}, which is not available",
                b.long()
            ))
        }
    };
    match (found, pref) {
        (Found::Virtual(b), Preference::Only(want)) if want != b => Err(format!(
            "this work tree is served by {b}; run 'asc worktree convert --plain' before setting asc.vfs.backend to {want}"
        )),
        (Found::Virtual(b), _) | (Found::Plain, Preference::Only(b)) => need(b),
        (Found::Plain, Preference::Auto) if can.projfs => Ok(Backend::ProjFs),
        (Found::Plain, Preference::Auto) if can.cloudfiles => Ok(Backend::CloudFiles),
        (Found::Plain, Preference::Auto) => Err(
            "virtual workspaces need the Windows Projected File System or the Cloud Files API (Windows 10 1709 or later)"
                .into(),
        ),
    }
}

/// Which backends load here, for the backends this build has.
pub fn loadable() -> Loadable {
    Loadable {
        #[cfg(feature = "projfs")]
        projfs: crate::projfs::ffi::Api::load().is_ok(),
        #[cfg(not(feature = "projfs"))]
        projfs: false,
        #[cfg(feature = "cloudfiles")]
        cloudfiles: crate::cloudfiles::ffi::platform_available().is_ok(),
        #[cfg(not(feature = "cloudfiles"))]
        cloudfiles: false,
    }
}

/// What the absolute `root` is now. Reads only the root's own reparse tag and sync root
/// registration; nothing under it is opened.
pub fn found(root: &Path) -> Found {
    #[cfg(feature = "cloudfiles")]
    if crate::cloudfiles::ffi::is_our_sync_root(root) {
        return Found::Virtual(Backend::CloudFiles);
    }
    #[cfg(feature = "projfs")]
    if is_projfs_root(root) {
        return Found::Virtual(Backend::ProjFs);
    }
    let _ = root;
    Found::Plain
}

/// Whether `root` carries the ProjFS reparse point of a virtualization root.
#[cfg(feature = "projfs")]
fn is_projfs_root(root: &Path) -> bool {
    use crate::projfs::ffi::{IO_REPARSE_TAG_PROJFS, path_to_wide, read_dir_on_disk};
    let root = super::verbatim(root);
    let (Some(parent), Some(name)) = (root.parent(), root.file_name()) else {
        return false;
    };
    let name = name.to_string_lossy();
    read_dir_on_disk(&path_to_wide(parent), usize::MAX).is_ok_and(|(entries, _)| {
        entries
            .iter()
            .any(|e| e.tag == IO_REPARSE_TAG_PROJFS && e.name.eq_ignore_ascii_case(&name))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const BOTH: Loadable = Loadable {
        projfs: true,
        cloudfiles: true,
    };
    const CF: Loadable = Loadable {
        projfs: false,
        cloudfiles: true,
    };
    const NONE: Loadable = Loadable {
        projfs: false,
        cloudfiles: false,
    };

    #[test]
    fn preferences_parse() {
        assert_eq!("".parse(), Ok(Preference::Auto));
        assert_eq!(" Auto ".parse(), Ok(Preference::Auto));
        assert_eq!("projfs".parse(), Ok(Preference::Only(Backend::ProjFs)));
        assert_eq!(
            "CloudFiles".parse(),
            Ok(Preference::Only(Backend::CloudFiles))
        );
        assert!("fuse".parse::<Preference>().is_err());
    }

    #[test]
    fn auto_prefers_projfs_then_cloud_files() {
        assert_eq!(
            choose(Preference::Auto, Found::Plain, BOTH),
            Ok(Backend::ProjFs)
        );
        assert_eq!(
            choose(Preference::Auto, Found::Plain, CF),
            Ok(Backend::CloudFiles)
        );
        assert!(choose(Preference::Auto, Found::Plain, NONE).is_err());
    }

    #[test]
    fn an_explicit_backend_must_load() {
        let cf = Preference::Only(Backend::CloudFiles);
        let pf = Preference::Only(Backend::ProjFs);
        assert_eq!(choose(cf, Found::Plain, BOTH), Ok(Backend::CloudFiles));
        assert!(choose(pf, Found::Plain, CF).unwrap_err().contains("ProjFS"));
    }

    #[test]
    fn a_virtual_root_keeps_its_backend() {
        let cf_root = Found::Virtual(Backend::CloudFiles);
        let pf_root = Found::Virtual(Backend::ProjFs);
        assert_eq!(
            choose(Preference::Auto, cf_root, BOTH),
            Ok(Backend::CloudFiles)
        );
        assert_eq!(choose(Preference::Auto, pf_root, BOTH), Ok(Backend::ProjFs));
        assert!(
            choose(Preference::Auto, pf_root, CF)
                .unwrap_err()
                .contains("not available")
        );
        let e = choose(Preference::Only(Backend::ProjFs), cf_root, BOTH).unwrap_err();
        assert!(e.contains("convert --plain"), "{e}");
    }

    #[test]
    fn a_plain_directory_is_found_plain() {
        let d = tempfile::tempdir().unwrap();
        assert_eq!(found(d.path()), Found::Plain);
    }
}
