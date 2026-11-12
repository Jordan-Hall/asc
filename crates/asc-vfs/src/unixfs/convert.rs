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

#[cfg(test)]
mod tests {
    use asc_core::path::RepoPath;

    use super::*;

    fn rp(s: &str) -> RepoPath {
        RepoPath::new(s.as_bytes().to_vec()).unwrap()
    }

    fn dirs(t: &tempfile::TempDir) -> (std::path::PathBuf, std::path::PathBuf) {
        let (dir, root) = (t.path().join("overlay"), t.path().join("root"));
        fs::create_dir_all(&root).unwrap();
        (dir, root)
    }

    #[test]
    fn edits_move_out_and_deletions_are_listed() {
        let t = tempfile::tempdir().unwrap();
        let (dir, root) = dirs(&t);
        let o = Overlay::open(&dir).unwrap();
        o.ensure_parents(&rp("src/lib.rs")).unwrap();
        fs::write(o.root().join("src/lib.rs"), b"edited").unwrap();
        fs::set_permissions(
            o.root().join("src/lib.rs"),
            fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        symlink("lib.rs", o.root().join("src/link")).unwrap();
        o.set_white(&rp("gone")).unwrap();
        o.set_white(&rp("docs/old.md")).unwrap();
        drop(o);

        let done = unvirtualize(&dir, &root).unwrap();

        assert_eq!(
            done.deleted,
            vec!["docs/old.md".to_owned(), "gone".to_owned()]
        );
        assert_eq!(fs::read(root.join("src/lib.rs")).unwrap(), b"edited");
        let mode = fs::metadata(root.join("src/lib.rs")).unwrap().mode() & 0o777;
        assert_eq!(mode, 0o755);
        assert_eq!(
            fs::read_link(root.join("src/link")).unwrap(),
            Path::new("lib.rs")
        );
        assert!(fs::read_dir(dir.join("data")).unwrap().next().is_none());
    }

    #[test]
    fn overlay_entries_replace_files_and_merge_directories() {
        let t = tempfile::tempdir().unwrap();
        let (dir, root) = dirs(&t);
        fs::create_dir_all(root.join("keep")).unwrap();
        fs::write(root.join("keep/old"), b"left").unwrap();
        fs::write(root.join("file"), b"stale").unwrap();
        fs::create_dir(root.join("was_dir")).unwrap();
        let o = Overlay::open(&dir).unwrap();
        o.ensure_parents(&rp("keep/new")).unwrap();
        fs::write(o.root().join("keep/new"), b"new").unwrap();
        fs::write(o.root().join("file"), b"fresh").unwrap();
        fs::write(o.root().join("was_dir"), b"now a file").unwrap();
        drop(o);

        unvirtualize(&dir, &root).unwrap();

        assert_eq!(fs::read(root.join("keep/old")).unwrap(), b"left");
        assert_eq!(fs::read(root.join("keep/new")).unwrap(), b"new");
        assert_eq!(fs::read(root.join("file")).unwrap(), b"fresh");
        assert_eq!(fs::read(root.join("was_dir")).unwrap(), b"now a file");
    }

    #[test]
    fn a_symlinked_directory_in_the_root_is_replaced_not_followed() {
        let t = tempfile::tempdir().unwrap();
        let (dir, root) = dirs(&t);
        let outside = t.path().join("outside");
        fs::create_dir(&outside).unwrap();
        symlink(&outside, root.join("sub")).unwrap();
        let o = Overlay::open(&dir).unwrap();
        o.ensure_parents(&rp("sub/x")).unwrap();
        fs::write(o.root().join("sub/x"), b"x").unwrap();
        drop(o);

        unvirtualize(&dir, &root).unwrap();

        assert!(fs::read_dir(&outside).unwrap().next().is_none());
        assert!(fs::symlink_metadata(root.join("sub")).unwrap().is_dir());
        assert_eq!(fs::read(root.join("sub/x")).unwrap(), b"x");
    }

    #[test]
    fn the_gitfile_name_is_left_for_conversion() {
        let t = tempfile::tempdir().unwrap();
        let (dir, root) = dirs(&t);
        let o = Overlay::open(&dir).unwrap();
        fs::write(o.root().join(".git"), b"gitdir: /elsewhere\n").unwrap();
        drop(o);

        let done = unvirtualize(&dir, &root).unwrap();

        assert_eq!(done.moved, 0);
        assert!(!root.join(".git").exists());
    }

    #[test]
    fn copies_keep_modes_and_links() {
        let t = tempfile::tempdir().unwrap();
        let (src, dst) = (t.path().join("src"), t.path().join("dst"));
        fs::create_dir(&src).unwrap();
        fs::write(src.join("run"), b"#!").unwrap();
        fs::set_permissions(src.join("run"), fs::Permissions::from_mode(0o750)).unwrap();
        symlink("run", src.join("alias")).unwrap();

        copy_entry(&src, &dst, 0).unwrap();

        assert_eq!(fs::metadata(dst.join("run")).unwrap().mode() & 0o777, 0o750);
        assert_eq!(fs::read_link(dst.join("alias")).unwrap(), Path::new("run"));
    }

    #[test]
    fn no_overlay_means_nothing_to_move() {
        let t = tempfile::tempdir().unwrap();
        let done = unvirtualize(&t.path().join("none"), t.path()).unwrap();
        assert_eq!(done, Unvirtualized::default());
    }
}
