//! `rev-list` and `merge-base` (SPEC §11.2). `rev-list` reuses `log`'s option grammar, revision
//! selection and streaming walk, and prints git ids; `merge-base` asks the history service.

use std::ffi::OsString;
use std::io::Write;
use std::process::ExitCode;

use asc_core::id::ChangesetId;

use super::common::{CliError, Ctx, Result, WsCtx};
use super::log::opts::Opts;
use super::log::revs;
use super::log::walk::{Entry, Hist, Walker};
use super::output::Output;
use super::pager::PagerWant;
use super::revparse::commit_of;
use crate::native::NativeSpec;

/// Serves two commands: clap's multicall mode picks the subcommand from the command name.
#[derive(clap::Parser, Debug)]
#[command(
    multicall = true,
    disable_help_flag = true,
    disable_version_flag = true,
    disable_help_subcommand = true
)]
pub struct Args {
    #[command(subcommand)]
    which: Which,
}

#[derive(clap::Subcommand, Debug)]
enum Which {
    #[command(
        name = "rev-list",
        disable_help_flag = true,
        disable_version_flag = true
    )]
    RevList(Rest),
    #[command(
        name = "merge-base",
        disable_help_flag = true,
        disable_version_flag = true
    )]
    MergeBase(Rest),
}

#[derive(clap::Args, Debug)]
struct Rest {
    #[arg(trailing_var_arg = true, allow_hyphen_values = true, num_args = 0..)]
    rest: Vec<OsString>,
}

const LIST_FLAGS: &[&str] = &[
    "--count",
    "--parents",
    "-n",
    "--max-count",
    "-<n>",
    "--skip",
    "--reverse",
    "--first-parent",
    "--merges",
    "--no-merges",
    "--min-parents",
    "--max-parents",
    "--all",
    "--branches",
    "--tags",
    "--remotes",
    "--not",
    "--topo-order",
    "--date-order",
    "--author-date-order",
    "--left-right",
    "--since",
    "--after",
    "--until",
    "--before",
    "--author",
    "--committer",
    "--grep",
    "-i",
    "--regexp-ignore-case",
    "-E",
    "--extended-regexp",
    "-F",
    "--fixed-strings",
    "--invert-grep",
    "--abbrev-commit",
    "--no-abbrev-commit",
    "--abbrev",
    "--no-abbrev",
];

const BASE_FLAGS: &[&str] = &["--all", "-a", "--is-ancestor"];

pub const NATIVE: &[NativeSpec] = &[
    NativeSpec {
        cmd: "rev-list",
        flags: LIST_FLAGS,
        placeholders: &[],
    },
    NativeSpec {
        cmd: "merge-base",
        flags: BASE_FLAGS,
        placeholders: &[],
    },
];

fn fatal(e: impl std::fmt::Display) -> CliError {
    CliError::Fatal(e.to_string())
}

pub async fn run(ctx: &Ctx, a: Args) -> Result<ExitCode> {
    let result = match a.which {
        Which::RevList(r) => rev_list(ctx, r.rest).await,
        Which::MergeBase(r) => merge_base(ctx, r.rest).await,
    };
    match result {
        Err(CliError::Io(e)) if e.kind() == std::io::ErrorKind::BrokenPipe => Ok(ExitCode::SUCCESS),
        other => other,
    }
}

// --------------------------------------------------------------------------------- rev-list

/// What `rev-list` adds to `log`'s grammar.
struct Extra {
    count: bool,
    parents: bool,
}

/// Takes `--count` and `--parents` out of `args` (before any `--`).
fn split_extra(args: Vec<OsString>) -> (Extra, Vec<OsString>) {
    let mut extra = Extra {
        count: false,
        parents: false,
    };
    let mut rest = Vec::with_capacity(args.len());
    let mut ended = false;
    for a in args {
        ended = ended || a == "--";
        match a.to_str() {
            Some("--count") if !ended => extra.count = true,
            Some("--parents") if !ended => extra.parents = true,
            _ => rest.push(a),
        }
    }
    (extra, rest)
}

fn usage() -> CliError {
    CliError::Usage("usage: git rev-list [<options>] <commit>... [--] [<path>...]".into())
}

async fn rev_list(ctx: &Ctx, args: Vec<OsString>) -> Result<ExitCode> {
    let (extra, args) = split_extra(args);
    let o = Opts::parse(&args)?;
    if o.revs.is_empty() {
        return Err(usage());
    }
    let ws = ctx.ws()?;
    let hist = Hist::select(ws);
    let sel = revs::select(ctx, hist.api(ws), &o).await?;
    let mut walker = Walker::new(ws, &hist, &sel, &o).await?;
    let abbrev = (o.abbrev_commit == Some(true)).then(|| o.abbrev.unwrap_or(7));
    let mut out = Output::open(ctx, "rev-list", PagerWant::Never)?;
    let result = if extra.count {
        count(ws, &mut walker, &o, &mut out).await
    } else if o.reverse {
        let mut all: Vec<Entry> = Vec::new();
        while let Some(e) = walker.next().await? {
            all.push(e);
        }
        let mut r = Ok(());
        for e in all.iter().rev() {
            r = line(ws, e, &o, &extra, abbrev, &mut out).await;
            if r.is_err() {
                break;
            }
        }
        r
    } else {
        let mut r = Ok(());
        while let Some(e) = walker.next().await? {
            r = line(ws, &e, &o, &extra, abbrev, &mut out).await;
            if r.is_err() {
                break;
            }
        }
        r
    };
    let flushed = out.finish();
    result?;
    flushed?;
    Ok(ExitCode::SUCCESS)
}

/// `--count`: one number, or `left<TAB>right` with `--left-right`.
async fn count(ws: &WsCtx, walker: &mut Walker<'_>, o: &Opts, out: &mut Output) -> Result<()> {
    let _ = ws;
    let (mut left, mut right) = (0u64, 0u64);
    while let Some(e) = walker.next().await? {
        if e.left {
            left += 1;
        } else {
            right += 1;
        }
    }
    if o.left_right {
        writeln!(out, "{left}\t{right}")?;
    } else {
        writeln!(out, "{}", left + right)?;
    }
    Ok(())
}

async fn line(
    ws: &WsCtx,
    e: &Entry,
    o: &Opts,
    extra: &Extra,
    abbrev: Option<usize>,
    out: &mut Output,
) -> Result<()> {
    let short = |hex: String| match abbrev {
        Some(n) => hex[..n.clamp(4, 40)].to_owned(),
        None => hex,
    };
    let mut text = String::new();
    if o.left_right {
        text.push(if e.left { '<' } else { '>' });
    }
    text.push_str(&short(e.git.to_string()));
    if extra.parents {
        for p in e.cs.parents.iter() {
            text.push(' ');
            text.push_str(
                &ws.engine
                    .git_commit_of(*p)
                    .await
                    .map_err(fatal)?
                    .to_string(),
            );
        }
    }
    writeln!(out, "{text}")?;
    Ok(())
}

// ------------------------------------------------------------------------------- merge-base

fn base_usage() -> CliError {
    CliError::Usage("usage: git merge-base [-a | --all] <commit> <commit>...\n   or: git merge-base --is-ancestor <commit> <commit>".into())
}

async fn merge_base(ctx: &Ctx, args: Vec<OsString>) -> Result<ExitCode> {
    let (mut all, mut ancestor) = (false, false);
    let mut words = Vec::new();
    for a in &args {
        match a.to_string_lossy().as_ref() {
            "--all" | "-a" => all = true,
            "--is-ancestor" => ancestor = true,
            w => words.push(w.to_owned()),
        }
    }
    if words.len() < 2 || (ancestor && words.len() != 2) {
        return Err(base_usage());
    }
    let ws = ctx.ws()?;
    let mut ids: Vec<ChangesetId> = Vec::with_capacity(words.len());
    for w in &words {
        let id = commit_of(ctx, w)
            .await?
            .ok_or_else(|| fatal(format!("Not a valid object name {w}")))?;
        ids.push(id);
    }
    let hist = Hist::select(ws);
    let api = hist.api(ws);
    if ancestor {
        let yes = api.is_ancestor(ids[0], ids[1]).await.map_err(fatal)?;
        return Ok(ExitCode::from(u8::from(!yes)));
    }
    let bases = api
        .merge_base(ids[0], &ids[1..], all)
        .await
        .map_err(fatal)?;
    if bases.is_empty() {
        return Ok(ExitCode::from(1));
    }
    let mut out = Output::open(ctx, "merge-base", PagerWant::Never)?;
    let mut result = Ok(());
    for b in bases.iter().take(if all { usize::MAX } else { 1 }) {
        match ws.engine.git_commit_of(*b).await {
            Ok(git) => {
                if let Err(e) = writeln!(out, "{git}") {
                    result = Err(CliError::Io(e));
                    break;
                }
            }
            Err(e) => {
                result = Err(fatal(e));
                break;
            }
        }
    }
    let flushed = out.finish();
    result?;
    flushed?;
    Ok(ExitCode::SUCCESS)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn os(items: &[&str]) -> Vec<OsString> {
        items.iter().map(OsString::from).collect()
    }

    #[test]
    fn extra_flags_stop_at_the_pathspec_separator() {
        let (e, rest) = split_extra(os(&["--count", "HEAD", "--", "--parents"]));
        assert!(e.count && !e.parents);
        assert_eq!(rest, os(&["HEAD", "--", "--parents"]));
    }

    #[test]
    fn multicall_picks_the_command() {
        use clap::Parser as _;
        let a = Args::try_parse_from(["merge-base", "--all", "a", "b"]).unwrap();
        assert!(matches!(a.which, Which::MergeBase(r) if r.rest == os(&["--all", "a", "b"])));
        let a = Args::try_parse_from(["rev-list", "--count", "HEAD", "--", "x"]).unwrap();
        assert!(
            matches!(a.which, Which::RevList(r) if r.rest == os(&["--count", "HEAD", "--", "x"]))
        );
    }
}
