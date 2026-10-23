//! What `asc split`, `asc squash` and `asc fold` share (SPEC §20.2, §20.7): finding the change a
//! verb acts on together with the changes above it, writing a replacement commit, recording
//! its change id by the §20.2 rules table, and handing the changes above to the restack driver
//! of C10.
//!
//! Every replacement commit has the tree of the change it stands in for, so the changes above
//! restack onto it without any content merge, the work tree needs no update, and no ref moves
//! until the driver's single `update_local_refs` transaction.

use std::collections::HashSet;
use std::io::Write;
use std::process::ExitCode;

use asc_core::change::RewriteOp;
use asc_core::id::ChangesetId;
use asc_core::sig::Signature;
use asc_engine::change::{self, Recorder, RewriteSink as _};
use asc_git::projection::Head;
use asc_state::changes::StateChanges;
use serde::Serialize;

use super::restack::{Flow, drive, followers, resign};
use super::sl::{LStacks, clean, fatal, stack_err};
use crate::cli::common::{CliError, Ctx, Result, WsCtx};
use crate::cli::conflict::{self, NewCommit};
use crate::cli::output::Output;
use crate::cli::pager::PagerWant;
use crate::ext::{self, OpId};
use crate::stack::json::{VERSION, line};
use crate::stack::opstate::{self, StackOp};

/// The change a verb acts on: the stack from its bottom to the change, and what sits above it.
pub(crate) struct Located {
    /// Bottom first, ending with the target.
    pub lower: Vec<ChangesetId>,
    /// Changes above the target, bottom first.
    pub upper: Vec<ChangesetId>,
}

/// Finds `target` on the stack of the current HEAD or of a branch head, with the changes above
/// it. Several different chains above it (a fork) are refused: one run rewrites one chain.
pub(crate) async fn locate(st: &LStacks<'_, '_>, target: ChangesetId, verb: &str) -> Result<Located> {
    let mut heads: Vec<ChangesetId> = st.current_head().into_iter().collect();
    heads.extend(st.heads().iter().copied());
    let mut seen = HashSet::new();
    let mut lower: Option<Vec<ChangesetId>> = None;
    let mut upper: Vec<ChangesetId> = Vec::new();
    for h in heads {
        if !seen.insert(h) {
            continue;
        }
        let stack = st.stack_of(h).await.map_err(stack_err)?;
        let Some(i) = stack.changes.iter().position(|&c| c == target) else { continue };
        lower.get_or_insert_with(|| stack.changes[..=i].to_vec());
        let above = &stack.changes[i + 1..];
        let (short, long) = if above.len() <= upper.len() { (above, upper.as_slice()) } else { (upper.as_slice(), above) };
        if !long.starts_with(short) {
            return Err(CliError::Raw(
                format!("asc: {verb}: the change has several children on different branches; restack them one at a time"),
                1,
            ));
        }
        if above.len() > upper.len() {
            upper = above.to_vec();
        }
    }
    match lower {
        Some(lower) => Ok(Located { lower, upper }),
        None => Err(CliError::Raw(format!("asc: {verb}: the change is not on the stack of any branch"), 1)),
    }
}

/// Refuses to start while another operation is in progress.
pub(crate) fn refuse_in_progress(ws: &WsCtx, verb: &str) -> Result<()> {
    let git = ws.projection.git_dir();
    if opstate::exists(git) {
        return Err(CliError::Fatal(format!(
            "asc: cannot {verb}: a restack is in progress (run 'asc continue' or 'asc abort')"
        )));
    }
    for f in ["rebase-merge", "rebase-apply", "MERGE_HEAD", "CHERRY_PICK_HEAD", "REVERT_HEAD", "sequencer"] {
        if git.join(f).exists() {
            return Err(CliError::Fatal(format!(
                "asc: cannot {verb}: an operation is in progress ({f}); finish it with 'asc continue' or 'asc abort'"
            )));
        }
    }
    Ok(())
}

/// A commit that stands in for `old` changes.
pub(crate) struct Piece {
    pub parent: ChangesetId,
    pub root: asc_core::id::TreeId,
    pub author: Signature,
    pub message: Vec<u8>,
}

/// Writes the commit of `p` (signed when `sign`) and returns it.
pub(crate) async fn write_piece(ctx: &Ctx, ws: &WsCtx, p: Piece, sign: bool) -> Result<ChangesetId> {
    let committer = conflict::identities(ctx)?.committer;
    let (id, _) = conflict::write_commit(
        ws,
        NewCommit { parents: vec![p.parent], author: p.author, committer, message: p.message, root: p.root },
    )
    .await?;
    if sign { resign(ctx, ws, id).await } else { Ok(id) }
}

/// What a recorded replacement looks like to the caller.
pub(crate) struct Made {
    pub cs: ChangesetId,
    pub change: String,
    pub subject: String,
}

fn subject_of(message: &[u8]) -> String {
    let first = message.split(|&b| b == b'\n').next().unwrap_or_default();
    String::from_utf8_lossy(first).chars().take(200).collect()
}

/// Records `new` by the §20.2 rules table (`preds` oldest first) and tells the sinks.
pub(crate) async fn record(
    ws: &WsCtx,
    new: ChangesetId,
    preds: &[ChangesetId],
    op: RewriteOp,
    keeps_split_id: Option<bool>,
    op_id: uuid::Uuid,
) -> Result<Made> {
    let recorder = Recorder::new(StateChanges::new(ws.db.clone(), ws.repo.key));
    let meta = match keeps_split_id {
        Some(keeps) => recorder.record_split(new, preds[0], keeps).await,
        None => recorder.record(new, preds, op, change::OpId(op_id)).await,
    }
    .map_err(fatal)?;
    ext::rewrite_sink().record(new, preds, op, OpId(op_id));
    ext::index_sink().on_commit(ws.workspace.ws.id, new).await.map_err(fatal)?;
    let cs = ws.engine.get_changeset(new).await?;
    Ok(Made { cs: new, change: meta.change.to_string(), subject: subject_of(&cs.message) })
}

/// Moves the refs and restacks the changes above, in one go.
///
/// `pairs` are the changes replaced and their replacement; `onto` is the commit the changes
/// above go on (the last replacement).
pub(crate) async fn finish(
    ctx: &Ctx,
    ws: &WsCtx,
    op_id: uuid::Uuid,
    pairs: Vec<(ChangesetId, ChangesetId)>,
    upper: Vec<ChangesetId>,
    onto: ChangesetId,
) -> Result<Flow> {
    let head = ws
        .head_commit()
        .await?
        .ok_or_else(|| CliError::Raw("asc: HEAD is not on a change".into(), 1))?;
    let head_branch = match ws.projection.read_head()? {
        Head::Branch(b) => Some(b),
        Head::Detached(_) => None,
    };
    let mut all: Vec<ChangesetId> = pairs.iter().map(|p| p.0).collect();
    all.extend(upper.iter().copied());
    let op = StackOp {
        op_id,
        head_branch,
        head,
        follow: followers(ws, &all).await?,
        done: Vec::new(),
        at: upper.first().copied().unwrap_or(onto),
        at_onto: onto,
        rest: Vec::new(),
        sign: ctx.config.bool("commit.gpgsign") == Some(true),
    };
    drive(ctx, ws, op, pairs, onto, upper).await
}

// ---------------------------------------------------------------- output

#[derive(Serialize)]
struct Item<'a> {
    change: &'a str,
    subject: &'a str,
}

#[derive(Serialize)]
struct Report<'a> {
    version: u32,
    verb: &'a str,
    /// `complete`, `nothing` or `paused`.
    status: &'a str,
    changes: Vec<Item<'a>>,
    restacked: usize,
}

/// Prints the outcome. `lead` is the text line label (`Folded`, `Squashed`, `Split`).
pub(crate) fn emit(ctx: &Ctx, json: bool, verb: &str, lead: &str, made: &[Made], replaced: usize, flow: &Flow) -> Result<ExitCode> {
    let (status, restacked) = match flow {
        Flow::Done(rows) => ("complete", rows.len().saturating_sub(replaced)),
        Flow::Paused(_) => ("paused", 0),
    };
    nothing_or(ctx, json, verb, lead, status, made, restacked, matches!(flow, Flow::Paused(_)))
}

/// Prints `nothing` for a verb that found nothing to do.
pub(crate) fn emit_nothing(ctx: &Ctx, json: bool, verb: &str) -> Result<ExitCode> {
    nothing_or(ctx, json, verb, "", "nothing", &[], 0, false)
}

#[allow(clippy::too_many_arguments)]
fn nothing_or(
    ctx: &Ctx,
    json: bool,
    verb: &str,
    lead: &str,
    status: &str,
    made: &[Made],
    restacked: usize,
    paused: bool,
) -> Result<ExitCode> {
    let mut out = Output::open(ctx, "stack", PagerWant::Never)?;
    if json {
        let changes = made.iter().map(|m| Item { change: &m.change, subject: &m.subject }).collect();
        let text = line(&Report { version: VERSION, verb, status, changes, restacked }).map_err(fatal)?;
        out.write_all(text.as_bytes())?;
    } else {
        for m in made {
            let short: String = m.change.chars().take(12).collect();
            writeln!(out, "{lead} {short} {}", clean(&m.subject))?;
        }
    }
    out.finish()?;
    Ok(if paused { ExitCode::from(1) } else { ExitCode::SUCCESS })
}
