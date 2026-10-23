//! `write-tree` and `read-tree` (SPEC §9.5): the index as a tree, and a tree as the index. Only
//! the directories on changed paths are read or written; the work tree is never touched.

use std::collections::HashSet;
use std::ffi::OsString;
use std::process::ExitCode;

use asc_core::git::ObjectKind;
use asc_core::id::TreeId;
use asc_core::object::{Change, Kind};
use asc_core::path::RepoPath;
use asc_state::state::stage::{Leaf, StageRow};

use super::index::Index;
use super::objects::{Object, Resolver};
use super::usage;
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
                "prefix" => prefix = Some(cur.value("prefix", inline)?),
                other => return Err(cur.usage_error(&format!("unknown option `{other}'"))),
            },
        }
    }
    if trees.len() > 1 {
        return Err(fatal(
            "asc: read-tree merges of several trees are not supported yet",
        ));
    }
    if ignore_worktree && !merge && prefix.is_none() {
        return Err(fatal("-i is meaningless without -m, --reset, or --prefix"));
    }
    if empty && !trees.is_empty() {
        return Err(fatal("passing trees as arguments contradicts --empty"));
    }
    if !empty && trees.is_empty() {
        eprintln!(
            "warning: read-tree: emptying the index with no arguments is deprecated; use --empty"
        );
    }
    let ws = ctx.ws()?;
    let resolver = Resolver::new(ctx, ws);
    let new_root = match trees.first() {
        None => CliEngine::empty_tree_id(),
        Some(word) => {
            let found = resolver
                .resolve(word)
                .await?
                .ok_or_else(|| fatal(format!("Not a valid object name {word}")))?;
            match resolver.peel(found, ObjectKind::Tree).await? {
                Some(Object::Tree { id, .. }) => id,
                _ => return Err(fatal(format!("failed to unpack tree object {word}"))),
            }
        }
    };
    let mut index = Index::open(ctx).await?;
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
            file: asc_core::id::FileId::from_bytes([0; 32]),
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
