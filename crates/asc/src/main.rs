//! The `asc` binary: argv[0] picks the program (`asc`, or `git` through the shim), then
//! `dispatch` runs the invocation.
// The frame defines the extension points, error types and accessors that the commands of the
// later units use; they are dead code until those units land.
#![allow(dead_code)]

mod child;
mod cli;
mod dispatch;
mod env;
mod ext;
mod msg;
mod native;
mod refs_guard;

use std::ffi::OsString;
use std::process::ExitCode;

use dispatch::Program;

/// The git shim (SPEC §11.4). U20a replaces this module with `shim.rs`; until then an
/// invocation as `git` is handed straight to `dispatch`, as the shim does inside a workspace.
mod shim {
    use std::ffi::OsString;
    use std::process::ExitCode;

    pub(crate) async fn run(args: Vec<OsString>) -> ExitCode {
        crate::dispatch::run(crate::dispatch::Program::Git, args).await
    }
}

#[tokio::main]
async fn main() -> ExitCode {
    let args: Vec<OsString> = std::env::args_os().collect();
    let argv0 = args.first().cloned().unwrap_or_default();
    match Program::from_argv0(&argv0) {
        Program::Git => shim::run(args).await,
        Program::Asc => dispatch::run(Program::Asc, args).await,
    }
}
