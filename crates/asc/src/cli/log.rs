//! `log` (SPEC §11.2): git's revision walk and output, plus the asc-only `--json`.
//!
//! The walk follows git's own order ([`order`]); a path filter, `--follow` or a server history
//! hands the walk to the engine's `HistoryApi` instead (SPEC §4.3, §6.3). Formatting is
//! [`super::format`]; `--graph` is a port of git's lane algorithm ([`graph`]).

use std::ffi::OsString;
use std::io;
use std::process::ExitCode;

use asc_core::id::{ChangesetId, RefTarget};
use asc_core::path::RefName;
use asc_store::refs::Refs;

use super::common::{CliError, Ctx, Result, WsCtx};
use super::format::Reflog;
use super::output::Output;
use super::pager::PagerWant;
use crate::native::NativeSpec;

pub mod decorate;
pub mod describe;
pub mod diffout;
pub mod emit;
pub mod graph;
pub mod json;
pub mod opts;
pub mod order;
pub mod pretty;
pub mod refs;
pub mod revs;
pub mod walk;

use emit::{Cmd, Printer};
use opts::Opts;
use walk::{Entry, Hist, Walker};

#[derive(clap::Parser, Debug)]
#[command(name = "log", disable_help_flag = true, disable_version_flag = true)]
pub struct Args {
    /// Flags, revisions and pathspecs, in git's grammar (parsed by [`Opts`]).
    #[arg(trailing_var_arg = true, allow_hyphen_values = true, num_args = 0..)]
    pub rest: Vec<OsString>,
}

/// The `--format` placeholders `log` and `show` serve: git 2.55's complete set.
pub const PLACEHOLDERS: &[&str] = &[
    "%H", "%h", "%T", "%t", "%P", "%p", "%a", "%c", "%d", "%D", "%S", "%e", "%s", "%f", "%b", "%B",
    "%N", "%G", "%g", "%m", "%x", "%C", "%w", "%<", "%>", "%+", "%-", "% ", "%(trailers)",
    "%(describe)", "%(decorate)", "%(count)", "%(total)",
];

/// The output flags `log` and `show` share.
pub const OUTPUT_FLAGS: &[&str] = &[
    "--oneline", "--format", "--pretty", "--abbrev-commit", "--no-abbrev-commit", "--abbrev",
    "--no-abbrev", "--date", "--relative-date", "--decorate", "--no-decorate", "--source",
    "--color", "--no-color", "-p", "-u", "--patch", "-s", "--no-patch", "--stat", "--shortstat",
    "--numstat", "--name-only", "--name-status", "--raw", "--summary", "-U", "--unified", "-z",
    "-M", "--no-renames", "--patience", "--histogram", "--minimal", "--diff-algorithm",
    "--end-of-options",
];

/// The flags only `log` serves.
const LOG_FLAGS: &[&str] = &[
    "-n", "--max-count", "-<n>", "--skip", "--since", "--after", "--until", "--before", "--author",
    "--committer", "--grep", "-i", "--regexp-ignore-case", "-E", "--extended-regexp", "-F",
    "--fixed-strings", "--invert-grep", "--first-parent", "--all", "--branches", "--tags",
    "--remotes", "--not", "--follow", "--merges", "--no-merges", "--min-parents", "--max-parents",
    "--reverse", "--topo-order", "--date-order", "--author-date-order", "--left-right", "-g",
    "--walk-reflogs", "--graph", "--json",
];

pub const NATIVE: &[NativeSpec] = &[
    NativeSpec { cmd: "log", flags: OUTPUT_FLAGS, placeholders: PLACEHOLDERS },
    NativeSpec { cmd: "log", flags: LOG_FLAGS, placeholders: PLACEHOLDERS },
];

fn fatal(e: impl std::fmt::Display) -> CliError {
    CliError::Fatal(e.to_string())
}

/// A broken pipe means the pager or the reader left: not an error.
fn quiet_pipe(r: Result<ExitCode>) -> Result<ExitCode> {
    match r {
        Err(CliError::Io(e)) if e.kind() == io::ErrorKind::BrokenPipe => Ok(ExitCode::SUCCESS),
        other => other,
    }
}

pub async fn run(ctx: &Ctx, a: Args) -> Result<ExitCode> {
    let o = Opts::parse(&a.rest)?;
    quiet_pipe(log(ctx, o).await)
}

async fn log(ctx: &Ctx, o: Opts) -> Result<ExitCode> {
    let ws = ctx.ws()?;
    let hist = Hist::select(ws);
    let api = hist.api(ws);
    if o.graph && o.reverse {
        return Err(CliError::Raw("fatal: options '--reverse' and '--graph' cannot be used together".into(), 128));
    }
    if o.max_count == Some(0) {
        return Ok(ExitCode::SUCCESS);
    }
    let sel = revs::select(ctx, api, &o).await?;
    if o.json {
        let walker = Walker::new(ws, &hist, &sel, &o).await?;
        json::emit(ws, walker, o.reverse).await?;
        return Ok(ExitCode::SUCCESS);
    }
    let mut out = Output::open(ctx, "log", PagerWant::Auto)?;
    let mut printer = Printer::new(ctx, ws, api, &o, &sel, Cmd::Log, out.color()).await?;
    let result = if o.walk_reflogs {
        reflog_walk(ctx, ws, &o, &mut printer, &mut out).await
    } else {
        let mut walker = Walker::new(ws, &hist, &sel, &o).await?;
        if o.reverse {
            let mut all: Vec<Entry> = Vec::new();
            while let Some(e) = walker.next().await? {
                all.push(e);
            }
            let mut r = Ok(());
            for e in all.iter().rev() {
                r = printer.print(&mut out, e, None).await;
                if r.is_err() {
                    break;
                }
            }
            r
        } else {
            let mut r = Ok(());
            while let Some(e) = walker.next().await? {
                r = printer.print(&mut out, &e, None).await;
                if r.is_err() {
                    break;
                }
            }
            r
        }
    };
    let flushed = out.finish();
    result?;
    flushed?;
    Ok(ExitCode::SUCCESS)
}

/// `log -g`: the reflog of a ref, newest first.
async fn reflog_walk(
    ctx: &Ctx,
    ws: &WsCtx,
    o: &Opts,
    printer: &mut Printer<'_>,
    out: &mut Output,
) -> Result<()> {
    let typed = o
        .revs
        .iter()
        .find_map(|t| match t {
            opts::Tok::Rev(r) => Some(r.clone()),
            _ => None,
        })
        .unwrap_or_else(|| "HEAD".into());
    let full = if typed == "HEAD" {
        match ws.projection.read_head()? {
            asc_git::projection::Head::Branch(b) => format!("refs/heads/{b}"),
            asc_git::projection::Head::Detached(_) => {
                return Err(CliError::Fatal("asc: the reflog of a detached HEAD is not kept".into()));
            }
        }
    } else if typed.starts_with("refs/") {
        typed.clone()
    } else {
        format!("refs/heads/{typed}")
    };
    let name = RefName::new(full.clone().into_bytes()).map_err(fatal)?;
    let limit = o.max_count.map_or(10_000, |n| (n + o.skip).min(10_000)) as u32;
    let log = ws.db.refs().log(ws.repo.key, &name, limit).await?;
    let short = full.strip_prefix("refs/heads/").unwrap_or(&full).to_owned();
    for (i, entry) in log.iter().enumerate().skip(o.skip) {
        let Some(RefTarget::Changeset(id)) = entry.new else { continue };
        let reflog = Reflog {
            selector: format!("{typed}@{{{i}}}"),
            short_selector: format!("{}@{{{i}}}", if typed == "HEAD" { "HEAD" } else { &short }),
            name: entry.actor.id.clone().into_bytes(),
            email: Vec::new(),
            subject: entry.reason.clone().into_bytes(),
        };
        let e = load(ws, id).await?;
        printer.print(out, &e, Some(&reflog)).await?;
    }
    let _ = ctx;
    Ok(())
}

/// A walk entry for `id` outside a walk (`show`, `log -g`).
pub async fn load(ws: &WsCtx, id: ChangesetId) -> Result<Entry> {
    let cs = ws.engine.get_changeset(id).await?;
    let git = ws.engine.git_commit_of(id).await.map_err(fatal)?;
    let shown_parents = cs.parents.clone();
    Ok(Entry { id, git, generation: 0, cs, shown_parents, left: false })
}
