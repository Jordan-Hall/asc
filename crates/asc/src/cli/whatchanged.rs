//! `whatchanged` (SPEC §11.2): `log --raw --no-merges` with git 2.55's refusal to run without
//! `--i-still-use-this`. Everything else is `log`: the same options, the same walk and output.

use std::ffi::OsString;
use std::process::ExitCode;

use super::common::{CliError, Ctx, Result};
use super::log::{self, OUTPUT_FLAGS};
use crate::native::NativeSpec;

#[derive(clap::Parser, Debug)]
#[command(
    name = "whatchanged",
    disable_help_flag = true,
    disable_version_flag = true
)]
pub struct Args {
    #[arg(trailing_var_arg = true, allow_hyphen_values = true, num_args = 0..)]
    pub rest: Vec<OsString>,
}

/// The flags `log` selects commits with, which `whatchanged` shares.
const SELECT_FLAGS: &[&str] = &[
    "--i-still-use-this",
    "-n",
    "--max-count",
    "-<n>",
    "--skip",
    "--since",
    "--after",
    "--until",
    "--before",
    "--author",
    "--committer",
    "--grep",
    "-i",
    "--regexp-ignore-case",
    "-E",
    "--extended-regexp",
    "-F",
    "--fixed-strings",
    "--invert-grep",
    "--first-parent",
    "--all",
    "--branches",
    "--tags",
    "--remotes",
    "--not",
    "--merges",
    "--no-merges",
    "--reverse",
    "--topo-order",
    "--date-order",
    "--author-date-order",
];

pub const NATIVE: &[NativeSpec] = &[
    NativeSpec {
        cmd: "whatchanged",
        flags: OUTPUT_FLAGS,
        placeholders: log::PLACEHOLDERS,
    },
    NativeSpec {
        cmd: "whatchanged",
        flags: SELECT_FLAGS,
        placeholders: log::PLACEHOLDERS,
    },
];

/// What git 2.55 prints (to standard error) when the flag is missing.
const REFUSAL: &str = "'git whatchanged' is nominated for removal.

hint: You can replace 'git whatchanged <opts>' with:
hint:\tgit log <opts> --raw --no-merges
hint: Or make an alias:
hint:\tgit config set --global alias.whatchanged 'log --raw --no-merges'

If you still use this command, here's what you can do:

- read https://git-scm.com/docs/BreakingChanges.html
- check if anyone has discussed this on the mailing
  list and if they came up with something that can
  help you: https://lore.kernel.org/git/?q=git%20whatchanged
- send an email to <git@vger.kernel.org> to let us
  know that you still use this command and were unable
  to determine a suitable replacement

fatal: refusing to run without --i-still-use-this";

/// The arguments with the acknowledgement removed, and whether it was there. Only the part
/// before `--` is searched.
fn strip_ack(rest: Vec<OsString>) -> (Vec<OsString>, bool) {
    let mut seen = false;
    let mut ended = false;
    let kept = rest
        .into_iter()
        .filter(|a| {
            if ended {
                return true;
            }
            if a == "--" {
                ended = true;
                return true;
            }
            if a == "--i-still-use-this" {
                seen = true;
                return false;
            }
            true
        })
        .collect();
    (kept, seen)
}

pub async fn run(ctx: &Ctx, a: Args) -> Result<ExitCode> {
    let (rest, acknowledged) = strip_ack(a.rest);
    if !acknowledged {
        return Err(CliError::Raw(REFUSAL.to_owned(), 128));
    }
    let mut argv: Vec<OsString> = vec!["--raw".into(), "--no-merges".into()];
    argv.extend(rest);
    log::run(ctx, log::Args { rest: argv }).await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn os(items: &[&str]) -> Vec<OsString> {
        items.iter().map(OsString::from).collect()
    }

    #[test]
    fn the_flag_is_removed_before_the_path_separator_only() {
        let (rest, seen) = strip_ack(os(&[
            "--i-still-use-this",
            "-3",
            "--",
            "--i-still-use-this",
        ]));
        assert!(seen);
        assert_eq!(rest, os(&["-3", "--", "--i-still-use-this"]));
        let (rest, seen) = strip_ack(os(&["--", "--i-still-use-this"]));
        assert!(!seen);
        assert_eq!(rest.len(), 2);
    }
}
