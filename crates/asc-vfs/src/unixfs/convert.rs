//! `worktree convert --plain` for a stopped Unix mount (SPEC §9.1): the mount point is an empty
//! directory again, so what the user changed moves out of the overlay into it, and the deletions
//! are reported for conversion to keep. Everything else is written from the object store later.
//!
//! The overlay's names were checked when they were created. On the work tree side an entry is
//! examined without following a symlink, so a link left at the mount point is replaced, never
//! followed out of the tree.

use std::fs;
use std::io;
use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
use std::path::Path;

use asc_core::path::MAX_TREE_DEPTH;

use super::overlay::Overlay;

/// What turning a stopped mount back into a plain directory did.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Unvirtualized {
    /// The paths the user deleted while the tree was mounted, `/`-separated and sorted.
    pub deleted: Vec<String>,
    /// Top-level and merged entries moved from the overlay into the work tree.
    pub moved: usize,
}

/// The name the mount serves for the workspace's gitfile; conversion writes the real one.
const DOT_GIT: &str = ".git";

/// Moves the overlay below `dir` (`wc/<ws>/overlay`) into the work tree `root`, which must not
/// be mounted, and lists the deletion markers. An overlay entry replaces whatever non-directory
/// the work tree holds at its name; directories merge. The overlay is left empty but in place:
/// the caller removes it once conversion has finished.
pub fn unvirtualize(dir: &Path, root: &Path) -> io::Result<Unvirtualized> {
    if !fs::symlink_metadata(root)?.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::NotADirectory,
            format!("{} is not a directory", root.display()),
        ));
    }
    if !dir.join("data").is_dir() {
        return Ok(Unvirtualized::default());
    }
    let overlay = Overlay::open(dir)?;
    let mut deleted: Vec<String> = overlay
        .whiteouts()?
        .into_iter()
        .map(|p| String::from_utf8_lossy(p.as_bytes()).into_owned())
        .collect();
    deleted.sort();
    deleted.dedup();
    let mut moved = 0;
    for e in fs::read_dir(overlay.root())? {
        let e = e?;
        if e.file_name() == DOT_GIT {
            continue;
        }
        moved += move_entry(&e.path(), &root.join(e.file_name()), 0)?;
    }
    Ok(Unvirtualized { deleted, moved })
}

fn too_deep(p: &Path) -> io::Error {
    io::Error::other(format!("{} is nested too deeply", p.display()))
}

/// Moves `src` to `dst`: a rename when both are on one file system, a copy and removal when
/// not. A directory at `dst` merges with a directory at `src`; anything else at `dst` is
/// replaced.
fn move_entry(src: &Path, dst: &Path, depth: usize) -> io::Result<usize> {
    if depth > MAX_TREE_DEPTH {
        return Err(too_deep(src));
    }
    let from = fs::symlink_metadata(src)?;
    match fs::symlink_metadata(dst) {
        Ok(to) if to.is_dir() && from.is_dir() => {
            let mut n = 0;
            for e in fs::read_dir(src)? {
                let e = e?;
                n += move_entry(&e.path(), &dst.join(e.file_name()), depth + 1)?;
            }
            fs::set_permissions(dst, fs::Permissions::from_mode(from.mode() & 0o7777))?;
            fs::remove_dir(src)?;
            return Ok(n.max(1));
        }
        Ok(to) if to.is_dir() => fs::remove_dir_all(dst)?,
        Ok(_) => fs::remove_file(dst)?,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    match fs::rename(src, dst) {
        Ok(()) => Ok(1),
        Err(e) if e.kind() == io::ErrorKind::CrossesDevices => {
            copy_entry(src, dst, depth)?;
            if from.is_dir() {
                fs::remove_dir_all(src)?;
            } else {
                fs::remove_file(src)?;
            }
            Ok(1)
        }
        Err(e) => Err(e),
    }
}

/// Copies `src` to the absent `dst`, keeping modes and symlink targets.
fn copy_entry(src: &Path, dst: &Path, depth: usize) -> io::Result<()> {
    if depth > MAX_TREE_DEPTH {
        return Err(too_deep(src));
    }
    let meta = fs::symlink_metadata(src)?;
    let ft = meta.file_type();
    if ft.is_symlink() {
        symlink(fs::read_link(src)?, dst)
    } else if ft.is_dir() {
        fs::create_dir(dst)?;
        for e in fs::read_dir(src)? {
            let e = e?;
            copy_entry(&e.path(), &dst.join(e.file_name()), depth + 1)?;
        }
        fs::set_permissions(dst, fs::Permissions::from_mode(meta.mode() & 0o7777))
    } else if ft.is_file() {
        fs::copy(src, dst)?;
        fs::set_permissions(dst, fs::Permissions::from_mode(meta.mode() & 0o7777))
    } else {
        // Sockets, fifos and devices are never created through the mount.
        Ok(())
    }
}

