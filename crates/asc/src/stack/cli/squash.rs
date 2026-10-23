//! `asc squash [-m <msg>] [-n]` (SPEC §20.2, §20.7): collapses the changes of the current stack,
//! from its bottom up to HEAD's change, into one.
//!
//! The commit has the tip's tree on the bottom change's parent. It takes the bottom change's
//! author, message (`-m` replaces it) and change id (the earliest folded change's). The changes
//! above HEAD's change restack onto it with no merge. `-e` is refused: asc never opens an editor
//! on its own; `-n` keeps the message and is the default.

use std::process::ExitCode;

use asc_core::change::RewriteOp;

use super::info::resolve_change;
use super::rewrite::{Piece, emit, emit_nothing, finish, locate, record, refuse_in_progress, write_piece};
use super::sl::Local;
use crate::cli::common::{CliError, Ctx, Result};
use crate::native::NativeSpec;

/// Longest message accepted from `-m`.
const MAX_MESSAGE: usize = 1024 * 1024;

#[derive(clap::Parser, Debug)]
#[command(name = "squash", disable_help_flag = true, disable_version_flag = true)]
pub struct Args {
    /// The message of the squashed change; each `-m` is one paragraph.
    #[arg(short = 'm', long = "message")]
    pub message: Vec<String>,
    /// Edit the message (refused: asc never opens an editor on its own).
    #[arg(short = 'e', long = "edit")]
    pub edit: bool,
    /// Keep the bottom change's message (the default).
    #[arg(short = 'n', long = "no-edit")]
    pub no_edit: bool,
    #[arg(long)]
    pub json: bool,
}

pub const NATIVE: &[NativeSpec] = &[NativeSpec {
    cmd: "squash",
    flags: &["-m", "--message", "-e", "--edit", "-n", "--no-edit", "--json"],
    placeholders: &[],
}];

super::verb!(SQUASH, "squash");

pub async fn run(ctx: &Ctx, a: Args) -> Result<ExitCode> {
    if a.edit {
        return Err(CliError::Raw("asc: squash: -e needs an editor, which asc never opens on its own; pass -m".into(), 1));
    }
    let message = if a.message.is_empty() {
        None
    } else {
        let mut text = a.message.join("\n\n");
        if text.len() > MAX_MESSAGE {
            return Err(CliError::Raw("asc: squash: the message is too long".into(), 1));
        }
        if !text.ends_with('\n') {
            text.push('\n');
        }
        Some(text.into_bytes())
    };
    let l = Local::open(ctx).await?;
    let ws = l.ws;
    refuse_in_progress(ws, "squash")?;
    let st = l.stacks().await?;
    let (x, _) = resolve_change(ctx, &l, &st, None).await?;
    let loc = locate(&st, x, "squash").await?;
    drop(st);
    if loc.lower.len() < 2 {
        return emit_nothing(ctx, a.json, "squash");
    }
    let bottom = ws.engine.get_changeset(loc.lower[0]).await?;
    if bottom.parents.len() != 1 {
        return Err(CliError::Raw("asc: squash: the bottom change is a merge".into(), 1));
    }
    let tip = ws.engine.get_changeset(x).await?;
    let op_id = uuid::Uuid::now_v7();
    let sign = ctx.config.bool("commit.gpgsign") == Some(true);
    let new = write_piece(
        ctx,
        ws,
        Piece {
            parent: bottom.parents[0],
            root: tip.root,
            author: bottom.author.clone(),
            message: message.unwrap_or_else(|| bottom.message.clone()),
        },
        sign,
    )
    .await?;
    let made = record(ws, new, &loc.lower, RewriteOp::Squash, None, op_id).await?;
    let pairs: Vec<_> = loc.lower.iter().map(|&o| (o, new)).collect();
    let n = pairs.len();
    let flow = finish(ctx, ws, op_id, pairs, loc.upper.clone(), new).await?;
    emit(ctx, a.json, "squash", "Squashed", &[made], n, &flow)
}
