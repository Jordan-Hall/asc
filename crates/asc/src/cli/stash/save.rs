//! Building a stash (`push`, `save`, `create`) and cleaning up after it.
//!
//! The stash commit W has the parents `[HEAD, I]` (and `U` with `-u`): `I` holds the index tree on
//! top of HEAD, `U` the untracked files as a parentless commit, and W's tree is the index tree
//! with the work tree's state of the tracked paths on top, exactly as git builds it. Nothing here
//! reads a file that status did not report as changed.

use std::ffi::OsString;

use asc_core::id::{ChangesetId, TreeId};
use asc_core::path::RepoPath;
use asc_git::projection::Head;
use asc_state::state::stage::{self, StageRow, StageSet};
use asc_state::state::workspaces;
use asc_wc::ignore::IgnoreSources;
use asc_wc::status;

use super::stack;
use crate::cli::commit::message;
use crate::cli::commit::stagebuild;
use crate::cli::common::{CliEngine, CliError, Ctx, Result, WsCtx};
use crate::cli::conflict::{self, NewCommit, fatal};
use crate::cli::restore::{Spec, WcParts, index_tree, root_of, stage_from_tree, sync_worktree};
use crate::cli::seqstate;
use crate::cli::status::expand_dir;
use crate::ext;

/// Most untracked files one stash takes.
const MAX_UNTRACKED: usize = 1_000_000;

/// What a push or create was asked for.
#[derive(Debug, Default)]
pub struct Opts {
    pub untracked: bool,
    pub keep_index: bool,
    pub quiet: bool,
    pub message: Option<String>,
    /// `create <message>`: the message is used as typed, not as `On <branch>: <message>`.
    pub verbatim: bool,
    /// Put the untracked files in W's own tree instead of a third parent, so the one commit
    /// is the whole working state (`asc share`).
    pub fold_untracked: bool,
    pub paths: Vec<OsString>,
}

/// A stash that was written, and what the clean-up needs.
pub struct Made {
    pub commit: ChangesetId,
    /// The reflog text, which is also W's message.
    pub message: String,
    head: ChangesetId,
    head_root: TreeId,
    index: TreeId,
    spec: Option<Spec>,
    /// Tracked paths whose work tree state differs from the index.
    dirty: Vec<RepoPath>,
    untracked: Vec<RepoPath>,
}

fn ignore_sources(ctx: &Ctx, ws: &WsCtx) -> IgnoreSources {
    let root = ws.projection.toplevel();
    IgnoreSources {
        asc_excludes: ctx
            .config
            .path("asc.excludesFile")
            .or_else(|| std::env::home_dir().map(|h| h.join(".config").join("asc").join("ignore")))
            .unwrap_or_else(|| root.join(".asc").join("no-global-ignore")),
        core_excludes: ctx.config.path("core.excludesFile"),
        local_exclude: root.join(".asc").join("info").join("exclude"),
        common_exclude: ws.projection.common_dir().join("info").join("exclude"),
    }
}

/// `<branch>: <abbrev> <subject>` of HEAD, the tail of every stash message.
async fn head_line(ctx: &Ctx, ws: &WsCtx, head: ChangesetId) -> Result<String> {
    let branch = match ws.projection.read_head()? {
        Head::Branch(b) => b,
        Head::Detached(_) => "(no branch)".to_owned(),
    };
    let git = conflict::git_id(ws, head).await?;
    let abbrev = ctx
        .config
        .get("core.abbrev")
        .and_then(|v| v.parse::<usize>().ok())
        .map_or(7, |n| n.clamp(4, 40));
    let subject = message::subject(&ws.engine.get_changeset(head).await?.message);
    Ok(format!(
        "{branch}: {} {subject}",
        &git.to_string()[..abbrev]
    ))
}

/// Writes the stash commits for the current state. `None` (after printing git's notice) when
/// there is nothing to stash. Nothing outside the object store changes.
pub async fn make(ctx: &Ctx, ws: &WsCtx, o: &Opts, notice: bool) -> Result<Option<Made>> {
    let Some(head) = ws.head_commit().await? else {
        return Err(CliError::Raw(
            "You do not have the initial commit yet".into(),
            1,
        ));
    };
    let head_root = root_of(ws, Some(head)).await?;
    let (index, unmerged) = index_tree(ws, head_root).await?;
    if !unmerged.is_empty() {
        for p in &unmerged {
            println!("{}: needs merge", String::from_utf8_lossy(p));
        }
        return Err(CliError::Raw("error: could not write index".into(), 1));
    }
    let spec = if o.paths.is_empty() {
        None
    } else {
        Some(Spec::new(ctx, &o.paths)?)
    };
    let matches = |p: &RepoPath| spec.as_ref().is_none_or(|s| s.spec.matches(p));

    let fresh = workspaces::get(&ws.db, ws.workspace.ws.id)
        .await?
        .ok_or_else(|| fatal("asc: this workspace is not registered in state.db"))?;
    let parts = WcParts::new(ctx, ws, ws.projection.git_dir().join("index"))?;
    let st = status::compute(&parts.wc(&ws.engine), &ws.db, &fresh)
        .await
        .map_err(fatal)?;

    // The work tree state of the changed tracked paths, on top of the index tree.
    let mut rows: Vec<StageRow> = Vec::new();
    let mut dirty = Vec::new();
    for c in st.unstaged.iter().filter(|c| matches(&c.path)) {
        let row = if c.kind == status::ChangeKind::Deleted {
            StageRow::from_leaf(c.path.as_bytes().to_vec(), None)
        } else {
            let prev = ws.engine.lookup(index, &c.path).await?.map(|e| e.kind);
            stagebuild::row_of_path(ws, &c.path, prev).await?
        };
        rows.push(row);
        dirty.push(c.path.clone());
    }
    let staged = ws
        .engine
        .leafdiff(head_root, index, spec.as_ref().map(|s| &s.spec))
        .await?;

    let mut untracked: Vec<RepoPath> = Vec::new();
    if o.untracked {
        let root = ws.projection.toplevel();
        let sources = ignore_sources(ctx, ws);
        for u in &st.untracked {
            let mut found = Vec::new();
            if u.dir {
                expand_dir(root, &sources, u, &mut found).map_err(fatal)?;
            } else {
                found.push(u.path.as_bytes().to_vec());
            }
            for p in found {
                // A nested repository is one `dir/` entry: it is not stashed.
                if p.ends_with(b"/") {
                    continue;
                }
                let Ok(path) = RepoPath::new(p) else { continue };
                if matches(&path) {
                    untracked.push(path);
                }
                if untracked.len() > MAX_UNTRACKED {
                    return Err(fatal("asc: too many untracked files to stash"));
                }
            }
        }
        untracked.sort();
        untracked.dedup();
    }

    if let Some(s) = &spec
        && let Some(orig) = s.first_unmatched(ws, &[index, head_root]).await?
        && untracked.is_empty()
    {
        let prefix = cwd_prefix_len(ctx, ws);
        return Err(CliError::Raw(
            format!(
                "error: pathspec ':(prefix:{prefix}){orig}' did not match any file(s) known to git\nDid you forget to 'git add'?"
            ),
            1,
        ));
    }
    if rows.is_empty() && staged.is_empty() && untracked.is_empty() {
        if notice {
            println!("No local changes to save");
        }
        return Ok(None);
    }

    let changes = stagebuild::rows_to_changes(ws, index, &rows).await?;
    let mut w_tree = ws.engine.apply(index, &changes).await?;

    let empty = CliEngine::empty_tree_id();
    let idents = conflict::identities(ctx)?;
    let line = head_line(ctx, ws, head).await?;
    let commit = |parents: Vec<ChangesetId>, text: String, root: TreeId| NewCommit {
        parents,
        author: idents.author.clone(),
        committer: idents.committer.clone(),
        message: text.into_bytes(),
        root,
    };
    let (i, _) =
        conflict::write_commit(ws, commit(vec![head], format!("index on {line}\n"), index)).await?;
    let mut parents = vec![head, i];
    if !untracked.is_empty() && o.fold_untracked {
        let mut urows = Vec::with_capacity(untracked.len());
        for p in &untracked {
            urows.push(stagebuild::row_of_path(ws, p, None).await?);
        }
        let uchanges = stagebuild::rows_to_changes(ws, w_tree, &urows).await?;
        w_tree = ws.engine.apply(w_tree, &uchanges).await?;
    } else if !untracked.is_empty() {
        let mut urows = Vec::with_capacity(untracked.len());
        for p in &untracked {
            urows.push(stagebuild::row_of_path(ws, p, None).await?);
        }
        let uchanges = stagebuild::rows_to_changes(ws, empty, &urows).await?;
        let u_tree = ws.engine.apply(empty, &uchanges).await?;
        let (u, _) = conflict::write_commit(
            ws,
            commit(Vec::new(), format!("untracked files on {line}\n"), u_tree),
        )
        .await?;
        parents.push(u);
    }
    let branch = line.split(':').next().unwrap_or_default().to_owned();
    let text = match (&o.message, o.verbatim) {
        (Some(m), true) => m.clone(),
        (Some(m), false) => format!("On {branch}: {m}"),
        (None, _) => format!("WIP on {line}"),
    };
    let (w, _) = conflict::write_commit(ws, commit(parents, text.clone(), w_tree)).await?;
    Ok(Some(Made {
        commit: w,
        message: text,
        head,
        head_root,
        index,
        spec,
        dirty,
        untracked,
    }))
}

/// The length of the current directory below the work tree root, for git's pathspec message.
fn cwd_prefix_len(ctx: &Ctx, ws: &WsCtx) -> usize {
    let prefix = crate::cli::add::cwd_prefix(ctx, ws);
    if prefix.is_empty() {
        0
    } else {
        prefix.len() + 1
    }
}

/// Removes what the stash took from the work tree and the index: `reset --hard` for the
/// selected paths (or, with `--keep-index`, only the work tree back to the index) and the
/// stashed untracked files.
pub async fn clean(ctx: &Ctx, ws: &WsCtx, o: &Opts, made: &Made) -> Result<()> {
    let matches = |p: &RepoPath| made.spec.as_ref().is_none_or(|s| s.spec.matches(p));
    if made.spec.is_none() {
        // git resets with `reset --hard`, which leaves ORIG_HEAD at the unchanged HEAD.
        let git = conflict::git_id(ws, made.head).await?;
        seqstate::write_orig_head(ws.projection.git_dir(), git)?;
        seqstate::clear_merge(ws.projection.git_dir());
        seqstate::clear_replay(ws.projection.git_dir());
    }
    if o.keep_index {
        sync_worktree(ctx, made.index, &made.dirty, true).await?;
    } else {
        let mut paths: std::collections::BTreeSet<RepoPath> = made.dirty.iter().cloned().collect();
        let spec = made.spec.as_ref().map(|s| &s.spec);
        for (p, _) in ws.engine.leafdiff(made.head_root, made.index, spec).await? {
            if matches(&p) {
                paths.insert(p);
            }
        }
        let paths: Vec<RepoPath> = paths.into_iter().collect();
        sync_worktree(ctx, made.head_root, &paths, true).await?;
        match &made.spec {
            Some(s) => {
                stage_from_tree(
                    ws,
                    made.index,
                    &Default::default(),
                    made.head_root,
                    made.head_root,
                    s,
                )
                .await?;
            }
            None => {
                stage::clear(&ws.db, ws.workspace.ws.id, StageSet::Default).await?;
                ext::index_sink()
                    .on_stage_changed(ws.workspace.ws.id, &[])
                    .await
                    .map_err(fatal)?;
            }
        }
    }
    // The untracked files are not in the index: a scratch stat cache takes the writes.
    sync_worktree(ctx, CliEngine::empty_tree_id(), &made.untracked, false).await?;
    Ok(())
}

/// `stash push|save`: write the stash, move `refs/stash`, restore the tree.
pub async fn push(ctx: &Ctx, o: &Opts) -> Result<std::process::ExitCode> {
    let ws = ctx.ws()?;
    let Some(made) = make(ctx, ws, o, true).await? else {
        return Ok(std::process::ExitCode::SUCCESS);
    };
    stack::push(ctx, ws, made.commit, &made.message).await?;
    clean(ctx, ws, o, &made).await?;
    if !o.quiet {
        println!("Saved working directory and index state {}", made.message);
    }
    Ok(std::process::ExitCode::SUCCESS)
}

/// `stash create [<message>]`: write the commit and print its id; nothing else changes.
pub async fn create(ctx: &Ctx, message: Option<String>) -> Result<std::process::ExitCode> {
    let ws = ctx.ws()?;
    let o = Opts {
        message,
        verbatim: true,
        ..Opts::default()
    };
    let Some(made) = make(ctx, ws, &o, false).await? else {
        return Ok(std::process::ExitCode::SUCCESS);
    };
    println!("{}", conflict::git_id(ws, made.commit).await?);
    Ok(std::process::ExitCode::SUCCESS)
}
