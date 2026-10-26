//! `asc create [<branch>] [-m <msg>] [-a|-u|-p] [--insert] [--onto <rev>] [--no-branch]`
//! (SPEC §20.7, §20.1, §21): a new change on `HEAD` (or `--onto`) from the staged changes; empty
//! when nothing is staged.
//!
//! The commit itself is `asc commit --allow-empty` in a child, so the commit hooks (`pre-commit`,
//! `prepare-commit-msg`, `commit-msg`, `post-commit`), the editor, `-a` and signing behave as
//! they do for `git commit`. A hook that rejects the commit moves no ref: the position HEAD had
//! (and a branch made for `<branch>`) is put back. The new changeset is then recorded as a new
//! change.
//!
//! No branch is made unless `<branch>` is given when the effective mode is branchless, the
//! default (SPEC §20.7, §21; B01). With `asc.branchless` off a branch is named from
//! `asc.stack.branchTemplate`, and `--no-branch` suppresses that.
//!
//! `--insert` reparents the draft children of the old HEAD onto the new change, restacking each
//! chain above them.

use std::process::{ExitCode, Stdio};

use asc_core::change::RewriteOp;
use asc_core::id::ChangesetId;
use asc_engine::change::{self, Recorder, RewriteSink as _};
use asc_git::projection::Head;
use asc_state::changes::StateChanges;
use uuid::Uuid;

use crate::branchless::mode::{Mode, Settings, branch_name};
use super::reword::{refuse_busy, reparent_above};
use super::restack;
use super::sl::fatal;
use crate::cli::common::{CliError, Ctx, Result, WsCtx};
use crate::cli::conflict;
use crate::cli::restore::resolve_commit;
use crate::native::NativeSpec;

#[derive(clap::Parser, Debug, Default)]
#[command(name = "create", disable_help_flag = true, disable_version_flag = true)]
pub struct Args {
    /// A branch to make for the new change.
    #[arg(conflicts_with = "no_branch")]
    pub branch: Option<String>,
    #[arg(short = 'm', long = "message")]
    pub message: Vec<String>,
    /// Stage the changes to tracked files first.
    #[arg(short = 'a', long = "all", conflicts_with_all = ["update", "patch"])]
    pub all: bool,
    /// As `-a`.
    #[arg(short = 'u', long = "update", conflicts_with_all = ["all", "patch"])]
    pub update: bool,
    /// Choose the hunks to stage (`asc add -p`; needs a terminal).
    #[arg(short = 'p', long = "patch", conflicts_with_all = ["all", "update"])]
    pub patch: bool,
    /// Reparent HEAD's children onto the new change.
    #[arg(long)]
    pub insert: bool,
    /// Create on this revision instead of HEAD.
    #[arg(long)]
    pub onto: Option<String>,
    #[arg(long = "no-branch")]
    pub no_branch: bool,
    #[arg(short = 'n', long = "no-verify")]
    pub no_verify: bool,
}

pub const NATIVE: &[NativeSpec] = &[NativeSpec {
    cmd: "create",
    flags: &[
        "-m",
        "--message",
        "-a",
        "--all",
        "-u",
        "--update",
        "-p",
        "--patch",
        "--insert",
        "--onto",
        "--no-branch",
        "-n",
        "--no-verify",
        "--json",
    ],
    placeholders: &[],
}];

super::verb!(CREATE, "create", json: super::json_op::Done::IfLogged);

/// Runs `asc <args>` with the terminal inherited (the editor and `add -p` need it).
pub(crate) fn run_tty(ctx: &Ctx, args: &[&str]) -> Result<i32> {
    let exe = std::env::current_exe().map_err(|e| fatal(format!("cannot find asc: {e}")))?;
    let status = ctx
        .child(exe)?
        .args(["-c", "advice.mergeConflict=false"])
        .args(args)
        .stdin(Stdio::inherit())
        .status()
        .map_err(|e| fatal(format!("cannot run asc: {e}")))?;
    Ok(status.code().unwrap_or(1))
}

/// Puts HEAD back where it was.
fn put_back(ctx: &Ctx, ws: &WsCtx, was: &Head, before: Option<ChangesetId>, git: Option<&str>) {
    let _ = ws;
    let _ = match was {
        Head::Branch(b) => restack::run_asc(ctx, &["switch", "-q", b]),
        Head::Detached(_) => match (git, before) {
            (Some(g), _) => restack::run_asc(ctx, &["switch", "-q", "--detach", g]),
            _ => Ok(0),
        },
    };
}

pub async fn run(ctx: &Ctx, a: Args) -> Result<ExitCode> {
    new_change(ctx, &a).await
}

/// The flow of `asc create`; `asc modify -c` calls it too.
pub(crate) async fn new_change(ctx: &Ctx, a: &Args) -> Result<ExitCode> {
    let ws = ctx.ws()?;
    refuse_busy(ws, "create")?;
    if let Some(b) = &a.branch
        && (b.starts_with('-') || b.contains('\0'))
    {
        return Err(CliError::Fatal(format!("asc: create: '{}' is not a branch name", super::sl::clean(b))));
    }
    let before = ws.head_commit().await?;
    let was = ws.projection.read_head()?;
    let before_git = match before {
        Some(b) => Some(conflict::git_id(ws, b).await?.to_string()),
        None => None,
    };

    if a.insert && before.is_none() {
        return Err(CliError::Fatal("asc: create --insert: HEAD has no change to insert after".into()));
    }
    // `--onto`: the new change is made on that revision.
    let mut base = before;
    if let Some(rev) = &a.onto {
        let target = resolve_commit(ctx, rev)
            .await?
            .ok_or_else(|| CliError::Fatal(format!("bad revision '{}'", super::sl::clean(rev))))?;
        if Some(target) != before {
            let id = conflict::git_id(ws, target).await?.to_string();
            let code = restack::run_asc(ctx, &["switch", "-q", "--detach", &id])?;
            if code != 0 {
                return Ok(ExitCode::from(u8::try_from(code).unwrap_or(1)));
            }
            base = Some(target);
        }
    }
    if a.patch {
        let code = run_tty(ctx, &["add", "-p"])?;
        if code != 0 {
            put_back(ctx, ws, &was, before, before_git.as_deref());
            return Ok(ExitCode::from(u8::try_from(code).unwrap_or(1)));
        }
    }
    // The branch is made before the commit so the commit advances it; a failure removes it.
    // Without `<branch>`, branchless mode (the default) and `--no-branch` name none; with
    // `asc.branchless` off the name comes from `asc.stack.branchTemplate` (SPEC §20.7).
    let branch = match &a.branch {
        Some(b) => Some(b.clone()),
        None if a.no_branch => None,
        None => {
            let getenv = |k: &str| std::env::var(k).ok();
            let settings = Settings::resolve(&ctx.config, &ctx.env, &getenv).map_err(CliError::Fatal)?;
            // The repo's own `branchless` is not known to the client yet (B07): the default applies.
            if settings.mode(None) == Mode::Branchless {
                None
            } else {
                let subject = a.message.first().map(|m| m.lines().next().unwrap_or(""));
                let fallback = format!("change-{}", &Uuid::now_v7().simple().to_string()[20..]);
                Some(branch_name(&ctx.config, subject, &fallback).map_err(CliError::Fatal)?)
            }
        }
    };
    let mut made_branch = false;
    if let Some(b) = &branch {
        let code = restack::run_asc(ctx, &["switch", "-q", "-c", b])?;
        if code != 0 {
            put_back(ctx, ws, &was, before, before_git.as_deref());
            return Ok(ExitCode::from(u8::try_from(code).unwrap_or(1)));
        }
        made_branch = true;
    }

    let mut args: Vec<&str> = vec!["commit", "--allow-empty"];
    if a.all || a.update {
        args.push("-a");
    }
    if a.no_verify {
        args.push("-n");
    }
    for m in &a.message {
        args.push("-m");
        args.push(m);
    }
    let code = run_tty(ctx, &args)?;
    let head = ws.head_commit().await?;
    if code != 0 || head.is_none() || head == base {
        // Nothing was committed: undo the branch and the switch.
        put_back(ctx, ws, &was, before, before_git.as_deref());
        if made_branch && let Some(b) = &branch {
            let _ = restack::run_asc(ctx, &["branch", "-q", "-D", b]);
        }
        return Ok(ExitCode::from(u8::try_from(if code == 0 { 1 } else { code }).unwrap_or(1)));
    }
    let new = head.ok_or_else(|| fatal("HEAD is unborn after the commit"))?;
    let op = Uuid::now_v7();
    let rows = StateChanges::new(ws.db.clone(), ws.repo.key);
    Recorder::new(rows).record(new, &[], RewriteOp::New, change::OpId(op)).await.map_err(fatal)?;

    if a.insert
        && let Some(old) = base
    {
        return reparent_above(ctx, old, new).await;
    }
    Ok(ExitCode::SUCCESS)
}
