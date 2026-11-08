//! Shared blob links (`asc.worktree.link = symlink`).
//!
//! A plain checkout in link mode keeps one copy of each unchanged file per machine, in its
//! repository's shared area `<asc home>/shared-blobs/<repo-id>/`, and puts a symbolic link to it
//! in the work tree. The area is content-addressed by git blob id: `ab/cdef…` for a regular file
//! and `ab/cdef….x` for an executable one. A link into a trusted area therefore names the blob it
//! stands for, and every work-tree reader takes such a link as the regular file it points to
//! ([`metadata`], [`link_blob`]); status reads the blob id from the link without opening the file.
//! See [`asc_state::shared_blob`] for what makes a link trusted.
//!
//! Area files are read-only and carry the fixed modification time [`SEALED`]. A write through a
//! link (an editor that saves in place and overrides the read-only bit) changes that time, so
//! status hashes such a file instead of trusting its name, and a checkout never links to it again.
//! An editor that saves by replacing the file replaces the link, which leaves the area untouched.
//!
//! Never a hardlink: a hardlink cannot be told apart from the file and an edit through it would
//! change the shared copy unseen.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use asc_core::id::GitOid;

pub use asc_state::shared_blob::{
    AREA, area_of_target, blob_of_target, link_blob, link_target, metadata, names_area_file,
    path_in, shaped_like_area_file, trust_area,
};

/// The modification time of every sealed area file: 2000-01-01T00:00:00Z.
pub const SEALED: Duration = Duration::from_secs(946_684_800);

/// Whether the area file `meta` describes is still sealed: nothing wrote through a link to it.
pub fn is_sealed(meta: &fs::Metadata) -> bool {
    meta.is_file()
        && meta
            .modified()
            .is_ok_and(|t| t == SystemTime::UNIX_EPOCH + SEALED)
}

/// The blob behind the shared link at `abs`, if the link's area file is sealed: the file then
/// holds that blob and need not be read.
pub fn sealed_blob(abs: &Path) -> Option<(GitOid, bool)> {
    let blob = link_blob(abs)?;
    fs::metadata(abs).ok().filter(is_sealed).map(|_| blob)
}

/// Seals the finished area file `tmp`: the fixed time, then read-only (and executable for an
/// executable blob).
pub fn seal(tmp: &Path, exec: bool) -> io::Result<()> {
    fs::File::options()
        .write(true)
        .open(tmp)?
        .set_modified(SystemTime::UNIX_EPOCH + SEALED)?;
    read_only(tmp, exec)
}

#[cfg(unix)]
fn read_only(path: &Path, exec: bool) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(
        path,
        fs::Permissions::from_mode(if exec { 0o555 } else { 0o444 }),
    )
}

#[cfg(not(unix))]
fn read_only(path: &Path, _exec: bool) -> io::Result<()> {
    let mut perms = fs::metadata(path)?.permissions();
    perms.set_readonly(true);
    fs::set_permissions(path, perms)
}

/// Makes `link` a symbolic link to the area file `target`.
pub fn make_link(target: &Path, link: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(target, link)
    }
    #[cfg(windows)]
    {
        std::os::windows::fs::symlink_file(target, link)
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = (target, link);
        Err(io::ErrorKind::Unsupported.into())
    }
}

/// What [`prune`] did.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Pruned {
    pub files: u64,
    pub bytes: u64,
}

/// Removes the area files under `areas` (a home's `shared-blobs` directory, one area per
/// repository) that no link in `used` names and that are older than `grace` (a checkout may have
/// sealed a file and not yet linked it). With `remove` false it only counts them.
pub fn prune(
    areas: &Path,
    used: &std::collections::HashSet<PathBuf>,
    grace: Duration,
    remove: bool,
) -> io::Result<Pruned> {
    let mut out = Pruned::default();
    let Ok(repos) = fs::read_dir(areas) else {
        return Ok(out);
    };
    let now = SystemTime::now();
    for repo in repos {
        let repo = repo?;
        if !repo.file_type()?.is_dir() {
            continue;
        }
        prune_area(&repo.path(), used, grace, remove, now, &mut out)?;
        if remove {
            let _ = fs::remove_dir(repo.path());
        }
    }
    Ok(out)
}

fn prune_area(
    area: &Path,
    used: &std::collections::HashSet<PathBuf>,
    grace: Duration,
    remove: bool,
    now: SystemTime,
    out: &mut Pruned,
) -> io::Result<()> {
    for fan in fs::read_dir(area)? {
        let fan = fan?;
        if !fan.file_type()?.is_dir() {
            continue;
        }
        for f in fs::read_dir(fan.path())? {
            let f = f?;
            let p = f.path();
            let meta = f.metadata()?;
            if used.contains(&p) || !meta.is_file() {
                continue;
            }
            // A sealed file's time is fixed, so its age is its inode change (unix) or creation
            // time; a file still being written has no seal yet.
            let born = created(&meta).unwrap_or(now);
            if now.duration_since(born).unwrap_or_default() < grace {
                continue;
            }
            if remove {
                writable(&p)?;
                fs::remove_file(&p)?;
            }
            out.files += 1;
            out.bytes += meta.len();
        }
        if remove {
            let _ = fs::remove_dir(fan.path());
        }
    }
    Ok(())
}

/// Adds to `used` the area file of every link under the work tree `root` whose target has the shape
/// of an area file (trusted or not, so nothing a link could name is pruned). The walk never follows
/// a link and skips `.git`; an unreadable directory is an error, so a caller never prunes on a
/// partial view.
pub fn links_under(root: &Path, used: &mut std::collections::HashSet<PathBuf>) -> io::Result<()> {
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for e in fs::read_dir(&dir)? {
            let e = e?;
            let kind = e.file_type()?;
            let p = e.path();
            if kind.is_dir() {
                if e.file_name() != ".git" {
                    stack.push(p);
                }
            } else if kind.is_symlink()
                && let Ok(target) = fs::read_link(&p)
                && shaped_like_area_file(&target)
            {
                used.insert(target);
            }
        }
    }
    Ok(())
}

#[cfg(unix)]
fn created(meta: &fs::Metadata) -> Option<SystemTime> {
    use std::os::unix::fs::MetadataExt;
    let secs = u64::try_from(meta.ctime()).ok()?;
    Some(SystemTime::UNIX_EPOCH + Duration::from_secs(secs))
}

#[cfg(not(unix))]
fn created(meta: &fs::Metadata) -> Option<SystemTime> {
    meta.created().ok()
}

#[cfg(unix)]
fn writable(_: &Path) -> io::Result<()> {
    // Removing needs a writable directory, not a writable file.
    Ok(())
}

#[cfg(not(unix))]
#[allow(clippy::permissions_set_readonly_false)] // Windows only: clears the read-only attribute.
fn writable(path: &Path) -> io::Result<()> {
    let mut perms = fs::metadata(path)?.permissions();
    perms.set_readonly(false);
    fs::set_permissions(path, perms)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn oid(b: u8) -> GitOid {
        GitOid::from_bytes([b; 20])
    }

    /// A trusted `shared-blobs` directory under `dir` and one repository area in it.
    fn areas(dir: &Path) -> (PathBuf, PathBuf) {
        let areas = dir.join(AREA);
        trust_area(&areas);
        let area = areas.join(asc_state::home::RepoId::new().to_string());
        (areas, area)
    }

    #[test]
    fn a_sealed_link_reads_as_its_file_and_an_edit_breaks_the_seal() {
        let d = tempfile::tempdir().unwrap();
        let (_, area) = areas(d.path());
        let target = path_in(&area, oid(7), false);
        fs::create_dir_all(target.parent().unwrap()).unwrap();
        fs::write(&target, b"shared\n").unwrap();
        seal(&target, false).unwrap();
        let link = d.path().join("w.txt");
        if make_link(&target, &link).is_err() {
            // No symlink rights here (Windows without Developer Mode).
            return;
        }
        let meta = metadata(&link).unwrap();
        assert!(meta.is_file());
        assert_eq!(meta.len(), 7);
        assert!(fs::metadata(&target).unwrap().permissions().readonly());
        assert_eq!(sealed_blob(&link), Some((oid(7), false)));

        writable(&target).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&target, fs::Permissions::from_mode(0o644)).unwrap();
        }
        fs::write(&link, b"edited through the link\n").unwrap();
        assert_eq!(link_blob(&link), Some((oid(7), false)));
        assert_eq!(sealed_blob(&link), None);
    }

    #[test]
    fn a_link_into_an_untrusted_area_or_to_a_link_is_not_a_shared_file() {
        let d = tempfile::tempdir().unwrap();
        let fake = d
            .path()
            .join("fake")
            .join(AREA)
            .join("0190a000-0000-7000-8000-000000000000");
        let target = path_in(&fake, oid(5), false);
        fs::create_dir_all(target.parent().unwrap()).unwrap();
        fs::write(
            &target, b"secret
",
        )
        .unwrap();
        let link = d.path().join("w.txt");
        if make_link(&target, &link).is_err() {
            // No symlink rights here (Windows without Developer Mode).
            return;
        }
        assert_eq!(link_blob(&link), None);
        assert!(metadata(&link).unwrap().file_type().is_symlink());

        // A trusted area name whose "file" is itself a link to something else.
        let (_, area) = areas(d.path());
        let hop = path_in(&area, oid(6), false);
        fs::create_dir_all(hop.parent().unwrap()).unwrap();
        make_link(&d.path().join("elsewhere"), &hop).unwrap();
        fs::write(
            d.path().join("elsewhere"),
            b"other
",
        )
        .unwrap();
        let link2 = d.path().join("w2.txt");
        make_link(&hop, &link2).unwrap();
        assert_eq!(link_blob(&link2), None);
        assert!(metadata(&link2).unwrap().file_type().is_symlink());
    }

    #[test]
    fn links_under_finds_shared_links_only() {
        let d = tempfile::tempdir().unwrap();
        let (_, area) = areas(d.path());
        let target = path_in(&area, oid(3), false);
        fs::create_dir_all(target.parent().unwrap()).unwrap();
        fs::write(&target, b"x").unwrap();
        let wt = d.path().join("wt");
        fs::create_dir_all(wt.join("sub")).unwrap();
        fs::create_dir_all(wt.join(".git")).unwrap();
        if make_link(&target, &wt.join("sub").join("a")).is_err() {
            // No symlink rights here (Windows without Developer Mode).
            return;
        }
        make_link(&target, &wt.join(".git").join("skipped")).unwrap();
        make_link(&wt.join("sub").join("a"), &wt.join("other")).unwrap();
        let mut used = std::collections::HashSet::new();
        links_under(&wt, &mut used).unwrap();
        assert_eq!(used, [target].into());
        assert!(links_under(&d.path().join("absent"), &mut used).is_err());
    }

    #[test]
    fn prune_keeps_used_and_young_files() {
        let d = tempfile::tempdir().unwrap();
        let (all, area) = areas(d.path());
        let (used, old) = (path_in(&area, oid(1), false), path_in(&area, oid(2), false));
        for p in [&used, &old] {
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(p, b"x").unwrap();
            seal(p, false).unwrap();
        }
        let keep: std::collections::HashSet<_> = [used.clone()].into();
        let young = prune(&all, &keep, Duration::from_secs(3600), true).unwrap();
        assert_eq!(young, Pruned::default());
        let counted = prune(&all, &keep, Duration::ZERO, false).unwrap();
        assert_eq!(counted, Pruned { files: 1, bytes: 1 });
        assert!(old.exists());
        let done = prune(&all, &keep, Duration::ZERO, true).unwrap();
        assert_eq!(done, Pruned { files: 1, bytes: 1 });
        assert!(used.exists());
        assert!(!old.exists());
        assert_eq!(
            prune(&d.path().join("absent"), &keep, Duration::ZERO, true).unwrap(),
            Pruned::default()
        );
    }
}
