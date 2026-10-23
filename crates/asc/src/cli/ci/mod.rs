//! `ci <subcommand>` (SPEC §12): `checkout` is built (U62); `changed`, `affected` and `warm`
//! (U63) and `cache` and `artifact` (U75) are built.

use std::ffi::OsString;
use std::process::ExitCode;

use crate::cli::common::{CliError, Ctx, Result};
use crate::native::NativeSpec;

mod affected;
mod artifact;
mod cache;
mod changed;
pub mod checkout;
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
];

const USAGE: &str = "usage: asc ci <checkout|changed|affected|warm|cache|artifact> [<args>]";

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
        _ => Err(CliError::Usage(USAGE.to_owned())),
    }
}
