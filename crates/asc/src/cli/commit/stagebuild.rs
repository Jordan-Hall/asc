//! Stage rows to changes (SPEC §9.7 step 5), the work tree to stage rows (`-a`, `commit
//! <paths>`) and the temporary index file for hooks. Commit itself reads no file content: only
//! the paths that `-a` or a path list names are ingested, as `asc add` would.

use std::collections::BTreeSet;
use std::fs;
use std::path::Path;

use asc_core::codec::Enveloped;
use asc_core::content::{Body, ingest};
use asc_core::git::ObjectKind;
use asc_core::id::TreeId;
use asc_core::object::{Change, Kind};
use asc_core::path::RepoPath;
use asc_engine::derive::GitMapRow;
use asc_engine::tree::Pathspec;
use asc_git::index::{self, PlainStat, RenderOptions};
use asc_state::state::git_map;
use asc_state::state::stage::{Leaf, StageRow};
use asc_store::cachedir::CacheDir;
use asc_store::codec::encode_stored;
use asc_store::key::ObjKey;
use asc_store::objects::Objects;
use asc_store::storeset::StoreSet;
use asc_wc::ignore::IgnoreSources;
use asc_wc::SafeRoot;
use asc_wc::plain::{Sparse, Wc};
use asc_wc::redirect::Redirections;
use asc_wc::status::{self, ChangeKind, NoEol};
use futures::TryStreamExt;

use crate::cli::common::{CliError, Result, WsCtx};

fn fatal(text: impl Into<String>) -> CliError {
    CliError::Fatal(text.into())
}

/// git's refusal to commit with conflict stages in the index.
fn unmerged() -> CliError {
    CliError::Raw(
        "error: Committing is not possible because you have unmerged files.\n\
         hint: Fix them up in the work tree, and then use 'git add/rm <file>'\n\
         hint: as appropriate to mark resolution and make a commit.\n\
         fatal: Exiting because of an unresolved conflict."
            .to_owned(),
        128,
    )
}

/// The changes the rows make on top of `base`. Intent-to-add rows stage nothing; a delete of a
/// path `base` does not have, and a row equal to `base`'s entry, change nothing.
pub async fn rows_to_changes(
    ws: &WsCtx,
    base: TreeId,
    rows: &[StageRow],
) -> Result<Vec<(RepoPath, Change)>> {
    let mut out = Vec::with_capacity(rows.len());
    for r in rows {
        if r.stage != 0 {
            return Err(unmerged());
        }
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

/// A leaf for the file at `path` in the work tree, ingested into `store/`; `None` when the path
/// does not exist. `prev` keeps the executable bit where the file system has none.
pub async fn ingest_path(ws: &WsCtx, path: &RepoPath, prev: Option<Kind>) -> Result<Option<Leaf>> {
    // Behind a symlinked or reparse-point ancestor the path is outside the work tree: it is gone.
    let Some(abs) = SafeRoot::new(ws.projection.toplevel()).resolve(path) else { return Ok(None) };
    let meta = match fs::symlink_metadata(&abs) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    let ingested = if meta.file_type().is_symlink() {
        let target = fs::read_link(&abs)?;
        let text = target.to_string_lossy().replace('\\', "/");
        ingest(std::io::Cursor::new(text.into_bytes()))
    } else if meta.is_file() {
        ingest(fs::File::open(&abs)?)
    } else {
        return Err(fatal(format!("asc: cannot stage {path}: not a file")));
    }
    .map_err(|e| fatal(e.to_string()))?;
    let kind = if meta.file_type().is_symlink() {
        Kind::Symlink
    } else if is_exec(&meta, prev) {
        Kind::Exec
    } else {
        Kind::File
    };
    match ingested.body {
        Body::Inline(bytes) => {
            ws.objects
                .put(&ObjKey::File(ingested.file), encode_stored(&bytes))
                .await?;
        }
        Body::Chunked { list, chunks } => {
            for (id, data) in chunks {
                ws.objects
                    .put(&ObjKey::Chunk(id), encode_stored(&data))
                    .await?;
            }
            ws.objects
                .put(
                    &ObjKey::ChunkList(ingested.file),
                    encode_stored(&list.to_envelope()),
                )
                .await?;
        }
    }
    git_map::insert_many(
        &ws.db,
        ws.repo.key,
        &[GitMapRow {
            git: ingested.git,
            kind: ObjectKind::Blob,
            asc: *ingested.file.as_bytes(),
        }],
    )
    .await?;
    Ok(Some(Leaf {
        kind,
        file: ingested.file,
        size: ingested.size,
        git: ingested.git,
    }))
}

#[cfg(unix)]
fn is_exec(meta: &fs::Metadata, _prev: Option<Kind>) -> bool {
    use std::os::unix::fs::PermissionsExt;
    meta.permissions().mode() & 0o111 != 0
}

#[cfg(not(unix))]
fn is_exec(_meta: &fs::Metadata, prev: Option<Kind>) -> bool {
    prev == Some(Kind::Exec)
}

/// The row that stages the work tree's state of `path` (a staged delete when it is gone).
pub async fn row_of_path(ws: &WsCtx, path: &RepoPath, prev: Option<Kind>) -> Result<StageRow> {
    let leaf = ingest_path(ws, path, prev).await?;
    Ok(StageRow::from_leaf(path.as_bytes().to_vec(), leaf.as_ref()))
}

/// `commit -a`: a row for every tracked path whose work tree state differs from the stage.
pub async fn stat_dirty_rows(
    ws: &WsCtx,
    config: &crate::cli::config::ConfigSnapshot,
) -> Result<Vec<StageRow>> {
    let root = ws.projection.toplevel();
    let store = StoreSet {
        store: CacheDir::open(ws.home.store(), ws.home.tmp())?,
        cache: CacheDir::open(ws.home.cache(), ws.home.tmp())?,
    };
    let redirections = Redirections::load(root).map_err(|e| fatal(e.to_string()))?;
    let sparse = ws.workspace.ws.sparse.as_deref().map(Sparse::parse);
    let ignore = IgnoreSources {
        asc_excludes: config
            .path("asc.excludesFile")
            .or_else(|| std::env::home_dir().map(|h| h.join(".config").join("asc").join("ignore")))
            .unwrap_or_default(),
        core_excludes: config.path("core.excludesFile"),
        local_exclude: root.join(".asc").join("info").join("exclude"),
        common_exclude: ws.projection.common_dir().join("info").join("exclude"),
    };
    let index_path = ws.projection.git_dir().join("index");
    let wc = Wc {
        engine: &ws.engine,
        store: &store,
        root,
        index_path: &index_path,
        symlinks: config.bool("core.symlinks").unwrap_or(!cfg!(windows)),
        sparse: sparse.as_ref(),
        redirections: &redirections,
        ignore: &ignore,
        eol: &NoEol,
    };
    let state = status::compute(&wc, &ws.db, &ws.workspace)
        .await
        .map_err(|e| fatal(e.to_string()))?;
    let mut rows = Vec::new();
    for change in state.unstaged {
        match change.kind {
            ChangeKind::Deleted => {
                rows.push(StageRow::from_leaf(change.path.as_bytes().to_vec(), None))
            }
            _ => rows.push(row_of_path(ws, &change.path, None).await?),
        }
    }
    Ok(rows)
}

/// `path` as typed in `cwd_prefix` (the current directory below the root) to a root-relative
/// path with `.` and `..` resolved. `None` when it leaves the work tree.
fn normalise(cwd_prefix: &str, arg: &str) -> Option<String> {
    let mut parts: Vec<&str> = cwd_prefix.split('/').filter(|p| !p.is_empty()).collect();
    for part in arg.split(['/', '\\']) {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            p => parts.push(p),
        }
    }
    Some(parts.join("/"))
}

/// A pathspec from the command line, relative to `cwd_prefix`. `None` selects every path.
pub fn pathspec(cwd_prefix: &str, args: &[String]) -> Result<Option<Pathspec>> {
    let mut items = Vec::new();
    for arg in args {
        if arg.starts_with(':') {
            items.push(arg.clone());
            continue;
        }
        let Some(rel) = normalise(cwd_prefix, arg) else {
            return Err(fatal(format!("{arg}: '{arg}' is outside repository")));
        };
        if rel.is_empty() {
            return Ok(None);
        }
        items.push(rel);
    }
    Pathspec::parse(items)
        .map(Some)
        .map_err(|e| fatal(e.to_string()))
}

/// Tracked paths selected by `spec`: leaves of `base` and rows of the stage.
pub async fn tracked_matches(
    ws: &WsCtx,
    base: TreeId,
    rows: &[StageRow],
    spec: Option<&Pathspec>,
) -> Result<Vec<(RepoPath, Option<Kind>)>> {
    let mut found: std::collections::BTreeMap<RepoPath, Option<Kind>> = Default::default();
    let mut walk = ws.engine.walk(base, spec);
    while let Some((path, entry)) = walk.try_next().await? {
        found.insert(path, Some(entry.kind));
    }
    drop(walk);
    for r in rows {
        let Ok(path) = RepoPath::new(r.path.clone()) else {
            continue;
        };
        if spec.is_none_or(|s| s.matches(&path)) {
            found.entry(path).or_insert(Some(r.kind));
        }
    }
    Ok(found.into_iter().collect())
}

/// Writes the index file `path` for `root`: git's format, stat data from the work tree.
pub async fn write_index_file(ws: &WsCtx, root: TreeId, path: &Path) -> Result<()> {
    let mut entries = Vec::new();
    let mut walk = ws.engine.walk(root, None);
    while let Some(pair) = walk.try_next().await? {
        entries.push(pair);
    }
    drop(walk);
    let stat = PlainStat {
        root: ws.projection.toplevel().to_owned(),
    };
    let file = index::render(entries.into_iter(), &stat, &RenderOptions::default());
    index::write(path, &file).map_err(|e| fatal(e.to_string()))
}

/// The git map rows of the trees along the changed paths, so `ls-tree` and export find them.
pub async fn index_spine(ws: &WsCtx, root: TreeId, changes: &[(RepoPath, Change)]) -> Result<()> {
    let sink = git_map::RepoGitMap {
        db: ws.db.clone(),
        repo: ws.repo.key,
    };
    let mut seen: BTreeSet<RepoPath> = BTreeSet::new();
    asc_engine::derive::index_tree_git(&ws.engine, root, &sink)
        .await
        .map_err(|e| fatal(e.to_string()))?;
    for (path, change) in changes {
        let mut rows = Vec::new();
        if let Change::Write { file, git, .. } = change {
            rows.push(GitMapRow {
                git: *git,
                kind: ObjectKind::Blob,
                asc: *file.as_bytes(),
            });
        }
        if !rows.is_empty() {
            git_map::insert_many(&ws.db, ws.repo.key, &rows).await?;
        }
        let mut dir = path.parent();
        while let Some(d) = dir {
            dir = d.parent();
            if !seen.insert(d.clone()) {
                break;
            }
            if let Some(e) = ws.engine.lookup(root, &d).await?
                && e.kind == Kind::Dir
            {
                asc_engine::derive::index_tree_git(&ws.engine, TreeId::from_bytes(e.id), &sink)
                    .await
                    .map_err(|e| fatal(e.to_string()))?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalise_resolves_dots_against_the_prefix() {
        assert_eq!(normalise("sub/", "a.txt").as_deref(), Some("sub/a.txt"));
        assert_eq!(normalise("sub/", "../b").as_deref(), Some("b"));
        assert_eq!(normalise("", ".").as_deref(), Some(""));
        assert_eq!(normalise("", "../x"), None);
    }

    #[test]
    fn dot_selects_every_path() {
        assert!(pathspec("", &[".".to_owned()]).unwrap().is_none());
        assert!(pathspec("", &["a".to_owned()]).unwrap().is_some());
    }
}
