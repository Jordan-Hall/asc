//! Verification of stored git tree overrides (SPEC §3.3).
//!
//! A `GitTreeOverride` body replaces the canonical render of an asc tree so a mirrored git tree
//! keeps its original oid. It may differ from the canonical render only in encoding (mode
//! spelling that git tolerates, entry order); it must never describe different content.

use std::collections::HashMap;

use asc_core::id::GitOid;
use asc_core::object::{Entry, Kind, Tree};

/// Why an override body does not describe its asc tree.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum OverrideError {
    #[error("malformed tree body at byte {0}")]
    Malformed(usize),
    #[error("entry {0:?} appears twice")]
    Duplicate(Vec<u8>),
    #[error("entry {0:?} is not in the asc tree")]
    ExtraEntry(Vec<u8>),
    #[error("entry {0:?} of the asc tree is missing")]
    MissingEntry(Vec<u8>),
    #[error("entry {name:?} points at {actual}, expected {expected}")]
    OidMismatch {
        name: Vec<u8>,
        expected: GitOid,
        actual: GitOid,
    },
    #[error("entry {name:?} has mode {mode:?}, not valid for its kind")]
    ModeMismatch { name: Vec<u8>, mode: Vec<u8> },
}

/// Modes git tolerates for each kind: the canonical one, a single zero-padded spelling, and
/// `100664` for a regular file.
fn mode_ok(kind: Kind, mode: &[u8]) -> bool {
    let (canon, legacy): (&[u8], &[&[u8]]) = match kind {
        Kind::File => (b"100644", &[b"100664", b"0100664"]),
        Kind::Exec => (b"100755", &[]),
        Kind::Symlink => (b"120000", &[]),
        Kind::Dir => (b"40000", &[]),
        Kind::Gitlink => (b"160000", &[]),
    };
    mode == canon || legacy.contains(&mode) || (mode.first() == Some(&b'0') && &mode[1..] == canon)
}

/// Checks that `body` (a git tree body without header) lists exactly the entries of `tree`:
/// same names, same child oids, and a mode of the same kind. Order is the body's own.
pub fn verify(body: &[u8], tree: &Tree) -> Result<(), OverrideError> {
    let mut want: HashMap<&[u8], &Entry> = tree
        .entries()
        .iter()
        .map(|e| (e.name.as_bytes(), e))
        .collect();
    let mut pos = 0;
    while pos < body.len() {
        let start = pos;
        let sp = body[pos..]
            .iter()
            .position(|&c| c == b' ')
            .ok_or(OverrideError::Malformed(start))?;
        let mode = &body[pos..pos + sp];
        pos += sp + 1;
        let nul = body[pos..]
            .iter()
            .position(|&c| c == 0)
            .ok_or(OverrideError::Malformed(start))?;
        let name = &body[pos..pos + nul];
        pos += nul + 1;
        let oid: [u8; 20] = body
            .get(pos..pos + 20)
            .and_then(|s| s.try_into().ok())
            .ok_or(OverrideError::Malformed(start))?;
        pos += 20;
        if mode.is_empty() || !mode.iter().all(u8::is_ascii_digit) || name.is_empty() {
            return Err(OverrideError::Malformed(start));
        }
        let Some(entry) = want.remove(name) else {
            let known = tree.entries().iter().any(|e| e.name.as_bytes() == name);
            return Err(if known {
                OverrideError::Duplicate(name.to_vec())
            } else {
                OverrideError::ExtraEntry(name.to_vec())
            });
        };
        let actual = GitOid::from_bytes(oid);
        if actual != entry.git {
            return Err(OverrideError::OidMismatch {
                name: name.to_vec(),
                expected: entry.git,
                actual,
            });
        }
        if !mode_ok(entry.kind, mode) {
            return Err(OverrideError::ModeMismatch {
                name: name.to_vec(),
                mode: mode.to_vec(),
            });
        }
    }
    if let Some(name) = want.keys().min() {
        return Err(OverrideError::MissingEntry(name.to_vec()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use asc_core::git::render_tree;
    use asc_core::path::Name;

    fn entry(name: &str, kind: Kind, n: u8) -> Entry {
        Entry {
            name: Name::new(name).unwrap(),
            kind,
            id: [0; 32],
            size: 0,
            git: GitOid::from_bytes([n; 20]),
        }
    }

    fn tree() -> Tree {
        Tree::new(
            vec![
                entry("a", Kind::File, 1),
                entry("b", Kind::Dir, 2),
                entry("c", Kind::Exec, 3),
            ],
            false,
        )
        .unwrap()
    }

    fn body(parts: &[(&str, &str, u8)]) -> Vec<u8> {
        let mut out = Vec::new();
        for (mode, name, n) in parts {
            out.extend_from_slice(mode.as_bytes());
            out.push(b' ');
            out.extend_from_slice(name.as_bytes());
            out.push(0);
            out.extend_from_slice(&[*n; 20]);
        }
        out
    }

    #[test]
    fn canonical_render_passes() {
        let t = tree();
        assert_eq!(verify(&render_tree(&t), &t), Ok(()));
    }

    #[test]
    fn legacy_modes_pass() {
        let b = body(&[("100664", "a", 1), ("040000", "b", 2), ("100755", "c", 3)]);
        assert_eq!(verify(&b, &tree()), Ok(()));
    }

    #[test]
    fn different_child_oid_fails() {
        let b = body(&[("100644", "a", 9), ("40000", "b", 2), ("100755", "c", 3)]);
        assert!(matches!(
            verify(&b, &tree()),
            Err(OverrideError::OidMismatch { .. })
        ));
    }

    #[test]
    fn extra_entry_fails() {
        let b = body(&[
            ("100644", "a", 1),
            ("40000", "b", 2),
            ("100755", "c", 3),
            ("100644", "d", 4),
        ]);
        assert!(matches!(
            verify(&b, &tree()),
            Err(OverrideError::ExtraEntry(_))
        ));
    }

    #[test]
    fn missing_entry_fails() {
        let b = body(&[("100644", "a", 1), ("100755", "c", 3)]);
        assert_eq!(
            verify(&b, &tree()),
            Err(OverrideError::MissingEntry(b"b".to_vec()))
        );
    }

    #[test]
    fn renamed_entry_fails() {
        let b = body(&[("100644", "a", 1), ("40000", "x", 2), ("100755", "c", 3)]);
        assert!(matches!(
            verify(&b, &tree()),
            Err(OverrideError::ExtraEntry(_))
        ));
    }

    #[test]
    fn duplicate_entry_fails() {
        let b = body(&[("100644", "a", 1), ("100644", "a", 1)]);
        assert!(matches!(
            verify(&b, &tree()),
            Err(OverrideError::Duplicate(_))
        ));
    }

    #[test]
    fn trailing_garbage_fails() {
        let mut b = body(&[("100644", "a", 1), ("40000", "b", 2), ("100755", "c", 3)]);
        b.extend_from_slice(b"junk");
        assert!(matches!(
            verify(&b, &tree()),
            Err(OverrideError::Malformed(_))
        ));
    }

    #[test]
    fn different_mode_kind_fails() {
        for mode in ["100755", "120000", "40000", "160000", "100775"] {
            let b = body(&[(mode, "a", 1), ("40000", "b", 2), ("100755", "c", 3)]);
            assert!(
                matches!(verify(&b, &tree()), Err(OverrideError::ModeMismatch { .. })),
                "{mode}"
            );
        }
    }
}
