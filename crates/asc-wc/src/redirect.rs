//! Redirection list (`.asc/redirections`, SPEC §10.5): directory names that are never tracked,
//! descended into or reported by status.

use std::collections::BTreeSet;
use std::io;
use std::path::Path;

use asc_core::path::RepoPath;

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

    /// Loads `<root>/.asc/redirections`; a missing file is an empty list.
    pub fn load(root: &Path) -> io::Result<Self> {
        match std::fs::read_to_string(root.join(FILE)) {
            Ok(t) => Ok(Self::parse(&t)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e),
        }
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
    }
}
