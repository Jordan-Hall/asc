//! The `asc` binary: argv[0] picks the program (`asc`, or `git` through the shim), then
//! `dispatch` runs the invocation.
// The frame defines the extension points, error types and accessors that the commands of the
// later units use; they are dead code until those units land.
#![allow(dead_code)]

mod branchless;
mod bridge;
mod child;
mod cli;
mod codeintel_local;
mod credential;
mod daemon;
mod dispatch;
mod env;
mod ext;
mod hooks_runner;
mod index_sink;
mod msg;
mod native;
mod refs_guard;
mod remote;
mod shim;
mod stack;
mod telemetry;

use std::ffi::OsString;
use std::process::ExitCode;

use dispatch::Program;

fn main() -> ExitCode {
    let args: Vec<OsString> = std::env::args_os().collect();
    // The `git` shim's fall-through to real git needs no runtime and none of asc's registrations;
    // it is decided here, before either costs time (SPEC §11.4).
    #[cfg(all(feature = "serve", feature = "git-compat"))]
    let hook = cli::serve_git_hook::is_hook_invocation(&args);
    #[cfg(not(all(feature = "serve", feature = "git-compat")))]
    let hook = false;
    let argv0 = args.first().cloned().unwrap_or_default();
    if !hook
        && Program::from_argv0(&argv0) == Program::Git
        && let Some(code) = shim::fall_through(&args)
    {
        return code;
    }
    match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime.block_on(run(args, hook)),
        Err(e) => {
            eprintln!("fatal: asc: cannot start the async runtime: {e}");
            ExitCode::from(128)
        }
    }
}

async fn run(args: Vec<OsString>, hook: bool) -> ExitCode {
    // The `pre-receive` hook of the git protocol layer (SPEC §25.2) runs before anything else is
    // set up: it starts inside a git view, with git's own environment.
    #[cfg(all(feature = "serve", feature = "git-compat"))]
    if hook {
        return cli::serve_git_hook::run().await;
    }
    #[cfg(not(all(feature = "serve", feature = "git-compat")))]
    let _ = hook;
    let argv0 = args.first().cloned().unwrap_or_default();
    let _ = ext::set_ref_txn_hook(Box::new(hooks_runner::RefTxnRunner::new()));
    let _ = ext::set_index_sink(Box::new(index_sink::IndexSink::default()));
    remote::bucket::register();
    remote::server::register();
    remote::projects::register();
    let _ = ext::set_rewrite_sink(Box::new(stack::sinks::Sinks));
    let _ = ext::set_op_sink(Box::new(stack::sinks::Sinks));
    let _ = ext::set_workspace_factory(Box::new(
        daemon::workspace_factory::DaemonWorkspaceFactory::system(),
    ));
    match Program::from_argv0(&argv0) {
        Program::Git => dispatch::run(Program::Git, args).await,
        Program::Asc => dispatch::run(Program::Asc, args).await,
    }
}
