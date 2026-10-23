//! `add -p` (SPEC §9.5): hunk-by-hunk staging. Every tracked file whose work tree content (after
//! the §2.4.1 normalisation `add` applies) differs from its staged content is shown as git's
//! `diff-files -p` shows it; the hunks the user selects are applied to the staged content and
//! that blob is staged the way `add` stages a file. The dialogue lives in `add_patch_ui`, the
//! diff and its hunks in `add_patch_diff`.
//!
//! Limits: a side over [`MAX_SIDE_BYTES`] or [`MAX_SIDE_LINES`] and a type change are not offered (`add` stages them
//! whole); binary files are skipped, as in git. Answers are read a line at a time.

use std::fs;
use std::io::{self, Cursor, Read};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use asc_core::content::{Body, ingest};
use asc_core::object::Kind;
use asc_core::path::RepoPath;
use asc_engine::derive::GitMapRow;
use asc_engine::diff::line::is_binary;
use asc_engine::diff::{Attributes, DiffAttr, DiffOptions, SideKind, mode};
use asc_state::state::git_map;
use asc_state::state::stage::Leaf;
use asc_wc::SafeRoot;

use super::add::attrs::Attrs;
use super::add::spec::Spec;
use super::add::target::{Edit, Staged, Target};
use super::add::{self, Args, convert};
use super::add_patch_diff::{self as pd, FileDiff, MAX_EDIT_BYTES, MAX_SIDE_BYTES, MAX_SIDE_LINES, Settings, Shape, Side};
use super::add_patch_ui::{Input, Io, read_line};
use super::commit::message;
use super::common::{CliError, Ctx, Result, WsCtx};
use super::diff::attrs::DiffAttrs;
use super::diff::opts::apply_config;

/// The answers: standard input, locked for one line at a time (nothing is held across an await).
struct Stdin;

impl Input for Stdin {
    fn line(&mut self) -> io::Result<Option<Vec<u8>>> {
        read_line(&mut io::stdin().lock())
    }
}

/// A file ready to be reviewed, and what staging it needs to know.
struct Item {
    path: RepoPath,
    diff: FileDiff,
    staged: Leaf,
    /// Staged as intent-to-add: the file is not in the index yet.
    ita: bool,
    /// The kind the work tree entry would be staged as; `None` when it is gone.
    work_kind: Option<Kind>,
}

enum Prep {
    /// Nothing to offer: unchanged, not a regular file, or too large to diff here.
    Skip,
    Binary,
    Item(Box<Item>),
}

pub async fn run(ctx: &Ctx, a: &Args) -> Result<ExitCode> {
    let ws = ctx.ws()?;
    if a.dry_run {
        return Err(CliError::Fatal(
            "options '--dry-run' and '--interactive/--patch' cannot be used together".into(),
        ));
    }
    let prefix = add::cwd_prefix(ctx, ws);
    // Without a pathspec git offers the whole repository, not just the current directory.
    let spec = Spec::parse(&a.pathspec, &prefix, ws.projection.toplevel())?;
    let mut target = Target::open(ctx).await?;
    let (opts, settings) = diff_settings(ctx, a)?;
    let mut attrs = add::workspace_attrs(ctx, ws)?;
    let diff_attrs = DiffAttrs::load(ctx, ws)
        .map_err(|e| CliError::Fatal(format!("cannot read attributes: {e}")))?;
    let gate = SafeRoot::new(ws.projection.toplevel());

    let mut found = Vec::new();
    target
        .tracked(ws, &spec, |path, tracked| {
            found.push((path.clone(), tracked.staged));
            Ok(())
        })
        .await?;
    found.sort_by(|x, y| x.0.as_bytes().cmp(y.0.as_bytes()));
    for (path, staged) in &found {
        if *staged == Staged::Unmerged {
            println!("{path}: needs merge");
        }
    }

    let tmp = ws.home.tmp();
    let mut editor = |text: &[u8]| run_editor(ctx, &tmp, text);
    let mut io = Io { input: Stdin, out: io::stdout() };
    let (mut offered, mut binary) = (false, false);
    for (path, staged) in found {
        let Staged::Leaf { leaf, ita } = staged else { continue };
        let env = Env { ctx, ws, gate: &gate, opts: &opts, settings: &settings, diff_attrs: &diff_attrs };
        let mut item = match prepare(&env, &mut attrs, path, leaf, ita).await? {
            Prep::Skip => continue,
            Prep::Binary => {
                binary = true;
                continue;
            }
            Prep::Item(item) => item,
        };
        offered = true;
        let quit = io.review(&mut item.diff, &mut editor)?;
        stage(ctx, ws, &mut target, *item).await?;
        if quit {
            return Ok(ExitCode::SUCCESS);
        }
    }
    if !offered {
        println!("{}", if binary { "Only binary files changed." } else { "No changes." });
    }
    Ok(ExitCode::SUCCESS)
}

/// The `diff.*` settings and `-U`/`--inter-hunk-context`, which win over them.
fn diff_settings(ctx: &Ctx, a: &Args) -> Result<(DiffOptions, Settings)> {
    let mut opts = DiffOptions { sides: (SideKind::Index, SideKind::Worktree), ..DiffOptions::default() };
    apply_config(ctx, &mut opts, false)?;
    if let Some(n) = a.unified {
        opts.context = n;
    }
    if let Some(n) = a.inter_hunk_context {
        opts.inter_hunk = n;
    }
    let settings = Settings {
        context: opts.context,
        inter_hunk: opts.inter_hunk,
        algorithm: opts.algorithm,
        indent_heuristic: opts.indent_heuristic,
    };
    Ok((opts, settings))
}

struct Env<'a> {
    ctx: &'a Ctx,
    ws: &'a WsCtx,
    gate: &'a SafeRoot,
    opts: &'a DiffOptions,
    settings: &'a Settings,
    diff_attrs: &'a DiffAttrs,
}

fn skip(why: impl std::fmt::Display) -> Prep {
    eprintln!("warning: {why}");
    Prep::Skip
}

/// What the work tree holds at a tracked path.
enum Work {
    Gone,
    Present(PathBuf, fs::Metadata),
}

/// Reads both sides of `path` and builds its diff.
async fn prepare(env: &Env<'_>, attrs: &mut Attrs, path: RepoPath, leaf: Leaf, ita: bool) -> Result<Prep> {
    let fatal = |e: &dyn std::fmt::Display| CliError::Fatal(format!("{path}: {e}"));
    if matches!(leaf.kind, Kind::Gitlink | Kind::Dir) {
        return Ok(Prep::Skip);
    }
    if leaf.size > MAX_SIDE_BYTES {
        return Ok(too_large(&path));
    }
    let work = match env.gate.resolve(&path) {
        None => Work::Gone,
        Some(abs) => match fs::symlink_metadata(&abs) {
            Ok(m) if m.is_dir() => Work::Gone,
            Ok(m) => Work::Present(abs, m),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Work::Gone,
            Err(e) => return Err(fatal(&format!("cannot stat: {e}"))),
        },
    };
    let core_symlinks = env.ctx.config.bool("core.symlinks").unwrap_or(cfg!(unix));
    let filemode = env.ctx.config.bool("core.filemode").unwrap_or(cfg!(unix));
    let (new, work_kind) = match work {
        Work::Gone if ita => return Ok(Prep::Skip),
        Work::Gone => (None, None),
        Work::Present(abs, meta) => {
            let link = meta.file_type().is_symlink();
            if !link && !meta.is_file() {
                return Ok(skip(format!("'{path}' is not a regular file; not offered by --patch")));
            }
            let staged_link = leaf.kind == Kind::Symlink;
            if link != staged_link && !(staged_link && !core_symlinks) {
                return Ok(skip(format!("'{path}' changed type; stage it with 'add {path}'")));
            }
            let (bytes, oid) = if link {
                let target = convert::link_target(&abs).map_err(|e| fatal(&e))?;
                let oid = convert::bytes_oid(&target).map_err(|e| fatal(&e))?;
                (target, oid)
            } else {
                if meta.len() > MAX_SIDE_BYTES {
                    return Ok(too_large(&path));
                }
                let eol = attrs.lookup(&path).map_err(|e| CliError::Fatal(e.to_string()))?.eol;
                let got = convert::ingest_file(&abs, eol, &env.ws.home.tmp()).map_err(|e| fatal(&e))?;
                let Body::Inline(bytes) = got.body else {
                    return Ok(too_large(&path));
                };
                (bytes.to_vec(), got.git)
            };
            let exec = convert::exec_bit(&meta).filter(|_| filemode);
            let kind = add::staged_kind(link, exec, Some(leaf.kind), core_symlinks);
            (Some((bytes, oid)), Some(kind))
        }
    };

    let old_bytes = if ita || leaf.size == 0 {
        Vec::new()
    } else {
        let bytes = env.ws.engine.read_file(leaf.file, leaf.size).await.map_err(|e| fatal(&e))?;
        if bytes.len() as u64 != leaf.size {
            return Err(fatal(&"the staged object does not match its recorded size"));
        }
        bytes.to_vec()
    };
    if too_many_lines(&old_bytes) || new.as_ref().is_some_and(|(b, _)| too_many_lines(b)) {
        return Ok(too_large(&path));
    }
    let new_bytes = new.as_ref().map_or(&[][..], |(b, _)| b.as_slice());
    let unchanged = !ita && new.is_some() && new_bytes == old_bytes.as_slice() && work_kind == Some(leaf.kind);
    if unchanged {
        return Ok(Prep::Skip);
    }
    let is_binary_side = match env.diff_attrs.diff_attr(&path) {
        DiffAttr::Binary => true,
        DiffAttr::Text => false,
        _ => is_binary(&old_bytes) || is_binary(new_bytes),
    };
    if is_binary_side {
        return Ok(Prep::Binary);
    }

    let old = (!ita).then(|| Side { content: old_bytes, mode: mode(leaf.kind), oid: Some(leaf.git) });
    let new_side = match (new, work_kind) {
        (Some((content, oid)), Some(kind)) => Some(Side { content, mode: mode(kind), oid: Some(oid) }),
        _ => None,
    };
    let (pa, pb) = env.opts.prefixes();
    let name = |prefix: &str| [prefix.as_bytes(), path.as_bytes()].concat();
    let mut diff = pd::build(&name(&pa), &name(&pb), old, new_side, env.opts.abbrev, env.settings);
    diff.path = path.as_bytes().to_vec();
    if !diff.offered() {
        return Ok(Prep::Skip);
    }
    Ok(Prep::Item(Box::new(Item { path, diff, staged: leaf, ita, work_kind })))
}

fn too_large(path: &RepoPath) -> Prep {
    skip(format!(
        "'{path}' is over {MAX_SIDE_BYTES} bytes or {MAX_SIDE_LINES} lines; not offered by --patch, stage it with 'add {path}'"
    ))
}

/// Whether a side has more lines than [`MAX_SIDE_LINES`].
fn too_many_lines(bytes: &[u8]) -> bool {
    bytes.iter().filter(|&&b| b == b'\n').count() >= MAX_SIDE_LINES
}

/// Stages what the dialogue chose for one file.
async fn stage(ctx: &Ctx, ws: &WsCtx, target: &mut Target, item: Item) -> Result<()> {
    let Item { path, diff, staged, ita, work_kind } = item;
    if !diff.any_used() {
        return Ok(());
    }
    let edit = if diff.shape == Shape::Deleted {
        Edit::Drop(path)
    } else {
        let content = diff
            .assemble()
            .map_err(|_| CliError::Fatal(format!("{path}: the chosen hunks do not apply to the staged file")))?;
        let kind = if diff.mode_selected() { work_kind.unwrap_or(staged.kind) } else { staged.kind };
        let leaf = if ita || content != diff.base() {
            let got = ingest(Cursor::new(content))
                .map_err(|e| CliError::Fatal(format!("{path}: unable to index file: {e}")))?;
            add::store(ctx, ws, &got).await?;
            let row = GitMapRow { git: got.git, kind: asc_core::git::ObjectKind::Blob, asc: *got.file.as_bytes() };
            git_map::insert_many(&ws.db, ws.repo.key, &[row]).await?;
            Leaf { kind, file: got.file, size: got.size, git: got.git }
        } else {
            Leaf { kind, ..staged }
        };
        Edit::Put { path, leaf, ita: false }
    };
    target.apply(ctx, vec![edit]).await
}

/// Opens `text` in the editor and returns what it left in the file; `None` when the editor
/// failed. The editor is started as git starts it (`sh -c '<editor> "$@"'`).
fn run_editor(ctx: &Ctx, dir: &Path, text: &[u8]) -> io::Result<Option<Vec<u8>>> {
    fs::create_dir_all(dir)?;
    let file = dir.join(format!("addp-hunk-edit-{}.diff", uuid::Uuid::now_v7()));
    fs::write(&file, text)?;
    let editor = message::editor(ctx);
    let ok = if editor == ":" {
        true
    } else {
        let mut cmd = ctx.child(ctx.shell()).map_err(|e| io::Error::other(e.to_string()))?;
        cmd.arg("-c").arg(format!("{editor} \"$@\"")).arg(&editor).arg(&file);
        let status = cmd.status()?;
        if !status.success() {
            eprintln!("error: There was a problem with the editor '{editor}'.");
        }
        status.success()
    };
    let mut edited = Vec::new();
    if ok {
        fs::File::open(&file)?.take(MAX_EDIT_BYTES as u64 + 1).read_to_end(&mut edited)?;
    }
    let _ = fs::remove_file(&file);
    Ok(ok.then_some(edited))
}
