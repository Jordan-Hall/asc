//! `asc modify [-a|-u|-p] [-c] [-m <msg>|-e] [--into <change>]` (SPEC §20.7, §20.1): amends the
//! current change, then restacks its descendants. `-c` adds a new change instead (`asc create`).
//!
//! The amend is `asc commit --amend` in a child, so `pre-commit`, `prepare-commit-msg`,
//! `commit-msg`, `post-commit`, `post-rewrite amend`, the editor and signing behave as git's;
//! without `-m` or `-e` the message is kept. The amend is recorded as a rewrite of the change
//! (same change id) and every other branch that pointed at the old changeset moves with it.
//!
//! `--into <change>` amends a draft change below HEAD without checking it out: the staged
//! changes are committed on HEAD (so the hooks see them), merged into the target's tree in
//! memory, and HEAD goes back to where it was; the restack then carries the content up to HEAD.
//! A merge conflict leaves the staged changes staged and moves nothing else.

use std::process::ExitCode;

use asc_engine::merge::MergeOptions;
use uuid::Uuid;

use super::create;
use super::reword::{self, draft, refuse_busy, reparent_above};
use super::restack;
use super::sl::{Local, fatal, stack_err};
use crate::cli::common::{CliError, Ctx, Result};
use crate::cli::conflict;
use crate::hooks_runner::{Hook, HookEnv, RewriteKind, Runner};
use crate::native::NativeSpec;
use asc_core::change::RewriteOp;
use asc_engine::change::{self, Recorder, RewriteSink as _};

#[derive(clap::Parser, Debug)]
#[command(name = "modify", disable_help_flag = true, disable_version_flag = true)]
pub struct Args {
    /// Stage the changes to tracked files first.
    #[arg(short = 'a', long = "all", conflicts_with_all = ["update", "patch"])]
    pub all: bool,
    /// As `-a`.
    #[arg(short = 'u', long = "update", conflicts_with_all = ["all", "patch"])]
    pub update: bool,
    /// Choose the hunks to stage (`asc add -p`; needs a terminal).
    #[arg(short = 'p', long = "patch", conflicts_with_all = ["all", "update"])]
    pub patch: bool,
    /// Add a new change instead of amending.
    #[arg(short = 'c')]
    pub new_change: bool,
    #[arg(short = 'm', long = "message", conflicts_with = "edit")]
    pub message: Vec<String>,
    #[arg(short = 'e', long)]
    pub edit: bool,
    /// Amend this draft change below HEAD without checking it out.
    #[arg(long, conflicts_with = "new_change")]
    pub into: Option<String>,
    #[arg(short = 'n', long = "no-verify")]
    pub no_verify: bool,
}

pub const NATIVE: &[NativeSpec] = &[NativeSpec {
    cmd: "modify",
    flags: &[
        "-a",
        "--all",
        "-u",
        "--update",
        "-p",
        "--patch",
        "-c",
        "-m",
        "--message",
        "-e",
        "--edit",
        "--into",
        "-n",
        "--no-verify",
    ],
    placeholders: &[],
}];

super::verb!(MODIFY, "modify");

fn code_of(code: i32) -> ExitCode {
    ExitCode::from(u8::try_from(code).unwrap_or(1))
}

pub async fn run(ctx: &Ctx, a: Args) -> Result<ExitCode> {
    if a.new_change {
        let c = create::Args {
            message: a.message.clone(),
            all: a.all,
            update: a.update,
            patch: a.patch,
            no_verify: a.no_verify,
            ..create::Args::default()
        };
        return create::new_change(ctx, &c).await;
    }
    if let Some(word) = a.into.clone() {
        return into(ctx, &a, &word).await;
    }
    let l = Local::open(ctx).await?;
    let ws = l.ws;
    refuse_busy(ws, "modify")?;
    let st = l.stacks().await?;
    let old = draft(ctx, &l, &st, None, "modify").await?;
    drop(st);
    drop(l);
    restack::refuse_frozen(ws, std::iter::once(old)).await?;

    if a.patch {
        let code = create::run_tty(ctx, &["add", "-p"])?;
        if code != 0 {
            return Ok(code_of(code));
        }
    }
    let mut args: Vec<&str> = vec!["commit", "--amend"];
    if a.all || a.update {
        args.push("-a");
    }
    if a.no_verify {
        args.push("-n");
    }
    if a.message.is_empty() {
        args.push(if a.edit { "-e" } else { "--no-edit" });
    }
    for m in &a.message {
        args.push("-m");
        args.push(m);
    }
    let code = create::run_tty(ctx, &args)?;
    if code != 0 {
        return Ok(code_of(code));
    }
    let new = ws.head_commit().await?.ok_or_else(|| fatal("HEAD is unborn after the amend"))?;
    if new == old {
        return Ok(ExitCode::SUCCESS);
    }
    let op = Uuid::now_v7();
    let rows = asc_state::changes::StateChanges::new(ws.db.clone(), ws.repo.key);
    Recorder::new(rows).record(new, &[old], RewriteOp::Amend, change::OpId(op)).await.map_err(fatal)?;
    reword::move_refs(ctx, ws, old, new, op, "modify: follow the amend").await?;
    reparent_above(ctx, old, new).await
}

/// `--into`: see the module comment.
async fn into(ctx: &Ctx, a: &Args, word: &str) -> Result<ExitCode> {
    let l = Local::open(ctx).await?;
    let ws = l.ws;
    refuse_busy(ws, "modify")?;
    let st = l.stacks().await?;
    let head = st.current_head().ok_or_else(|| CliError::Fatal("asc: modify: HEAD is not on a draft change".into()))?;
    let target = draft(ctx, &l, &st, Some(word), "modify").await?;
    restack::refuse_frozen(ws, std::iter::once(target)).await?;
    let stack = st.stack_of(head).await.map_err(stack_err)?;
    let below = stack.changes.iter().position(|&c| c == target).is_some_and(|i| i + 1 < stack.changes.len());
    drop(st);
    drop(l);
    if !below {
        return Err(CliError::Raw("asc: modify --into: the change is not below HEAD in its stack".into(), 1));
    }

    if a.patch {
        let code = create::run_tty(ctx, &["add", "-p"])?;
        if code != 0 {
            return Ok(code_of(code));
        }
    }
    let head_git = head.to_string();
    let subject: String = String::from_utf8_lossy(
        ws.engine.get_changeset(target).await?.message.split(|&b| b == b'\n').next().unwrap_or_default(),
    )
    .chars()
    .take(200)
    .collect();
    let fixup = format!("fixup! {}", super::sl::clean(&subject));
    let mut args: Vec<&str> = vec!["commit", "--allow-empty"];
    if a.all || a.update {
        args.push("-a");
    }
    if a.no_verify {
        args.push("-n");
    }
    args.push("-m");
    args.push(a.message.first().map_or(fixup.as_str(), String::as_str));
    let code = create::run_tty(ctx, &args)?;
    if code != 0 {
        return Ok(code_of(code));
    }
    let f = ws.head_commit().await?.ok_or_else(|| fatal("HEAD is unborn after the commit"))?;
    if f == head {
        return Ok(code_of(1));
    }

    let (base, ours, theirs) = (
        ws.engine.get_changeset(head).await?.root,
        ws.engine.get_changeset(target).await?.root,
        ws.engine.get_changeset(f).await?.root,
    );
    let merged = ws.engine.merge_trees(base, ours, theirs, &MergeOptions::default()).await.map_err(fatal)?;
    if !merged.conflicts.is_empty() {
        // The staged changes stay staged, as before the verb.
        restack::run_asc(ctx, &["reset", "-q", "--soft", &head_git])?;
        return Err(CliError::Raw(
            "asc: modify --into: the staged changes conflict with that change; nothing was changed".into(),
            1,
        ));
    }
    let op = Uuid::now_v7();
    let new = reword::rewrite(ctx, ws, target, None, Some(merged.root), &[f], op).await?;
    // HEAD goes back; the work tree follows when the restack reaches HEAD.
    let code = restack::run_asc(ctx, &["reset", "-q", "--hard", &head_git])?;
    if code != 0 {
        return Err(CliError::Fatal(format!("asc: modify --into: could not move HEAD back (exit {code}); the changes are in {f}")));
    }
    reword::move_refs(ctx, ws, target, new, op, "modify --into: follow the amend").await?;

    let runner = Runner::for_ctx(ctx)?;
    let env = HookEnv::of(ctx, "modify");
    let pair = (conflict::git_id(ws, target).await?, conflict::git_id(ws, new).await?);
    let _ = runner.run(Hook::PostRewrite { kind: RewriteKind::Amend, pairs: vec![pair] }, &env).await?;

    reparent_above(ctx, target, new).await
}
