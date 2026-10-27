//! `asc self-update [--check] [--from <dir>]`: the same signature-verified, side-by-side update as
//! `asc upgrade` (see `upgrade.rs`), under the name the installers and the documentation use.
//!
//! The release source is `asc.releasesUrl`, else `ASC_DOWNLOAD_BASE_URL`, else `--from <dir>`.

use std::path::PathBuf;
use std::process::ExitCode;

use super::common::{Ctx, Result};
use super::upgrade;
use crate::native::NativeSpec;

#[derive(clap::Parser, Debug)]
#[command(
    name = "self-update",
    disable_help_flag = true,
    disable_version_flag = true
)]
pub struct Args {
    /// Only look for a newer version and say so.
    #[arg(long)]
    pub check: bool,
    /// A directory laid out like the releases URL (offline and fleet installs).
    #[arg(long, value_name = "DIR")]
    pub from: Option<PathBuf>,
}

pub const NATIVE: &[NativeSpec] = &[NativeSpec {
    cmd: "self-update",
    flags: &["--check", "--from"],
    placeholders: &[],
}];

pub async fn run(ctx: &Ctx, args: Args) -> Result<ExitCode> {
    upgrade::run(
        ctx,
        upgrade::Args {
            check: args.check,
            from: args.from,
        },
    )
    .await
}
