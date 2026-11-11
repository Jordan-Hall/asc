//! Turning a Cloud Files root back into a plain directory once its provider has disconnected
//! (`worktree convert --plain`). Nothing here asks the provider; every open is made without
//! reading data (`FILE_FLAG_OPEN_REPARSE_POINT`, attributes only), so nothing hydrates:
//!
//! * a placeholder directory is walked (only what is on disk), then reverted to a plain one; one
//!   that was never listed is first marked as listed, its missing children being tracked entries;
//! * a placeholder file that is in sync holds the tracked content or none of it: it is removed, to
//!   be written again from the object store;
//! * a placeholder file that is out of sync holds the user's edit (a write hydrates it in full
//!   first): it is reverted to a plain file and kept;
//! * a plain file or directory is kept; other reparse points (symbolic links, junctions) are
//!   neither changed nor entered.
//!
//! The sync root is unregistered last. Cloud Files keeps no tombstones, so the user's deletions
//! are what [`super::CloudFilesProvider::tombstones`] returned before the provider stopped; the
//! `deleted` list of the result is empty. Every step reads the state on disk, so a walk that stops
//! part way is finished by running it again.

use std::io;
use std::path::{Path, PathBuf};

use windows::Win32::Storage::FileSystem::{
    FILE_READ_ATTRIBUTES, FILE_WRITE_ATTRIBUTES, FILE_WRITE_DATA,
};

use super::ffi::{self, FILE_ATTRIBUTE_NORMAL};
pub use crate::win::Unvirtualized;
use crate::win::verbatim;

/// The error of `op` on `p`, naming both, with the OS error kind kept.
fn at(op: &str, p: &Path) -> impl FnOnce(io::Error) -> io::Error {
    let what = format!("{op} '{}'", p.display());
    move |e| io::Error::new(e.kind(), format!("{what}: {e}"))
}

/// Reverts the placeholder at `p` to a plain file or directory.
fn revert(p: &Path) -> io::Result<()> {
    let h = ffi::open(
        p,
        FILE_WRITE_DATA.0 | FILE_READ_ATTRIBUTES.0 | FILE_WRITE_ATTRIBUTES.0,
        ffi::SHARE_ALL,
    )
    .map_err(at("open", p))?;
    ffi::revert(&h).map_err(at("revert the placeholder", p))
}

/// Marks the directory at `p` as holding all its children. Without a provider a directory that was
/// never listed cannot be reverted; its children that are not on disk are tracked entries, written
/// again from the object store like the in-sync files removed here.
fn populated(p: &Path) -> io::Result<()> {
    ffi::disable_on_demand(p).map_err(at("mark as listed", p))
}

/// Unvirtualizes the root at the absolute `root`; see the module comment. A root that is not (or
/// no longer) a sync root is walked the same way and comes out unchanged.
pub fn unvirtualize(root: &Path) -> io::Result<Unvirtualized> {
    let top = verbatim(root);
    let mut out = Unvirtualized::default();
    // Directories are reverted after their children, deepest first.
    let mut dirs: Vec<PathBuf> = Vec::new();
    let mut stack: Vec<PathBuf> = vec![top.clone()];
    while let Some(dir) = stack.pop() {
        let entries = match std::fs::read_dir(&dir) {
            Ok(rd) => rd,
            Err(e) if ffi::is_not_found(&e) => continue,
            Err(e) => return Err(at("list", &dir)(e)),
        };
        for e in entries {
            let e = e.map_err(at("list", &dir))?;
            let abs = dir.join(e.file_name());
            let Some(p) = ffi::probe_opt(&abs).map_err(at("read the state of", &abs))? else {
                continue;
            };
            if p.is_foreign_reparse() {
                continue;
            }
            if p.is_dir() {
                if p.is_unpopulated() {
                    populated(&abs)?;
                }
                if p.is_placeholder() {
                    dirs.push(abs.clone());
                }
                stack.push(abs);
                continue;
            }
            if !p.is_placeholder() {
                continue;
            }
            if p.in_sync {
                ffi::set_attributes(&abs, FILE_ATTRIBUTE_NORMAL)
                    .map_err(at("clear the attributes of", &abs))?;
                std::fs::remove_file(&abs).map_err(at("remove", &abs))?;
                out.dropped += 1;
            } else {
                revert(&abs)?;
            }
        }
    }
    for d in dirs.iter().rev() {
        revert(d)?;
    }
    if let Ok(Some(p)) = ffi::probe_opt(&top)
        && p.is_placeholder()
    {
        if p.is_unpopulated() {
            populated(&top)?;
        }
        revert(&top)?;
    }
    if ffi::is_our_sync_root(root) {
        // Gone already is fine; a registration left behind is cleared on the next mount.
        let _ = ffi::unregister(root);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_plain_directory_comes_out_unchanged() {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(d.path().join("sub")).unwrap();
        std::fs::write(d.path().join("sub").join("f.txt"), b"kept").unwrap();
        std::fs::write(d.path().join("top.txt"), b"kept too").unwrap();
        let done = unvirtualize(d.path()).unwrap();
        assert_eq!(done, Unvirtualized::default());
        assert_eq!(
            std::fs::read(d.path().join("sub").join("f.txt")).unwrap(),
            b"kept"
        );
        assert_eq!(
            std::fs::read(d.path().join("top.txt")).unwrap(),
            b"kept too"
        );
    }
}
