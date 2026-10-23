//! D5 (SPEC §20.1, §11.4): under argv[0] `git` a stack verb runs only where git would print
//! "not a git command", so a user alias or a `git-<verb>` on PATH wins. The order itself is
//! `dispatch::resolve`; this module owns the verb lists and the rule that decides which
//! registered verbs may be reached at all.

use crate::dispatch::{ASC_ONLY_VERBS, GIT_BUILTINS, is_unavailable};

/// Every stack verb word of SPEC §20.7 (aliases included). Each is also in
/// `dispatch::ASC_ONLY_VERBS`, which is what gives it D5 precedence and keeps it out of the
/// autocorrect candidates.
pub const STACK_VERBS: &[&str] = &[
    "create",
    "modify",
    "absorb",
    "restack",
    "continue",
    "abort",
    "sync",
    "submit",
    "sl",
    "smartlog",
    "up",
    "down",
    "top",
    "bottom",
    "next",
    "prev",
    "reword",
    "uncommit",
    "unamend",
    "abandon",
    "change-id",
    "split",
    "squash",
    "fold",
    "move",
    "reorder",
    "stack",
    "undo",
    "redo",
    "op",
    "evolog",
    "interdiff",
    "share",
];

/// Whether `name` is a stack verb word.
pub fn is_stack_verb(name: &str) -> bool {
    STACK_VERBS.contains(&name)
}

/// Whether a verb registered under `name` can be reached: an asc-only verb that is not also a
/// git command. A registration outside this set is ignored, so a unit can never shadow git
/// or take a name `dispatch` does not route with D5 precedence.
pub fn routable(name: &str) -> bool {
    ASC_ONLY_VERBS.contains(&name) && !GIT_BUILTINS.contains(&name) && !is_unavailable(name)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::dispatch::{Lookup, Program, Resolved, resolve, similar};

    struct Fake {
        aliases: Vec<(&'static str, &'static str)>,
        externals: Vec<&'static str>,
    }

    impl Lookup for Fake {
        fn alias(&self, name: &str) -> Option<String> {
            self.aliases
                .iter()
                .find(|(n, _)| *n == name)
                .map(|(_, v)| (*v).to_owned())
        }
        fn external(&self, prefix: &str, name: &str) -> Option<PathBuf> {
            let file = format!("{prefix}{name}");
            self.externals
                .iter()
                .any(|e| *e == file)
                .then(|| PathBuf::from(file))
        }
    }

    #[test]
    fn every_stack_verb_is_asc_only_and_never_a_git_command() {
        for v in STACK_VERBS {
            assert!(ASC_ONLY_VERBS.contains(v), "{v} missing from ASC_ONLY_VERBS");
            assert!(!GIT_BUILTINS.contains(v), "{v} is a git builtin");
            assert!(routable(v), "{v}");
        }
        let mut sorted = STACK_VERBS.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), STACK_VERBS.len(), "duplicate stack verb");
        assert!(!routable("status") && !routable("nosuch") && !routable("lfs"));
    }

    #[test]
    fn git_runs_a_stack_verb_only_where_git_would_fail() {
        let bare = Fake {
            aliases: vec![],
            externals: vec![],
        };
        let ext = Fake {
            aliases: vec![],
            externals: vec!["git-sync"],
        };
        let alias = Fake {
            aliases: vec![("undo", "reset --soft HEAD~1")],
            externals: vec!["git-undo"],
        };
        for v in STACK_VERBS {
            assert_eq!(
                resolve(Program::Git, v, &bare),
                Resolved::Command((*v).to_owned())
            );
            assert_eq!(
                resolve(Program::Asc, v, &ext),
                Resolved::Command((*v).to_owned())
            );
        }
        assert_eq!(
            resolve(Program::Git, "sync", &ext),
            Resolved::External(PathBuf::from("git-sync"))
        );
        assert_eq!(
            resolve(Program::Git, "undo", &alias),
            Resolved::Alias("reset --soft HEAD~1".to_owned())
        );
    }

    #[test]
    fn stack_verbs_are_never_autocorrect_candidates() {
        let git: Vec<&str> = GIT_BUILTINS.to_vec();
        // `comit` is a typo of `commit`; no stack verb is offered.
        assert_eq!(
            similar("comit", git.iter().copied()),
            vec!["commit".to_owned()]
        );
        // A stack verb's own name matches nothing that is not a git command.
        for v in STACK_VERBS {
            assert!(!git.contains(v), "{v}");
        }
    }
}
