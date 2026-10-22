//! `diff` (SPEC §4.3, §11.1): the work tree against the index, the index against a commit
//! (`--cached`), the work tree or the index against a commit, two commits, `A..B` and `A...B`,
//! and two paths on disk (`--no-index`). Output is git's byte for byte in every served form;
//! `--json` (asc only) prints one `DiffResponse` line per changed path. The sides are built in
//! `diff/sides.rs`, the output forms are written by `asc_engine::diff` and ordered in
//! `diff/render.rs`.

pub(crate) mod attrs;
pub(crate) mod binary;
pub(crate) mod noindex;
pub(crate) mod opts;
pub(crate) mod render;
pub(crate) mod sides;
pub(crate) mod work;

use std::ffi::OsString;
use std::io::Write;
use std::path::Path;
use std::process::ExitCode;

use asc_core::id::ChangesetId;
use asc_core::path::RepoPath;
use asc_engine::diff::{Attributes, ContentSource, NoAttributes, SideKind};
use asc_engine::history::HistoryApi;
use asc_engine::tree::Pathspec;

use self::opts::{Common, Formats, RawArgs, apply_config, parse};
use self::attrs::DiffAttrs;
use self::render::{JsonShape, Target, render};
use self::sides::Built;
use self::work::LocalFiles;
use super::add::cwd_prefix;
use super::common::{CliError, Ctx, Result, WsCtx};
use super::output::Output;
use super::pager::PagerWant;
use super::restore::{Spec, resolve_commit, root_of};
use crate::msg::Msg;
use crate::native::NativeSpec;

pub type Args = RawArgs;

/// Flags every diff command serves.
macro_rules! flags {
    ($($extra:literal),*) => {
        &[
            "-p", "-u", "--patch", "-s", "--no-patch", "--raw", "--patch-with-raw", "--patch-with-stat",
            "--stat", "--stat-width", "--stat-name-width", "--stat-graph-width", "--stat-count",
            "--numstat", "--shortstat", "--summary", "--name-only", "--name-status", "-z", "-U",
            "--unified", "--inter-hunk-context", "--minimal", "--patience", "--histogram",
            "--diff-algorithm", "--indent-heuristic", "--no-indent-heuristic", "-M", "--find-renames",
            "--no-renames", "--renames", "--src-prefix", "--dst-prefix", "--no-prefix", "--relative",
            "--no-relative", "--abbrev", "--full-index", "--binary", "-a", "--text", "--no-color",
            "--color", "--no-ext-diff", "--no-textconv", "--submodule", "--word-diff", "--exit-code",
            "--quiet", "--output", "--diff-filter", "--json",
            $($extra),*
        ]
    };
}
pub(crate) use flags;

pub const NATIVE: &[NativeSpec] = &[NativeSpec {
    cmd: "diff",
    flags: flags!("--cached", "--staged", "--no-index"),
    placeholders: &[],
}];

pub(crate) fn fatal(text: impl Into<String>) -> CliError {
    CliError::Fatal(text.into())
}

pub(crate) fn unsupported(flag: &str) -> CliError {
    CliError::Msg(Msg::AscOptionUnsupported { flag: flag.to_owned() })
}

/// Hands back the flags of `c.extra` that `allowed` does not name.
pub(crate) fn reject_extras(c: &Common, allowed: &[&str]) -> Result<()> {
    match c.extra.iter().find(|f| !allowed.contains(&f.as_str())) {
        Some(flag) => Err(unsupported(flag)),
        None => Ok(()),
    }
}

/// `--relative` without a directory means the current one.
pub(crate) fn resolve_relative(ctx: &Ctx, ws: &WsCtx, c: &mut Common) {
    if c.opts.relative_config && c.opts.relative.is_none() {
        let prefix = cwd_prefix(ctx, ws);
        c.opts.relative = RepoPath::new(prefix.trim_end_matches('/').as_bytes().to_vec()).ok();
    }
}

/// The pathspec of `items`, or `None` for the whole tree.
pub(crate) fn pathspec(ctx: &Ctx, items: &[OsString]) -> Result<Option<Pathspec>> {
    if items.is_empty() {
        return Ok(None);
    }
    Ok(Some(Spec::new(ctx, items)?.spec))
}

/// How [`finish`] prints.
pub(crate) struct Emit<'a> {
    pub pager: PagerWant,
    /// A line before the diff (`diff-tree` prints the commit id), written when there are changes.
    pub header: Option<&'a [u8]>,
    /// The plumbing commands print raw ids in full.
    pub plumbing: bool,
    /// Read `diff` attributes from the work tree (not for `--no-index`).
    pub attrs: bool,
}

/// Applies the status filter and writes the result: to `--output`, else stdout (paged for the
/// porcelain), as text or as `--json` lines. Returns git's exit code.
pub(crate) async fn finish(
    ctx: &Ctx,
    c: &Common,
    fmt: Formats,
    src: &dyn ContentSource,
    mut built: Built,
    emit: Emit<'_>,
) -> Result<ExitCode> {
    let Emit { pager, header, plumbing, attrs } = emit;
    if let Some(filter) = &c.filter {
        built.changes.retain(|ch| filter.keeps(ch.status));
    }
    let differs = !built.changes.is_empty() || !built.unmerged.is_empty();
    if !c.quiet {
        let level = binary::zlib_level(ctx.config.get("core.looseCompression").as_deref(), ctx.config.get("core.compression").as_deref());
        let loaded = match ctx.ws_opt().filter(|_| attrs) {
            Some(ws) => Some(DiffAttrs::load(ctx, ws)?),
            None => None,
        };
        let attrs: &dyn Attributes = match &loaded {
            Some(a) => a,
            None => &NoAttributes,
        };
        let raw_abbrev = if plumbing && !c.abbrev_given { 40 } else { c.opts.abbrev };
        let target = Target {
            fmt,
            opts: &c.opts,
            src,
            attrs,
            text: c.text,
            binary: c.binary,
            raw_abbrev,
            zlib_level: level,
        };
        let json = if c.json { Some(JsonShape::load()?) } else { None };
        let written = match &c.output {
            Some(path) => {
                let mut file = std::io::BufWriter::new(std::fs::File::create(path)?);
                write_all(&mut file, &target, json.as_ref(), &built, header).await?;
                file.flush().map_err(CliError::Io)
            }
            None => {
                let want = if c.json { PagerWant::Never } else { pager };
                let mut out = Output::open(ctx, "diff", want)?;
                let result = write_all(&mut out, &target, json.as_ref(), &built, header).await;
                let closed = out.finish();
                result.and(closed.map_err(CliError::Io))
            }
        };
        match written {
            Err(CliError::Io(e)) if e.kind() == std::io::ErrorKind::BrokenPipe => {}
            other => other?,
        }
    }
    Ok(if c.exit_code && differs { ExitCode::from(1) } else { ExitCode::SUCCESS })
}

async fn write_all<W: Write>(
    w: &mut W,
    t: &Target<'_>,
    json: Option<&JsonShape>,
    built: &Built,
    header: Option<&[u8]>,
) -> Result<()> {
    match json {
        Some(shape) => shape.emit(w, t, &built.changes, &built.unmerged).await,
        None => {
            if let Some(line) = header.filter(|_| !built.changes.is_empty() || !built.unmerged.is_empty()) {
                w.write_all(line)?;
            }
            render(w, t, &built.changes, &built.unmerged).await
        }
    }
}

// ------------------------------------------------------------------ revisions

/// The commit `spec` names, or git's "bad revision" error.
async fn commit_of(ctx: &Ctx, spec: &str) -> Result<ChangesetId> {
    resolve_commit(ctx, spec).await?.ok_or_else(|| {
        fatal(format!(
            "ambiguous argument '{spec}': unknown revision or path not in the working tree.\n\
             Use '--' to separate paths from revisions, like this:\n\
             'git <command> [<revision>...] -- [<file>...]'"
        ))
    })
}

/// A commit, or `HEAD` for an empty side of `A..`.
async fn side_commit(ctx: &Ctx, spec: &str) -> Result<ChangesetId> {
    let spec = if spec.is_empty() { "HEAD" } else { spec };
    commit_of(ctx, spec).await
}

/// The revisions and paths among the arguments before `--`: revisions first, then the first
/// argument that is no revision starts the paths, which must exist unless `--` was given.
pub(crate) async fn split_args(ctx: &Ctx, c: &Common, limit: usize) -> Result<(Vec<String>, Vec<OsString>)> {
    let mut revs = Vec::new();
    let mut paths: Vec<OsString> = Vec::new();
    for arg in &c.positional {
        let text = arg.to_string_lossy().into_owned();
        if paths.is_empty() && revs.len() < limit {
            let range = text.split_once("..").is_some();
            let found = range || resolve_commit(ctx, &text).await?.is_some();
            if found {
                if !c.dashdash && ctx.cwd.join(&text).symlink_metadata().is_ok() && !range {
                    return Err(fatal(format!(
                        "ambiguous argument '{text}': both revision and filename\n\
                         Use '--' to separate paths from revisions, like this:\n\
                         'git <command> [<revision>...] -- [<file>...]'"
                    )));
                }
                revs.push(text);
                continue;
            }
        }
        if !c.dashdash && ctx.cwd.join(arg).symlink_metadata().is_err() {
            return Err(fatal(format!(
                "ambiguous argument '{text}': unknown revision or path not in the working tree.\n\
                 Use '--' to separate paths from revisions, like this:\n\
                 'git <command> [<revision>...] -- [<file>...]'"
            )));
        }
        paths.push(arg.clone());
    }
    paths.extend(c.paths.iter().cloned());
    Ok((revs, paths))
}

/// The two commits an `A..B` or `A...B` argument stands for.
async fn range(ctx: &Ctx, ws: &WsCtx, text: &str) -> Result<(ChangesetId, ChangesetId)> {
    let (left, right, merge_base) = match text.split_once("...") {
        Some((l, r)) => (l, r, true),
        None => {
            let (l, r) = text.split_once("..").unwrap_or((text, ""));
            (l, r, false)
        }
    };
    let (a, b) = (side_commit(ctx, left).await?, side_commit(ctx, right).await?);
    if !merge_base {
        return Ok((a, b));
    }
    let bases = HistoryApi::merge_base(&ws.engine, a, &[b], false).await.map_err(|e| fatal(e.to_string()))?;
    let base = bases.first().copied().ok_or_else(|| fatal(format!("{text}: no merge base")))?;
    Ok((base, b))
}

// ------------------------------------------------------------------------ run

pub async fn run(ctx: &Ctx, args: Args) -> Result<ExitCode> {
    let mut seed = asc_engine::diff::DiffOptions::default();
    apply_config(ctx, &mut seed, true)?;
    let mut c = parse(seed, &args.rest)?;
    reject_extras(&c, &["--cached", "--staged", "--no-index"])?;
    let flag = |name: &str| c.extra.iter().any(|f| f == name);
    let (cached, no_index) = (flag("--cached") || flag("--staged"), flag("--no-index"));
    let fmt = c.fmt.resolve(false);
    if no_index || (ctx.ws_opt().is_none() && c.positional.len() + c.paths.len() == 2) {
        return run_no_index(ctx, c, fmt).await;
    }
    let ws = ctx.ws()?;
    resolve_relative(ctx, ws, &mut c);
    let (revs, paths) = split_args(ctx, &c, 2).await?;
    let spec = pathspec(ctx, &paths)?;
    let mut local = LocalFiles::new(&ws.engine);

    let (built, sides) = match (cached, revs.as_slice()) {
        (true, []) => {
            let base = sides::head_root(ws).await?;
            (sides::cached(ws, base, spec.as_ref(), &c.opts).await?, (SideKind::Commit, SideKind::Index))
        }
        (true, [rev]) => {
            let base = root_of(ws, Some(commit_of(ctx, rev).await?)).await?;
            (sides::cached(ws, base, spec.as_ref(), &c.opts).await?, (SideKind::Commit, SideKind::Index))
        }
        (false, []) => {
            let built = sides::worktree(ctx, ws, &mut local, None, spec.as_ref()).await?;
            (built, (SideKind::Index, SideKind::Worktree))
        }
        (false, [rev]) if !rev.contains("..") => {
            let base = root_of(ws, Some(commit_of(ctx, rev).await?)).await?;
            let built = sides::worktree(ctx, ws, &mut local, Some(base), spec.as_ref()).await?;
            (built, (SideKind::Commit, SideKind::Worktree))
        }
        (true, _) => return Err(CliError::Usage("error: --cached takes at most one revision".into())),
        (false, revs) => {
            let (a, b) = match revs {
                [one] => range(ctx, ws, one).await?,
                [x, y] => (commit_of(ctx, x).await?, commit_of(ctx, y).await?),
                _ => return Err(CliError::Usage("error: too many revisions".into())),
            };
            let (ra, rb) = (root_of(ws, Some(a)).await?, root_of(ws, Some(b)).await?);
            (sides::trees(ws, ra, rb, spec.as_ref(), &c.opts).await?, (SideKind::Commit, SideKind::Commit))
        }
    };
    c.opts.sides = sides;
    let mut built = built;
    if sides.1 == SideKind::Worktree {
        built.changes = sides::detect_renames(built.changes, &local, &c.opts).await?;
    }
    finish(ctx, &c, fmt, &local, built, Emit { pager: PagerWant::Auto, header: None, plumbing: false, attrs: true }).await
}

async fn run_no_index(ctx: &Ctx, mut c: Common, fmt: Formats) -> Result<ExitCode> {
    let mut args = c.positional.iter().chain(c.paths.iter());
    let (Some(a), Some(b), None) = (args.next(), args.next(), args.next()) else {
        return Err(CliError::Usage("usage: git diff --no-index [<options>] <path> <path>".into()));
    };
    c.exit_code = true;
    c.opts.sides = (SideKind::Object, SideKind::Object);
    let mut local = match ctx.ws_opt() {
        Some(ws) => LocalFiles::new(&ws.engine),
        None => LocalFiles::detached(),
    };
    let changes = noindex::changes(&mut local, &ctx.cwd, Path::new(a), Path::new(b))?;
    finish(ctx, &c, fmt, &local, Built { changes, unmerged: Vec::new() }, Emit { pager: PagerWant::Auto, header: None, plumbing: false, attrs: false }).await
}
