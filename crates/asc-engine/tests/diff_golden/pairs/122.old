//! `asc codeintel worker`: the local index helper (SPEC §23.2.5). The other `codeintel`
//! subcommands belong to the code intelligence CLI (I17); this entry serves `worker` only.

use std::ffi::OsString;
use std::process::ExitCode;

use crate::cli::common::{CliError, Ctx, Result};
use crate::native::NativeSpec;

const USAGE: &str = "usage: asc codeintel worker [--once] [--now]

    --once    index the current HEAD and exit
    --now     do not wait for the workspace to be idle
";

#[derive(clap::Parser, Debug)]
#[command(
    name = "codeintel",
    disable_help_flag = true,
    disable_version_flag = true
)]
pub struct Args {
    #[arg(trailing_var_arg = true, allow_hyphen_values = true, num_args = 0..)]
    pub rest: Vec<OsString>,
}

pub const NATIVE: &[NativeSpec] = &[NativeSpec {
    cmd: "codeintel",
    flags: &["--once", "--now"],
    placeholders: &[],
}];

pub async fn run(ctx: &Ctx, a: Args) -> Result<ExitCode> {
    let mut words = a.rest.iter();
    match words.next().and_then(|w| w.to_str()) {
        Some("worker") => {
            let mut opts = super::worker::Options::default();
            for w in words {
                match w.to_str() {
                    Some("--once") => opts.once = true,
                    Some("--now") => opts.now = true,
                    _ => return Err(CliError::Usage(USAGE.trim_end().to_owned())),
                }
            }
            super::worker::run(ctx, opts).await
        }
        _ => Err(CliError::Usage(USAGE.trim_end().to_owned())),
    }
}
