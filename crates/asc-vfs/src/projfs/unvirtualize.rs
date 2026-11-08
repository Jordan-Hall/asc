//! Turning a virtualization root back into a plain directory once its provider has stopped
//! (`worktree convert --plain`). Nothing here asks the provider: with it gone, any open of a
//! placeholder fails with `ERROR_FILE_SYSTEM_VIRTUALIZATION_UNAVAILABLE` (os error 369), so the
//! walk lists only what is physically on disk and acts on the reparse tag of each entry:
//!
//! * a ProjFS placeholder directory loses its reparse point and is walked;
//! * a ProjFS placeholder file (empty, hydrated or with dirty metadata: its content is the
//!   tracked blob in every case) loses its reparse point and is removed, to be written again from
//!   the object store;
//! * a tombstone is the user's deletion: it is removed and its path reported, so the deletion
//!   survives the conversion;
//! * a full file or directory (the user's edits and new files) is kept; other reparse points
//!   (symbolic links, junctions) are neither changed nor entered.
//!
//! The root loses its own reparse point last, so a walk that stops part way leaves a root the
//! provider can still mount.

use std::io;
use std::path::{Path, PathBuf};

use super::ffi::{
    self, FILE_ATTRIBUTE_NORMAL, IO_REPARSE_TAG_PROJFS, IO_REPARSE_TAG_PROJFS_TOMBSTONE,
};

/// `ERROR_NOT_A_REPARSE_POINT`.
const NOT_A_REPARSE_POINT: i32 = 4390;
/// `ERROR_REPARSE_TAG_MISMATCH`.
const TAG_MISMATCH: i32 = 4394;

/// What the walk did, with the deleted items as `/`-separated root-relative paths.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Unvirtualized {
    /// Items the user deleted while the root was virtual (tombstones).
    pub deleted: Vec<String>,
    /// Placeholder files removed, to be written again.
    pub dropped: usize,
}

/// The `\\?\` form of an absolute path, so long paths work.
fn verbatim(p: &Path) -> PathBuf {
    let s = p.as_os_str().to_string_lossy();
    if s.starts_with(r"\\?\") {
        p.to_path_buf()
    } else if let Some(unc) = s.strip_prefix(r"\\") {
        PathBuf::from(format!(r"\\?\UNC\{unc}"))
    } else {
        PathBuf::from(format!(r"\\?\{s}"))
    }
}

/// Clears the attributes (read-only included) of `w` and removes the file or empty directory.
fn remove(w: &[u16], p: &Path, dir: bool) -> io::Result<()> {
    ffi::set_attributes(w, FILE_ATTRIBUTE_NORMAL)?;
    if dir {
        std::fs::remove_dir_all(p)
    } else {
        std::fs::remove_file(p)
    }
}

/// Unvirtualizes the root at the absolute `root`; see the module comment. A root that is not
/// (or no longer) a virtualization root is walked the same way and comes out unchanged.
pub fn unvirtualize(root: &Path) -> io::Result<Unvirtualized> {
    let root = verbatim(root);
    let mut out = Unvirtualized::default();
    let mut stack: Vec<(PathBuf, String)> = vec![(root.clone(), String::new())];
    while let Some((dir, rel)) = stack.pop() {
        let (entries, _) = ffi::read_dir_on_disk(&ffi::path_to_wide(&dir), usize::MAX)?;
        for e in entries {
            let abs = dir.join(&e.name);
            let child = if rel.is_empty() {
                e.name.clone()
            } else {
                format!("{rel}/{}", e.name)
            };
            let w = ffi::path_to_wide(&abs);
            match e.tag {
                IO_REPARSE_TAG_PROJFS_TOMBSTONE => {
                    ffi::delete_reparse_point(&w, e.tag)?;
                    remove(&w, &abs, e.is_dir())?;
                    out.deleted.push(child);
                }
                IO_REPARSE_TAG_PROJFS => {
                    ffi::delete_reparse_point(&w, e.tag)?;
                    if e.is_dir() {
                        stack.push((abs, child));
                    } else {
                        remove(&w, &abs, false)?;
                        out.dropped += 1;
                    }
                }
                0 if e.is_dir() => stack.push((abs, child)),
                _ => {}
            }
        }
    }
    match ffi::delete_reparse_point(&ffi::path_to_wide(&root), IO_REPARSE_TAG_PROJFS) {
        Err(e) if matches!(e.raw_os_error(), Some(NOT_A_REPARSE_POINT | TAG_MISMATCH)) => {}
        r => r?,
    }
    out.deleted.sort();
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verbatim_paths() {
        assert_eq!(verbatim(Path::new(r"C:\w\x")), PathBuf::from(r"\\?\C:\w\x"));
        assert_eq!(
            verbatim(Path::new(r"\\srv\share\w")),
            PathBuf::from(r"\\?\UNC\srv\share\w")
        );
        assert_eq!(verbatim(Path::new(r"\\?\C:\w")), PathBuf::from(r"\\?\C:\w"));
    }

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
