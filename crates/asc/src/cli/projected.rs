//! The provider's view of a virtual work tree, asked per file without reading it: whether the
//! file still holds the base tree's content. Status takes the tree's entry for such a file, so a
//! clean file is never hydrated (docs/designs/build-cache.md §1). A file the provider cannot
//! answer for is read as in a plain work tree.

use asc_state::state::workspaces::Mode;
use asc_wc::plain::Projected;

use super::common::WsCtx;

/// The view for `ws`: `None` for a plain workspace, or where this build has no provider.
pub(crate) fn for_workspace(ws: &WsCtx) -> Option<Box<dyn Projected>> {
    if ws.workspace.ws.mode != Mode::Virtual {
        return None;
    }
    let overlay = ws.home.workspace_dir(ws.workspace.ws.id).join("overlay");
    imp::open(&overlay, ws.projection.toplevel())
}

#[cfg(all(windows, any(feature = "vfs-projfs", feature = "vfs-cloudfiles")))]
mod imp {
    use std::io;
    use std::path::Path;

    use asc_core::path::RepoPath;
    use asc_vfs::win::select;
    use asc_wc::SafeRoot;
    use asc_wc::plain::Projected;

    /// `PrjGetOnDiskFileState`: a placeholder, or a hydrated placeholder that was not changed.
    #[cfg(feature = "vfs-projfs")]
    struct Projfs {
        api: asc_vfs::projfs::ffi::Api,
        gate: SafeRoot,
    }

    #[cfg(feature = "vfs-projfs")]
    impl Projected for Projfs {
        fn unchanged(&self, path: &RepoPath) -> io::Result<bool> {
            use asc_vfs::projfs::ffi::{self, file_state};
            let Some(abs) = self.gate.resolve(path) else {
                return Ok(false);
            };
            let changed = file_state::FULL | file_state::DIRTY_PLACEHOLDER | file_state::TOMBSTONE;
            let clean = file_state::PLACEHOLDER | file_state::HYDRATED_PLACEHOLDER;
            Ok(self
                .api
                .file_state(&ffi::path_to_wide(&abs))
                .is_ok_and(|bits| bits & clean != 0 && bits & changed == 0))
        }
    }

    /// A Cloud Files placeholder file still in sync: nothing wrote to it since the provider made
    /// or last updated it. Read from its attributes alone, so nothing hydrates.
    #[cfg(feature = "vfs-cloudfiles")]
    struct CloudFiles {
        gate: SafeRoot,
    }

    #[cfg(feature = "vfs-cloudfiles")]
    impl Projected for CloudFiles {
        fn unchanged(&self, path: &RepoPath) -> io::Result<bool> {
            let Some(abs) = self.gate.resolve(path) else {
                return Ok(false);
            };
            Ok(
                asc_vfs::cloudfiles::ffi::probe(&asc_vfs::win::verbatim(&abs))
                    .is_ok_and(|p| p.is_placeholder() && !p.is_dir() && p.in_sync),
            )
        }
    }

    pub(super) fn open(_overlay: &Path, root: &Path) -> Option<Box<dyn Projected>> {
        let gate = SafeRoot::new(root);
        match select::found(root) {
            #[cfg(feature = "vfs-cloudfiles")]
            select::Found::Virtual(select::Backend::CloudFiles) => {
                Some(Box::new(CloudFiles { gate }))
            }
            // A root not seen as a sync root is asked as a ProjFS one: the state of a file that
            // is not a placeholder reads as changed, so the worst case is a plain read.
            #[cfg(feature = "vfs-projfs")]
            _ => {
                let api = asc_vfs::projfs::ffi::Api::load().ok()?;
                Some(Box::new(Projfs { api, gate }))
            }
            #[cfg(not(feature = "vfs-projfs"))]
            _ => None,
        }
    }
}

#[cfg(any(
    all(target_os = "linux", feature = "vfs-fuse"),
    all(unix, feature = "vfs-nfs")
))]
mod imp {
    use std::io;
    use std::path::Path;

    use asc_core::path::RepoPath;
    use asc_vfs::unixfs::overlay::Overlay;
    use asc_wc::plain::Projected;
    use asc_wc::redirect::Redirections;

    /// The workspace's overlay, read beside the running mount: a file shows the base tree's
    /// content while nothing was written at it and no deletion hides it. FUSE and NFS mounts
    /// share the overlay, and NFS has no extended attributes to ask through the mount.
    struct Unix {
        overlay: Overlay,
        redirections: Redirections,
    }

    impl Projected for Unix {
        fn unchanged(&self, path: &RepoPath) -> io::Result<bool> {
            // A redirected directory is a real one outside the overlay.
            if self.redirections.covers(path) {
                return Ok(false);
            }
            self.overlay.shows_base(path)
        }
    }

    pub(super) fn open(overlay: &Path, root: &Path) -> Option<Box<dyn Projected>> {
        Some(Box::new(Unix {
            overlay: Overlay::peek(overlay)?,
            redirections: Redirections::load(root).ok()?,
        }))
    }
}

#[cfg(not(any(
    all(windows, any(feature = "vfs-projfs", feature = "vfs-cloudfiles")),
    all(target_os = "linux", feature = "vfs-fuse"),
    all(unix, feature = "vfs-nfs")
)))]
mod imp {
    use std::path::Path;

    use asc_wc::plain::Projected;

    pub(super) fn open(_overlay: &Path, _root: &Path) -> Option<Box<dyn Projected>> {
        None
    }
}
