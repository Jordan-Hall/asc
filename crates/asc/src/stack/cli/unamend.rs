//! `asc unamend [-n] [--json]` (SPEC §20.2, §20.7): restores the previous version of the current
//! change from its predecessors, and leaves what the amend added staged.
//!
//! The current change must be an amend of one earlier version. A new version is written with
//! that version's message and tree on the current change's parents, recorded as an amend of the
//! current version (same change id), and the branches that pointed at the current version move
//! to it. The work tree and the stage are not touched; the stage is rebased across the move, so
//! the content the amend had added is staged, as after `asc reset --soft`.

use std::process::ExitCode;

use asc_core::change::RewriteOp;
use asc_engine::change::ChangeRows as _;
use uuid::Uuid;

use super::restack::Flow;
use super::reword::{move_refs, refuse_busy, rewrite};
use super::rewrite::{Made, emit};
use super::sl::{Local, fatal};
use super::uncommit::{checked_out, post_rewrite};
use crate::cli::common::{CliError, Ctx, Result};
use crate::ext::{self, OpId};
use crate::native::NativeSpec;

#[derive(clap::Parser, Debug)]
#[command(name = "unamend", disable_help_flag = true, disable_version_flag = true)]
pub struct Args {
    /// Report what would be restored; change nothing.
    #[arg(short = 'n', long = "dry-run")]
    pub dry_run: bool,
    #[arg(long)]
    pub json: bool,
}

pub const NATIVE: &[NativeSpec] =
    &[NativeSpec { cmd: "unamend", flags: &["-n", "--dry-run", "--json"], placeholders: &[] }];

super::verb!(UNAMEND, "unamend");

pub async fn run(ctx: &Ctx, a: Args) -> Result<ExitCode> {
    let l = Local::open(ctx).await?;
    let ws = l.ws;
    refuse_busy(ws, "unamend")?;
    let x = checked_out(ctx, &l, None, "unamend").await?;
    let row = l.rows.get(x).await.map_err(fatal)?.ok_or_else(|| fatal("the change has no record"))?;
    let preds = l.rows.preds(x).await.map_err(fatal)?;
    let [pred] = preds.as_slice() else {
        return Err(CliError::Raw("asc: unamend: the change has no single earlier version".into(), 1));
    };
    if row.op != RewriteOp::Amend {
        return Err(CliError::Raw("asc: unamend: the last rewrite of the change was not an amend".into(), 1));
    }
    let pred = *pred;
    let cp = ws.engine.get_changeset(pred).await?;
    let subject = super::uncommit::subject_of(&cp.message);
    let change = row.change.to_string();
    if a.dry_run {
        let made = Made { cs: pred, change, subject };
        return emit(ctx, a.json, "unamend", "Would restore", &[made], 0, &Flow::Done(Vec::new()));
    }
    drop(l);
    let op = Uuid::now_v7();
    let new = rewrite(ctx, ws, x, Some(cp.message.to_vec()), Some(cp.root), &[], op).await?;
    ext::rewrite_sink().record(new, &[x], RewriteOp::Amend, OpId(op));
    ext::index_sink().on_commit(ws.workspace.ws.id, new).await.map_err(fatal)?;
    move_refs(ctx, ws, x, new, op, "unamend").await?;
    post_rewrite(ctx, ws, "unamend", x, new).await?;
    let made = Made { cs: new, change, subject };
    emit(ctx, a.json, "unamend", "Unamended", &[made], 0, &Flow::Done(Vec::new()))
}
