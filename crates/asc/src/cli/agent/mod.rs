//! `asc agent`: agents that work on a repository and leave a stack of changes for review, the
//! tokens of agents that run elsewhere, and the AI keys they use (docs/AGENTS.md).
//!
//! ```text
//! asc agent run [--agent NAME] [--path GLOB]... [--max-cost USD] [--watch] <task...>
//! asc agent list | show <id> | watch <id> | cancel <id> | say <id> <text...>
//! asc agent token create|list|show|revoke|budget
//! asc agent key add|list|show|test|update|rm|models|usage
//! ```
//!
//! Every command works against a server (`--remote`, default `origin`) and needs no workspace.
//! The server is the authority: it enforces the scope, the budgets and that an agent never writes
//! trunk. A secret (an AI key, a token) is read from a file, an environment variable or standard
//! input, never from an argument, and is shown once at most.

pub(crate) mod conn;
mod key;
mod runs;
mod token;

use std::process::ExitCode;

use super::common::{CliError, Ctx, Result};
use crate::native::NativeSpec;

#[derive(clap::Parser, Debug)]
#[command(
    name = "agent",
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
    /// Starts an agent run from a task.
    Run(runs::RunArgs),
    /// Lists runs, newest first.
    List(runs::ListArgs),
    /// Shows one run.
    Show(runs::ShowArgs),
    /// Follows the events of a run.
    Watch(runs::WatchArgs),
    /// Stops a run.
    Cancel(runs::CancelArgs),
    /// Sends a message to a run.
    Say(runs::SayArgs),
    /// Tokens for agents that run outside the server.
    Token(token::TokenArgs),
    /// AI provider keys.
    Key(key::KeyArgs),
}

pub const NATIVE: &[NativeSpec] = &[NativeSpec {
    cmd: "agent",
    flags: &[
        "--agent",
        "--title",
        "--target",
        "--base",
        "--path",
        "--key",
        "--model",
        "--max-changes",
        "--max-tokens",
        "--max-cost",
        "--max-seconds",
        "--max-paths",
        "--watch",
        "-w",
        "-F",
        "--file",
        "--remote",
        "--json",
        "--state",
        "--by",
        "--limit",
        "--page",
        "--after",
        "--reason",
        "--name",
        "--ttl",
        "--max-changes-per-run",
        "--provider",
        "--owner",
        "--label",
        "--base-url",
        "--monthly-cap",
        "--key-env",
        "--key-file",
        "--enable",
        "--disable",
        "--group-by",
        "--since",
        "--until",
    ],
    placeholders: &[],
}];

pub async fn run(ctx: &Ctx, a: Args) -> Result<ExitCode> {
    match a.cmd {
        Sub::Run(x) => runs::run(ctx, x).await,
        Sub::List(x) => runs::list(ctx, x).await,
        Sub::Show(x) => runs::show(ctx, x).await,
        Sub::Watch(x) => runs::watch(ctx, x).await,
        Sub::Cancel(x) => runs::cancel(ctx, x).await,
        Sub::Say(x) => runs::say(ctx, x).await,
        Sub::Token(x) => token::run(ctx, x).await,
        Sub::Key(x) => key::run(ctx, x).await,
    }
}

/// A usage failure with the text of the command.
pub(crate) fn bad(text: impl Into<String>) -> CliError {
    CliError::Usage(format!("asc agent: {}", text.into()))
}

/// Reads a secret without ever taking it from an argument: from the file `file`, the environment
/// variable `env`, or standard input when neither is named. Surrounding whitespace is dropped.
pub(crate) fn read_secret(file: Option<&str>, env: Option<&str>, what: &str) -> Result<String> {
    use std::io::Read as _;
    let raw = if let Some(path) = file {
        std::fs::read_to_string(path)
            .map_err(|e| CliError::Fatal(format!("asc: cannot read {path}: {}", e.kind())))?
    } else if let Some(var) = env {
        std::env::var(var).map_err(|_| {
            CliError::Fatal(format!("asc: the environment variable {var} is not set"))
        })?
    } else {
        if std::io::IsTerminal::is_terminal(&std::io::stdin()) {
            eprintln!("asc: reading {what} from standard input; end it with a newline and Ctrl-D");
        }
        let mut s = String::new();
        std::io::stdin()
            .take(64 * 1024)
            .read_to_string(&mut s)
            .map_err(|e| {
                CliError::Fatal(format!("asc: cannot read standard input: {}", e.kind()))
            })?;
        s
    };
    let value = raw.trim().to_owned();
    if value.is_empty() {
        return Err(CliError::Fatal(format!("asc: the {what} is empty")));
    }
    Ok(value)
}
