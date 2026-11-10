//! `asc fold [<change>] [--keep] [--stack]` (SPEC §20.2, §20.7): folds a change into its parent
//! change and reparents the changes above it.
//!
//! The folded commit has the change's tree on the parent's parent, so the changes above restack
//! onto it with no merge. It keeps the parent's message and author (`--keep`: the folded
//! change's message) and the change id of the earliest folded change, the parent's. `--stack`
//! folds the change and every change above it into the parent.

use std::process::ExitCode;

use asc_core::change::RewriteOp;
use asc_core::id::ChangesetId;

use super::info::resolve_change;
use super::rewrite::{Piece, emit, finish, locate, record, refuse_in_progress, write_piece};
use super::sl::Local;
use crate::cli::common::{CliError, Ctx, Result};
use crate::native::NativeSpec;

#[derive(clap::Parser, Debug)]
#[command(name = "fold", disable_help_flag = true, disable_version_flag = true)]
pub struct Args {
    /// The change to fold; default the current HEAD.
    pub change: Option<String>,
    /// Keep the folded change's message instead of the parent's.
    #[arg(long)]
    pub keep: bool,
    /// Fold the change and everything above it into the parent.
    #[arg(long)]
    pub stack: bool,
    #[arg(long)]
    pub json: bool,
}

pub const NATIVE: &[NativeSpec] =
    &[NativeSpec { cmd: "fold", flags: &["--keep", "--stack", "--json"], placeholders: &[] }];

super::verb!(FOLD, "fold");

pub async fn run(ctx: &Ctx, a: Args) -> Result<ExitCode> {
    let l = Local::open(ctx).await?;
    let ws = l.ws;
    refuse_in_progress(ws, "fold")?;
    let st = l.stacks().await?;
    let (x, _) = resolve_change(ctx, &l, &st, a.change.as_deref()).await?;
    let loc = locate(&st, x, "fold").await?;
    drop(st);
    let [.., p, _] = loc.lower.as_slice() else {
        return Err(CliError::Raw("asc: fold: the change has no parent change to fold into".into(), 1));
    };
    let p = *p;
    let cp = ws.engine.get_changeset(p).await?;
    let cx = ws.engine.get_changeset(x).await?;
    if cp.parents.len() != 1 {
        return Err(CliError::Raw("asc: fold: the parent change is a merge".into(), 1));
    }
    let (folded_upper, steps): (Vec<ChangesetId>, Vec<ChangesetId>) =
        if a.stack { (loc.upper.clone(), Vec::new()) } else { (Vec::new(), loc.upper.clone()) };
    let top = folded_upper.last().copied().unwrap_or(x);
    let top_root = ws.engine.get_changeset(top).await?.root;
    let op_id = uuid::Uuid::now_v7();
    let sign = ctx.config.bool("commit.gpgsign") == Some(true);
    let new = write_piece(
        ctx,
        ws,
        Piece {
            parent: cp.parents[0],
            root: top_root,
            author: cp.author.clone(),
            message: if a.keep { cx.message.clone() } else { cp.message.clone() },
        },
        sign,
    )
    .await?;
    let mut preds = vec![p, x];
    preds.extend(folded_upper.iter().copied());
    let made = record(ws, new, &preds, RewriteOp::Fold, None, op_id).await?;
    let pairs: Vec<(ChangesetId, ChangesetId)> = preds.iter().map(|&o| (o, new)).collect();
    let n = pairs.len();
    let flow = finish(ctx, ws, op_id, pairs, steps, new).await?;
    emit(ctx, a.json, "fold", "Folded", &[made], n, &flow)
}
