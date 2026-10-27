//! `asc workflow`: the workflows of a repository (`.asc/workflows/<name>.toml` on trunk), their
//! runs, approvals and the secrets their command steps read (`Workflows`).
//!
//! ```text
//! asc workflow list | show <name> | validate <name> [-F file] | enable <name> | disable <name>
//! asc workflow run <name> [--input k=v]... [--change ID] [-w]
//! asc workflow runs [<name>] [--state S]... | status <id> | watch <id> | cancel <id>
//! asc workflow approve <id> <step> [--comment ...] | reject <id> <step> [--comment ...]
//! asc workflow secret list | set <name> | rm <name>
//! ```
//!
//! The server is the authority: it reads the definition from the trunk tip, never from an update,
//! and decides who may run, approve or administer. A secret value is read from a file, an
//! environment variable or standard input, never from an argument, and is never shown again.

mod runs;
mod secrets;

use std::process::ExitCode;

use asc_rpc::server::wire;
use asc_rpc::services::workflows_client::WorkflowsClient;
use tonic::transport::Channel;

use super::agent::conn::{MAX_MESSAGE, short};
use super::common::{CliError, Ctx, Result};
use crate::native::NativeSpec;
use crate::remote::server::Session;

pub(crate) fn client(s: &Session) -> WorkflowsClient<Channel> {
    WorkflowsClient::new(s.wire.channel.clone())
        .max_decoding_message_size(MAX_MESSAGE)
        .max_encoding_message_size(MAX_MESSAGE)
}

/// A usage failure with the text of the command.
pub(crate) fn bad(text: impl Into<String>) -> CliError {
    CliError::Usage(format!("asc workflow: {}", text.into()))
}

#[derive(clap::Parser, Debug)]
#[command(
    name = "workflow",
    disable_help_flag = true,
    disable_version_flag = true,
    disable_help_subcommand = true
)]
pub struct Args {
    #[command(subcommand)]
    pub cmd: Sub,
}

#[derive(clap::Subcommand, Debug)]
pub enum Sub {
    /// Lists the workflows on trunk.
    List(runs::ListArgs),
    /// Shows one workflow and its file.
    Show(runs::ShowArgs),
    /// Checks a workflow file without keeping anything.
    Validate(runs::ValidateArgs),
    /// Lets a workflow start from its triggers.
    Enable(runs::EnableArgs),
    /// Stops a workflow from starting.
    Disable(runs::EnableArgs),
    /// Starts a run by hand.
    Run(runs::RunArgs),
    /// Lists runs, newest first.
    Runs(runs::RunsArgs),
    /// Shows one run with its steps.
    Status(runs::StatusArgs),
    /// Follows a run until it ends.
    Watch(runs::WatchArgs),
    /// Stops a run.
    Cancel(runs::CancelArgs),
    /// Approves an approval step.
    Approve(runs::DecideArgs),
    /// Rejects an approval step.
    Reject(runs::DecideArgs),
    /// The secrets the command steps read.
    Secret(secrets::SecretArgs),
}

pub const NATIVE: &[NativeSpec] = &[NativeSpec {
    cmd: "workflow",
    flags: &[
        "--remote",
        "--json",
        "--state",
        "--input",
        "--change",
        "--watch",
        "-w",
        "-F",
        "--file",
        "--comment",
        "--reason",
        "--limit",
        "--page",
        "--after",
        "--key-env",
        "--key-file",
    ],
    placeholders: &[],
}];

pub async fn run(ctx: &Ctx, a: Args) -> Result<ExitCode> {
    match a.cmd {
        Sub::List(x) => runs::list(ctx, x).await,
        Sub::Show(x) => runs::show(ctx, x).await,
        Sub::Validate(x) => runs::validate(ctx, x).await,
        Sub::Enable(x) => runs::set_enabled(ctx, x, true).await,
        Sub::Disable(x) => runs::set_enabled(ctx, x, false).await,
        Sub::Run(x) => runs::trigger(ctx, x).await,
        Sub::Runs(x) => runs::list_runs(ctx, x).await,
        Sub::Status(x) => runs::status(ctx, x).await,
        Sub::Watch(x) => runs::watch(ctx, x).await,
        Sub::Cancel(x) => runs::cancel(ctx, x).await,
        Sub::Approve(x) => runs::decide(ctx, x, true).await,
        Sub::Reject(x) => runs::decide(ctx, x, false).await,
        Sub::Secret(x) => secrets::run(ctx, x).await,
    }
}

/// A workflow or step name: the characters the server accepts, so a bad name fails here, with a
/// message, and not after a round trip.
pub(crate) fn name_ok(what: &str, s: &str) -> Result<()> {
    let ok = !s.is_empty()
        && s.len() <= 64
        && s.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '-' | '_'));
    if ok {
        Ok(())
    } else {
        Err(bad(format!("{what} {s:?} is 1 to 64 of a-z, 0-9, - and _")))
    }
}

pub(crate) fn run_state(n: i32) -> String {
    wire::WorkflowRunState::try_from(n).map_or_else(
        |_| "?".to_owned(),
        |s| short(s.as_str_name(), "WORKFLOW_RUN_STATE_"),
    )
}

pub(crate) fn step_state(n: i32) -> String {
    wire::WorkflowStepState::try_from(n).map_or_else(
        |_| "?".to_owned(),
        |s| short(s.as_str_name(), "WORKFLOW_STEP_STATE_"),
    )
}
