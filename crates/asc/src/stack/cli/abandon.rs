//! `asc abandon [<change>…] [--keep-descendants]` (SPEC §20.2, §20.7): drops draft changes and
//! restacks the changes above them onto the dropped change's parent.
//!
//! A dropped change is not deleted: it stops being reachable from any branch or HEAD, so it is
//! hidden, and its commit stays addressable through the op log (SPEC BD8). Branches that
//! pointed at it move to its parent. When HEAD is on it, HEAD moves to the parent and the work
//! tree follows (`asc reset --hard`), which is refused while the work tree or the stage has
//! changes. The changes above are kept and restacked onto the parent, which is what
//! `--keep-descendants` asks for as well: it is accepted as the explicit form of the default.
//!
//! Several changes are dropped top first, so the ids of the lower ones do not change while the
//! upper ones go. A conflict while restacking stops the run with exit 1, as `asc restack` does.

use std::collections::HashSet;
use std::process::ExitCode;

use asc_core::id::ChangesetId;
use uuid::Uuid;

use super::info::resolve_change;
use super::restack::{Flow, Row, dirty, run_asc};
use super::reword::{move_refs, refuse_busy};
use super::rewrite::{Made, emit, emit_nothing, finish, locate};
use super::sl::{Local, stack_err};
use super::uncommit::subject_of;
use crate::cli::common::{CliError, Ctx, Result};
use crate::cli::restore::root_of;
use crate::native::NativeSpec;

/// Most changes one run drops.
const MAX_CHANGES: usize = 1024;

#[derive(clap::Parser, Debug)]
#[command(name = "abandon", disable_help_flag = true, disable_version_flag = true)]
pub struct Args {
    /// The changes to drop; default the current HEAD.
    pub changes: Vec<String>,
    /// Keep the changes above (the default; accepted for clarity).
    #[arg(long = "keep-descendants")]
    pub keep_descendants: bool,
    /// Report what would be dropped; change nothing.
    #[arg(short = 'n', long = "dry-run")]
    pub dry_run: bool,
    #[arg(long)]
    pub json: bool,
}

pub const NATIVE: &[NativeSpec] = &[NativeSpec {
    cmd: "abandon",
    flags: &["--keep-descendants", "-n", "--dry-run", "--json"],
    placeholders: &[],
}];

super::verb!(ABANDON, "abandon");

pub async fn run(ctx: &Ctx, a: Args) -> Result<ExitCode> {
    if a.changes.len() > MAX_CHANGES {
        return Err(CliError::Raw("asc: abandon: too many changes".into(), 1));
    }
    let l = Local::open(ctx).await?;
    let ws = l.ws;
    refuse_busy(ws, "abandon")?;

    // Resolve every target before anything moves; drop the deepest first.
    let mut targets: Vec<(usize, ChangesetId, Made)> = Vec::new();
    let mut seen = HashSet::new();
    {
        let st = l.stacks().await?;
        let words: Vec<Option<&str>> =
            if a.changes.is_empty() { vec![None] } else { a.changes.iter().map(|w| Some(w.as_str())).collect() };
        for w in words {
            let cs = super::reword::draft(ctx, &l, &st, w, "abandon").await?;
            if !seen.insert(cs) {
                continue;
            }
            let loc = locate(&st, cs, "abandon").await?;
            let (_, row) = resolve_change(ctx, &l, &st, w).await?;
            let c = ws.engine.get_changeset(cs).await?;
            targets.push((loc.lower.len(), cs, Made { cs, change: row.change.to_string(), subject: subject_of(&c.message) }));
        }
    }
    drop(l);
    targets.sort_by(|p, q| q.0.cmp(&p.0));
    let made: Vec<Made> = targets.iter().map(|t| Made { cs: t.2.cs, change: t.2.change.clone(), subject: t.2.subject.clone() }).collect();
    if made.is_empty() {
        return emit_nothing(ctx, a.json, "abandon");
    }
    if a.dry_run {
        return emit(ctx, a.json, "abandon", "Would abandon", &made, 0, &Flow::Done(Vec::new()));
    }

    let mut restacked: Vec<Row> = Vec::new();
    for (i, (_, cs, _)) in targets.iter().enumerate() {
        let cs = *cs;
        let l = Local::open(ctx).await?;
        let st = l.stacks().await?;
        let loc = locate(&st, cs, "abandon").await?;
        let c = ws.engine.get_changeset(cs).await?;
        let [parent] = c.parents.as_slice() else {
            return Err(CliError::Raw("asc: abandon: the change is a merge or a root".into(), 1));
        };
        let parent = *parent;
        let _ = st.draft_row(cs).await.map_err(stack_err)?.ok_or_else(|| CliError::Fatal("asc: abandon: not a draft change".into()))?;
        drop(st);
        drop(l);

        // HEAD on the change: it moves to the parent with the work tree.
        if ws.head_commit().await? == Some(cs) {
            let root = root_of(ws, Some(cs)).await?;
            if dirty(ctx, ws, root).await? {
                return Err(refuse_dirty_abandon());
            }
            let code = run_asc(ctx, &["reset", "-q", "--hard", &parent.to_string()])?;
            if code != 0 {
                return Err(CliError::Fatal(format!("asc: abandon: could not move HEAD to the parent (exit {code})")));
            }
        }
        let op = Uuid::now_v7();
        move_refs(ctx, ws, cs, parent, op, "abandon").await?;
        if !loc.upper.is_empty() {
            let flow = finish(ctx, ws, Uuid::now_v7(), Vec::new(), loc.upper.clone(), parent).await?;
            match flow {
                Flow::Done(rows) => restacked.extend(rows),
                Flow::Paused(_) => {
                    let done = &made[..=i];
                    return emit(ctx, a.json, "abandon", "Abandoned", done, 0, &flow);
                }
            }
        }
    }
    emit(ctx, a.json, "abandon", "Abandoned", &made, 0, &Flow::Done(restacked))
}

fn refuse_dirty_abandon() -> CliError {
    CliError::Raw(
        "error: cannot abandon the checked-out change: You have uncommitted changes.\nerror: Please commit or stash them.".into(),
        1,
    )
}
