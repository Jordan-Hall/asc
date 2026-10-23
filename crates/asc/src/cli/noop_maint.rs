//! `maintenance`, `prune` and `repack` (SPEC §11.2): asc has no packfiles and no loose objects,
//! so there is nothing to repack or prune and `maintenance` has no tasks (`asc gc` evicts the
//! cache; the daemon schedules the rest). Each prints "not needed in asc" and exits 0.
//!
//! The flags of the three git commands are accepted so scripts that pass them keep working;
//! none of them changes anything here, and operands (`maintenance run`, `prune <head>`) are
//! accepted the same way.

use std::ffi::OsString;
use std::process::ExitCode;

use super::common::{Ctx, Result};
use crate::native::NativeSpec;

#[derive(clap::Parser, Debug)]
#[command(
    name = "maintenance",
    disable_help_flag = true,
    disable_version_flag = true
)]
pub struct Args {
    #[arg(trailing_var_arg = true, allow_hyphen_values = true, num_args = 0..)]
    pub rest: Vec<OsString>,
}

pub const NATIVE: &[NativeSpec] = &[
    NativeSpec {
        cmd: "repack",
        flags: &[
            "-a",
            "-A",
            "-d",
            "-l",
            "-f",
            "-F",
            "-q",
            "-n",
            "-k",
            "--quiet",
            "--no-quiet",
            "--local",
            "--no-write-bitmap-index",
            "--write-bitmap-index",
            "--keep-unreachable",
            "--pack-kept-objects",
            "--unpack-unreachable",
        ],
        placeholders: &[],
    },
    NativeSpec {
        cmd: "prune",
        flags: &[
            "-n",
            "--dry-run",
            "-v",
            "--verbose",
            "--progress",
            "--no-progress",
            "--expire",
        ],
        placeholders: &[],
    },
    NativeSpec {
        cmd: "maintenance",
        flags: &["--auto", "--quiet", "--no-quiet", "--task", "--schedule"],
        placeholders: &[],
    },
];

/// Whether the arguments ask for silence.
fn quiet(rest: &[OsString]) -> bool {
    let mut quiet = false;
    for a in rest {
        match a.to_str() {
            Some("--") => break,
            Some("-q" | "--quiet") => quiet = true,
            Some("--no-quiet") => quiet = false,
            _ => {}
        }
    }
    quiet
}

pub async fn run(_ctx: &Ctx, args: Args) -> Result<ExitCode> {
    if !quiet(&args.rest) {
        eprintln!("not needed in asc");
    }
    Ok(ExitCode::SUCCESS)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn os(a: &[&str]) -> Vec<OsString> {
        a.iter().map(OsString::from).collect()
    }

    #[test]
    fn quiet_flags() {
        assert!(quiet(&os(&["-q"])));
        assert!(quiet(&os(&["--quiet"])));
        assert!(!quiet(&os(&["--quiet", "--no-quiet"])));
        assert!(!quiet(&os(&["-a", "-d"])));
    }
}
