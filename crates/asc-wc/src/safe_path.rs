//! Path-safety rules for materialising a tree below a work tree root (SPEC §2.8, §10.4):
//! the file system path of a repository path is always strictly below the root, no ancestor
//! directory is a symlink or reparse point, and on case-insensitive systems siblings that
//! fold to the same name are refused. Hardening in the spirit of git's fixes for
//! CVE-2014-9390 and CVE-2021-21300.

use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};

use asc_core::path::{RepoPath, casefold};

/// The file system path of `path` below `root`, or `None` when it cannot be represented
/// faithfully: a component that is not a plain name on this OS (prefix, root, `..`, `.`), a
/// name that is not valid UTF-8 or not `windows_valid` on Windows.
pub fn try_fs_path(root: &Path, path: &RepoPath) -> Option<PathBuf> {
    let mut rel = PathBuf::new();
    for comp in path.components() {
        let name = os_name(comp)?;
        #[cfg(windows)]
        if !asc_core::path::Name::new(comp).is_ok_and(|n| asc_core::path::windows_valid(&n)) {
            return None;
        }
        let mut parts = Path::new(&name).components();
        match (parts.next(), parts.next()) {
            (Some(Component::Normal(n)), None) if n == name.as_os_str() => rel.push(n),
            _ => return None,
        }
    }
    Some(root.join(rel))
}

#[cfg(unix)]
fn os_name(b: &[u8]) -> Option<OsString> {
    use std::os::unix::ffi::OsStrExt;
    Some(std::ffi::OsStr::from_bytes(b).to_owned())
}

#[cfg(not(unix))]
fn os_name(b: &[u8]) -> Option<OsString> {
    // Lossy decoding would silently rename the entry.
    std::str::from_utf8(b).ok().map(OsString::from)
}

/// Whether `meta` describes a symlink or (Windows) any reparse point such as a junction.
pub fn is_link(meta: &fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        meta.file_type().is_symlink() || meta.file_attributes() & 0x400 != 0
    }
    #[cfg(not(windows))]
    {
        meta.file_type().is_symlink()
    }
}

/// Removes a symlink or file without following it; Windows directory links need `remove_dir`.
pub fn remove_link(path: &Path) -> io::Result<()> {
    fs::remove_file(path).or_else(|e| fs::remove_dir(path).map_err(|_| e))
}

/// The paths that collide under case folding with an earlier path (in byte order), counting
/// shared directory prefixes. Only meaningful on case-insensitive file systems.
pub fn case_collisions<'a>(paths: impl IntoIterator<Item = &'a RepoPath>) -> HashSet<RepoPath> {
    let mut sorted: Vec<&RepoPath> = paths.into_iter().collect();
    sorted.sort();
    let mut seen: HashMap<Vec<u8>, Vec<u8>> = HashMap::new();
    let mut out = HashSet::new();
    for p in sorted {
        let bytes = p.as_bytes();
        let prefixes: Vec<&[u8]> = bytes
            .iter()
            .enumerate()
            .filter(|(_, c)| **c == b'/')
            .map(|(i, _)| &bytes[..i])
            .chain([bytes])
            .collect();
        let clash = prefixes.iter().any(|pre| {
            seen.get(&casefold(pre))
                .is_some_and(|o| o.as_slice() != *pre)
        });
        if clash {
            out.insert(p.clone());
        } else {
            for pre in prefixes {
                seen.insert(casefold(pre), pre.to_vec());
            }
        }
    }
    out
}

fn refused(path: &Path) -> io::Error {
    io::Error::new(
        io::ErrorKind::PermissionDenied,
        format!("refusing to use symlinked directory {}", path.display()),
    )
}

fn unrepresentable(path: &RepoPath) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        format!("path {path} cannot be represented on this file system"),
    )
}

/// Directories below the root already verified to be real directories during one checkout,
/// so each ancestor is examined once rather than once per file.
#[derive(Default)]
pub struct Verified {
    dirs: HashSet<RepoPath>,
}

impl Verified {
    /// Ancestors of `path` not yet verified, outermost first.
    fn pending(&self, path: &RepoPath) -> Vec<RepoPath> {
        let mut out = Vec::new();
        let mut cur = path.parent();
        while let Some(dir) = cur {
            if self.dirs.contains(&dir) {
                break;
            }
            cur = dir.parent();
            out.push(dir);
        }
        out.reverse();
        out
    }

    /// Makes every ancestor of `path` a real directory. A symlink or reparse point in the way
    /// is an error, or with `force` is removed itself (never followed) and replaced. With
    /// `force` a file in the way is removed too.
    pub fn ensure_parent(&mut self, root: &Path, path: &RepoPath, force: bool) -> io::Result<()> {
        for dir in self.pending(path) {
            let abs = try_fs_path(root, &dir).ok_or_else(|| unrepresentable(&dir))?;
            match fs::symlink_metadata(&abs) {
                Ok(m) if is_link(&m) => {
                    if !force {
                        return Err(refused(&abs));
                    }
                    remove_link(&abs)?;
                    fs::create_dir(&abs)?;
                }
                Ok(m) if m.is_dir() => {}
                Ok(_) if force => {
                    fs::remove_file(&abs)?;
                    fs::create_dir(&abs)?;
                }
                Ok(_) => fs::create_dir(&abs)?,
                Err(e) if e.kind() == io::ErrorKind::NotFound => fs::create_dir(&abs)?,
                Err(e) => return Err(e),
            }
            self.dirs.insert(dir);
        }
        Ok(())
    }

    /// Whether every ancestor of `path` exists as a real directory; false when one is missing,
    /// not a directory or a symlink (a delete must not go through it).
    pub fn check_parent(&mut self, root: &Path, path: &RepoPath) -> io::Result<bool> {
        for dir in self.pending(path) {
            let Some(abs) = try_fs_path(root, &dir) else {
                return Ok(false);
            };
            match fs::symlink_metadata(&abs) {
                Ok(m) if !is_link(&m) && m.is_dir() => {
                    self.dirs.insert(dir);
                }
                Ok(_) => return Ok(false),
                Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(false),
                Err(e) => return Err(e),
            }
        }
        Ok(true)
    }

    /// Forgets `dir` and everything below it (it was removed or replaced).
    pub fn forget(&mut self, dir: &RepoPath) {
        self.dirs.retain(|d| d != dir && !d.starts_with_dir(dir));
    }
}

/// Creates a directory symlink; false when the OS refuses (Windows without developer mode).
#[cfg(test)]
pub fn symlink_dir(target: &Path, link: &Path) -> bool {
    #[cfg(unix)]
    let r = std::os::unix::fs::symlink(target, link);
    #[cfg(windows)]
    let r: io::Result<()> = std::os::windows::fs::symlink_dir(target, link).or_else(|_| {
        // Without the symlink privilege a junction (a reparse point) stands in.
        let ok = std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(link)
            .arg(target)
            .output()
            .is_ok_and(|o| o.status.success());
        if ok {
            Ok(())
        } else {
            Err(io::ErrorKind::Unsupported.into())
        }
    });
    #[cfg(not(any(unix, windows)))]
    let r: io::Result<()> = Err(io::ErrorKind::Unsupported.into());
    r.is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> RepoPath {
        RepoPath::new(s).unwrap()
    }

    #[test]
    fn fs_path_stays_below_root() {
        let root = Path::new("root");
        let got = try_fs_path(root, &p("a/b.txt")).unwrap();
        assert_eq!(got, root.join("a").join("b.txt"));
    }

    #[cfg(windows)]
    #[test]
    fn windows_refuses_unrepresentable_names() {
        let root = Path::new("root");
        for bad in ["aux", "dir/NUL.txt", "dot.", "sp ", "C:x", "a/b:s", "x\\y"] {
            assert!(try_fs_path(root, &p(bad)).is_none(), "{bad}");
        }
        let non_utf8 = RepoPath::new(b"a/\xff".to_vec()).unwrap();
        assert!(try_fs_path(root, &non_utf8).is_none());
    }

    #[test]
    fn case_collisions_pick_the_later_sibling() {
        let paths = [p("A/x"), p("a/y"), p("b"), p("B"), p("c")];
        let got = case_collisions(&paths);
        assert_eq!(got, HashSet::from([p("a/y"), p("b")]));
    }

    #[test]
    fn symlinked_ancestor_is_refused_or_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("w");
        let outside = dir.path().join("o");
        fs::create_dir_all(&root).unwrap();
        fs::create_dir_all(&outside).unwrap();
        if !symlink_dir(&outside, &root.join("a")) {
            return;
        }
        let err = Verified::default()
            .ensure_parent(&root, &p("a/b/x"), false)
            .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
        assert!(!Verified::default().check_parent(&root, &p("a/x")).unwrap());
        assert_eq!(fs::read_dir(&outside).unwrap().count(), 0);

        Verified::default()
            .ensure_parent(&root, &p("a/b/x"), true)
            .unwrap();
        assert!(root.join("a/b").is_dir());
        assert!(!is_link(&fs::symlink_metadata(root.join("a")).unwrap()));
        assert_eq!(fs::read_dir(&outside).unwrap().count(), 0);
    }
}
