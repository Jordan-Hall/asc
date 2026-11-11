//! The path rules every notification applies (SPEC §10.5): a path with a redirected component
//! or a `.git` component never reaches the journal, and the deletions a served root has seen are
//! remembered for `worktree convert --plain`.

use std::collections::BTreeSet;

use asc_core::path::RepoPath;
use asc_wc::journal::Op;
use asc_wc::redirect::Redirections;

/// Whether one path component is a redirected name. Windows names are case-insensitive, so the
/// folded spelling is checked as well as the exact one.
pub fn is_redirected(red: &Redirections, name: &[u8]) -> bool {
    red.is_redirected_name(name) || red.is_redirected_name(&name.to_ascii_lowercase())
}

/// Whether a path is outside what the journal tracks: it has a redirected component or `.git`.
pub fn dropped(red: &Redirections, path: &RepoPath) -> bool {
    path.components()
        .any(|c| c.eq_ignore_ascii_case(b".git") || is_redirected(red, c))
}

/// Most deletions one served root remembers; past it the on-disk walk answers for the rest.
pub const MAX_DELETED: usize = 1 << 16;

/// Keeps in `set` the paths the user removed while the root is served: a deletion or the source of
/// a rename adds the path; a creation or the destination of a rename takes out the path and what
/// was under it, because something stands there again.
pub fn track(set: &mut BTreeSet<RepoPath>, op: Op, path: &RepoPath) {
    match op {
        Op::Deleted | Op::RenamedFrom | Op::DirRenamedFrom => {
            if set.len() < MAX_DELETED {
                set.insert(path.clone());
            }
        }
        Op::Created | Op::DirRenamedTo => {
            // Children sort right after the path among the names it prefixes.
            let gone: Vec<RepoPath> = set
                .range(path.clone()..)
                .take_while(|p| p.as_bytes().starts_with(path.as_bytes()))
                .filter(|p| *p == path || p.starts_with_dir(path))
                .cloned()
                .collect();
            for p in gone {
                set.remove(&p);
            }
        }
        Op::Modified => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rp(s: &str) -> RepoPath {
        RepoPath::new(s).unwrap()
    }

    #[test]
    fn deletions_are_remembered_until_something_stands_there_again() {
        let mut set = BTreeSet::new();
        for (op, p) in [
            (Op::Deleted, "a.txt"),
            (Op::Deleted, "src/x.rs"),
            (Op::DirRenamedFrom, "src"),
            (Op::RenamedFrom, "src-old.txt"),
            (Op::Modified, "b.txt"),
        ] {
            track(&mut set, op, &rp(p));
        }
        let names = |s: &BTreeSet<RepoPath>| -> Vec<String> {
            s.iter()
                .map(|p| String::from_utf8_lossy(p.as_bytes()).into_owned())
                .collect()
        };
        assert_eq!(names(&set), ["a.txt", "src", "src-old.txt", "src/x.rs"]);
        // A directory made again takes itself and what was under it, not a sibling it prefixes.
        track(&mut set, Op::DirRenamedTo, &rp("src"));
        assert_eq!(names(&set), ["a.txt", "src-old.txt"]);
        track(&mut set, Op::Created, &rp("a.txt"));
        assert_eq!(names(&set), ["src-old.txt"]);
    }

    #[test]
    fn the_remembered_deletions_are_bounded() {
        let mut set = BTreeSet::new();
        for i in 0..MAX_DELETED + 3 {
            track(&mut set, Op::Deleted, &rp(&format!("f{i}")));
        }
        assert_eq!(set.len(), MAX_DELETED);
    }

    #[test]
    fn redirected_components_are_dropped_in_any_case() {
        let red = Redirections::parse(
            "node_modules
",
        );
        assert!(dropped(&red, &rp("web/Node_Modules/x.js")));
        assert!(!dropped(&red, &rp("src/main.rs")));
    }
}
