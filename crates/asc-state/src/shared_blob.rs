//! Names in the shared blob areas (`asc.worktree.link = symlink`). Each repository has its own
//! area, `<home>/shared-blobs/<repo-id>/`, holding `ab/cdef…` for a regular file and `ab/cdef….x`
//! for an executable one, keyed by git blob id.
//!
//! A link is taken as the file it points to only when all of these hold:
//!
//! * its target is absolute and has the shape `<home>/shared-blobs/<repo-id>/<fan>/<file>`;
//! * `<home>` is a home this process opened ([`crate::home::AscHome::open`] trusts its area), so a
//!   look-alike directory elsewhere is never trusted;
//! * the target itself is a regular file, not another link.
//!
//! The working copy never checks out a committed symlink whose target has the area shape
//! ([`names_area_file`]), so a link like that in a work tree was made by link mode. A reader that
//! must not see another repository's files (the MCP path gate) also checks the repository part
//! with [`area_of_target`].

use std::path::{Component, Path, PathBuf};
use std::sync::RwLock;

use asc_core::id::GitOid;

pub use crate::home::SHARED_BLOBS as AREA;

/// The suffix of an executable blob's area file.
const EXEC: &str = ".x";

/// The `shared-blobs` directories of the homes this process opened.
static TRUSTED: RwLock<Vec<PathBuf>> = RwLock::new(Vec::new());

/// Trusts the `shared-blobs` directory `areas` (a home's [`AREA`] directory): links into it read
/// as their files. [`crate::home::AscHome::open`] calls this for every home it opens.
pub fn trust_area(areas: &Path) {
    let mut held = TRUSTED
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if !held.iter().any(|p| p == areas) {
        held.push(areas.to_path_buf());
    }
}

fn trusted(areas: &Path) -> bool {
    TRUSTED
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .iter()
        .any(|p| p == areas)
}

/// The area file for blob `oid` under the repository area `area` (`<home>/shared-blobs/<repo>`).
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

/// The shape check alone: the blob, whether it is executable, and the repository area, if
/// `target` is an absolute path shaped `…/shared-blobs/<repo-id>/<fan>/<file>`.
fn parse(target: &Path) -> Option<(GitOid, bool, &Path)> {
    if !target.is_absolute() {
        return None;
    }
    let mut parts = target.components().rev().map(|c| match c {
        Component::Normal(n) => n.to_str(),
        _ => None,
    });
    let file = parts.next()??;
    let fan = parts.next()??;
    let repo = parts.next()??;
    if parts.next()?? != AREA || fan.len() != 2 || uuid::Uuid::parse_str(repo).is_err() {
        return None;
    }
    let (rest, exec) = match file.strip_suffix(EXEC) {
        Some(r) => (r, true),
        None => (file, false),
    };
    let oid = format!("{fan}{rest}").parse().ok()?;
    Some((oid, exec, target.parent()?.parent()?))
}

/// The blob, and whether it is executable, that the link target `target` names: `None` unless
/// `target` is an absolute path to a file in a repository area of a trusted home.
pub fn blob_of_target(target: &Path) -> Option<(GitOid, bool)> {
    area_of_target(target).map(|(oid, exec, _)| (oid, exec))
}

/// [`blob_of_target`] and the repository area (`<home>/shared-blobs/<repo-id>`) the target is in.
pub fn area_of_target(target: &Path) -> Option<(GitOid, bool, &Path)> {
    let (oid, exec, area) = parse(target)?;
    trusted(area.parent()?).then_some((oid, exec, area))
}

/// Whether `target` has the shape of an area file, trusted or not. A caller that must keep every
/// file a link could name (`asc gc`) counts links by shape.
pub fn shaped_like_area_file(target: &Path) -> bool {
    parse(target).is_some()
}

/// Whether the link target text `target` (as committed) would name an area file. Only the shape
/// counts: a committed link must not point into any area, trusted or not.
pub fn names_area_file(target: &[u8]) -> bool {
    let text = String::from_utf8_lossy(target);
    #[cfg(windows)]
    let text = text.replace('/', "\\");
    shaped_like_area_file(Path::new(&*text))
}

/// The target of the shared link at `abs`: `None` if `abs` is not a link into a trusted area or
/// the target is not a regular file.
pub fn link_target(abs: &Path) -> Option<PathBuf> {
    let meta = std::fs::symlink_metadata(abs).ok()?;
    if !meta.file_type().is_symlink() {
        return None;
    }
    let target = std::fs::read_link(abs).ok()?;
    blob_of_target(&target)?;
    std::fs::symlink_metadata(&target)
        .is_ok_and(|m| m.file_type().is_file())
        .then_some(target)
}

/// The blob the symbolic link at `abs` stands for; `None` if `abs` is not a link into a trusted
/// area or its target is not a regular file.
pub fn link_blob(abs: &Path) -> Option<(GitOid, bool)> {
    blob_of_target(&link_target(abs)?)
}

/// `symlink_metadata`, except that a shared link ([`link_blob`]) reads as the file it points to.
pub fn metadata(abs: &Path) -> std::io::Result<std::fs::Metadata> {
    let meta = std::fs::symlink_metadata(abs)?;
    if meta.file_type().is_symlink()
        && let Some(target) = link_target(abs)
    {
        return std::fs::symlink_metadata(target);
    }
    Ok(meta)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn oid(b: u8) -> GitOid {
        GitOid::from_bytes([b; 20])
    }

    fn repo() -> String {
        uuid::Uuid::now_v7().to_string()
    }

    #[test]
    fn area_paths_name_their_blob() {
        let areas = std::env::temp_dir().join("asc-trusted").join(AREA);
        trust_area(&areas);
        let area = areas.join(repo());
        for exec in [false, true] {
            let p = path_in(&area, oid(0xab), exec);
            assert_eq!(blob_of_target(&p), Some((oid(0xab), exec)));
            assert_eq!(area_of_target(&p).unwrap().2, area);
            assert!(names_area_file(p.to_string_lossy().as_bytes()));
        }
        let p = path_in(&area, oid(0xab), false);
        assert_eq!(p.parent().unwrap().file_name().unwrap(), "ab");
    }

    #[test]
    fn other_targets_are_not_blobs() {
        let areas = std::env::temp_dir().join("asc-trusted-2").join(AREA);
        trust_area(&areas);
        let good = path_in(&areas.join(repo()), oid(1), false);
        let name = good.file_name().unwrap().to_owned();
        // Relative, outside an area, a bad fan-out, a bad id, no repository part.
        assert_eq!(blob_of_target(Path::new("shared-blobs/r/01/0101")), None);
        assert!(!names_area_file(b"../shared-blobs/r/01/0101"));
        let elsewhere = std::env::temp_dir().join("other").join("01").join(&name);
        assert_eq!(blob_of_target(&elsewhere), None);
        let r = areas.join(repo());
        assert_eq!(blob_of_target(&r.join("011").join(&name)), None);
        assert_eq!(blob_of_target(&r.join("01").join("zz")), None);
        assert_eq!(blob_of_target(&areas.join("01").join(&name)), None);
        assert_eq!(
            blob_of_target(&areas.join("x").join("01").join(&name)),
            None
        );
    }

    #[test]
    fn an_area_of_a_home_never_opened_is_not_trusted() {
        let fake = std::env::temp_dir()
            .join("asc-fake")
            .join(AREA)
            .join(repo());
        let p = path_in(&fake, oid(4), false);
        assert_eq!(blob_of_target(&p), None);
        // A committed link with that shape is still refused.
        assert!(names_area_file(p.to_string_lossy().as_bytes()));
        assert!(shaped_like_area_file(&p));
    }
}
