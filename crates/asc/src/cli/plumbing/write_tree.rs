//! `write-tree` and `read-tree` (SPEC §9.5): the index as a tree, and a tree as the index. Only
//! the directories on changed paths are read or written; the work tree is never touched.
//! `read-tree` with several trees reads their union, or merges them into the index as git's
//! two-way and three-way merges do ([`super::tree_merge`]); `-u` is not offered.

use std::collections::HashSet;
use std::ffi::OsString;
use std::process::ExitCode;

use asc_core::git::ObjectKind;
use asc_core::id::{FileId, TreeId};
use asc_core::object::{Change, Entry, Kind};
use asc_core::path::RepoPath;
use asc_state::state::stage::{Leaf, StageRow};

use futures::TryStreamExt as _;

use super::index::Index;
use super::objects::{Object, Resolver};
use super::tree_merge::{self, Ent, Tree};
use super::usage;
use crate::cli::add::scan::UpToDate;
use crate::cli::add::target::Edit;
use crate::cli::common::{CliEngine, CliError, Ctx, Result, WsCtx};
use crate::cli::refs_plumbing::args::{Cursor, Word};

fn fatal(text: impl Into<String>) -> CliError {
    CliError::Fatal(text.into())
}

/// The changes the stage rows make on top of `base` (intent-to-add rows stage nothing).
async fn rows_to_changes(
    ws: &WsCtx,
    base: TreeId,
    rows: &[StageRow],
) -> Result<Vec<(RepoPath, Change)>> {
    let mut out = Vec::with_capacity(rows.len());
    for r in rows {
        if r.intent_to_add {
            continue;
        }
        let path = RepoPath::new(r.path.clone()).map_err(|e| fatal(e.to_string()))?;
        let change = match (r.file, r.git) {
            (None, _) => {
                if ws.engine.lookup(base, &path).await?.is_none() {
                    continue;
                }
                Change::Delete
            }
            (Some(_), Some(git)) if r.kind == Kind::Gitlink => Change::Gitlink { commit: git },
            (Some(file), Some(git)) => {
                Change::write(r.kind, file, r.size, git, None).map_err(|e| fatal(e.to_string()))?
            }
            (Some(_), None) => return Err(fatal("asc: a stage row has no git oid")),
        };
        out.push((path, change));
    }
    Ok(out)
}

pub async fn write_tree(ctx: &Ctx, args: Vec<OsString>) -> Result<ExitCode> {
    let mut cur = Cursor::new(&args, usage::WRITE_TREE);
    let mut prefix: Option<String> = None;
    while let Some(word) = cur.next() {
        match word {
            Word::Long(name, inline) => match name.as_str() {
                "missing-ok" => {}
                "prefix" => prefix = Some(cur.value("prefix", inline)?),
                other => return Err(cur.usage_error(&format!("unknown option `{other}'"))),
            },
            _ => return Err(cur.usage_only()),
        }
    }
    let ws = ctx.ws()?;
    let index = Index::open(ctx).await?;
    let rows = index.rows(ws).await?;
    let unmerged: Vec<&StageRow> = rows.iter().filter(|r| r.stage > 0).collect();
    if !unmerged.is_empty() {
        for r in unmerged {
            if let Some(git) = r.git {
                eprintln!(
                    "error: {}: unmerged ({git})",
                    String::from_utf8_lossy(&r.path)
                );
            }
        }
        return Err(fatal("git-write-tree: error building trees"));
    }
    let changes = rows_to_changes(ws, index.base(), &rows).await?;
    let root = ws.engine.apply(index.base(), &changes).await?;
    let resolver = Resolver::new(ctx, ws);
    let mut seen: HashSet<Vec<u8>> = HashSet::new();
    for (path, _) in &changes {
        let bytes = path.as_bytes();
        for (at, b) in bytes.iter().enumerate() {
            if *b == b'/'
                && seen.insert(bytes[..at].to_vec())
                && let Ok(dir) = RepoPath::new(bytes[..at].to_vec())
                && let Some(entry) = ws.engine.lookup(root, &dir).await?
            {
                resolver.resolve_entry(&entry);
            }
        }
    }
    let Object::Tree { oid, .. } = resolver.tree(root).await? else {
        return Err(fatal("asc: the root is not a tree"));
    };
    let shown = match prefix {
        None => oid,
        Some(p) => {
            let trimmed = p.trim_end_matches('/');
            let missing = || fatal(format!("git-write-tree: prefix {p} not found"));
            let dir = RepoPath::new(trimmed.as_bytes().to_vec()).map_err(|_| missing())?;
            let entry = ws
                .engine
                .lookup(root, &dir)
                .await?
                .filter(|e| e.kind == Kind::Dir)
                .ok_or_else(missing)?;
            resolver.resolve_entry(&entry);
            entry.git
        }
    };
    resolver.flush().await?;
    println!("{shown}");
    Ok(ExitCode::SUCCESS)
}

pub async fn read_tree(ctx: &Ctx, args: Vec<OsString>) -> Result<ExitCode> {
    let mut cur = Cursor::new(&args, usage::READ_TREE);
    let (mut empty, mut prefix, mut trees) = (false, None::<String>, Vec::new());
    let (mut merge, mut ignore_worktree) = (false, false);
    let (mut aggressive, mut trivial) = (false, false);
    while let Some(word) = cur.next() {
        match word {
            Word::Operand(t) => trees.push(t),
            Word::End => trees.extend(cur.rest()),
            Word::Short(flags) => {
                for c in flags.chars() {
                    match c {
                        'm' => merge = true,
                        'i' => ignore_worktree = true,
                        other => return Err(cur.usage_error(&format!("unknown switch `{other}'"))),
                    }
                }
            }
            Word::Long(name, inline) => match name.as_str() {
                "empty" => empty = true,
                "aggressive" => aggressive = true,
                "trivial" => trivial = true,
                "prefix" => prefix = Some(cur.value("prefix", inline)?),
                other => return Err(cur.usage_error(&format!("unknown option `{other}'"))),
            },
        }
    }
    if merge && prefix.is_some() {
        return Err(fatal("Which one? -m, --reset, or --prefix?"));
    }
    let merging = merge || prefix.is_some();
    let ws = ctx.ws()?;
    let mut index = Index::open(ctx).await?;
    if merging && index.rows(ws).await?.iter().any(|r| r.stage > 0) {
        return Err(fatal("You need to resolve your current index first"));
    }
    let resolver = Resolver::new(ctx, ws);
    let mut roots = Vec::with_capacity(trees.len());
    for (at, word) in trees.iter().enumerate() {
        let found = resolver
            .resolve(word)
            .await?
            .ok_or_else(|| fatal(format!("Not a valid object name {word}")))?;
        if at >= MAX_TREES {
            return Err(fatal(format!("I cannot read more than {MAX_TREES} trees")));
        }
        match resolver.peel(found, ObjectKind::Tree).await? {
            Some(Object::Tree { id, .. }) => roots.push(id),
            _ => return Err(fatal(format!("failed to unpack tree object {word}"))),
        }
    }
    if !empty && trees.is_empty() && !merging {
        eprintln!(
            "warning: read-tree: emptying the index with no arguments is deprecated; use --empty"
        );
    } else if empty && !trees.is_empty() {
        return Err(fatal("passing trees as arguments contradicts --empty"));
    }
    if ignore_worktree && !merging {
        return Err(fatal("-i is meaningless without -m, --reset, or --prefix"));
    }
    if merging && roots.is_empty() {
        return Err(fatal("you must specify at least one tree to merge"));
    }
    if roots.len() > 1 {
        let how = Many {
            merge: merging,
            prefix: prefix.as_deref(),
            check_files: !ignore_worktree,
            opts: tree_merge::Opts {
                aggressive,
                trivial,
                initial: false,
            },
        };
        read_many(ctx, &mut index, &roots, how).await?;
        resolver.flush().await?;
        return Ok(ExitCode::SUCCESS);
    }
    let new_root = roots
        .first()
        .copied()
        .unwrap_or_else(CliEngine::empty_tree_id);
    index.seed(ctx).await?;
    let mut edits: Vec<Edit> = Vec::new();
    let leaf_of = |change: &Change| match change {
        Change::Write {
            kind,
            file,
            size,
            git,
            ..
        } => Some(Leaf {
            kind: *kind,
            file: *file,
            size: *size,
            git: *git,
        }),
        Change::Gitlink { commit } => Some(Leaf {
            kind: Kind::Gitlink,
            file: FileId::from_bytes([0; 32]),
            size: 0,
            git: *commit,
        }),
        Change::Delete => None,
    };
    if let Some(prefix) = prefix {
        let dir = prefix.trim_end_matches('/').to_owned();
        let mut incoming: Vec<(RepoPath, Leaf)> = Vec::new();
        for (path, change) in ws
            .engine
            .leafdiff(CliEngine::empty_tree_id(), new_root, None)
            .await?
        {
            if let Some(leaf) = leaf_of(&change) {
                let full = format!("{dir}/{path}");
                let Ok(full) = RepoPath::new(full.clone().into_bytes()) else {
                    return Err(CliError::Raw(format!("error: invalid path '{full}'"), 128));
                };
                incoming.push((full, leaf));
            }
        }
        let taken: HashSet<&[u8]> = incoming.iter().map(|(p, _)| p.as_bytes()).collect();
        let mut present = index.entries(ws, None).await?;
        while let Some(e) = present.next().await? {
            if taken.contains(e.path.as_slice()) {
                let path = String::from_utf8_lossy(&e.path);
                return Err(CliError::Raw(
                    format!("error: Entry '{path}' overlaps with '{path}'.  Cannot bind."),
                    128,
                ));
            }
        }
        edits.extend(incoming.into_iter().map(|(path, leaf)| Edit::Put {
            path,
            leaf,
            ita: false,
        }));
    } else {
        let mut touched: HashSet<Vec<u8>> = HashSet::new();
        for (path, change) in ws.engine.leafdiff(index.root, new_root, None).await? {
            touched.insert(path.as_bytes().to_vec());
            edits.push(match leaf_of(&change) {
                Some(leaf) => Edit::Put {
                    path,
                    leaf,
                    ita: false,
                },
                None => Edit::Drop(path),
            });
        }
        // Staged differences the tree does not carry go back to HEAD's state.
        for row in index.rows(ws).await? {
            if touched.insert(row.path.clone()) {
                let path = RepoPath::new(row.path).map_err(|e| fatal(e.to_string()))?;
                edits.push(match index.target.head_leaf(ws, &path).await? {
                    Some(leaf) => Edit::Put {
                        path,
                        leaf,
                        ita: false,
                    },
                    None => Edit::Drop(path),
                });
            }
        }
    }
    index.apply_untrusted(ctx, edits).await?;
    resolver.flush().await?;
    Ok(ExitCode::SUCCESS)
}

/// git's `MAX_UNPACK_TREES`.
const MAX_TREES: usize = 8;

/// How `read-tree` combines several trees.
struct Many<'a> {
    /// `-m` or `--prefix`: merge into the index; otherwise the union replaces it.
    merge: bool,
    /// `--prefix`: the trees are read under this directory.
    prefix: Option<&'a str>,
    /// Without `-i`: an entry the merge replaces or drops must match its work-tree file.
    check_files: bool,
    opts: tree_merge::Opts,
}

fn ent_of(entry: &Entry) -> Ent {
    Ent {
        kind: entry.kind,
        git: entry.git,
        file: FileId::from_bytes(entry.id),
        size: entry.size,
        ita: false,
    }
}

async fn read_whole(ws: &WsCtx, root: TreeId, prefix: Option<&str>) -> Result<Tree> {
    let mut out = Tree::new();
    let mut walk = ws.engine.walk(root, None);
    while let Some((path, entry)) = walk.try_next().await? {
        let path = match prefix {
            None => path.as_bytes().to_vec(),
            Some(dir) => format!("{dir}/{path}").into_bytes(),
        };
        out.insert(path, ent_of(&entry));
    }
    Ok(out)
}

/// `read-tree` with two or more trees: the union, `twoway_merge` or `threeway_merge`
/// ([`tree_merge`]), written as the whole set of stage rows over HEAD.
async fn read_many(ctx: &Ctx, index: &mut Index, roots: &[TreeId], how: Many<'_>) -> Result<()> {
    let ws = ctx.ws()?;
    let prefix = how.prefix.map(|p| p.trim_end_matches('/'));
    let mut trees = Vec::with_capacity(roots.len());
    for root in roots {
        trees.push(read_whole(ws, *root, prefix).await?);
    }
    if prefix.is_some() {
        let bad = trees
            .iter()
            .flat_map(|t| t.keys())
            .find(|p| RepoPath::new((*p).clone()).is_err());
        if let Some(path) = bad {
            let path = String::from_utf8_lossy(path);
            return Err(CliError::Raw(format!("error: invalid path '{path}'"), 128));
        }
    }
    let merged = if how.merge {
        let mut current = Tree::new();
        let mut entries = index.entries(ws, None).await?;
        while let Some(e) = entries.next().await? {
            current.insert(
                e.path,
                Ent {
                    kind: e.kind,
                    git: e.oid,
                    file: e.file.unwrap_or_else(|| FileId::from_bytes([0; 32])),
                    size: e.size,
                    ita: e.ita,
                },
            );
        }
        drop(entries);
        let mut opts = how.opts;
        opts.initial = index.is_fresh()
            || (index.root == CliEngine::empty_tree_id() && index.rows(ws).await?.is_empty());
        let mut files = if how.check_files {
            Some(UpToDate::new(ctx, ws)?)
        } else {
            None
        };
        let mut uptodate = |path: &[u8], e: &Ent| -> Result<bool> {
            let Some(files) = files.as_mut() else {
                return Ok(true);
            };
            let Ok(path) = RepoPath::new(path.to_vec()) else {
                return Ok(true);
            };
            let leaf = Leaf {
                kind: e.kind,
                file: e.file,
                size: e.size,
                git: e.git,
            };
            files.check(&path, &leaf, e.ita)
        };
        match tree_merge::merge(&current, &trees, opts, &mut uptodate)? {
            Ok(merged) => merged,
            Err(refusal) => return Err(CliError::Raw(refusal.message(), 128)),
        }
    } else {
        tree_merge::union(&trees)
    };
    let head = read_whole(ws, index.root, None).await?;
    let mut rows = Vec::new();
    for (path, stages) in &merged {
        if let [(0, e)] = stages.as_slice()
            && !e.ita
            && head
                .get(path)
                .is_some_and(|h| h.kind == e.kind && h.git == e.git)
        {
            continue;
        }
        rows.extend(stages.iter().map(|(stage, e)| StageRow {
            path: path.clone(),
            stage: *stage,
            kind: e.kind,
            file: Some(e.file),
            size: e.size,
            git: Some(e.git),
            intent_to_add: e.ita,
        }));
    }
    for path in head.keys().filter(|p| !merged.contains_key(*p)) {
        rows.push(StageRow::from_leaf(path.clone(), None));
    }
    index.target.replace(ctx, rows).await
}
