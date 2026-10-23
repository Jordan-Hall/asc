//! `asc continue [-a]` (SPEC §20.7): finishes a paused operation. For a paused restack it runs
//! `rebase --continue` first when git's `rebase-merge/` is still there, then rewrites the rest
//! of the plan and moves the refs. With no stack operation it continues whichever git operation
//! is in progress (rebase, merge, cherry-pick, revert, am), so one verb serves every conflict
//! screen of an editor. `-a` stages every tracked modification first.

use std::process::ExitCode;

use super::restack::{Flow, drive, emit, rebase_paused, run_asc, settle};
use super::sl::fatal;
use crate::cli::common::{CliError, Ctx, Result, WsCtx};
use crate::native::NativeSpec;
use crate::stack::opstate;

#[derive(clap::Parser, Debug)]
#[command(name = "continue", disable_help_flag = true, disable_version_flag = true)]
pub struct Args {
    /// Stage every tracked modification before continuing.
    #[arg(short = 'a', long = "all")]
    pub all: bool,
    #[arg(long)]
    pub json: bool,
}

pub const NATIVE: &[NativeSpec] =
    &[NativeSpec { cmd: "continue", flags: &["-a", "--all", "--json"], placeholders: &[] }];

super::verb!(CONTINUE, "continue");

/// Which git operation is in progress, as the child command that continues or aborts it.
pub(crate) fn git_operation(ws: &WsCtx) -> Option<&'static str> {
    let git = ws.projection.git_dir();
    [
        ("rebase-merge", "rebase"),
        ("rebase-apply", "am"),
        ("MERGE_HEAD", "merge"),
        ("CHERRY_PICK_HEAD", "cherry-pick"),
        ("REVERT_HEAD", "revert"),
    ]
    .into_iter()
    .find(|(f, _)| git.join(f).exists())
    .map(|(_, verb)| verb)
}

/// Runs `asc <verb> <flag>` for the operation in progress and returns its exit code.
pub(crate) fn passthrough(ctx: &Ctx, ws: &WsCtx, flag: &str) -> Result<ExitCode> {
    let Some(verb) = git_operation(ws) else {
        return Err(CliError::Raw("asc: no operation in progress".into(), 1));
    };
    let code = run_asc(ctx, &[verb, flag])?;
    Ok(ExitCode::from(u8::try_from(code).unwrap_or(1)))
}

pub async fn run(ctx: &Ctx, a: Args) -> Result<ExitCode> {
    let ws = ctx.ws()?;
    let git = ws.projection.git_dir().to_owned();
    let op = opstate::load(&git).map_err(|m| CliError::Fatal(format!("asc: {m}")))?;
    let Some(mut op) = op else {
        if a.all {
            let _ = run_asc(ctx, &["add", "-u"])?;
        }
        return passthrough(ctx, ws, "--continue");
    };
    if rebase_paused(ws) {
        if a.all && run_asc(ctx, &["add", "-u"])? != 0 {
            return Err(fatal("continue: staging the tracked changes failed"));
        }
        let code = run_asc(ctx, &["rebase", "-q", "--continue"])?;
        if code != 0 {
            if rebase_paused(ws) {
                eprintln!("asc: the restack is still paused; fix the conflicts, mark them with 'asc add', then run 'asc continue' again, or run 'asc abort'.");
                return Ok(ExitCode::from(1));
            }
            return Err(fatal(format!("continue: the rebase failed (exit {code})")));
        }
    }
    let replayed = settle(ctx, ws, &op).await?;
    let mut done = std::mem::take(&mut op.done);
    done.push((op.at, replayed));
    let rest = std::mem::take(&mut op.rest);
    let flow = drive(ctx, ws, op, done, replayed, rest).await?;
    if let Flow::Paused(_) = flow {
        // `drive` already told the user what stopped.
    }
    emit(ctx, a.json, &flow)
}
