//! `ci <subcommand>` (SPEC §12): `checkout` is built (U62); `changed`, `affected` and `warm`
//! (U63) and `cache` and `artifact` (U75) are built; `prebuilt` serves the build path level (§15.5.1);
//! `status` and `log` read the runs recorded against a change (§12.9).

use std::ffi::OsString;
use std::process::ExitCode;

use crate::cli::common::{CliError, Ctx, Result};
use crate::native::NativeSpec;

mod affected;
mod artifact;
pub(crate) mod cache;
mod changed;
pub mod checkout;
mod prebuilt;
mod runs;
mod warm;

#[derive(clap::Parser, Debug)]
#[command(name = "ci", disable_help_flag = true, disable_version_flag = true)]
pub struct Args {
    #[arg(trailing_var_arg = true, allow_hyphen_values = true, num_args = 0..)]
    pub rest: Vec<OsString>,
}

pub const NATIVE: &[NativeSpec] = &[
    NativeSpec {
        cmd: "ci",
        flags: checkout::FLAGS,
        placeholders: &[],
    },
    NativeSpec {
        cmd: "ci",
        flags: changed::FLAGS,
        placeholders: &[],
    },
    NativeSpec {
        cmd: "ci",
        flags: warm::FLAGS,
        placeholders: &[],
    },
    NativeSpec {
        cmd: "ci",
        flags: cache::FLAGS,
        placeholders: &[],
    },
    NativeSpec {
        cmd: "ci",
        flags: artifact::FLAGS,
        placeholders: &[],
    },
    NativeSpec {
        cmd: "ci",
        flags: prebuilt::FLAGS,
        placeholders: &[],
    },
    NativeSpec {
        cmd: "ci",
        flags: runs::FLAGS,
        placeholders: &[],
    },
];

const USAGE: &str =
    "usage: asc ci <status|log|checkout|changed|affected|warm|cache|artifact|prebuilt> [<args>]";

pub async fn run(ctx: &Ctx, a: Args) -> Result<ExitCode> {
    let Some((sub, rest)) = a.rest.split_first() else {
        return Err(CliError::Usage(USAGE.to_owned()));
    };
    match sub.to_str() {
        Some("checkout") => checkout::run(ctx, rest).await,
        Some("changed") => changed::run(ctx, rest).await,
        Some("affected") => affected::run(ctx, rest).await,
        Some("warm") => warm::run(ctx, rest).await,
        Some("cache") => cache::run(ctx, rest).await,
        Some("artifact") => artifact::run(ctx, rest).await,
        Some("prebuilt") => prebuilt::run(ctx, rest).await,
        Some("status") => runs::status(ctx, rest).await,
        Some("log") => runs::log(ctx, rest).await,
        _ => Err(CliError::Usage(USAGE.to_owned())),
    }
}
