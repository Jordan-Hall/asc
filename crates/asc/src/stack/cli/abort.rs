//! `asc abort` (SPEC §20.7): restores the before-state of a paused operation. A restack moves no
//! ref until it finishes, so abort only has to drop git's `rebase-merge/` (through
//! `rebase --abort`), put HEAD back where the restack began and delete `asc-stack-op.json`. With
//! no stack operation it aborts whichever git operation is in progress.

use std::process::ExitCode;

use super::continue_::passthrough;
use super::restack::{reattach, rebase_paused, run_asc};
use super::sl::fatal;
use crate::cli::common::{CliError, Ctx, Result};
use crate::cli::switch::{self, Dest};
use crate::native::NativeSpec;
use crate::stack::opstate;

#[derive(clap::Parser, Debug)]
#[command(name = "abort", disable_help_flag = true, disable_version_flag = true)]
pub struct Args {}

pub const NATIVE: &[NativeSpec] = &[NativeSpec { cmd: "abort", flags: &[], placeholders: &[] }];

super::verb!(ABORT, "abort");

pub async fn run(ctx: &Ctx, _a: Args) -> Result<ExitCode> {
    let ws = ctx.ws()?;
    let git = ws.projection.git_dir().to_owned();
    let op = opstate::load(&git).map_err(|m| CliError::Fatal(format!("asc: {m}")))?;
    let Some(op) = op else { return passthrough(ctx, ws, "--abort") };
    if rebase_paused(ws) {
        let code = run_asc(ctx, &["rebase", "-q", "--abort"])?;
        if code != 0 && rebase_paused(ws) {
            return Err(fatal(format!("abort: 'rebase --abort' failed (exit {code}); the restack state was kept")));
        }
    }
    let dest = match &op.head_branch {
        Some(b) => match switch::branch_tip(ws, b).await? {
            Some(tip) => Dest::Existing { branch: b.clone(), tip: Some(tip) },
            None => Dest::Detach { commit: op.head },
        },
        None => Dest::Detach { commit: op.head },
    };
    reattach(ctx, dest).await?;
    opstate::clear(&git);
    eprintln!("asc: restack aborted; no branch was moved.");
    Ok(ExitCode::SUCCESS)
}
