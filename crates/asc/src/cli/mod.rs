//! The commands. One file per command (or per family) holds its clap `Args`, its `NATIVE`
//! table and `pub async fn run(ctx, args)`; a command whose unit has not landed is a stub that
//! `stub!` generates. This file lists the modules and the command table; after U20 it changes
//! only by registration lines (PLAN "Stubs and extension points").

use std::ffi::OsString;
use std::future::Future;
use std::pin::Pin;
use std::process::ExitCode;

use crate::native::{self, NativeSpec, Route};

/// Defines the stub of a command: no flags are served, so the native table routes every
/// invocation to the "not supported yet" error before `run` is reached.
macro_rules! stub {
    ($name:literal) => {
        #[derive(clap::Parser, Debug)]
        #[command(name = $name, disable_help_flag = true, disable_version_flag = true)]
        pub struct Args {
            #[arg(trailing_var_arg = true, allow_hyphen_values = true, num_args = 0..)]
            pub rest: Vec<std::ffi::OsString>,
        }

        /// Nothing is served until the owning unit lands.
        pub const NATIVE: &[$crate::native::NativeSpec] = &[];

        pub async fn run(
            _ctx: &$crate::cli::common::Ctx,
            _args: Args,
        ) -> $crate::cli::common::Result<std::process::ExitCode> {
            Err($crate::cli::common::CliError::Msg($crate::msg::Msg::AscNotYet {
                cmd: $name.to_owned(),
            }))
        }
    };
}

pub mod common;
pub mod config;
pub mod init;
pub mod json;
pub mod output;
pub mod pager;

pub mod add;
pub mod affected;
pub mod am;
pub mod api;
pub mod apply;
pub mod archive;
pub mod auth;
pub mod bisect;
pub mod blame;
pub mod branch;
pub mod check_attr;
pub mod check_ignore;
pub mod checkout;
pub mod cherry;
pub mod ci;
pub mod claude_hook;
pub mod clean;
pub mod clone;
pub mod commit;
pub mod commit_tree;
pub mod count_objects;
pub mod daemon;
pub mod debug;
pub mod describe;
pub mod diff;
pub mod diff_tree;
pub mod doctor;
pub mod export;
pub mod fetch;
pub mod format_patch;
pub mod fsck;
pub mod gc;
pub mod graph;
pub mod grep;
pub mod help_git;
pub mod import;
pub mod interpret_trailers;
pub mod landed;
pub mod log;
pub mod ls_remote;
pub mod mcp;
pub mod merge;
pub mod merge_tree;
pub mod migrate_ignore;
pub mod mktree;
pub mod mv;
pub mod name_rev;
pub mod noop_maint;
pub mod notes;
pub mod pick;
pub mod plumbing;
pub mod prefetch;
pub mod projects;
pub mod pull;
pub mod push;
pub mod range_diff;
pub mod rebase;
pub mod reflog;
pub mod refs_plumbing;
pub mod remote;
pub mod reset;
pub mod restore;
pub mod revert;
pub mod revlist;
pub mod revparse;
pub mod rm;
pub mod serve;
pub mod shortlog;
pub mod show;
pub mod sparse;
pub mod stash;
pub mod status;
pub mod submodule;
pub mod switch;
pub mod tag;
pub mod upgrade;
pub mod watch;
pub mod whatchanged;
pub mod worktree;

use common::{CliError, Ctx, Result};

/// The future a command returns.
pub type BoxFut<'a> = Pin<Box<dyn Future<Output = Result<ExitCode>> + 'a>>;

/// One entry of the command table.
pub struct Command {
    pub name: &'static str,
    pub native: &'static [NativeSpec],
    pub run: for<'a> fn(&'a Ctx, Vec<OsString>) -> BoxFut<'a>,
}

fn usage(err: clap::Error) -> CliError {
    CliError::Usage(err.render().to_string().trim_end().to_owned())
}

macro_rules! entry {
    ($name:literal, $m:ident) => {
        Command {
            name: $name,
            native: $m::NATIVE,
            run: {
                fn go<'a>(ctx: &'a Ctx, argv: Vec<OsString>) -> BoxFut<'a> {
                    Box::pin(async move {
                        let args = <$m::Args as clap::Parser>::try_parse_from(argv).map_err(usage)?;
                        $m::run(ctx, args).await
                    })
                }
                go
            },
        }
    };
}

/// Every command, by the name typed after `git`/`asc`. Registering a command is one line here.
pub static COMMANDS: &[Command] = &[
    entry!("init", init),
    entry!("config", config),
    entry!("clone", clone),
    entry!("worktree", worktree),
    entry!("status", status),
    entry!("add", add),
    entry!("rm", rm),
    entry!("mv", mv),
    entry!("restore", restore),
    entry!("reset", reset),
    entry!("commit", commit),
    entry!("switch", switch),
    entry!("checkout", checkout),
    entry!("branch", branch),
    entry!("tag", tag),
    entry!("fetch", fetch),
    entry!("pull", pull),
    entry!("push", push),
    entry!("merge", merge),
    entry!("rebase", rebase),
    entry!("cherry-pick", pick),
    entry!("revert", revert),
    entry!("stash", stash),
    entry!("log", log),
    entry!("show", show),
    entry!("diff", diff),
    entry!("blame", blame),
    entry!("grep", grep),
    entry!("remote", remote),
    entry!("rev-parse", revparse),
    entry!("rev-list", revlist),
    entry!("merge-base", revlist),
    entry!("describe", describe),
    entry!("reflog", reflog),
    entry!("show-ref", refs_plumbing),
    entry!("for-each-ref", refs_plumbing),
    entry!("symbolic-ref", refs_plumbing),
    entry!("update-ref", refs_plumbing),
    entry!("check-ref-format", refs_plumbing),
    entry!("var", refs_plumbing),
    entry!("ls-files", plumbing),
    entry!("ls-tree", plumbing),
    entry!("cat-file", plumbing),
    entry!("hash-object", plumbing),
    entry!("update-index", plumbing),
    entry!("write-tree", plumbing),
    entry!("read-tree", plumbing),
    entry!("diff-tree", diff_tree),
    entry!("diff-index", diff_tree),
    entry!("diff-files", diff_tree),
    entry!("check-ignore", check_ignore),
    entry!("check-attr", check_attr),
    entry!("clean", clean),
    entry!("apply", apply),
    entry!("am", am),
    entry!("format-patch", format_patch),
    entry!("bisect", bisect),
    entry!("commit-tree", commit_tree),
    entry!("mktree", mktree),
    entry!("merge-tree", merge_tree),
    entry!("interpret-trailers", interpret_trailers),
    entry!("ls-remote", ls_remote),
    entry!("count-objects", count_objects),
    entry!("shortlog", shortlog),
    entry!("name-rev", name_rev),
    entry!("whatchanged", whatchanged),
    entry!("range-diff", range_diff),
    entry!("cherry", cherry),
    entry!("notes", notes),
    entry!("gc", gc),
    entry!("maintenance", noop_maint),
    entry!("prune", noop_maint),
    entry!("repack", noop_maint),
    entry!("fsck", fsck),
    entry!("sparse-checkout", sparse),
    entry!("archive", archive),
    entry!("submodule", submodule),
    entry!("help", help_git),
    entry!("daemon", daemon),
    entry!("serve", serve),
    entry!("import", import),
    entry!("export", export),
    entry!("auth", auth),
    entry!("doctor", doctor),
    entry!("prefetch", prefetch),
    entry!("sparse", sparse),
    entry!("debug", debug),
    entry!("ci", ci),
    entry!("affected", affected),
    entry!("projects", projects),
    entry!("graph", graph),
    entry!("landed", landed),
    entry!("api", api),
    entry!("watch", watch),
    entry!("mcp", mcp),
    entry!("migrate", migrate_ignore),
    entry!("upgrade", upgrade),
    entry!("internal", claude_hook),
];

/// The command with this name.
pub fn find(name: &str) -> Option<&'static Command> {
    COMMANDS.iter().find(|c| c.name == name)
}

/// Whether a command of this name has an entry (stub or real).
pub fn is_registered(name: &str) -> bool {
    find(name).is_some()
}

/// Every `NATIVE` spec of every command (SPEC §11.5).
pub fn native_specs() -> impl Iterator<Item = &'static NativeSpec> {
    COMMANDS.iter().flat_map(|c| c.native.iter())
}

/// Routes `argv` (`[command, args…]`) through the native table and runs the command.
pub async fn run_command(ctx: &Ctx, argv: Vec<OsString>) -> Result<ExitCode> {
    let name = argv
        .first()
        .and_then(|a| a.to_str())
        .unwrap_or_default()
        .to_owned();
    if let Route::Unsupported(msg) = native::route(&argv) {
        return Err(CliError::Msg(msg));
    }
    let Some(command) = find(&name) else {
        return Err(CliError::Msg(native::not_served(&name)));
    };
    (command.run)(ctx, argv).await
}
