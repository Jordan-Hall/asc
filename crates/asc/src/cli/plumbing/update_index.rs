//! `update-index` (SPEC §9.5): stages files, `--cacheinfo` entries and removals in the stage set
//! `GIT_INDEX_FILE` names, then tells the synthetic index through `Target::apply`. `--refresh`
//! reports the tracked files whose work tree content differs.

use std::ffi::{OsStr, OsString};
use std::process::ExitCode;

use asc_core::content::ingest;
use asc_core::git::ObjectKind;
use asc_core::id::{FileId, GitOid};
use asc_core::object::Kind;
use asc_core::path::RepoPath;
use asc_state::state::git_map;
use asc_state::state::stage::Leaf;
use asc_store::cachedir::CacheDir;
use asc_store::storeset::StoreSet;
use asc_wc::SafeRoot;
use asc_wc::plain::{Sparse, Wc};
use asc_wc::redirect::Redirections;
use asc_wc::status::{NoEol, compute};

use super::hash_object::store;
use super::index::Index;
use super::objects::{Object, size};
use super::usage;
use crate::cli::add::target::{Edit, Staged};
use crate::cli::add::{convert, cwd_prefix, spec::repo_path, workspace_attrs};
use crate::cli::common::{CliError, Ctx, Result, WsCtx};
use crate::cli::refs_plumbing::args::{Cursor, Word, parse_oid};
use crate::ext;

#[derive(Default)]
struct Opts {
    add: bool,
    remove: bool,
    force_remove: bool,
    refresh: bool,
    quiet: bool,
    /// `--cacheinfo` entries and files in command-line order.
    steps: Vec<Step>,
}

enum Step {
    Cacheinfo {
        mode: String,
        oid: String,
        path: String,
    },
    File {
        path: String,
        add: bool,
        remove: bool,
        force_remove: bool,
    },
}

fn parse(args: &[OsString]) -> Result<Opts> {
    let mut cur = Cursor::new(args, usage::UPDATE_INDEX);
    let mut o = Opts::default();
    while let Some(word) = cur.next() {
        match word {
            Word::End => {
                for path in cur.rest() {
                    o.steps.push(Step::File {
                        path,
                        add: o.add,
                        remove: o.remove,
                        force_remove: o.force_remove,
                    });
                }
            }
            Word::Operand(path) => {
                o.steps.push(Step::File {
                    path,
                    add: o.add,
                    remove: o.remove,
                    force_remove: o.force_remove,
                });
            }
            Word::Short(flags) => {
                for c in flags.chars() {
                    match c {
                        'q' => o.quiet = true,
                        other => return Err(cur.usage_error(&format!("unknown switch `{other}'"))),
                    }
                }
            }
            Word::Long(name, inline) => match name.as_str() {
                "add" => o.add = true,
                "remove" => o.remove = true,
                "force-remove" => o.force_remove = true,
                "refresh" | "really-refresh" => o.refresh = true,
                "cacheinfo" => {
                    let first = cur.value("cacheinfo", inline)?;
                    let parts: Vec<&str> = first.splitn(3, ',').collect();
                    let (mode, oid, path) = if let [m, h, p] = parts.as_slice() {
                        ((*m).to_owned(), (*h).to_owned(), (*p).to_owned())
                    } else if cur.remaining() >= 2 {
                        let oid = cur.value("cacheinfo", None)?;
                        let path = cur.value("cacheinfo", None)?;
                        (first, oid, path)
                    } else {
                        return Err(CliError::Raw(
                            "error: option 'cacheinfo' expects <mode>,<sha1>,<path>".into(),
                            129,
                        ));
                    };
                    o.steps.push(Step::Cacheinfo { mode, oid, path });
                }
                other => return Err(cur.usage_error(&format!("unknown option `{other}'"))),
            },
        }
    }
    Ok(o)
}

fn cannot_process(path: &str) -> CliError {
    CliError::Raw(format!("fatal: Unable to process path {path}"), 128)
}

/// The leaf for a `--cacheinfo` entry.
async fn cacheinfo_leaf(ws: &WsCtx, mode: &str, oid: &str, path: &str) -> Result<Leaf> {
    let invalid = || {
        eprintln!("error: invalid object {mode} {oid} for '{path}'");
        CliError::Raw(
            format!("fatal: git update-index: --cacheinfo cannot add {path}"),
            128,
        )
    };
    let kind = match u32::from_str_radix(mode, 8) {
        Ok(0o100_644) | Ok(0o100_664) => Kind::File,
        Ok(0o100_755) => Kind::Exec,
        Ok(0o120_000) => Kind::Symlink,
        Ok(0o160_000) => Kind::Gitlink,
        _ => return Err(invalid()),
    };
    let git: GitOid = parse_oid(oid).ok_or_else(invalid)?;
    if kind == Kind::Gitlink {
        return Ok(Leaf {
            kind,
            file: FileId::from_bytes([0; 32]),
            size: 0,
            git,
        });
    }
    let asc = git_map::git_to_asc(&ws.db, ws.repo.key, git, ObjectKind::Blob)
        .await?
        .ok_or_else(invalid)?;
    let file = FileId::from_bytes(asc);
    let size = size(ws, Object::Blob { oid: git, file })
        .await
        .map_err(|_| invalid())?;
    Ok(Leaf {
        kind,
        file,
        size,
        git,
    })
}

/// Ingests the work tree file at `path` as `add` would; `None` when it does not exist.
async fn stage_file(
    ctx: &Ctx,
    ws: &WsCtx,
    path: &RepoPath,
    prev: Option<Kind>,
) -> Result<Option<Leaf>> {
    let gate = SafeRoot::new(ws.projection.toplevel());
    let Some(abs) = gate.resolve(path) else {
        return Ok(None);
    };
    let meta = match std::fs::symlink_metadata(&abs) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    if meta.is_dir() {
        eprintln!("error: {path}: is a directory - add files inside instead");
        return Err(cannot_process(&path.to_string()));
    }
    let mut attrs = workspace_attrs(ctx, ws)?;
    let eol = attrs
        .lookup(path)
        .map_err(|e| CliError::Fatal(e.to_string()))?
        .eol;
    let symlink = meta.file_type().is_symlink();
    let got = if symlink {
        ingest(std::io::Cursor::new(convert::link_target(&abs)?))
    } else {
        convert::ingest_file(&abs, eol, &ws.home.tmp())
    }
    .map_err(|e| CliError::Fatal(e.to_string()))?;
    let filemode = ctx.config.bool("core.filemode").unwrap_or(cfg!(unix));
    let core_symlinks = ctx.config.bool("core.symlinks").unwrap_or(cfg!(unix));
    let exec = convert::exec_bit(&meta).filter(|_| filemode);
    let kind = match (symlink, exec, prev) {
        (true, ..) => Kind::Symlink,
        (false, _, Some(Kind::Symlink)) if !core_symlinks => Kind::Symlink,
        (false, Some(x), _) => {
            if x {
                Kind::Exec
            } else {
                Kind::File
            }
        }
        (false, None, Some(Kind::Exec)) => Kind::Exec,
        (false, None, _) => Kind::File,
    };
    store(ws, &got).await?;
    Ok(Some(Leaf {
        kind,
        file: got.file,
        size: got.size,
        git: got.git,
    }))
}

/// Tracked paths whose work tree content differs from the staged state.
async fn needing_update(ctx: &Ctx, ws: &WsCtx) -> Result<Vec<String>> {
    let root = ws.projection.toplevel().to_owned();
    let index_path = ws.projection.git_dir().join("index");
    let redirections = Redirections::load(&root).map_err(|e| CliError::Fatal(e.to_string()))?;
    let sparse = ws.workspace.ws.sparse.as_deref().map(Sparse::parse);
    let sources = super::ls_files::ignore_sources(ctx, ws);
    let store = StoreSet {
        store: CacheDir::open(ws.home.store(), ws.home.tmp())?,
        cache: CacheDir::open(ws.home.cache(), ws.home.tmp())?,
    };
    let wc = Wc {
        engine: &ws.engine,
        store: &store,
        root: &root,
        index_path: &index_path,
        symlinks: ctx.config.bool("core.symlinks").unwrap_or(!cfg!(windows)),
        sparse: sparse.as_ref(),
        redirections: &redirections,
        ignore: &sources,
        eol: &NoEol,
    };
    let status = compute(&wc, &ws.db, &ws.workspace)
        .await
        .map_err(|e| CliError::Fatal(e.to_string()))?;
    Ok(status.unstaged.iter().map(|c| c.path.to_string()).collect())
}

async fn flush(ctx: &Ctx, index: &mut Index, edits: Vec<Edit>, trusted: bool) -> Result<()> {
    if edits.is_empty() {
        Ok(())
    } else if trusted {
        index.target.apply(ctx, edits).await
    } else {
        index.apply_untrusted(ctx, edits).await
    }
}

pub async fn run(ctx: &Ctx, args: Vec<OsString>) -> Result<ExitCode> {
    let o = parse(&args)?;
    let ws = ctx.ws()?;
    let mut index = Index::open(ctx).await?;
    let prefix = cwd_prefix(ctx, ws);
    // Each edit and whether the work tree file holds the staged content.
    let mut edits: Vec<(Edit, bool)> = Vec::new();
    for step in &o.steps {
        match step {
            Step::Cacheinfo { mode, oid, path } => {
                let Ok(rp) = RepoPath::new(path.as_bytes().to_vec()) else {
                    eprintln!("error: Invalid path '{path}'");
                    return Err(CliError::Raw(
                        format!("fatal: git update-index: --cacheinfo cannot add {path}"),
                        128,
                    ));
                };
                let leaf = cacheinfo_leaf(ws, mode, oid, path).await?;
                if !o.add && matches!(index.staged(ws, &rp).await?, Staged::Absent) {
                    eprintln!("error: {path}: cannot add to the index - missing --add option?");
                    return Err(CliError::Raw(
                        format!("fatal: git update-index: --cacheinfo cannot add {path}"),
                        128,
                    ));
                }
                edits.push((
                    Edit::Put {
                        path: rp,
                        leaf,
                        ita: false,
                    },
                    false,
                ));
            }
            Step::File {
                path,
                add,
                remove,
                force_remove,
            } => {
                let bytes = repo_path(OsStr::new(path), &prefix, ws.projection.toplevel())?;
                let rp = RepoPath::new(bytes).map_err(|e| CliError::Fatal(e.to_string()))?;
                let staged = index.staged(ws, &rp).await?;
                let tracked = !matches!(staged, Staged::Absent);
                if *force_remove {
                    if tracked {
                        edits.push((Edit::Drop(rp), true));
                    }
                    continue;
                }
                let prev = match &staged {
                    Staged::Leaf { leaf, .. } => Some(leaf.kind),
                    _ => None,
                };
                match stage_file(ctx, ws, &rp, prev).await? {
                    Some(leaf) => {
                        if !tracked && !add {
                            eprintln!(
                                "error: {path}: cannot add to the index - missing --add option?"
                            );
                            return Err(cannot_process(path));
                        }
                        edits.push((
                            Edit::Put {
                                path: rp,
                                leaf,
                                ita: false,
                            },
                            true,
                        ));
                    }
                    None if *remove => {
                        if tracked {
                            edits.push((Edit::Drop(rp), true));
                        }
                    }
                    None => {
                        eprintln!("error: {path}: does not exist and --remove not passed");
                        return Err(cannot_process(path));
                    }
                }
            }
        }
    }
    if !edits.is_empty() {
        index.seed(ctx).await?;
        // Runs of one kind apply together, so edits to one path keep their command-line order.
        let mut run: Vec<Edit> = Vec::new();
        let mut trusted = true;
        for (edit, is_trusted) in edits {
            if is_trusted != trusted && !run.is_empty() {
                flush(ctx, &mut index, std::mem::take(&mut run), trusted).await?;
            }
            trusted = is_trusted;
            run.push(edit);
        }
        flush(ctx, &mut index, run, trusted).await?;
    }
    if o.refresh {
        let stale = needing_update(ctx, ws).await?;
        if stale.is_empty() {
            ext::index_sink()
                .on_stage_changed(ws.workspace.ws.id, &[])
                .await
                .map_err(|e| CliError::Fatal(e.to_string()))?;
        } else if !o.quiet {
            for path in &stale {
                println!("{path}: needs update");
            }
            return Ok(ExitCode::from(1));
        }
    }
    Ok(ExitCode::SUCCESS)
}
