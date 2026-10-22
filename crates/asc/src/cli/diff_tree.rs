//! `diff-tree`, `diff-index` and `diff-files` (SPEC §11.1): the plumbing forms of `diff`. They
//! share its options and output (`cli/diff/`), print raw lines by default and, unlike `diff`,
//! detect renames only with `-M`. `diff-tree` is not recursive unless `-r` (or a patch or stat
//! form) asks, in which case directories print as their own entries, like git.

use std::collections::HashSet;
use std::ffi::OsString;
use std::process::ExitCode;

use asc_core::id::TreeId;
use asc_core::object::Kind;
use asc_core::path::RepoPath;
use asc_engine::diff::{DiffOptions, Status, TreeChange};
use asc_engine::history::HistoryApi;
use asc_engine::tree::Pathspec;

use super::common::{CliEngine, CliError, Ctx, Result, WsCtx};
use super::diff::opts::{Common, Formats, RawArgs, apply_config, parse};
use super::diff::sides::{self, Built};
use super::diff::work::LocalFiles;
use super::diff::{Emit, fatal, finish, flags, pathspec, reject_extras, resolve_relative, split_args};
use super::pager::PagerWant;
use super::restore::{resolve_commit, root_of};
use crate::native::NativeSpec;

pub type Args = RawArgs;

pub const NATIVE: &[NativeSpec] = &[
    NativeSpec {
        cmd: "diff-tree",
        flags: flags!("-r", "-t", "--root", "--no-commit-id"),
        placeholders: &[],
    },
    NativeSpec { cmd: "diff-index", flags: flags!("--cached"), placeholders: &[] },
    NativeSpec { cmd: "diff-files", flags: flags!(), placeholders: &[] },
];

pub async fn run(ctx: &Ctx, args: Args) -> Result<ExitCode> {
    let mut seed = DiffOptions::default();
    apply_config(ctx, &mut seed, false)?;
    let mut c = parse(seed, &args.rest)?;
    let ws = ctx.ws()?;
    resolve_relative(ctx, ws, &mut c);
    let fmt = c.fmt.resolve(true);
    match args.cmd.as_str() {
        "diff-tree" => tree(ctx, ws, c, fmt).await,
        "diff-index" => index(ctx, ws, c, fmt).await,
        _ => files(ctx, ws, c, fmt).await,
    }
}

/// Plumbing output: never paged, raw ids in full.
fn plumbing(header: Option<&[u8]>) -> Emit<'_> {
    Emit { pager: PagerWant::Never, header, plumbing: true, attrs: true }
}

fn usage(text: &str) -> CliError {
    CliError::Usage(text.to_owned())
}

/// Forms that need file paths, not directories, so they imply `-r`.
fn implies_recursion(fmt: Formats) -> bool {
    fmt.patch || fmt.stat || fmt.numstat || fmt.shortstat || fmt.summary || fmt.word_diff
}

async fn dir_change(ws: &WsCtx, a: TreeId, b: TreeId, dir: &[u8]) -> Result<Option<TreeChange>> {
    let path = RepoPath::new(dir.to_vec()).map_err(|e| fatal(e.to_string()))?;
    let mut sides = Vec::with_capacity(2);
    for root in [a, b] {
        let found = ws.engine.lookup(root, &path).await.map_err(|e| fatal(e.to_string()))?;
        sides.push(found.filter(|e| e.kind == Kind::Dir));
    }
    let new = sides.pop().flatten();
    let old = sides.pop().flatten();
    let status = match (&old, &new) {
        (None, Some(_)) => Status::Added,
        (Some(_), None) => Status::Deleted,
        (Some(_), Some(_)) => Status::Modified,
        (None, None) => return Ok(None),
    };
    Ok(Some(TreeChange { old_path: path.clone(), path, old, new, status }))
}

/// Non-recursive output: every directory with changes below it is one entry. With `show_trees`
/// (`-r -t`) the directories are added before their contents instead.
async fn shape(ws: &WsCtx, a: TreeId, b: TreeId, changes: Vec<TreeChange>, recursive: bool, show_trees: bool) -> Result<Vec<TreeChange>> {
    if recursive && !show_trees {
        return Ok(changes);
    }
    let mut out = Vec::with_capacity(changes.len());
    let mut seen: HashSet<Vec<u8>> = HashSet::new();
    for c in changes {
        let bytes = c.path.as_bytes().to_vec();
        let slashes: Vec<usize> = bytes.iter().enumerate().filter(|(_, b)| **b == b'/').map(|(i, _)| i).collect();
        if recursive {
            for &at in &slashes {
                if seen.insert(bytes[..at].to_vec()) {
                    out.extend(dir_change(ws, a, b, &bytes[..at]).await?);
                }
            }
            out.push(c);
        } else if let Some(&first) = slashes.first() {
            if seen.insert(bytes[..first].to_vec()) {
                out.extend(dir_change(ws, a, b, &bytes[..first]).await?);
            }
        } else {
            out.push(c);
        }
    }
    Ok(out)
}

async fn tree(ctx: &Ctx, ws: &WsCtx, mut c: Common, fmt: Formats) -> Result<ExitCode> {
    reject_extras(&c, &["-r", "-t", "--root", "--no-commit-id"])?;
    let has = |name: &str| c.extra.iter().any(|f| f == name);
    let (recursive, show_trees, root, no_id) = (has("-r"), has("-t"), has("--root"), has("--no-commit-id"));
    let (revs, paths) = split_args(ctx, &c, 2).await?;
    let spec = pathspec(ctx, &paths)?;
    let mut header = None;
    let (a, b) = match revs.as_slice() {
        [one] => {
            let cs = resolve_commit(ctx, one).await?.ok_or_else(|| fatal(format!("Not a valid object name {one}")))?;
            let set = ws.engine.get_changeset(cs).await?;
            let from = match set.parents.as_slice() {
                [] if root => CliEngine::empty_tree_id(),
                [parent] => root_of(ws, Some(*parent)).await?,
                _ => return Ok(ExitCode::SUCCESS),
            };
            if !no_id {
                let map = HistoryApi::git_map(&ws.engine, &[cs], &[]).await.map_err(|e| fatal(e.to_string()))?;
                header = map.asc_to_git.first().copied().flatten();
            }
            (from, set.root)
        }
        [x, y] => {
            let side = |spec: &String| {
                let spec = spec.clone();
                async move { resolve_commit(ctx, &spec).await?.ok_or_else(|| fatal(format!("Not a valid object name {spec}"))) }
            };
            (root_of(ws, Some(side(x).await?)).await?, root_of(ws, Some(side(y).await?)).await?)
        }
        _ => return Err(usage("usage: git diff-tree [--stdin] [-m] [-c | --cc] [-s] [-v] [--pretty] [-t] [-r] [--root] [<common-diff-options>] <tree-ish> [<tree-ish>] [<path>...]")),
    };
    let recursive = recursive || implies_recursion(fmt);
    let mut built = sides::trees(ws, a, b, spec.as_ref(), &c.opts).await?;
    built.changes = shape(ws, a, b, built.changes, recursive, show_trees).await?;
    let prefix = header.map(|oid| {
        let mut line = oid.to_string().into_bytes();
        line.push(if c.opts.nul { 0 } else { b'\n' });
        line
    });
    c.opts.sides = (asc_engine::diff::SideKind::Commit, asc_engine::diff::SideKind::Commit);
    finish(ctx, &c, fmt, &ws.engine, built, plumbing(prefix.as_deref())).await
}

async fn index(ctx: &Ctx, ws: &WsCtx, mut c: Common, fmt: Formats) -> Result<ExitCode> {
    reject_extras(&c, &["--cached"])?;
    let cached = c.extra.iter().any(|f| f == "--cached");
    let (revs, paths) = split_args(ctx, &c, 1).await?;
    let [rev] = revs.as_slice() else {
        return Err(usage("usage: git diff-index [-m] [--cached] [<common-diff-options>] <tree-ish> [<path>...]"));
    };
    let cs = resolve_commit(ctx, rev).await?.ok_or_else(|| fatal(format!("Not a valid object name {rev}")))?;
    let base = root_of(ws, Some(cs)).await?;
    let spec = pathspec(ctx, &paths)?;
    let mut local = LocalFiles::new(&ws.engine);
    let built = if cached {
        sides::cached(ws, base, spec.as_ref(), &c.opts).await?
    } else {
        let mut built = sides::worktree(ctx, ws, &mut local, Some(base), spec.as_ref()).await?;
        built.changes = sides::detect_renames(built.changes, &local, &c.opts).await?;
        built
    };
    c.exit_code |= c.quiet;
    finish(ctx, &c, fmt, &local, built, plumbing(None)).await
}

async fn files(ctx: &Ctx, ws: &WsCtx, mut c: Common, fmt: Formats) -> Result<ExitCode> {
    reject_extras(&c, &[])?;
    let items: Vec<OsString> = c.positional.iter().chain(c.paths.iter()).cloned().collect();
    let spec: Option<Pathspec> = pathspec(ctx, &items)?;
    let mut local = LocalFiles::new(&ws.engine);
    let mut built: Built = sides::worktree(ctx, ws, &mut local, None, spec.as_ref()).await?;
    built.changes = sides::detect_renames(built.changes, &local, &c.opts).await?;
    c.exit_code |= c.quiet;
    finish(ctx, &c, fmt, &local, built, plumbing(None)).await
}
