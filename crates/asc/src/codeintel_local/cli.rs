//! `asc codeintel worker`: the local index helper (SPEC §23.2.5). The other `codeintel`
//! subcommands belong to the code intelligence CLI (I17); this entry serves `worker` only.

use std::ffi::OsString;
use std::process::ExitCode;

use crate::cli::common::{CliError, Ctx, Result};
use crate::native::NativeSpec;

use crate::cli::codeintel::scip;

const USAGE: &str = "usage: asc codeintel worker [--once] [--now]
       asc codeintel enable-embed [--from <dir>]
       asc codeintel upload-scip <index.scip> [--rev <revspec>] [--root <dir>] [--remote <name|url>]
       asc codeintel status [--rev <rev>] [--remote <name|url>] [--json]

    --once    index the current HEAD and exit
    --now     do not wait for the workspace to be idle
    --from    install the embed helper from a release directory instead of asc.releasesUrl
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
    flags: &[
        "--once", "--now", "--from", "--rev", "--root", "--remote", "--json",
    ],
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
        Some("status") => {
            let rest: Vec<OsString> = words.cloned().collect();
            crate::cli::codeintel::status::run(ctx, &rest).await
        }
        Some("upload-scip") => {
            let words: Option<Vec<&str>> = words.map(|w| w.to_str()).collect();
            let words = words.ok_or_else(|| CliError::Usage(USAGE.trim_end().to_owned()))?;
            scip::run(ctx, words.into_iter()).await
        }
        Some("enable-embed") => {
            let mut from: Option<std::path::PathBuf> = None;
            while let Some(w) = words.next() {
                match (w.to_str(), from.is_none()) {
                    (Some("--from"), true) => match words.next() {
                        Some(d) => from = Some(d.into()),
                        None => return Err(CliError::Usage(USAGE.trim_end().to_owned())),
                    },
                    _ => return Err(CliError::Usage(USAGE.trim_end().to_owned())),
                }
            }
            let fatal = |e: &dyn std::fmt::Display| CliError::Fatal(format!("asc: codeintel: {e}"));
            let source = match (from, ctx.config.get("asc.releasesUrl")) {
                (Some(d), _) => super::embed::Source::Dir(d),
                (None, Some(u)) => super::embed::Source::url(&u).map_err(|e| fatal(&e))?,
                (None, None) => {
                    return Err(CliError::Fatal(
                        "asc: codeintel: no release source: set asc.releasesUrl or pass --from <dir>"
                            .to_owned(),
                    ));
                }
            };
            let home = asc_state::AscHome::locate().map_err(|e| fatal(&e))?;
            let msg = super::embed::enable_command(home.root(), &source)
                .await
                .map_err(|e| fatal(&e))?;
            println!("{msg}");
            Ok(ExitCode::SUCCESS)
        }
        _ => Err(CliError::Usage(USAGE.trim_end().to_owned())),
    }
}
