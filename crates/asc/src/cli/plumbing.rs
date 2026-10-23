//! `ls-files ls-tree cat-file hash-object update-index write-tree read-tree` (SPEC §11.2): the
//! object and index plumbing. The index is the HEAD tree overlaid with the stage rows of the set
//! `GIT_INDEX_FILE` names (SPEC §9.5); git ids of trees and blobs are recorded in the git map as
//! they are printed, so one command's output is the next one's input.

mod cat_file;
mod hash_object;
mod index;
mod ls_files;
mod ls_tree;
pub(crate) mod objects;
mod quote;
mod update_index;
mod usage;
mod write_tree;

use std::ffi::OsString;
use std::process::ExitCode;

use super::common::{CliError, Ctx, Result};
use crate::native::NativeSpec;

/// Serves seven commands: clap's multicall mode picks the subcommand from the command name.
#[derive(clap::Parser, Debug)]
#[command(multicall = true, disable_help_flag = true, disable_version_flag = true, disable_help_subcommand = true)]
pub struct Args {
    #[command(subcommand)]
    which: Which,
}

#[derive(clap::Subcommand, Debug)]
enum Which {
    #[command(name = "ls-files", disable_help_flag = true, disable_version_flag = true)]
    LsFiles(Rest),
    #[command(name = "ls-tree", disable_help_flag = true, disable_version_flag = true)]
    LsTree(Rest),
    #[command(name = "cat-file", disable_help_flag = true, disable_version_flag = true)]
    CatFile(Rest),
    #[command(name = "hash-object", disable_help_flag = true, disable_version_flag = true)]
    HashObject(Rest),
    #[command(name = "update-index", disable_help_flag = true, disable_version_flag = true)]
    UpdateIndex(Rest),
    #[command(name = "write-tree", disable_help_flag = true, disable_version_flag = true)]
    WriteTree(Rest),
    #[command(name = "read-tree", disable_help_flag = true, disable_version_flag = true)]
    ReadTree(Rest),
}

#[derive(clap::Args, Debug)]
struct Rest {
    #[arg(trailing_var_arg = true, allow_hyphen_values = true, num_args = 0..)]
    rest: Vec<OsString>,
}

pub const NATIVE: &[NativeSpec] = &[
    NativeSpec {
        cmd: "ls-files",
        flags: &[
            "-c", "--cached", "-s", "--stage", "-o", "--others", "--exclude-standard", "-z", "--full-name",
            "--error-unmatch",
        ],
        placeholders: &[],
    },
    NativeSpec {
        cmd: "ls-tree",
        flags: &[
            "-d", "-r", "-t", "-l", "--long", "-z", "--name-only", "--name-status", "--object-only", "--abbrev",
            "--full-name", "--full-tree", "--format",
        ],
        placeholders: &["%(objectmode)", "%(objecttype)", "%(objectname)", "%(objectsize)", "%(path)", "%x"],
    },
    NativeSpec {
        cmd: "cat-file",
        flags: &["-t", "-s", "-e", "-p", "--batch", "--batch-check", "--buffer"],
        placeholders: &[],
    },
    NativeSpec {
        cmd: "hash-object",
        flags: &["-t", "-w", "--stdin", "--path", "--no-filters", "--literally"],
        placeholders: &[],
    },
    NativeSpec {
        cmd: "update-index",
        flags: &["--add", "--remove", "--force-remove", "--refresh", "--really-refresh", "--cacheinfo", "-q"],
        placeholders: &[],
    },
    NativeSpec { cmd: "write-tree", flags: &["--missing-ok", "--prefix"], placeholders: &[] },
    NativeSpec { cmd: "read-tree", flags: &["-m", "-i", "--empty", "--prefix"], placeholders: &[] },
];

pub async fn run(ctx: &Ctx, a: Args) -> Result<ExitCode> {
    let result = match a.which {
        Which::LsFiles(r) => ls_files::run(ctx, r.rest).await,
        Which::LsTree(r) => ls_tree::run(ctx, r.rest).await,
        Which::CatFile(r) => cat_file::run(ctx, r.rest).await,
        Which::HashObject(r) => hash_object::run(ctx, r.rest).await,
        Which::UpdateIndex(r) => update_index::run(ctx, r.rest).await,
        Which::WriteTree(r) => write_tree::write_tree(ctx, r.rest).await,
        Which::ReadTree(r) => write_tree::read_tree(ctx, r.rest).await,
    };
    match result {
        Err(CliError::Io(e)) if e.kind() == std::io::ErrorKind::BrokenPipe => Ok(ExitCode::SUCCESS),
        other => other,
    }
}
