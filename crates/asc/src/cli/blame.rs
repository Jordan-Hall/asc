//! `blame` (SPEC §11.2): the origin of every line of a file, in git's default layout and in
//! `--porcelain`, `--line-porcelain` and `--incremental`, plus the asc-only `--json`
//! (`BlameResponse` lines).
//!
//! Committed content comes from the engine's `blame.v1` ([`compute::committed`]). Without a
//! revision the working file is blamed too: lines unchanged from HEAD keep HEAD's origin, the
//! others are `Not Committed Yet`. `-w` and `--reverse` are applied here ([`compute`]).
//! Not served: `-M`, `-C`, `-L` with a pattern or function name, `--contents`, `.mailmap`.

use std::ffi::OsString;
use std::io::{self, Write};
use std::process::ExitCode;

use asc_core::path::RepoPath;

use super::add::target::{Staged, Target};
use super::add::{cwd_prefix, spec::repo_path};
use super::common::{CliError, Ctx, Result};
use super::output::Output;
use super::pager::PagerWant;
use super::restore::resolve_commit;
use crate::native::NativeSpec;

mod compute;
mod emit;
mod opts;

use compute::{Blamed, Piece, fatal};
use opts::{Format, Opts};

#[derive(clap::Parser, Debug)]
#[command(name = "blame", disable_help_flag = true, disable_version_flag = true)]
pub struct Args {
    /// Flags, an optional revision and the file, in git's grammar (parsed by [`Opts`]).
    #[arg(trailing_var_arg = true, allow_hyphen_values = true, num_args = 0..)]
    pub rest: Vec<OsString>,
}

pub const NATIVE: &[NativeSpec] = &[NativeSpec {
    cmd: "blame",
    flags: &[
        "-L", "-w", "-p", "--porcelain", "--line-porcelain", "--incremental", "--reverse", "--root",
        "--json", "--end-of-options",
    ],
    placeholders: &[],
}];

pub async fn run(ctx: &Ctx, a: Args) -> Result<ExitCode> {
    let o = Opts::parse(&a.rest)?;
    match blame(ctx, o).await {
        Err(CliError::Io(e)) if e.kind() == io::ErrorKind::BrokenPipe => Ok(ExitCode::SUCCESS),
        other => other,
    }
}

fn bad_revision(word: &str) -> CliError {
    CliError::Fatal(format!("bad revision '{word}'"))
}

async fn rev_of(ctx: &Ctx, word: &str) -> Result<asc_core::id::ChangesetId> {
    resolve_commit(ctx, word).await?.ok_or_else(|| bad_revision(word))
}

async fn compute(ctx: &Ctx, o: &Opts, path: &RepoPath) -> Result<Blamed> {
    let ws = ctx.ws()?;
    if o.reverse {
        let Some(word) = &o.rev else {
            return Err(CliError::Fatal("No commit to dig up from?".into()));
        };
        let (from, to) = word.split_once("..").unwrap_or((word, ""));
        let from = if from.is_empty() { "HEAD" } else { from };
        let to = if to.is_empty() { "HEAD" } else { to };
        let (start, end) = (rev_of(ctx, from).await?, rev_of(ctx, to).await?);
        return compute::reverse(ws, start, end, path, from, o.ignore_ws).await;
    }
    if let Some(word) = &o.rev {
        let cs = rev_of(ctx, word).await?;
        return compute::committed(ws, cs, path, Some(word), o.ignore_ws).await;
    }
    let head = ws.head_commit().await?.ok_or_else(|| CliError::Fatal("no such ref: HEAD".into()))?;
    let in_head = compute::entry_at(ws, head, path).await?.is_some();
    if !in_head {
        let staged = Target::open(ctx).await?.staged(ws, path).await?;
        if staged == Staged::Absent {
            return Err(compute::no_such_path(path, None));
        }
    }
    let gate = asc_wc::SafeRoot::new(ws.projection.toplevel());
    match compute::read_work_file(&gate, path)? {
        Some(work) => compute::working(ws, head, path, work, o.ignore_ws).await,
        None => Err(CliError::Fatal(format!(
            "Cannot lstat '{}': No such file or directory",
            String::from_utf8_lossy(path.as_bytes())
        ))),
    }
}

/// The pieces inside the zero-based `ranges`, clipped to them, in line order.
fn shown_pieces(blamed: &Blamed, ranges: &[std::ops::Range<u32>]) -> Vec<Piece> {
    let mut out = Vec::new();
    for r in ranges {
        let first = blamed.pieces.partition_point(|p| p.end() <= r.start);
        out.extend(
            blamed.pieces[first..]
                .iter()
                .take_while(|p| p.start < r.end)
                .filter_map(|p| p.clip(r.start, r.end)),
        );
    }
    out
}

async fn blame(ctx: &Ctx, o: Opts) -> Result<ExitCode> {
    let ws = ctx.ws()?;
    let prefix = cwd_prefix(ctx, ws);
    let bytes = repo_path(&o.file, &prefix, ws.projection.toplevel())?;
    let path = RepoPath::new(bytes).map_err(fatal)?;
    let blamed = compute(ctx, &o, &path).await?;
    let shown_name = String::from_utf8_lossy(path.as_bytes()).into_owned();
    let ranges = opts::resolve_ranges(&o.ranges, blamed.lines(), &shown_name)?;
    let shown = shown_pieces(&blamed, &ranges);
    if shown.is_empty() {
        return Ok(ExitCode::SUCCESS);
    }
    let facts = emit::Facts::gather(ws, &blamed, &shown, o.root).await?;
    let style = emit::Style {
        abbrev: super::switch::abbrev_len(ctx),
        quote_high: ctx.config.get("core.quotepath").as_deref() != Some("false"),
    };
    let want = if o.format == Format::Json { PagerWant::Never } else { PagerWant::Auto };
    let mut out = Output::open(ctx, "blame", want)?;
    let written = match o.format {
        Format::Default => emit::default_layout(&mut out, &blamed, &shown, &facts, &path, &style),
        Format::Porcelain => emit::porcelain(&mut out, &blamed, &shown, &facts, false, &style),
        Format::LinePorcelain => emit::porcelain(&mut out, &blamed, &shown, &facts, true, &style),
        Format::Incremental => emit::incremental(&mut out, &shown, &facts, &style),
        Format::Json => emit::json(ws, &shown, &facts, &mut out).await,
    };
    let flushed = out.flush().and_then(|()| out.finish());
    written?;
    flushed?;
    Ok(ExitCode::SUCCESS)
}
