//! `asc serve git-hook pre-receive` (SPEC §25.2 "Writes", §25.8 item 3; PLAN U82): the hidden
//! command that the asc-owned `pre-receive` hook of the git protocol layer executes.
//!
//! git runs the hook with the push's command lines on stdin, its own quarantine variables and
//! asc's two variables (`ASC_GIT_HOOK_TOKEN`, `ASC_GIT_HOOK_ENDPOINT`) in the environment. The hook
//! reads them, asks the server that spawned git over its local socket, prints the server's
//! refusal reason on stderr (git relays it as `remote:` lines) and exits 0 or 1. All of that is
//! `asc_serve::git_push::run_hook`; this file only decides whether an invocation is the hook.
//!
//! It runs from `main` before anything else is set up: the hook starts in a git view directory,
//! with `GIT_DIR` and the object directory variables set, which no other command may be handed,
//! and a push waits for it. Only the exact argument list the script uses is the hook: no global
//! option, no alias, no prefix of it.

use std::ffi::{OsStr, OsString};
use std::process::ExitCode;

/// The words after the program name.
const WORDS: [&str; 3] = ["serve", "git-hook", "pre-receive"];

/// Whether `args` (the process's, with the program name first) is exactly `asc serve git-hook
/// pre-receive`.
pub fn is_hook_invocation(args: &[OsString]) -> bool {
    args.len() == WORDS.len() + 1
        && args[1..]
            .iter()
            .zip(WORDS)
            .all(|(arg, word)| arg == OsStr::new(word))
}

/// Runs the hook; the exit status is git's verdict on the push.
pub async fn run() -> ExitCode {
    let code = asc_serve::git_push::run_hook().await;
    ExitCode::from(u8::try_from(code).unwrap_or(1))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(words: &[&str]) -> Vec<OsString> {
        words.iter().map(OsString::from).collect()
    }

    #[test]
    fn only_the_exact_hook_command_line_is_the_hook() {
        assert!(is_hook_invocation(&args(&[
            "asc",
            "serve",
            "git-hook",
            "pre-receive"
        ])));
        assert!(is_hook_invocation(&args(&[
            "/usr/bin/asc",
            "serve",
            "git-hook",
            "pre-receive"
        ])));
        for other in [
            &["asc"][..],
            &["asc", "serve"],
            &["asc", "serve", "git-hook"],
            &["asc", "serve", "git-hook", "post-receive"],
            &["asc", "serve", "git-hook", "pre-receive", "extra"],
            &["asc", "-C", "x", "serve", "git-hook", "pre-receive"],
            &["asc", "serve", "git-hook", "Pre-Receive"],
            &["asc", "Serve", "git-hook", "pre-receive"],
        ] {
            assert!(!is_hook_invocation(&args(other)), "{other:?}");
        }
    }
}
