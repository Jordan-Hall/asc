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
    imp::open(ws.projection.toplevel())
}

#[cfg(all(windows, feature = "vfs-projfs"))]
mod imp {
    use std::io;
    use std::path::Path;

    use asc_core::path::RepoPath;
    use asc_vfs::projfs::ffi::{self, Api, file_state};
    use asc_wc::SafeRoot;
    use asc_wc::plain::Projected;

    /// `PrjGetOnDiskFileState`: a placeholder, or a hydrated placeholder that was not changed.
    struct Projfs {
        api: Api,
        gate: SafeRoot,
    }

    impl Projected for Projfs {
        fn unchanged(&self, path: &RepoPath) -> io::Result<bool> {
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

    pub(super) fn open(root: &Path) -> Option<Box<dyn Projected>> {
        let api = Api::load().ok()?;
        Some(Box::new(Projfs {
            api,
            gate: SafeRoot::new(root),
        }))
    }
}

#[cfg(all(target_os = "linux", feature = "vfs-fuse"))]
mod imp {
    use std::io;
    use std::path::Path;

    use asc_core::path::RepoPath;
    use asc_vfs::fuse::PROJECTED_XATTR;
    use asc_wc::SafeRoot;
    use asc_wc::plain::Projected;

    /// The mount's `user.asc.projected` attribute: a base-tree file nobody wrote to.
    struct Fuse {
        gate: SafeRoot,
    }

    impl Projected for Fuse {
        fn unchanged(&self, path: &RepoPath) -> io::Result<bool> {
            let Some(abs) = self.gate.resolve(path) else {
                return Ok(false);
            };
            let mut value = [0u8; 1];
            Ok(rustix::fs::lgetxattr(&abs, PROJECTED_XATTR, &mut value[..])
                .is_ok_and(|n| value[..n] == *b"1"))
        }
    }

    pub(super) fn open(root: &Path) -> Option<Box<dyn Projected>> {
        Some(Box::new(Fuse {
            gate: SafeRoot::new(root),
        }))
    }
}

#[cfg(not(any(
    all(windows, feature = "vfs-projfs"),
    all(target_os = "linux", feature = "vfs-fuse")
)))]
mod imp {
    use std::path::Path;

    use asc_wc::plain::Projected;

    pub(super) fn open(_root: &Path) -> Option<Box<dyn Projected>> {
        None
    }
}
