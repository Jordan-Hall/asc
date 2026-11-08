//! Names in the shared blob area (`asc.worktree.link = symlink`): `<home>/shared-blobs/ab/cdef…`
//! for a regular file and `…/ab/cdef….x` for an executable one, keyed by git blob id. Only an
//! absolute link target can name an area file; the working copy never checks out a committed
//! symlink with such a target, so a link like that in a work tree was made by link mode.

use std::path::{Component, Path, PathBuf};

use asc_core::id::GitOid;

pub use crate::home::SHARED_BLOBS as AREA;

/// The suffix of an executable blob's area file.
const EXEC: &str = ".x";

/// The area file for blob `oid` under the area directory `area`.
pub fn path_in(area: &Path, oid: GitOid, exec: bool) -> PathBuf {
    let hex = oid.to_string();
    let (fan, rest) = hex.split_at(2);
    let name = if exec {
        format!("{rest}{EXEC}")
    } else {
        rest.to_owned()
    };
    area.join(fan).join(name)
}

/// The blob, and whether it is executable, that the link target `target` names: `None` unless
/// `target` is an absolute path to a file in a shared area.
pub fn blob_of_target(target: &Path) -> Option<(GitOid, bool)> {
    if !target.is_absolute() {
        return None;
    }
    let mut parts = target.components().rev().map(|c| match c {
        Component::Normal(n) => n.to_str(),
        _ => None,
    });
    let file = parts.next()??;
    let fan = parts.next()??;
    if parts.next()?? != AREA || fan.len() != 2 {
        return None;
    }
    let (rest, exec) = match file.strip_suffix(EXEC) {
        Some(r) => (r, true),
        None => (file, false),
    };
    let oid = format!("{fan}{rest}").parse().ok()?;
    Some((oid, exec))
}

/// Whether the link target text `target` (as committed) would name an area file.
pub fn names_area_file(target: &[u8]) -> bool {
    let text = String::from_utf8_lossy(target);
    #[cfg(windows)]
    let text = text.replace('/', "\\");
    blob_of_target(Path::new(&*text)).is_some()
}

/// The blob the symbolic link at `abs` stands for; `None` if `abs` is not a link into an area.
pub fn link_blob(abs: &Path) -> Option<(GitOid, bool)> {
    let meta = std::fs::symlink_metadata(abs).ok()?;
    if !meta.file_type().is_symlink() {
        return None;
    }
    blob_of_target(&std::fs::read_link(abs).ok()?)
}

/// `symlink_metadata`, except that a link into a shared area reads as the file it points to.
pub fn metadata(abs: &Path) -> std::io::Result<std::fs::Metadata> {
    let meta = std::fs::symlink_metadata(abs)?;
    if meta.file_type().is_symlink() && link_blob(abs).is_some() {
        return std::fs::metadata(abs);
    }
    Ok(meta)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn oid(b: u8) -> GitOid {
        GitOid::from_bytes([b; 20])
    }

    #[test]
    fn area_paths_name_their_blob() {
        let area = std::env::temp_dir().join(AREA);
        for exec in [false, true] {
            let p = path_in(&area, oid(0xab), exec);
            assert_eq!(blob_of_target(&p), Some((oid(0xab), exec)));
            assert!(names_area_file(p.to_string_lossy().as_bytes()));
        }
        let p = path_in(&area, oid(0xab), false);
        assert_eq!(p.parent().unwrap().file_name().unwrap(), "ab");
    }

    #[test]
    fn other_targets_are_not_blobs() {
        let area = std::env::temp_dir().join(AREA);
        let good = path_in(&area, oid(1), false);
        let name = good.file_name().unwrap().to_owned();
        // Relative, outside an area, a bad fan-out, a bad id.
        assert_eq!(blob_of_target(Path::new("shared-blobs/01/0101")), None);
        assert!(!names_area_file(b"../shared-blobs/01/0101"));
        let elsewhere = std::env::temp_dir().join("other").join("01").join(&name);
        assert_eq!(blob_of_target(&elsewhere), None);
        assert_eq!(blob_of_target(&area.join("011").join(&name)), None);
        assert_eq!(blob_of_target(&area.join("01").join("zz")), None);
    }
}
