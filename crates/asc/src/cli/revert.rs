//! `asc revert` (SPEC §4.4): the replay of `pick.rs` run backwards. The new commit's author is the
//! current identity and its message is `Revert "<subject>"` plus the "This reverts commit" line.

use std::ffi::OsString;
use std::process::ExitCode;

use super::common::{Ctx, Result};
use super::pick::{Flags, replay};
use super::seqstate::{Opts, Replay};
use crate::native::NativeSpec;

#[derive(clap::Parser, Debug)]
#[command(name = "revert", disable_help_flag = true, disable_version_flag = true)]
pub struct Args {
    #[arg(short = 'n', long = "no-commit")]
    pub no_commit: bool,
    #[arg(short = 'm', long = "mainline")]
    pub mainline: Option<usize>,
    #[arg(long = "continue")]
    pub cont: bool,
    #[arg(long = "skip")]
    pub skip: bool,
    #[arg(long = "abort")]
    pub abort: bool,
    #[arg(long = "quit")]
    pub quit: bool,
    #[arg(short = 'X', long = "strategy-option")]
    pub strategy_option: Vec<String>,
    #[arg(short = 's', long = "strategy")]
    pub strategy: Vec<String>,
    #[arg(long = "no-edit")]
    pub no_edit: bool,
    pub commits: Vec<OsString>,
}

pub const NATIVE: &[NativeSpec] = &[NativeSpec {
    cmd: "revert",
    flags: &[
        "-n",
        "--no-commit",
        "-m",
        "--mainline",
        "--continue",
        "--skip",
        "--abort",
        "--quit",
        "-X",
        "--strategy-option",
        "-s",
        "--strategy",
        "--no-edit",
    ],
    placeholders: &[],
}];

pub async fn run(ctx: &Ctx, a: Args) -> Result<ExitCode> {
    let flags = Flags {
        opts: Opts {
            no_commit: a.no_commit,
            mainline: a.mainline,
            strategy_options: a.strategy_option,
            ..Opts::default()
        },
        cont: a.cont,
        skip: a.skip,
        abort: a.abort,
        quit: a.quit,
        strategy: a.strategy,
        commits: a.commits,
    };
    replay(ctx, Replay::Revert, flags).await
}
