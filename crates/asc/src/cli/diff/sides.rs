//! The two sides of each comparison `diff`, `diff-index` and `diff-files` make (SPEC §4.3,
//! §9.5): a commit's tree, the index (HEAD's tree with the stage-0 rows applied) and the work
//! tree. Only changed paths are produced; the index tree is read lazily and the work tree is
//! compared through the synthetic index's stat cache.

use std::collections::{BTreeMap, BTreeSet, HashSet};

use asc_core::id::TreeId;
use asc_core::object::{Entry, Kind};
use asc_core::path::RepoPath;
use asc_engine::diff::{DiffOptions, Status, TreeChange, rename};
use asc_engine::tree::Pathspec;
use asc_state::state::stage::{self, StageSet};
use futures::TryStreamExt;

use super::work::{LocalFiles, Work, WorkTree};
use crate::cli::common::{CliEngine, CliError, Result, WsCtx};
use crate::cli::restore::{index_tree, root_of};

/// The changes between two sides and the paths that are in conflict.
#[derive(Default)]
pub struct Built {
    pub changes: Vec<TreeChange>,
    pub unmerged: Vec<RepoPath>,
}

fn fatal(e: impl std::fmt::Display) -> CliError {
    CliError::Fatal(e.to_string())
}

/// The root tree of the workspace's HEAD (empty on an unborn branch).
pub async fn head_root(ws: &WsCtx) -> Result<TreeId> {
    root_of(ws, ws.head_commit().await?).await
}

/// What the index holds: its tree and the conflicted paths that are in `spec`.
async fn index_side(ws: &WsCtx, spec: Option<&Pathspec>) -> Result<(TreeId, Vec<RepoPath>)> {
    let head = head_root(ws).await?;
    let (tree, unmerged) = index_tree(ws, head).await?;
    let unmerged = unmerged
        .into_iter()
        .filter_map(|p| RepoPath::new(p).ok())
        .filter(|p| spec.is_none_or(|s| s.matches(p)))
        .collect();
    Ok((tree, unmerged))
}

/// `a` against `b`, both commit trees.
pub async fn trees(ws: &WsCtx, a: TreeId, b: TreeId, spec: Option<&Pathspec>, opts: &DiffOptions) -> Result<Built> {
    let changes = ws.engine.diff_trees(a, b, spec, opts).await.map_err(fatal)?;
    Ok(Built { changes, unmerged: Vec::new() })
}

/// Paths added with intent only (`add -N`): new files to the work tree, absent from the index.
async fn intent_to_add(ws: &WsCtx) -> Result<BTreeSet<Vec<u8>>> {
    let rows = stage::overlay(&ws.db, ws.workspace.ws.id, StageSet::Default).await?;
    Ok(rows.into_iter().filter(|r| r.intent_to_add && r.stage == 0).map(|r| r.path).collect())
}

/// `base` (a tree, HEAD for `--cached`) against the index.
pub async fn cached(ws: &WsCtx, base: TreeId, spec: Option<&Pathspec>, opts: &DiffOptions) -> Result<Built> {
    let (index, unmerged) = index_side(ws, spec).await?;
    let mut built = trees(ws, base, index, spec, opts).await?;
    let skip: HashSet<&RepoPath> = unmerged.iter().collect();
    let ita = intent_to_add(ws).await?;
    built.changes.retain(|c| !skip.contains(&c.path) && !ita.contains(c.path.as_bytes()));
    built.unmerged = unmerged;
    Ok(built)
}

fn same(a: &Option<Entry>, b: &Option<Entry>) -> bool {
    match (a, b) {
        (None, None) => true,
        (Some(a), Some(b)) => a.kind == b.kind && a.git == b.git,
        _ => false,
    }
}

fn class(kind: Kind) -> u8 {
    match kind {
        Kind::Symlink => 1,
        Kind::Gitlink => 2,
        _ => 0,
    }
}

fn change(path: RepoPath, old: Option<Entry>, new: Option<Entry>) -> Option<TreeChange> {
    let status = match (&old, &new) {
        (None, Some(_)) => Status::Added,
        (Some(_), None) => Status::Deleted,
        (Some(o), Some(n)) if class(o.kind) == class(n.kind) => Status::Modified,
        (Some(_), Some(_)) => Status::TypeChanged,
        (None, None) => return None,
    };
    Some(TreeChange { old_path: path.clone(), path, old, new, status })
}

/// A tracked path whose work tree file differs from the index.
struct Delta {
    old: Option<Entry>,
    new: Option<Entry>,
}

/// Every path of the index tree (inside `spec`) whose work tree file differs from it.
async fn work_deltas(
    ctx: &super::super::common::Ctx,
    ws: &WsCtx,
    local: &mut LocalFiles<'_>,
    index: TreeId,
    spec: Option<&Pathspec>,
) -> Result<BTreeMap<RepoPath, Delta>> {
    let root = ws.projection.toplevel();
    let symlinks = ctx.config.bool("core.symlinks").unwrap_or(!cfg!(windows));
    let tree = WorkTree::new(root, &ws.projection.git_dir().join("index"), symlinks).map_err(fatal)?;
    let sparse = ws.workspace.ws.sparse.as_deref().map(asc_wc::plain::Sparse::parse);
    let ita = intent_to_add(ws).await?;
    let mut out = BTreeMap::new();
    let mut walk = ws.engine.walk(index, spec);
    while let Some((path, entry)) = walk.try_next().await? {
        if entry.kind == Kind::Gitlink || sparse.as_ref().is_some_and(|s| !s.contains(&path)) {
            continue;
        }
        let old = (!ita.contains(path.as_bytes())).then(|| entry.clone());
        match tree.probe(&path, &entry).map_err(fatal)? {
            Work::Same if old.is_some() => {}
            Work::Same => {
                // Intent-to-add of a file that is as the placeholder says: still a new file.
                let Some(abs) = tree.resolve(&path) else {
                    out.insert(path, Delta { old, new: None });
                    continue;
                };
                let new = local.add(&path, abs, entry.kind, entry.size, entry.git);
                out.insert(path, Delta { old, new: Some(new) });
            }
            Work::Missing => {
                out.insert(path, Delta { old, new: None });
            }
            Work::Changed { kind, size, git, abs } => {
                let new = local.add(&path, abs, kind, size, git);
                out.insert(path, Delta { old, new: Some(new) });
            }
        }
    }
    Ok(out)
}

/// The work tree against the index (`base` is `None`) or against the tree `base`.
pub async fn worktree(
    ctx: &super::super::common::Ctx,
    ws: &WsCtx,
    local: &mut LocalFiles<'_>,
    base: Option<TreeId>,
    spec: Option<&Pathspec>,
) -> Result<Built> {
    let (index, unmerged) = index_side(ws, spec).await?;
    let skip: HashSet<&RepoPath> = unmerged.iter().collect();
    let mut deltas = work_deltas(ctx, ws, local, index, spec).await?;
    deltas.retain(|p, _| !skip.contains(p));
    let mut changes = Vec::new();
    match base {
        None => {
            for (path, d) in deltas {
                changes.extend(change(path, d.old, d.new));
            }
        }
        Some(base) => {
            let engine: &CliEngine = &ws.engine;
            let mut staged: BTreeMap<RepoPath, TreeChange> = engine
                .tree_changes(base, index, spec)
                .await
                .map_err(fatal)?
                .into_iter()
                .filter(|c| !skip.contains(&c.path))
                .map(|c| (c.path.clone(), c))
                .collect();
            let paths: BTreeSet<RepoPath> = staged.keys().chain(deltas.keys()).cloned().collect();
            for path in paths {
                let s = staged.remove(&path);
                let old = match &s {
                    Some(c) => c.old.clone(),
                    None => engine.lookup(base, &path).await.map_err(fatal)?.filter(|e| e.kind != Kind::Dir),
                };
                let new = match deltas.remove(&path) {
                    Some(d) => d.new,
                    None => s.and_then(|c| c.new),
                };
                if !same(&old, &new) {
                    changes.extend(change(path, old, new));
                }
            }
        }
    }
    Ok(Built { changes, unmerged })
}

/// Rename detection over changes whose sides may be on disk.
pub async fn detect_renames(changes: Vec<TreeChange>, local: &LocalFiles<'_>, opts: &DiffOptions) -> Result<Vec<TreeChange>> {
    match &opts.renames {
        Some(r) => rename::detect(changes, local, r).await.map_err(fatal),
        None => Ok(changes),
    }
}
