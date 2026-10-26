//! `asc stack rename <new>`, `asc stack delete [--upstack|--downstack] [-c]` and `asc stack pop`
//! (SPEC §20.7): the lifecycle of a stack's branch label and of its checked-out change.
//!
//! * `rename` renames the branch HEAD is on (`asc branch -m`).
//! * `delete` deletes the checked-out change's branch and drops the change; `--upstack` also
//!   takes the changes above it and their branches, `--downstack` those below it. Changes that
//!   stay restack onto the parent of what went (the `asc abandon` rules). The work tree must be
//!   clean, as for `asc abandon`. `-c` (close the pull requests) needs a server: it is refused.
//! * `pop` removes the checked-out change but keeps its content: HEAD moves to the parent, the
//!   content stays in the work tree and the stage, the branch HEAD was on is deleted, other
//!   branches on the change move to the parent, and the changes above restack onto the parent.

use std::process::ExitCode;

use asc_core::id::{ChangesetId, RefTarget};
use asc_core::path::RefName;
use asc_git::projection::Head;
use asc_store::refs::{Expect, RefTxn, RefUpdate, Refs as _};

use super::abandon;
use super::info::{resolve_change, sub};
use super::json_op::Done;
use super::restack::{Flow, dirty, reattach, refuse_dirty, run_asc};
use super::reword::{move_refs, refuse_busy};
use super::rewrite::{Made, emit, finish, locate};
use super::sl::{Local, clean, fatal};
use crate::cli::common::{CliError, Ctx, Result, StagingAfterMove, WsCtx};
use crate::cli::restore::{actor, root_of};
use crate::cli::switch::Dest;

/// Most branches one run reads.
const MAX_REFS: usize = 100_000;
/// Most changes one `delete` takes.
const MAX_CHANGES: usize = 1024;

/// The branch HEAD is on, if any.
fn head_branch(ws: &WsCtx) -> Result<Option<String>> {
    Ok(match ws.projection.read_head()? {
        Head::Branch(b) => Some(b),
        Head::Detached(_) => None,
    })
}

/// Deletes branches (full ref names with the commit each must still point at), in one go.
async fn delete_branches(
    ctx: &Ctx,
    _ws: &WsCtx,
    names: Vec<(RefName, ChangesetId)>,
    reason: &str,
) -> Result<()> {
    if names.is_empty() {
        return Ok(());
    }
    let updates = names
        .into_iter()
        .map(|(name, at)| RefUpdate {
            name,
            expect: Expect::Value(RefTarget::Changeset(at)),
            new: None,
            expires_at: None,
        })
        .collect();
    let txn = RefTxn {
        updates,
        actor: actor(),
        reason: String::new(),
        op_id: uuid::Uuid::now_v7(),
    };
    ctx.update_local_refs(txn, reason, StagingAfterMove::Keep)
        .await
}

/// The branches that point at one of `set`: `(ref name, change)`.
async fn branches_at(ws: &WsCtx, set: &[ChangesetId]) -> Result<Vec<(RefName, ChangesetId)>> {
    let entries = ws.db.refs().list(ws.repo.key, "refs/heads/").await?;
    if entries.len() > MAX_REFS {
        return Err(fatal(format!("more than {MAX_REFS} branches")));
    }
    Ok(entries
        .into_iter()
        .filter_map(|e| match e.target {
            RefTarget::Changeset(cs) if set.contains(&cs) => Some((e.name, cs)),
            _ => None,
        })
        .collect())
}

/// The branches called `names` that still exist, with the commit each points at.
async fn branches_named(ws: &WsCtx, names: &[RefName]) -> Result<Vec<(RefName, ChangesetId)>> {
    let entries = ws.db.refs().list(ws.repo.key, "refs/heads/").await?;
    if entries.len() > MAX_REFS {
        return Err(fatal(format!("more than {MAX_REFS} branches")));
    }
    Ok(entries
        .into_iter()
        .filter_map(|e| match e.target {
            RefTarget::Changeset(cs) if names.contains(&e.name) => Some((e.name, cs)),
            _ => None,
        })
        .collect())
}

async fn made_of(l: &Local<'_>, cs: ChangesetId) -> Result<Made> {
    super::move_::made_of(l, cs).await
}

// ---------------------------------------------------------------- rename

mod rename {
    use super::*;

    #[derive(clap::Parser, Debug)]
    #[command(name = "rename", disable_help_flag = true, disable_version_flag = true)]
    pub struct Args {
        /// The new branch name.
        pub new: String,
    }

    pub async fn run(ctx: &Ctx, a: Args) -> Result<ExitCode> {
        let ws = ctx.ws()?;
        refuse_busy(ws, "stack rename")?;
        let Some(old) = head_branch(ws)? else {
            return Err(CliError::Raw(
                "asc: stack rename: HEAD is not on a branch".into(),
                1,
            ));
        };
        if a.new.is_empty()
            || a.new.starts_with('-')
            || RefName::new(format!("refs/heads/{}", a.new).into_bytes()).is_err()
        {
            return Err(CliError::Fatal(format!(
                "'{}' is not a valid branch name",
                clean(&a.new)
            )));
        }
        if a.new == old {
            return Ok(ExitCode::SUCCESS);
        }
        let code = run_asc(ctx, &["branch", "-m", &old, &a.new])?;
        Ok(ExitCode::from(u8::try_from(code).unwrap_or(128)))
    }
}
use rename::run as run_rename;
sub!(STACK_RENAME, "rename", rename::Args, run_rename, json: Done::IfLogged);

// ---------------------------------------------------------------- delete

mod delete {
    use super::*;

    #[derive(clap::Parser, Debug)]
    #[command(name = "delete", disable_help_flag = true, disable_version_flag = true)]
    pub struct Args {
        /// Also delete the changes above.
        #[arg(long)]
        pub upstack: bool,
        /// Also delete the changes below, down to the trunk.
        #[arg(long)]
        pub downstack: bool,
        /// Also close the pull requests (needs a server).
        #[arg(short = 'c', long = "close")]
        pub close: bool,
        /// Report what would go; change nothing.
        #[arg(short = 'n', long = "dry-run")]
        pub dry_run: bool,
        #[arg(long)]
        pub json: bool,
    }

    pub async fn run(ctx: &Ctx, a: Args) -> Result<ExitCode> {
        if a.close {
            return Err(CliError::Raw(
                "asc: stack delete: -c closes pull requests, which needs an asc server; run without -c".into(),
                1,
            ));
        }
        let l = Local::open(ctx).await?;
        let ws = l.ws;
        refuse_busy(ws, "stack delete")?;
        let st = l.stacks().await?;
        let (x, _) = resolve_change(ctx, &l, &st, None).await?;
        let loc = locate(&st, x, "stack delete").await?;
        drop(st);
        let mut set: Vec<ChangesetId> = Vec::new();
        if a.downstack {
            set.extend(loc.lower[..loc.lower.len() - 1].iter().copied());
        }
        set.push(x);
        if a.upstack {
            set.extend(loc.upper.iter().copied());
        }
        if set.len() > MAX_CHANGES {
            return Err(CliError::Raw(
                "asc: stack delete: too many changes".into(),
                1,
            ));
        }
        let mut made = Vec::with_capacity(set.len());
        for &cs in &set {
            made.push(made_of(&l, cs).await?);
        }
        if a.dry_run {
            return emit(
                ctx,
                a.json,
                "stack delete",
                "Would delete",
                &made,
                0,
                &Flow::Done(Vec::new()),
            );
        }
        let root = root_of(ws, Some(x)).await?;
        if dirty(ctx, ws, root).await? {
            return Err(refuse_dirty());
        }
        let words: Vec<String> = made.iter().map(|m| m.change.clone()).collect();
        let names: Vec<RefName> = branches_at(ws, &set)
            .await?
            .into_iter()
            .map(|(n, _)| n)
            .collect();
        drop(l);

        // `asc abandon` drops the changes (a branch on one moves to its parent); the branches
        // named here then go, by name, so a branch that was already on the parent stays.
        let code = abandon::run(
            ctx,
            abandon::Args {
                changes: words,
                keep_descendants: false,
                dry_run: false,
                json: a.json,
            },
        )
        .await?;
        if code != ExitCode::SUCCESS {
            return Ok(code);
        }
        if let Some(b) = head_branch(ws)?
            && names
                .iter()
                .any(|n| n.as_bytes() == format!("refs/heads/{b}").as_bytes())
        {
            let head = ws
                .head_commit()
                .await?
                .ok_or_else(|| CliError::Raw("asc: HEAD is not on a change".into(), 1))?;
            reattach(ctx, Dest::Detach { commit: head }).await?;
        }
        let now = branches_named(ws, &names).await?;
        delete_branches(ctx, ws, now, "stack delete").await?;
        Ok(code)
    }
}
use delete::run as run_delete;
sub!(STACK_DELETE, "delete", delete::Args, run_delete);

// ---------------------------------------------------------------- pop

mod pop {
    use super::*;

    #[derive(clap::Parser, Debug)]
    #[command(name = "pop", disable_help_flag = true, disable_version_flag = true)]
    pub struct Args {
        /// Report what would pop; change nothing.
        #[arg(short = 'n', long = "dry-run")]
        pub dry_run: bool,
        #[arg(long)]
        pub json: bool,
    }

    pub async fn run(ctx: &Ctx, a: Args) -> Result<ExitCode> {
        let l = Local::open(ctx).await?;
        let ws = l.ws;
        refuse_busy(ws, "stack pop")?;
        let st = l.stacks().await?;
        let (x, _) = resolve_change(ctx, &l, &st, None).await?;
        let loc = locate(&st, x, "stack pop").await?;
        drop(st);
        let cx = ws.engine.get_changeset(x).await?;
        let [parent] = cx.parents.as_slice() else {
            return Err(CliError::Raw(
                "asc: stack pop: the change is a merge or a root".into(),
                1,
            ));
        };
        let parent = *parent;
        let made = made_of(&l, x).await?;
        if a.dry_run {
            return emit(
                ctx,
                a.json,
                "stack pop",
                "Would pop",
                std::slice::from_ref(&made),
                0,
                &Flow::Done(Vec::new()),
            );
        }
        let own = match head_branch(ws)? {
            Some(b) => branches_at(ws, &[x])
                .await?
                .into_iter()
                .filter(|(n, _)| n.as_bytes() == format!("refs/heads/{b}").as_bytes())
                .collect(),
            None => Vec::new(),
        };
        drop(l);

        let op = uuid::Uuid::now_v7();
        if !own.is_empty() {
            reattach(ctx, Dest::Detach { commit: x }).await?;
            delete_branches(ctx, ws, own, "stack pop").await?;
        }
        // HEAD (detached on the change) and every other branch on it move to the parent; the
        // work tree and the stage keep the change's content.
        move_refs(ctx, ws, x, parent, op, "stack pop").await?;
        if loc.upper.is_empty() {
            return emit_one(ctx, a.json, made, &Flow::Done(Vec::new()));
        }
        let flow = finish(
            ctx,
            ws,
            uuid::Uuid::now_v7(),
            Vec::new(),
            loc.upper.clone(),
            parent,
        )
        .await?;
        emit_one(ctx, a.json, made, &flow)
    }

    fn emit_one(ctx: &Ctx, json: bool, made: Made, flow: &Flow) -> Result<ExitCode> {
        emit(
            ctx,
            json,
            "stack pop",
            "Popped",
            std::slice::from_ref(&made),
            0,
            flow,
        )
    }
}
use pop::run as run_pop;
sub!(STACK_POP, "pop", pop::Args, run_pop);
