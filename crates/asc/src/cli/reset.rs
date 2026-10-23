//! `asc reset [--soft|--mixed|--hard] [-q] [<tree-ish>] [--] [<pathspec>…]` (SPEC §4.4).
//!
//! Without paths it moves HEAD's branch (or a detached HEAD) through `Ctx::update_local_refs`
//! after writing `ORIG_HEAD`; `--soft` keeps the staged state across the move, the others make
//! the index equal the new HEAD, and `--hard` also rewrites the working tree. With paths it
//! only resets those index entries to the tree (mixed).

use std::collections::BTreeSet;
use std::ffi::OsString;
use std::fs;
use std::process::ExitCode;

use asc_core::id::{ChangesetId, RefTarget};
use asc_core::path::{RefName, RepoPath};
use asc_git::projection::Head;
use asc_state::state::stage::{self, StageSet};
use asc_state::state::workspaces;
use asc_store::refs::{Expect, RefTxn, RefUpdate};
use asc_wc::status;

use super::common::{CliError, Ctx, Result, StagingAfterMove, WsCtx};
use super::restore::{
    Spec, WcParts, actor, index_tree, resolve_commit, root_of, stage_from_tree, sync_worktree, tool,
};
use crate::ext;
use crate::native::NativeSpec;

#[derive(clap::Parser, Debug)]
#[command(name = "reset", disable_help_flag = true, disable_version_flag = true)]
pub struct Args {
    /// Keep the index and the working tree.
    #[arg(long = "soft", conflicts_with_all = ["mixed", "hard"])]
    pub soft: bool,
    /// Reset the index but not the working tree (the default).
    #[arg(long = "mixed", conflicts_with_all = ["soft", "hard"])]
    pub mixed: bool,
    /// Reset the index and the working tree.
    #[arg(long = "hard", conflicts_with_all = ["soft", "mixed"])]
    pub hard: bool,
    #[arg(short = 'q', long = "quiet")]
    pub quiet: bool,
    /// `[<tree-ish>] [<pathspec>…]` before `--`.
    pub args: Vec<OsString>,
    /// Pathspecs after `--`.
    #[arg(last = true)]
    pub after: Vec<OsString>,
}

pub const NATIVE: &[NativeSpec] = &[NativeSpec {
    cmd: "reset",
    flags: &["--soft", "--mixed", "--hard", "-q", "--quiet"],
    placeholders: &[],
}];

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Soft,
    Mixed,
    Hard,
}

impl Kind {
    fn name(self) -> &'static str {
        match self {
            Self::Soft => "soft",
            Self::Mixed => "mixed",
            Self::Hard => "hard",
        }
    }
}

/// The revision and the pathspec items out of the positional arguments.
struct Parsed {
    rev: Option<String>,
    paths: Vec<OsString>,
}

async fn parse_args(ctx: &Ctx, a: &Args) -> Result<Parsed> {
    let ws = ctx.ws()?;
    let text = |o: &OsString| o.to_string_lossy().into_owned();
    if !a.after.is_empty() {
        // `--` was used: at most one revision before it.
        return match a.args.as_slice() {
            [] => Ok(Parsed {
                rev: None,
                paths: a.after.clone(),
            }),
            [rev] => Ok(Parsed {
                rev: Some(text(rev)),
                paths: a.after.clone(),
            }),
            _ => Err(CliError::Fatal("too many arguments before '--'".into())),
        };
    }
    let Some(first) = a.args.first() else {
        return Ok(Parsed {
            rev: None,
            paths: Vec::new(),
        });
    };
    let first_text = text(first);
    if resolve_commit(ctx, &first_text).await?.is_some() {
        return Ok(Parsed {
            rev: Some(first_text),
            paths: a.args[1..].to_vec(),
        });
    }
    // Not a revision: a path, when it exists on disk or in the index or HEAD.
    let on_disk = ctx.cwd.join(&first_text).exists();
    let head_root = root_of(ws, ws.head_commit().await?).await?;
    let (index, _) = index_tree(ws, head_root).await?;
    let spec = Spec::new(ctx, std::slice::from_ref(first))?;
    let known = spec
        .first_unmatched(ws, &[index, head_root])
        .await?
        .is_none();
    if on_disk || known {
        return Ok(Parsed {
            rev: None,
            paths: a.args.clone(),
        });
    }
    let bin = tool(ctx);
    Err(CliError::Raw(
        format!(
            "fatal: ambiguous argument '{first_text}': unknown revision or path not in the working tree.\n\
             Use '--' to separate paths from revisions, like this:\n\
             '{bin} <command> [<revision>...] -- [<file>...]'"
        ),
        128,
    ))
}

/// `git sha` of `commit`, abbreviated to seven digits, and the subject of its message.
async fn describe(ws: &WsCtx, commit: ChangesetId) -> Result<String> {
    let git = ws
        .engine
        .git_commit_of(commit)
        .await
        .map_err(|e| CliError::Fatal(e.to_string()))?;
    let message = ws.engine.get_changeset(commit).await?.message.clone();
    let text = String::from_utf8_lossy(&message).into_owned();
    let subject: Vec<&str> = text.lines().take_while(|l| !l.trim().is_empty()).collect();
    Ok(format!("{} {}", &git.to_string()[..7], subject.join(" ")))
}

/// Prints the paths whose working file differs from the index (git's refresh report).
async fn print_unstaged(ctx: &Ctx) -> Result<()> {
    let ws = ctx.ws()?;
    let fresh = workspaces::get(&ws.db, ws.workspace.ws.id)
        .await?
        .ok_or_else(|| {
            CliError::Fatal("asc: this workspace is not registered in state.db".into())
        })?;
    let parts = WcParts::new(ctx, ws, ws.projection.git_dir().join("index"))?;
    let st = status::compute(&parts.wc(&ws.engine), &ws.db, &fresh)
        .await
        .map_err(|e| CliError::Fatal(e.to_string()))?;
    if st.unstaged.is_empty() && st.unmerged.is_empty() {
        return Ok(());
    }
    println!("Unstaged changes after reset:");
    let mut lines: Vec<(Vec<u8>, char)> = st
        .unstaged
        .iter()
        .map(|c| (c.path.as_bytes().to_vec(), c.kind.letter()))
        .chain(st.unmerged.iter().map(|p| (p.as_bytes().to_vec(), 'U')))
        .collect();
    lines.sort();
    for (path, letter) in lines {
        println!("{letter}\t{}", String::from_utf8_lossy(&path));
    }
    Ok(())
}

/// The paths a hard reset to `target` must rewrite: every path the index and the target differ
/// at, every modified or deleted tracked file, and every conflicted path.
async fn hard_paths(
    ctx: &Ctx,
    index: asc_core::id::TreeId,
    target: asc_core::id::TreeId,
) -> Result<Vec<RepoPath>> {
    let ws = ctx.ws()?;
    let mut paths: BTreeSet<RepoPath> = ws
        .engine
        .leafdiff(index, target, None)
        .await?
        .into_iter()
        .map(|(p, _)| p)
        .collect();
    let parts = WcParts::new(ctx, ws, ws.projection.git_dir().join("index"))?;
    let st = status::compute(&parts.wc(&ws.engine), &ws.db, &ws.workspace)
        .await
        .map_err(|e| CliError::Fatal(e.to_string()))?;
    paths.extend(st.unstaged.into_iter().map(|c| c.path));
    paths.extend(st.unmerged);
    Ok(paths.into_iter().collect())
}

/// Moves HEAD (its branch, or the detached HEAD) to `target`.
async fn move_head(
    ctx: &Ctx,
    target: ChangesetId,
    rev: &str,
    staging: StagingAfterMove,
) -> Result<()> {
    let ws = ctx.ws()?;
    let old = ws.head_commit().await?;
    let name = match ws.projection.read_head()? {
        Head::Branch(b) => format!("refs/heads/{b}"),
        Head::Detached(_) => "HEAD".to_owned(),
    };
    let name =
        RefName::new_onelevel(name.into_bytes()).map_err(|e| CliError::Fatal(e.to_string()))?;
    let txn = RefTxn {
        updates: vec![RefUpdate {
            name,
            expect: old.map_or(Expect::Absent, |o| Expect::Value(RefTarget::Changeset(o))),
            new: Some(RefTarget::Changeset(target)),
            expires_at: None,
        }],
        actor: actor(),
        reason: String::new(),
        op_id: ext::OpId::new().0,
    };
    ctx.update_local_refs(txn, &format!("reset: moving to {rev}"), staging)
        .await
}

/// Writes `ORIG_HEAD` as the old HEAD, or removes it when HEAD is unborn.
async fn write_orig_head(ws: &WsCtx) -> Result<()> {
    let file = ws.projection.git_dir().join("ORIG_HEAD");
    match ws.head_commit().await? {
        Some(old) => {
            let git = ws
                .engine
                .git_commit_of(old)
                .await
                .map_err(|e| CliError::Fatal(e.to_string()))?;
            fs::write(&file, format!("{git}\n"))?;
        }
        None => {
            let _ = fs::remove_file(&file);
        }
    }
    Ok(())
}

pub async fn run(ctx: &Ctx, a: Args) -> Result<ExitCode> {
    let ws = ctx.ws()?;
    let kind = if a.soft {
        Kind::Soft
    } else if a.hard {
        Kind::Hard
    } else {
        Kind::Mixed
    };
    let parsed = parse_args(ctx, &a).await?;
    let head = ws.head_commit().await?;
    let rev_name = parsed.rev.clone().unwrap_or_else(|| "HEAD".to_owned());
    let target = match &parsed.rev {
        Some(r) => Some(resolve_commit(ctx, r).await?.ok_or_else(|| {
            CliError::Fatal(format!("Failed to resolve '{r}' as a valid revision."))
        })?),
        None => head,
    };
    let head_root = root_of(ws, head).await?;
    let target_root = root_of(ws, target).await?;

    if !parsed.paths.is_empty() {
        if kind != Kind::Mixed {
            return Err(CliError::Fatal(format!(
                "Cannot do {} reset with paths.",
                kind.name()
            )));
        }
        let (index, unmerged) = index_tree(ws, head_root).await?;
        let spec = Spec::new(ctx, &parsed.paths)?;
        stage_from_tree(ws, index, &unmerged, target_root, head_root, &spec).await?;
        if !a.quiet {
            print_unstaged(ctx).await?;
        }
        return Ok(ExitCode::SUCCESS);
    }

    let Some(target) = target else {
        // An unborn branch: only the index can be reset, to empty.
        if kind != Kind::Mixed {
            return Err(CliError::Fatal(
                "Failed to resolve 'HEAD' as a valid ref.".into(),
            ));
        }
        stage::clear(&ws.db, ws.workspace.ws.id, StageSet::Default).await?;
        ext::index_sink()
            .on_stage_changed(ws.workspace.ws.id, &[])
            .await
            .map_err(|e| CliError::Fatal(e.to_string()))?;
        return Ok(ExitCode::SUCCESS);
    };

    if kind == Kind::Hard {
        ext::op_sink().snapshot_wc();
        let (index, _) = index_tree(ws, head_root).await?;
        let paths = hard_paths(ctx, index, target_root).await?;
        sync_worktree(ctx, target_root, &paths, true).await?;
    }
    write_orig_head(ws).await?;
    // git's `remove_branch_state`: a reset ends any merge, pick or revert in progress.
    let gitdir = ws.projection.git_dir();
    super::seqstate::clear_merge(gitdir);
    super::seqstate::clear_replay(gitdir);
    let staging = if kind == Kind::Soft {
        StagingAfterMove::Keep
    } else {
        StagingAfterMove::Set(Vec::new())
    };
    move_head(ctx, target, &rev_name, staging).await?;
    if !a.quiet {
        match kind {
            Kind::Hard => println!("HEAD is now at {}", describe(ws, target).await?),
            Kind::Mixed => print_unstaged(ctx).await?,
            Kind::Soft => {}
        }
    }
    Ok(ExitCode::SUCCESS)
}
