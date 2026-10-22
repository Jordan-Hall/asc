//! Redirection list (`.asc/redirections`, SPEC §10.5): directory names that are never tracked,
//! descended into or reported by status.

use std::collections::BTreeSet;
use std::io::{self, Read};
use std::path::Path;

use asc_core::path::RepoPath;

use crate::safe_path::is_link;

/// Largest `.asc/redirections` read; a longer file is refused.
pub const MAX_BYTES: u64 = 64 * 1024;

/// ASC's default list, for `asc init` to write.
pub const DEFAULT_NAMES: [&str; 7] = [
    "node_modules",
    "dist",
    ".turbo",
    ".next",
    "target",
    ".venv",
    "__pycache__",
];

/// Location of the list, relative to the work tree root.
pub const FILE: &str = ".asc/redirections";

/// The set of redirected directory names.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Redirections {
    names: BTreeSet<Vec<u8>>,
}

impl Redirections {
    /// Parses one name per line; blank lines and `#` comments are skipped.
    pub fn parse(text: &str) -> Self {
        let names = text
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty() && !l.starts_with('#'))
            .map(|l| l.trim_end_matches('/').as_bytes().to_vec())
            .collect();
        Self { names }
    }

    /// Loads `<root>/.asc/redirections`; a missing file (or `.asc`) is an empty list. The list
    /// comes from the repository, so the load refuses, rather than follows or ignores, a link
    /// at `.asc` or at the file, anything but a regular file, a non-UTF-8 file and one larger
    /// than [`MAX_BYTES`].
    pub fn load(root: &Path) -> io::Result<Self> {
        let refuse =
            |what: &str| io::Error::new(io::ErrorKind::InvalidData, format!("{FILE}: {what}"));
        let dir = root.join(".asc");
        match std::fs::symlink_metadata(&dir) {
            Ok(m) if !is_link(&m) && m.is_dir() => {}
            Ok(_) => return Err(refuse("`.asc` is not a real directory")),
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(e) => return Err(e),
        }
        let path = root.join(FILE);
        match std::fs::symlink_metadata(&path) {
            Ok(m) if !is_link(&m) && m.is_file() => {
                if m.len() > MAX_BYTES {
                    return Err(refuse("file is too large"));
                }
            }
            Ok(_) => return Err(refuse("not a regular file")),
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(e) => return Err(e),
        }
        // The size is re-bounded on the read: the file may have grown since the stat.
        let mut bytes = Vec::new();
        std::fs::File::open(&path)?
            .take(MAX_BYTES + 1)
            .read_to_end(&mut bytes)?;
        if bytes.len() as u64 > MAX_BYTES {
            return Err(refuse("file is too large"));
        }
        let text = String::from_utf8(bytes).map_err(|_| refuse("not UTF-8"))?;
        Ok(Self::parse(&text))
    }

    pub fn is_empty(&self) -> bool {
        self.names.is_empty()
    }

    /// Whether a single path component is a redirected name.
    pub fn is_redirected_name(&self, name: &[u8]) -> bool {
        self.names.contains(name)
    }

    /// Whether any component of `path` is a redirected name.
    pub fn covers(&self, path: &RepoPath) -> bool {
        path.components().any(|c| self.is_redirected_name(c))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_names_and_matches_any_component() {
        let r = Redirections::parse("# c\nnode_modules\n\n target/ \n");
        assert!(r.is_redirected_name(b"node_modules"));
        assert!(r.is_redirected_name(b"target"));
        assert!(r.covers(&RepoPath::new("a/node_modules/x.js").unwrap()));
        assert!(!r.covers(&RepoPath::new("a/src/x.js").unwrap()));
    }

    #[test]
    fn missing_file_is_empty() {
        let d = tempfile::tempdir().unwrap();
        assert!(Redirections::load(d.path()).unwrap().is_empty());
        std::fs::create_dir(d.path().join(".asc")).unwrap();
        assert!(Redirections::load(d.path()).unwrap().is_empty());
    }

    #[test]
    fn loads_a_regular_file_up_to_the_limit() {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir(d.path().join(".asc")).unwrap();
        let mut text = b"node_modules\n".to_vec();
        text.resize(MAX_BYTES as usize, b'#');
        std::fs::write(d.path().join(FILE), &text).unwrap();
        let r = Redirections::load(d.path()).unwrap();
        assert!(r.is_redirected_name(b"node_modules"));
    }

    #[test]
    fn oversized_file_is_refused() {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir(d.path().join(".asc")).unwrap();
        std::fs::write(d.path().join(FILE), vec![b'#'; MAX_BYTES as usize + 1]).unwrap();
        let e = Redirections::load(d.path()).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn non_utf8_file_is_refused() {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir(d.path().join(".asc")).unwrap();
        std::fs::write(d.path().join(FILE), [0xff, 0xfe, b'\n']).unwrap();
        let e = Redirections::load(d.path()).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::InvalidData);
    }

    /// A tracked `.asc` link to a directory holding `redirections` is not followed.
    #[test]
    fn linked_asc_dir_is_refused() {
        let d = tempfile::tempdir().unwrap();
        let (root, x) = (d.path().join("w"), d.path().join("x"));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&x).unwrap();
        std::fs::write(x.join("redirections"), "secret\n").unwrap();
        if !crate::safe_path::symlink_dir(&x, &root.join(".asc")) {
            return;
        }
        let e = Redirections::load(&root).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::InvalidData);
    }

    /// A tracked `.asc/redirections` link (to `/dev/zero`, a credentials file or a share) is
    /// refused unread.
    #[test]
    fn linked_file_is_refused() {
        let d = tempfile::tempdir().unwrap();
        let root = d.path();
        std::fs::create_dir(root.join(".asc")).unwrap();
        let secret = root.join("secret");
        std::fs::write(&secret, "aws_secret\n").unwrap();
        #[cfg(unix)]
        let linked = std::os::unix::fs::symlink(&secret, root.join(FILE)).is_ok();
        #[cfg(windows)]
        let linked = std::os::windows::fs::symlink_file(&secret, root.join(FILE)).is_ok();
        #[cfg(not(any(unix, windows)))]
        let linked = false;
        if !linked {
            return;
        }
        let e = Redirections::load(root).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::InvalidData);
    }

    /// A directory where the file should be is not a regular file.
    #[test]
    fn non_regular_file_is_refused() {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(d.path().join(FILE)).unwrap();
        let e = Redirections::load(d.path()).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::InvalidData);
    }
}
