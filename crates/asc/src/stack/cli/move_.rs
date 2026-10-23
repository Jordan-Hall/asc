//! `asc move --onto <rev> [--source <change>] [--only] [-n]` (SPEC §20.7): reparents a change and
//! the changes above it onto another commit; `--only` moves the one change and leaves the
//! changes above it where they were (they restack onto its old parent).
//!
//! Every rewrite keeps the change id (a rebase, `RewriteOp::Restack`). The move runs through the
//! restack driver of C10: a conflict pauses it as `asc restack` does (exit 1, `asc continue` or
//! `asc abort`). `-n` replays in memory and reports whether it would conflict, moving no ref;
//! `--only` is applied in one transaction and a conflict there moves nothing (move the changes
//! one at a time to resolve it). `pre-rebase` runs once up front, `post-rewrite rebase` once
//! afterwards with every pair.

use std::collections::HashMap;
use std::process::ExitCode;

use asc_core::id::ChangesetId;
use asc_engine::change::ChangeRows as _;
use asc_engine::merge::MergeOptions;
use asc_engine::stack::restack::{Outcome, RestackPlan, TxnMeta};
use asc_git::projection::Head;

use super::info::resolve_change;
use super::restack::{Flow, Row, apply, followers, register, resign};
use super::reword::refuse_busy;
use super::rewrite::{Made, emit, emit_nothing, finish, locate};
use super::sl::{Local, fatal};
use super::uncommit::subject_of;
use crate::cli::common::{CliError, Ctx, Result, WsCtx};
use crate::cli::conflict;
use crate::cli::restore::{actor, resolve_commit};
use crate::hooks_runner::{Gate, Hook, HookEnv, Runner};
use crate::native::NativeSpec;
use crate::stack::opstate::StackOp;

#[derive(clap::Parser, Debug)]
#[command(name = "move", disable_help_flag = true, disable_version_flag = true)]
pub struct Args {
    /// The commit or change the source goes onto.
    #[arg(long)]
    pub onto: String,
    /// The change to move; default the current HEAD.
    #[arg(long)]
    pub source: Option<String>,
    /// Move the one change; the changes above it stay and restack onto its old parent.
    #[arg(long)]
    pub only: bool,
    /// Replay in memory and report; move nothing.
    #[arg(short = 'n', long = "dry-run")]
    pub dry_run: bool,
    #[arg(long)]
    pub json: bool,
}

pub const NATIVE: &[NativeSpec] = &[NativeSpec {
    cmd: "move",
    flags: &["--onto", "--source", "--only", "-n", "--dry-run", "--json"],
    placeholders: &[],
}];

super::verb!(MOVE, "move");

/// What an in-memory replay of rewrite groups ended with.
pub(crate) enum Replay {
    /// Every rewrite, oldest first: `(old, new)`.
    Done(Vec<(ChangesetId, ChangesetId)>),
    /// This change conflicts. Nothing was changed.
    Conflict(ChangesetId),
}

/// Replays each group `(onto, steps)` one change at a time, in order, without moving a ref. A
/// base that an earlier group rewrote is replaced by its rewrite. The steps of a group need not
/// be a chain: each is rebuilt from its own diff.
pub(crate) async fn replay(
    ctx: &Ctx,
    ws: &WsCtx,
    op_id: uuid::Uuid,
    groups: &[(ChangesetId, Vec<ChangesetId>)],
    sign: bool,
) -> Result<Replay> {
    let meta = TxnMeta {
        actor: actor(),
        reason: "restack".into(),
        op_id,
    };
    let mut map: HashMap<ChangesetId, ChangesetId> = HashMap::new();
    let mut pairs = Vec::new();
    for (onto, steps) in groups {
        let mut base = map.get(onto).copied().unwrap_or(*onto);
        for &old in steps {
            let plan = RestackPlan {
                onto: base,
                steps: vec![old],
            };
            let outcome = ws
                .engine
                .execute_restack(&plan, &[], None, &MergeOptions::default(), &meta)
                .await
                .map_err(fatal)?;
            let Outcome::Complete { rewrites, .. } = outcome else {
                return Ok(Replay::Conflict(old));
            };
            let mut new = rewrites
                .first()
                .ok_or_else(|| fatal("the restack returned no rewrite"))?
                .new;
            if new != old {
                register(ws, new).await?;
                if sign {
                    new = resign(ctx, ws, new).await?;
                }
            }
            map.insert(old, new);
            pairs.push((old, new));
            base = new;
        }
    }
    Ok(Replay::Done(pairs))
}

/// Moves the refs that follow `pairs`, syncs the work tree and records the rewrites.
pub(crate) async fn commit_pairs(
    ctx: &Ctx,
    ws: &WsCtx,
    op_id: uuid::Uuid,
    pairs: &[(ChangesetId, ChangesetId)],
) -> Result<Vec<Row>> {
    let Some(&(first_old, first_new)) = pairs.first() else {
        return Ok(Vec::new());
    };
    let head = ws
        .head_commit()
        .await?
        .ok_or_else(|| CliError::Raw("asc: HEAD is not on a change".into(), 1))?;
    let head_branch = match ws.projection.read_head()? {
        Head::Branch(b) => Some(b),
        Head::Detached(_) => None,
    };
    let all: Vec<ChangesetId> = pairs.iter().map(|p| p.0).collect();
    let op = StackOp {
        op_id,
        head_branch,
        head,
        follow: followers(ws, &all).await?,
        done: Vec::new(),
        at: first_old,
        at_onto: first_new,
        rest: Vec::new(),
        sign: ctx.config.bool("commit.gpgsign") == Some(true),
    };
    apply(ctx, ws, &op, pairs).await
}

/// Runs `pre-rebase` with `upstream` as git's `git rebase` does; a rejection stops the verb.
pub(crate) async fn pre_rebase(
    ctx: &Ctx,
    ws: &WsCtx,
    upstream: ChangesetId,
    verb: &str,
) -> Result<()> {
    let runner = Runner::for_ctx(ctx)?;
    let env = HookEnv::of(ctx, verb);
    let hook = Hook::PreRebase {
        upstream: conflict::git_id(ws, upstream).await?.to_string(),
        branch: None,
    };
    if let Gate::Reject(_) = runner.gate(hook, &env).await? {
        return Err(CliError::Fatal(
            "The pre-rebase hook refused to rebase.".into(),
        ));
    }
    Ok(())
}

/// The row a predicted conflict is reported as.
pub(crate) async fn conflict_row(l: &Local<'_>, cs: ChangesetId) -> Result<Row> {
    let c = l.ws.engine.get_changeset(cs).await?;
    let git = l.git_id(cs).await?;
    Ok(Row {
        change: l
            .rows
            .get(cs)
            .await
            .map_err(fatal)?
            .map(|r| r.change.to_string())
            .unwrap_or_default(),
        subject: subject_of(&c.message),
        old: git.clone(),
        new: git,
    })
}

/// A `Made` line for `cs`.
pub(crate) async fn made_of(l: &Local<'_>, cs: ChangesetId) -> Result<Made> {
    let c = l.ws.engine.get_changeset(cs).await?;
    let change = l
        .rows
        .get(cs)
        .await
        .map_err(fatal)?
        .map(|r| r.change.to_string())
        .unwrap_or_default();
    Ok(Made {
        cs,
        change,
        subject: subject_of(&c.message),
    })
}

pub async fn run(ctx: &Ctx, a: Args) -> Result<ExitCode> {
    let l = Local::open(ctx).await?;
    let ws = l.ws;
    refuse_busy(ws, "move")?;
    let st = l.stacks().await?;
    let (x, _) = resolve_change(ctx, &l, &st, a.source.as_deref()).await?;
    let loc = locate(&st, x, "move").await?;
    let target = match resolve_change(ctx, &l, &st, Some(&a.onto)).await {
        Ok((cs, _)) => cs,
        Err(_) => resolve_commit(ctx, &a.onto).await?.ok_or_else(|| {
            CliError::Fatal(format!("bad revision '{}'", super::sl::clean(&a.onto)))
        })?,
    };
    if target == x {
        return Err(CliError::Raw(
            "asc: move: a change cannot move onto itself".into(),
            1,
        ));
    }
    let below_target = loc.upper.contains(&target);
    if !below_target && st.graph().is_ancestor(x, target).await.map_err(fatal)? {
        return Err(CliError::Raw(
            "asc: move: the target is a descendant of the change".into(),
            1,
        ));
    }
    if below_target && !a.only {
        return Err(CliError::Raw(
            "asc: move: the target is a descendant of the change (use --only to move the one change)".into(),
            1,
        ));
    }
    drop(st);

    let cx = ws.engine.get_changeset(x).await?;
    let [parent] = cx.parents.as_slice() else {
        return Err(CliError::Raw(
            "asc: move: the change is a merge or a root".into(),
            1,
        ));
    };
    let parent = *parent;
    for &u in &loc.upper {
        if ws.engine.get_changeset(u).await?.parents.len() != 1 {
            return Err(CliError::Raw(
                "asc: move: a change above is a merge".into(),
                1,
            ));
        }
    }
    if parent == target && (!a.only || loc.upper.is_empty()) {
        return emit_nothing(ctx, a.json, "move");
    }

    let mut moved = vec![x];
    if !a.only {
        moved.extend(loc.upper.iter().copied());
    }
    let mut made = Vec::with_capacity(moved.len());
    for &cs in &moved {
        made.push(made_of(&l, cs).await?);
    }
    let sign = ctx.config.bool("commit.gpgsign") == Some(true);
    let op_id = uuid::Uuid::now_v7();
    pre_rebase(ctx, ws, target, "move").await?;

    // The whole chain goes through the restack driver, which pauses on a conflict.
    if !a.only && !a.dry_run {
        drop(l);
        let flow = finish(ctx, ws, op_id, Vec::new(), moved, target).await?;
        let n = made.len();
        return emit(ctx, a.json, "move", "Moved", &made, n, &flow);
    }

    let groups: Vec<(ChangesetId, Vec<ChangesetId>)> = if a.only {
        let mut g = Vec::new();
        if !loc.upper.is_empty() {
            g.push((parent, loc.upper.clone()));
        }
        g.push((target, vec![x]));
        g
    } else {
        vec![(target, moved)]
    };
    let outcome = replay(ctx, ws, op_id, &groups, sign && !a.dry_run).await?;
    let n = made.len();
    match outcome {
        Replay::Conflict(at) if a.dry_run => {
            let row = conflict_row(&l, at).await?;
            emit(ctx, a.json, "move", "Would move", &made, n, &Flow::Paused(row))
        }
        Replay::Conflict(_) => Err(CliError::Raw(
            "asc: move --only: the rewrite conflicts; nothing was changed (move the changes one at a time)".into(),
            1,
        )),
        Replay::Done(_) if a.dry_run => emit(ctx, a.json, "move", "Would move", &made, n, &Flow::Done(Vec::new())),
        Replay::Done(pairs) => {
            drop(l);
            let rows = commit_pairs(ctx, ws, op_id, &pairs).await?;
            emit(ctx, a.json, "move", "Moved", &made, n, &Flow::Done(rows))
        }
    }
}
