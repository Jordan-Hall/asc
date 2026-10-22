//! Switch planning (SPEC §10.4): which paths a switch writes, deletes or refuses. OS-neutral, so
//! the virtual file systems reuse it.

use std::collections::BTreeSet;

use asc_core::object::Change;
use asc_core::path::RepoPath;

/// How a switch treats local changes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// Refuse when a path to change has local modifications or an untracked file in the way.
    Safe,
    /// Keep locally modified paths as they are; refuse only for untracked files in the way.
    Merge,
    /// Overwrite local modifications and remove untracked files in the way.
    Force,
}

/// Local state that can collide with a switch. Only paths the switch touches need be listed.
#[derive(Clone, Debug, Default)]
pub struct Dirty {
    /// Tracked paths whose working content differs from the outgoing tree.
    pub modified: BTreeSet<RepoPath>,
    /// Untracked paths (files) that exist on disk.
    pub untracked: BTreeSet<RepoPath>,
}

impl Dirty {
    /// Whether an untracked file sits at `path` or at one of its ancestors.
    fn blocks(&self, path: &RepoPath) -> bool {
        if self.untracked.is_empty() {
            return false;
        }
        let mut cur = Some(path.clone());
        while let Some(p) = cur {
            if self.untracked.contains(&p) {
                return true;
            }
            cur = p.parent();
        }
        // An untracked file below a directory that must become a file.
        self.untracked.iter().any(|u| u.starts_with_dir(path))
    }
}

/// The outcome of [`plan`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Plan {
    /// `Write` and `Gitlink` changes to apply.
    pub updates: Vec<(RepoPath, Change)>,
    pub deletes: Vec<RepoPath>,
    /// Paths that make the switch unsafe; empty in `Force` mode.
    pub conflicts: Vec<RepoPath>,
    /// Locally modified paths left untouched (`Merge` mode).
    pub kept: Vec<RepoPath>,
}

/// Plans a switch from the leaf diff `diff` (outgoing to incoming tree, as
/// `Engine::leafdiff` returns it). Paths without a collision are always listed in `updates` or
/// `deletes`; the caller applies the plan only when `conflicts` is empty.
pub fn plan(diff: &[(RepoPath, Change)], dirty: &Dirty, mode: Mode) -> Plan {
    let mut out = Plan::default();
    for (path, change) in diff {
        let deleting = matches!(change, Change::Delete);
        let blocked = !deleting && dirty.blocks(path);
        let modified = dirty.modified.contains(path);
        match mode {
            Mode::Force => {}
            Mode::Safe if blocked || modified => {
                out.conflicts.push(path.clone());
                continue;
            }
            Mode::Merge if blocked => {
                out.conflicts.push(path.clone());
                continue;
            }
            Mode::Merge if modified => {
                out.kept.push(path.clone());
                continue;
            }
            _ => {}
        }
        if deleting {
            out.deletes.push(path.clone());
        } else {
            out.updates.push((path.clone(), change.clone()));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use asc_core::git::blob_oid;
    use asc_core::id::FileId;
    use asc_core::object::Kind;

    use super::*;

    fn p(s: &str) -> RepoPath {
        RepoPath::new(s).unwrap()
    }

    fn write(seed: u8) -> Change {
        Change::write(Kind::File, FileId::of(&[seed]), 1, blob_oid(&[seed]), None).unwrap()
    }

    fn diff() -> Vec<(RepoPath, Change)> {
        vec![
            (p("a"), write(1)),
            (p("b"), Change::Delete),
            (p("d/e"), write(2)),
        ]
    }

    #[test]
    fn clean_tree_plans_everything() {
        let pl = plan(&diff(), &Dirty::default(), Mode::Safe);
        assert_eq!(pl.updates.len(), 2);
        assert_eq!(pl.deletes, [p("b")]);
        assert!(pl.conflicts.is_empty());
    }

    #[test]
    fn safe_refuses_modified_and_blocked_paths() {
        let mut dirty = Dirty::default();
        dirty.modified.insert(p("b"));
        dirty.untracked.insert(p("d"));
        let pl = plan(&diff(), &dirty, Mode::Safe);
        assert_eq!(pl.conflicts, [p("b"), p("d/e")]);
        assert_eq!(pl.updates.len(), 1);
    }

    #[test]
    fn merge_keeps_modified_but_refuses_blocked() {
        let mut dirty = Dirty::default();
        dirty.modified.insert(p("a"));
        dirty.untracked.insert(p("d/e"));
        let pl = plan(&diff(), &dirty, Mode::Merge);
        assert_eq!(pl.kept, [p("a")]);
        assert_eq!(pl.conflicts, [p("d/e")]);
        assert_eq!(pl.deletes, [p("b")]);
    }

    #[test]
    fn force_overwrites_everything() {
        let mut dirty = Dirty::default();
        dirty.modified.insert(p("a"));
        dirty.untracked.insert(p("d"));
        let pl = plan(&diff(), &dirty, Mode::Force);
        assert!(pl.conflicts.is_empty() && pl.kept.is_empty());
        assert_eq!((pl.updates.len(), pl.deletes.len()), (2, 1));
    }

    #[test]
    fn untracked_below_a_new_file_blocks() {
        let mut dirty = Dirty::default();
        dirty.untracked.insert(p("a/x"));
        let pl = plan(&diff(), &dirty, Mode::Safe);
        assert_eq!(pl.conflicts, [p("a")]);
    }
}
