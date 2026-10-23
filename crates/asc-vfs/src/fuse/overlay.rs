//! The copy-up overlay (SPEC §10.3): `wc/<ws>/overlay/` holds what the user changed, and records
//! what the user deleted. Layout:
//!
//! - `data/`: real files, directories and symlinks that shadow (or add to) the base tree;
//! - `white/`: one marker per deleted path, named by the hash of the path and holding the path.
//!   A path with a marker has no base entry, neither itself nor anything below it. A path with a
//!   marker *and* an entry in `data/` is a new entry that replaced a deleted one;
//! - `tmp/`: files being written before they are renamed into `data/`;
//! - `hydrate/`: anonymous files holding verified content that is not in the local cache.
//!
//! Every path reaches the disk through [`SafeRoot`], so a symlink in an ancestor is never
//! followed.

use std::fs::{self, File, OpenOptions};
use std::io;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use asc_core::id::FileId;
use asc_core::path::RepoPath;
use asc_wc::SafeRoot;
use asc_wc::safe_path::is_link;

/// Most whiteout markers read back at once.
pub const MAX_WHITEOUTS: usize = 4_000_000;
/// Most entries one overlay directory listing returns.
pub const MAX_CHILDREN: usize = 1_000_000;

/// The overlay of one workspace.
pub struct Overlay {
    data: SafeRoot,
    white: PathBuf,
    tmp: PathBuf,
    hydrate: PathBuf,
    seq: AtomicU64,
}

fn private_dir(p: &Path) -> io::Result<()> {
    match fs::DirBuilder::new().recursive(true).mode(0o700).create(p) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Ok(()),
        Err(e) => Err(e),
    }
}

fn clear_dir(p: &Path) -> io::Result<()> {
    for e in fs::read_dir(p)? {
        let e = e?;
        let _ = fs::remove_file(e.path());
    }
    Ok(())
}

impl Overlay {
    /// Opens (creating) the overlay below `dir`, dropping leftovers of a crashed run.
    pub fn open(dir: &Path) -> io::Result<Self> {
        private_dir(dir)?;
        let o = Self {
            data: SafeRoot::new(dir.join("data")),
            white: dir.join("white"),
            tmp: dir.join("tmp"),
            hydrate: dir.join("hydrate"),
            seq: AtomicU64::new(0),
        };
        for d in [o.data.root(), &o.white, &o.tmp, &o.hydrate] {
            private_dir(d)?;
        }
        clear_dir(&o.tmp)?;
        clear_dir(&o.hydrate)?;
        Ok(o)
    }

    /// The disk path of `p` in `data/`, or `None` when an ancestor is a link or not a directory.
    pub fn resolve(&self, p: &RepoPath) -> Option<PathBuf> {
        self.data.resolve(p)
    }

    /// The entry at `p`, not following a link there. `None` when nothing is there.
    pub fn meta(&self, p: &RepoPath) -> io::Result<Option<fs::Metadata>> {
        let Some(abs) = self.data.resolve(p) else {
            return Ok(None);
        };
        match fs::symlink_metadata(abs) {
            Ok(m) => Ok(Some(m)),
            Err(e)
                if matches!(e.kind(), io::ErrorKind::NotFound) || e.raw_os_error() == Some(20) =>
            {
                Ok(None)
            }
            Err(e) => Err(e),
        }
    }

    /// Makes the directories above `p` exist in `data/` (a copy-up of the ancestors).
    pub fn ensure_parents(&self, p: &RepoPath) -> io::Result<()> {
        let mut dirs = Vec::new();
        let mut cur = p.parent();
        while let Some(d) = cur {
            cur = d.parent();
            dirs.push(d);
        }
        for d in dirs.into_iter().rev() {
            let abs = self
                .data
                .resolve(&d)
                .ok_or_else(|| io::Error::from(io::ErrorKind::PermissionDenied))?;
            match fs::symlink_metadata(&abs) {
                Ok(m) if m.is_dir() && !is_link(&m) => {}
                Ok(_) => return Err(io::Error::from_raw_os_error(20)),
                Err(e) if e.kind() == io::ErrorKind::NotFound => {
                    fs::DirBuilder::new().mode(0o755).create(&abs)?;
                }
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }

    /// Makes `p` a real directory in `data/`.
    pub fn make_dir(&self, p: &RepoPath, mode: u32) -> io::Result<()> {
        self.ensure_parents(p)?;
        let abs = self.abs(p)?;
        fs::DirBuilder::new().mode(mode & 0o7777).create(abs)
    }

    fn abs(&self, p: &RepoPath) -> io::Result<PathBuf> {
        self.data
            .resolve(p)
            .ok_or_else(|| io::Error::from(io::ErrorKind::PermissionDenied))
    }

    /// Creates a new file at `p` (failing if it exists) and opens it read-write.
    pub fn create_file(&self, p: &RepoPath, mode: u32, append: bool) -> io::Result<File> {
        self.ensure_parents(p)?;
        OpenOptions::new()
            .read(true)
            .write(true)
            .append(append)
            .create_new(true)
            .mode(mode & 0o7777)
            .custom_flags(libc_nofollow())
            .open(self.abs(p)?)
    }

    /// Opens the existing overlay file at `p` without following a link at the end.
    pub fn open_file(
        &self,
        p: &RepoPath,
        read: bool,
        write: bool,
        append: bool,
        truncate: bool,
    ) -> io::Result<File> {
        OpenOptions::new()
            .read(read)
            .write(write)
            .append(append)
            .truncate(truncate)
            .custom_flags(libc_nofollow())
            .open(self.abs(p)?)
    }

    /// Creates a symlink at `p`.
    pub fn make_symlink(&self, p: &RepoPath, target: &Path) -> io::Result<()> {
        self.ensure_parents(p)?;
        std::os::unix::fs::symlink(target, self.abs(p)?)
    }

    /// Removes the file or symlink at `p`.
    pub fn remove_file(&self, p: &RepoPath) -> io::Result<()> {
        fs::remove_file(self.abs(p)?)
    }

    /// Removes the (empty) directory at `p`.
    pub fn remove_dir(&self, p: &RepoPath) -> io::Result<()> {
        fs::remove_dir(self.abs(p)?)?;
        self.data.forget(p);
        Ok(())
    }

    /// Removes whatever is at `p`, with everything below it. Used when a switch overwrites it.
    pub fn remove_tree(&self, p: &RepoPath) -> io::Result<()> {
        let Some(abs) = self.data.resolve(p) else {
            return Ok(());
        };
        match fs::symlink_metadata(&abs) {
            Ok(m) if m.is_dir() && !is_link(&m) => fs::remove_dir_all(&abs)?,
            Ok(_) => fs::remove_file(&abs)?,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
        self.data.forget(p);
        Ok(())
    }

    /// Renames `from` to `to` inside `data/`.
    pub fn rename(&self, from: &RepoPath, to: &RepoPath) -> io::Result<()> {
        self.ensure_parents(to)?;
        fs::rename(self.abs(from)?, self.abs(to)?)?;
        self.data.forget(from);
        self.data.forget(to);
        Ok(())
    }

    /// The target of the symlink at `p`.
    pub fn read_link(&self, p: &RepoPath) -> io::Result<PathBuf> {
        fs::read_link(self.abs(p)?)
    }

    /// The names (and whether each is a directory) the overlay holds directly below `dir`.
    pub fn children(&self, dir: Option<&RepoPath>) -> io::Result<Vec<(Vec<u8>, bool)>> {
        use std::os::unix::ffi::OsStrExt;
        let abs = match dir {
            Some(d) => match self.data.resolve(d) {
                Some(a) => a,
                None => return Ok(Vec::new()),
            },
            None => self.data.root().to_owned(),
        };
        let rd = match fs::read_dir(&abs) {
            Ok(r) => r,
            Err(e) if e.kind() == io::ErrorKind::NotFound || e.raw_os_error() == Some(20) => {
                return Ok(Vec::new());
            }
            Err(e) => return Err(e),
        };
        let mut out = Vec::new();
        for e in rd {
            let e = e?;
            if out.len() >= MAX_CHILDREN {
                break;
            }
            let ft = e.file_type()?;
            out.push((e.file_name().as_bytes().to_vec(), ft.is_dir()));
        }
        Ok(out)
    }

    fn white_file(&self, p: &RepoPath) -> PathBuf {
        self.white.join(FileId::of(p.as_bytes()).to_string())
    }

    /// Whether `p` itself carries a deletion marker.
    pub fn is_white(&self, p: &RepoPath) -> bool {
        fs::symlink_metadata(self.white_file(p)).is_ok()
    }

    /// Whether `p` or an ancestor carries a deletion marker, which hides the base entry.
    pub fn hides_base(&self, p: &RepoPath) -> bool {
        let mut cur = Some(p.clone());
        while let Some(c) = cur {
            if self.is_white(&c) {
                return true;
            }
            cur = c.parent();
        }
        false
    }

    /// Records the deletion of `p` (idempotent).
    pub fn set_white(&self, p: &RepoPath) -> io::Result<()> {
        let f = self.white_file(p);
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&f)
        {
            Ok(mut w) => {
                use std::io::Write;
                w.write_all(p.as_bytes())
            }
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Ok(()),
            Err(e) => Err(e),
        }
    }

    /// Drops the marker of `p`.
    pub fn clear_white(&self, p: &RepoPath) -> io::Result<()> {
        match fs::remove_file(self.white_file(p)) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        }
    }

    /// The paths with a deletion marker, at most [`MAX_WHITEOUTS`]; markers that do not hold a
    /// valid path are ignored.
    pub fn whiteouts(&self) -> io::Result<Vec<RepoPath>> {
        let mut out = Vec::new();
        for e in fs::read_dir(&self.white)? {
            let e = e?;
            if out.len() >= MAX_WHITEOUTS {
                break;
            }
            let Ok(bytes) = fs::read(e.path()) else {
                continue;
            };
            if let Ok(p) = RepoPath::new(bytes)
                && self.white_file(&p) == e.path()
            {
                out.push(p);
            }
        }
        Ok(out)
    }

    /// A new file in `tmp/`, to be renamed into place by [`Self::install`].
    pub fn new_tmp(&self) -> io::Result<(File, PathBuf)> {
        let n = self.seq.fetch_add(1, Ordering::Relaxed);
        let path = self.tmp.join(format!("{}-{n}", std::process::id()));
        let f = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)?;
        Ok((f, path))
    }

    /// Moves a finished `tmp/` file to `p`, replacing a file or symlink there.
    pub fn install(&self, tmp: &Path, p: &RepoPath, mode: u32) -> io::Result<()> {
        use std::os::unix::fs::PermissionsExt;
        self.ensure_parents(p)?;
        fs::set_permissions(tmp, fs::Permissions::from_mode(mode & 0o7777))?;
        fs::rename(tmp, self.abs(p)?)
    }

    /// An anonymous file for verified content (already unlinked: it vanishes when closed).
    pub fn anonymous(&self) -> io::Result<File> {
        let n = self.seq.fetch_add(1, Ordering::Relaxed);
        let path = self.hydrate.join(format!("{}-{n}", std::process::id()));
        let f = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)?;
        fs::remove_file(&path)?;
        Ok(f)
    }

    /// The bytes of regular files held in `data/`.
    pub fn bytes(&self) -> io::Result<u64> {
        fn walk(dir: &Path, depth: usize) -> io::Result<u64> {
            if depth > asc_core::path::MAX_TREE_DEPTH {
                return Ok(0);
            }
            let mut n = 0u64;
            for e in fs::read_dir(dir)? {
                let e = e?;
                let ft = e.file_type()?;
                if ft.is_dir() {
                    n = n.saturating_add(walk(&e.path(), depth + 1)?);
                } else if ft.is_file() {
                    n = n.saturating_add(e.metadata()?.len());
                }
            }
            Ok(n)
        }
        walk(self.data.root(), 0)
    }

    /// The directory the overlay lives on, for `statfs`.
    pub fn root(&self) -> &Path {
        self.data.root()
    }
}

/// `O_NOFOLLOW`: opening a symlink at the last component fails instead of following it.
fn libc_nofollow() -> i32 {
    rustix::fs::OFlags::NOFOLLOW.bits() as i32
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};

    use super::*;

    fn rp(s: &str) -> RepoPath {
        RepoPath::new(s).unwrap()
    }

    #[test]
    fn files_dirs_and_parents() {
        let d = tempfile::tempdir().unwrap();
        let o = Overlay::open(&d.path().join("ov")).unwrap();
        let mut f = o.create_file(&rp("a/b/c"), 0o644, false).unwrap();
        f.write_all(b"hi").unwrap();
        assert!(o.meta(&rp("a/b")).unwrap().unwrap().is_dir());
        assert_eq!(o.meta(&rp("a/b/c")).unwrap().unwrap().len(), 2);
        assert!(o.meta(&rp("a/nope")).unwrap().is_none());
        assert!(o.meta(&rp("a/b/c/d")).unwrap().is_none());
        assert!(o.create_file(&rp("a/b/c"), 0o644, false).is_err());
        assert_eq!(o.bytes().unwrap(), 2);
        let kids = o.children(Some(&rp("a/b"))).unwrap();
        assert_eq!(kids, vec![(b"c".to_vec(), false)]);
        let mut s = String::new();
        o.open_file(&rp("a/b/c"), true, false, false, false)
            .unwrap()
            .read_to_string(&mut s)
            .unwrap();
        assert_eq!(s, "hi");
    }

    #[test]
    fn whiteouts_hide_descendants_and_survive_reopen() {
        let d = tempfile::tempdir().unwrap();
        let dir = d.path().join("ov");
        let o = Overlay::open(&dir).unwrap();
        o.set_white(&rp("x/y")).unwrap();
        o.set_white(&rp("x/y")).unwrap();
        assert!(o.hides_base(&rp("x/y")));
        assert!(o.hides_base(&rp("x/y/z")));
        assert!(!o.hides_base(&rp("x")));
        assert!(!o.hides_base(&rp("x/yy")));
        drop(o);
        let o = Overlay::open(&dir).unwrap();
        assert_eq!(o.whiteouts().unwrap(), vec![rp("x/y")]);
        o.clear_white(&rp("x/y")).unwrap();
        assert!(!o.hides_base(&rp("x/y")));
    }

    #[test]
    fn a_symlinked_ancestor_is_never_followed() {
        let d = tempfile::tempdir().unwrap();
        let outside = d.path().join("outside");
        fs::create_dir(&outside).unwrap();
        let o = Overlay::open(&d.path().join("ov")).unwrap();
        o.make_symlink(&rp("l"), &outside).unwrap();
        assert!(o.create_file(&rp("l/x"), 0o644, false).is_err());
        assert!(o.make_dir(&rp("l/d"), 0o755).is_err());
        assert!(o.meta(&rp("l/x")).unwrap().is_none());
        assert_eq!(fs::read_dir(&outside).unwrap().count(), 0);
        // A link at the last component is not followed by open_file either.
        fs::write(outside.join("secret"), b"s").unwrap();
        o.make_symlink(&rp("s"), &outside.join("secret")).unwrap();
        assert!(o.open_file(&rp("s"), true, false, false, false).is_err());
    }

    #[test]
    fn install_replaces_and_rename_moves() {
        let d = tempfile::tempdir().unwrap();
        let o = Overlay::open(&d.path().join("ov")).unwrap();
        let (mut f, tmp) = o.new_tmp().unwrap();
        f.write_all(b"new").unwrap();
        o.install(&tmp, &rp("p/q"), 0o755).unwrap();
        o.rename(&rp("p/q"), &rp("r/s")).unwrap();
        assert!(o.meta(&rp("p/q")).unwrap().is_none());
        assert_eq!(o.meta(&rp("r/s")).unwrap().unwrap().len(), 3);
        o.remove_tree(&rp("r")).unwrap();
        assert!(o.meta(&rp("r")).unwrap().is_none());
        assert_eq!(o.anonymous().unwrap().metadata().unwrap().len(), 0);
    }
}
