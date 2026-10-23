//! `show-ref for-each-ref symbolic-ref update-ref check-ref-format var` (SPEC §11.2): the ref
//! plumbing. Ref moves go through `Ctx::update_local_refs`, so a move of the checked-out branch
//! keeps the staged state as git keeps its index (SPEC §9.5); `for-each-ref` is the ref-filter
//! engine of `cli/ref_filter` (SPEC §11.1).

pub(crate) mod args;
mod check_ref_format;
mod for_each_ref;
mod show_ref;
mod symbolic_ref;
mod update_ref;
mod usage;
mod var;

use std::ffi::OsString;
use std::process::ExitCode;

use super::common::{CliError, Ctx, Result};
use super::ref_filter;
use crate::native::NativeSpec;

/// Serves six commands: clap's multicall mode picks the subcommand from the command name.
#[derive(clap::Parser, Debug)]
#[command(multicall = true, disable_help_flag = true, disable_version_flag = true, disable_help_subcommand = true)]
pub struct Args {
    #[command(subcommand)]
    which: Which,
}

#[derive(clap::Subcommand, Debug)]
enum Which {
    #[command(name = "show-ref", disable_help_flag = true, disable_version_flag = true)]
    ShowRef(Rest),
    #[command(name = "for-each-ref", disable_help_flag = true, disable_version_flag = true)]
    ForEachRef(Rest),
    #[command(name = "symbolic-ref", disable_help_flag = true, disable_version_flag = true)]
    SymbolicRef(Rest),
    #[command(name = "update-ref", disable_help_flag = true, disable_version_flag = true)]
    UpdateRef(Rest),
    #[command(name = "check-ref-format", disable_help_flag = true, disable_version_flag = true)]
    CheckRefFormat(Rest),
    #[command(name = "var", disable_help_flag = true, disable_version_flag = true)]
    Var(Rest),
}

#[derive(clap::Args, Debug)]
struct Rest {
    #[arg(trailing_var_arg = true, allow_hyphen_values = true, num_args = 0..)]
    rest: Vec<OsString>,
}

pub const NATIVE: &[NativeSpec] = &[
    NativeSpec {
        cmd: "show-ref",
        flags: &[
            "--head", "--tags", "--branches", "--heads", "-d", "--dereference", "-s", "--hash",
            "--abbrev", "-q", "--quiet", "--verify", "--exists",
        ],
        placeholders: &[],
    },
    NativeSpec {
        cmd: "for-each-ref",
        flags: &[
            "--count", "--shell", "-s", "--perl", "-p", "--python", "--tcl", "--sort", "--format",
            "--include-root-refs", "--points-at", "--merged", "--no-merged", "--contains",
            "--no-contains", "--ignore-case", "--omit-empty", "--exclude", "--stdin", "--color",
            "--no-color", "--start-after",
        ],
        placeholders: ref_filter::ATOMS,
    },
    NativeSpec {
        cmd: "symbolic-ref",
        flags: &["-q", "--quiet", "--short", "--no-recurse", "--recurse", "-m", "-d", "--delete"],
        placeholders: &[],
    },
    NativeSpec {
        cmd: "update-ref",
        flags: &["-m", "-d", "--no-deref", "--create-reflog"],
        placeholders: &[],
    },
    NativeSpec {
        cmd: "check-ref-format",
        flags: &[
            "--normalize", "--print", "--allow-onelevel", "--no-allow-onelevel", "--refspec-pattern",
            "--branch",
        ],
        placeholders: &[],
    },
    NativeSpec { cmd: "var", flags: &["-l"], placeholders: &[] },
];

pub async fn run(ctx: &Ctx, a: Args) -> Result<ExitCode> {
    let result = match a.which {
        Which::ShowRef(r) => show_ref::run(ctx, r.rest).await,
        Which::ForEachRef(r) => for_each_ref::run(ctx, r.rest).await,
        Which::SymbolicRef(r) => symbolic_ref::run(ctx, r.rest).await,
        Which::UpdateRef(r) => update_ref::run(ctx, r.rest).await,
        Which::CheckRefFormat(r) => check_ref_format::run(&r.rest),
        Which::Var(r) => var::run(ctx, &r.rest),
    };
    match result {
        Err(CliError::Io(e)) if e.kind() == std::io::ErrorKind::BrokenPipe => Ok(ExitCode::SUCCESS),
        other => other,
    }
}
