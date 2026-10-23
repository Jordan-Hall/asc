//! `stash apply` and the apply half of `pop` and `branch`.
//!
//! The stash's tracked changes are merged three ways, base = the tree HEAD had when the stash
//! was made, ours = the current index tree, theirs = W's tree (labels `Stash base`, `Updated
//! upstream`, `Stashed changes`). A clean result goes to the work tree; the index keeps its own
//! state, plus the files the stash added (or the stash's staged state with `--index`). A
//! conflicted result is left like a conflicted merge: markers in the work tree, stages 1/2/3 and
//! the clean paths staged. Untracked files of the stash are restored when absent.

use std::collections::BTreeSet;

use asc_core::id::{ChangesetId, TreeId};
use asc_core::path::RepoPath;
use asc_engine::merge::{Favor, Labels};
use asc_state::state::stage::{self, StageRow, StageSet};
use asc_wc::SafeRoot;
use clap::Parser as _;
use futures::TryStreamExt;

use crate::cli::common::{CliError, Ctx, Result, WsCtx};
use crate::cli::conflict::{self, Op, Sides, fatal};
use crate::cli::restore::{index_tree, root_of, sync_worktree};
use crate::cli::seqstate;
use crate::cli::status;
use crate::ext;

/// What `apply` was asked for.
#[derive(Clone, Copy, Debug, Default)]
pub struct Opts {
    pub index: bool,
    pub quiet: bool,
}

fn labels() -> Labels {
    Labels {
        base: "Stash base".into(),
        ours: "Updated upstream".into(),
        theirs: "Stashed changes".into(),
    }
}

/// Whether `commit` has the shape of a stash: W with parents `[base, index]` or `[base, index,
/// untracked]`.
pub async fn is_stash_like(ws: &WsCtx, commit: ChangesetId) -> Result<bool> {
    Ok((2..=3).contains(&ws.engine.get_changeset(commit).await?.parents.len()))
}

/// Stage rows for every path where `root` differs from `head_root`.
async fn rows_against(ws: &WsCtx, head_root: TreeId, root: TreeId) -> Result<Vec<StageRow>> {
    let mut rows = Vec::new();
    for (path, _) in ws.engine.leafdiff(head_root, root, None).await? {
        match ws.engine.lookup(root, &path).await? {
            Some(e) if e.kind != asc_core::object::Kind::Dir => {
                rows.push(conflict::leaf_row(&path, 0, &e))
            }
            Some(_) => {}
            None => rows.push(StageRow::from_leaf(path.as_bytes().to_vec(), None)),
        }
    }
    Ok(rows)
}

/// Restores the untracked files of the stash that are not there yet; `false` when one was.
async fn restore_untracked(ctx: &Ctx, ws: &WsCtx, u_root: TreeId) -> Result<bool> {
    let gate = SafeRoot::new(ws.projection.toplevel());
    let mut put: Vec<RepoPath> = Vec::new();
    let mut clash = false;
    let mut walk = ws.engine.walk(u_root, None);
    while let Some((path, _)) = walk.try_next().await? {
        // Behind a link the path is outside the work tree: never written.
        let free = gate
            .resolve(&path)
            .is_some_and(|abs| std::fs::symlink_metadata(abs).is_err());
        if free {
            put.push(path);
        } else {
            eprintln!("{path} already exists, no checkout");
            clash = true;
        }
    }
    drop(walk);
    sync_worktree(ctx, u_root, &put, false).await?;
    if clash {
        eprintln!("error: could not restore untracked files from stash");
    }
    Ok(!clash)
}

/// Applies the stash commit `w`. Returns whether it went through cleanly; a conflict or a refusal
/// to overwrite local work is reported (and the status printed) and gives `false`.
pub async fn apply(ctx: &Ctx, ws: &WsCtx, w: ChangesetId, o: Opts) -> Result<bool> {
    let cs = ws.engine.get_changeset(w).await?;
    let (Some(&base), Some(&index_commit)) = (cs.parents.first(), cs.parents.get(1)) else {
        return Err(CliError::Fatal("not a stash-like commit".into()));
    };
    let gitdir = ws.projection.git_dir().to_owned();
    if seqstate::exists(&gitdir, "MERGE_HEAD") {
        return Err(CliError::Raw(
            "error: Cannot apply a stash in the middle of a merge".into(),
            1,
        ));
    }
    let b_tree = root_of(ws, Some(base)).await?;
    let i_tree = root_of(ws, Some(index_commit)).await?;
    let w_tree = cs.root;
    let head = ws.head_commit().await?;
    let head_root = root_of(ws, head).await?;
    let (c_tree, unmerged) = index_tree(ws, head_root).await?;
    if !unmerged.is_empty() {
        for p in &unmerged {
            println!("{}: needs merge", String::from_utf8_lossy(p));
        }
        return Err(CliError::Raw("error: could not write index".into(), 1));
    }
    let no_index_work = b_tree == i_tree || c_tree == i_tree;
    let use_index = o.index && !no_index_work;

    let idx_tree = if use_index {
        let sides = Sides {
            base: b_tree,
            ours: c_tree,
            theirs: i_tree,
        };
        let (opts, _) = conflict::merge_options(ctx, ws, &sides, Favor::None, labels()).await?;
        let r = ws
            .engine
            .merge_trees(b_tree, c_tree, i_tree, &opts)
            .await
            .map_err(fatal)?;
        if !r.is_clean() {
            return Err(CliError::Raw(
                "error: Conflicts in index. Try without --index.".into(),
                1,
            ));
        }
        Some(r.root)
    } else {
        None
    };

    let sides = Sides {
        base: b_tree,
        ours: c_tree,
        theirs: w_tree,
    };
    let (opts, differ) = conflict::merge_options(ctx, ws, &sides, Favor::None, labels()).await?;
    let outcome = ws
        .engine
        .merge_trees(b_tree, c_tree, w_tree, &opts)
        .await
        .map_err(fatal)?;
    conflict::refuse_unsupported(&outcome)?;

    let mut ok = true;
    // A stash without tracked changes merges nothing.
    let nothing = b_tree == w_tree && idx_tree.is_none();
    if nothing {
        println!("Already up to date.");
    }
    match conflict::precheck(ctx, ws, Op::Stash, c_tree, &outcome, true).await {
        Err(CliError::Raw(text, _)) => {
            eprintln!("{text}");
            ok = false;
        }
        Err(e) => return Err(e),
        Ok(()) if nothing => {}
        Ok(()) => {
            seqstate::write_auto_merge(&gitdir, conflict::git_tree_id(ws, outcome.root).await?)?;
            conflict::print_messages(&conflict::messages(
                &outcome,
                &differ,
                "Updated upstream",
                "Stashed changes",
            ));
            if outcome.is_clean() {
                settle_clean(ctx, ws, head_root, c_tree, &outcome, idx_tree).await?;
            } else {
                conflict::materialise(ctx, ws, head_root, c_tree, &outcome).await?;
                ok = false;
            }
        }
    }
    if !ok && o.index {
        eprintln!("Index was not unstashed.");
    }

    if let Some(&u) = cs.parents.get(2) {
        let u_root = root_of(ws, Some(u)).await?;
        ok &= restore_untracked(ctx, ws, u_root).await?;
    }
    if !o.quiet {
        let args = status::Args::try_parse_from(["status"]).map_err(fatal)?;
        status::run(ctx, args).await?;
    }
    Ok(ok)
}

/// A clean merge: the result goes to the work tree; the index keeps its state plus the files the
/// stash added, or becomes the stash's staged state with `--index`.
async fn settle_clean(
    ctx: &Ctx,
    ws: &WsCtx,
    head_root: TreeId,
    c_tree: TreeId,
    outcome: &asc_engine::merge::MergeOutcome,
    idx_tree: Option<TreeId>,
) -> Result<()> {
    let changed = ws.engine.leafdiff(c_tree, outcome.root, None).await?;
    conflict::index_result(ws, c_tree, outcome.root).await?;
    let paths: Vec<RepoPath> = changed.iter().map(|(p, _)| p.clone()).collect();
    sync_worktree(ctx, outcome.root, &paths, true).await?;
    let id = ws.workspace.ws.id;
    if let Some(idx) = idx_tree {
        conflict::index_result(ws, c_tree, idx).await?;
        let rows = rows_against(ws, head_root, idx).await?;
        stage::replace(&ws.db, id, StageSet::Default, &rows).await?;
    } else {
        // Files the stash added are staged; every other change stays unstaged.
        let mut rows = Vec::new();
        let mut seen: BTreeSet<RepoPath> = BTreeSet::new();
        for (path, _) in &changed {
            if ws.engine.lookup(c_tree, path).await?.is_some() || !seen.insert(path.clone()) {
                continue;
            }
            if let Some(e) = ws.engine.lookup(outcome.root, path).await?
                && e.kind != asc_core::object::Kind::Dir
            {
                rows.push(conflict::leaf_row(path, 0, &e));
            }
        }
        stage::upsert(&ws.db, id, StageSet::Default, &rows).await?;
    }
    ext::index_sink()
        .on_stage_changed(id, &[])
        .await
        .map_err(fatal)?;
    Ok(())
}
