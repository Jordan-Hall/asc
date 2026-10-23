//! `for-each-ref` (SPEC §11.2): the refs under the patterns, formatted by ref-filter.

use std::ffi::OsString;
use std::io::{Read, Write};
use std::process::ExitCode;

use super::args::{Cursor, Word, resolve_oid, resolve_target};
use crate::cli::common::{CliError, Ctx, Result};
use crate::cli::output::Output;
use crate::cli::pager::PagerWant;
use crate::cli::ref_filter::{self, Filters, Format, Quote, Request, SortKey};
use asc_core::id::RefTarget;

const USAGE: &str = super::usage::FOR_EACH_REF;

/// The format when none is given.
const DEFAULT_FORMAT: &str = "%(objectname) %(objecttype)\t%(refname)";

#[derive(Default)]
struct Opts {
    count: usize,
    quote: Quote,
    sorts: Vec<String>,
    format: Option<String>,
    include_root: bool,
    points_at: Vec<String>,
    merged: Vec<String>,
    no_merged: Vec<String>,
    contains: Vec<String>,
    no_contains: Vec<String>,
    ignore_case: bool,
    omit_empty: bool,
    excludes: Vec<String>,
    stdin: bool,
    color: Option<bool>,
    start_after: Option<String>,
    patterns: Vec<String>,
}

fn parse_color(value: &str) -> Option<Option<bool>> {
    Some(match value {
        "always" | "true" | "yes" | "on" | "1" | "" => Some(true),
        "never" | "false" | "no" | "off" | "0" => Some(false),
        "auto" => None,
        _ => return None,
    })
}

fn parse(args: &[OsString]) -> Result<Opts> {
    let mut cur = Cursor::new(args, USAGE);
    let mut o = Opts::default();
    while let Some(word) = cur.next() {
        match word {
            Word::End => {
                o.patterns.extend(cur.rest());
            }
            Word::Operand(p) => o.patterns.push(p),
            Word::Short(flags) => match flags.as_str() {
                "s" => o.quote = Quote::Shell,
                "p" => o.quote = Quote::Perl,
                other => {
                    return Err(cur.usage_error(&format!(
                        "unknown switch `{}'",
                        other.chars().next().unwrap_or('-')
                    )));
                }
            },
            Word::Long(name, inline) => match name.as_str() {
                "shell" | "perl" | "python" | "tcl" => {
                    o.quote = Quote::from_flag(&format!("--{name}")).unwrap_or_default();
                }
                "count" => {
                    let v = cur.value("count", inline)?;
                    let n: i64 = v
                        .parse()
                        .map_err(|_| cur.usage_error("option `count' expects a numerical value"))?;
                    if n < 0 {
                        return Err(cur.usage_error(&format!("invalid --count argument: `{n}'")));
                    }
                    o.count = n as usize;
                }
                "sort" => o.sorts.push(cur.value("sort", inline)?),
                "format" => o.format = Some(cur.value("format", inline)?),
                "include-root-refs" => o.include_root = true,
                "points-at" => o.points_at.push(cur.value("points-at", inline)?),
                "merged" => o.merged.push(cur.value_or(inline, "HEAD")),
                "no-merged" => o.no_merged.push(cur.value_or(inline, "HEAD")),
                "contains" => o.contains.push(cur.value_or(inline, "HEAD")),
                "no-contains" => o.no_contains.push(cur.value_or(inline, "HEAD")),
                "ignore-case" => o.ignore_case = true,
                "omit-empty" => o.omit_empty = true,
                "exclude" => o.excludes.push(cur.value("exclude", inline)?),
                "stdin" => o.stdin = true,
                "color" => {
                    let v = inline.unwrap_or_else(|| "always".to_owned());
                    o.color = parse_color(&v).ok_or_else(|| {
                        CliError::Fatal(format!("bad boolean config value '{v}' for 'color'"))
                    })?;
                }
                "no-color" => o.color = Some(false),
                "start-after" => o.start_after = Some(cur.value("start-after", inline)?),
                other => return Err(cur.usage_error(&format!("unknown option `{other}'"))),
            },
        }
    }
    Ok(o)
}

/// The commits `words` name. A bad `--merged` value is fatal (128); the other options only
/// report it (129), as git's option callbacks do.
async fn commits(
    ctx: &Ctx,
    words: &[String],
    fatal: bool,
) -> Result<Vec<asc_core::id::ChangesetId>> {
    let ws = ctx.ws()?;
    let mut out = Vec::new();
    for w in words {
        let found = match resolve_target(ctx, ws, w).await? {
            Some(RefTarget::Changeset(c)) => Some(c),
            Some(RefTarget::Tag(t)) => Some(crate::cli::log::refs::tag_record(ws, t).await?.target),
            None => None,
        };
        out.push(found.ok_or_else(|| bad_name(w, fatal))?);
    }
    Ok(out)
}

fn bad_name(word: &str, fatal: bool) -> CliError {
    if fatal {
        CliError::Fatal(format!("malformed object name {word}"))
    } else {
        CliError::Raw(format!("error: malformed object name {word}"), 129)
    }
}

pub async fn run(ctx: &Ctx, args: Vec<OsString>) -> Result<ExitCode> {
    let mut o = parse(&args)?;
    if o.stdin {
        if !o.patterns.is_empty() {
            return Err(CliError::Fatal(
                "unknown arguments supplied with --stdin".into(),
            ));
        }
        let mut text = String::new();
        std::io::stdin().read_to_string(&mut text)?;
        o.patterns = text.lines().map(str::to_owned).collect();
    }
    if o.start_after.is_some() && (!o.sorts.is_empty() || o.stdin || !o.patterns.is_empty()) {
        return Err(CliError::Fatal(
            "cannot use --start-after with --sort, --stdin or patterns".into(),
        ));
    }
    let ws = ctx.ws()?;
    let text = o.format.as_deref().unwrap_or(DEFAULT_FORMAT);
    let format = Format::parse(text.as_bytes(), o.quote)
        .map_err(ref_filter::to_cli)
        .map_err(|e| match e {
            CliError::Usage(t) => CliError::Usage(format!("{t}\n{USAGE}")),
            other => other,
        })?;
    let sorts = o
        .sorts
        .iter()
        .map(|s| ref_filter::parse_sort_key(s).map_err(ref_filter::to_cli))
        .collect::<Result<Vec<SortKey>>>()?;
    let mut points_at = Vec::new();
    for w in &o.points_at {
        points_at.push(
            resolve_oid(ctx, ws, w)
                .await?
                .ok_or_else(|| CliError::Raw(format!("error: malformed object name '{w}'"), 129))?,
        );
    }
    let filters = Filters {
        points_at,
        merged: commits(ctx, &o.merged, true).await?,
        no_merged: commits(ctx, &o.no_merged, true).await?,
        contains: commits(ctx, &o.contains, false).await?,
        no_contains: commits(ctx, &o.no_contains, false).await?,
    };
    let mut out = Output::open(ctx, "for-each-ref", PagerWant::Never)?;
    let color = o.color.unwrap_or_else(|| out.color());
    let req = Request {
        query: ref_filter::Query {
            patterns: o.patterns,
            excludes: o.excludes,
            ignore_case: o.ignore_case,
            as_path: true,
            include_root: o.include_root,
            filters,
        },
        format: &format,
        sorts,
        color,
    };
    let (mut items, env) = ref_filter::list(ctx, ws, &req).await?;
    if let Some(marker) = &o.start_after {
        items.retain(|i| i.name.as_str() > marker.as_str());
    }
    let limit = if o.count == 0 { usize::MAX } else { o.count };
    let result = (|| -> Result<()> {
        for item in items.iter().take(limit) {
            let line = format.render(item, &env).map_err(ref_filter::to_cli)?;
            if o.omit_empty && line.is_empty() {
                continue;
            }
            out.write_all(&line)?;
            out.write_all(b"\n")?;
        }
        Ok(())
    })();
    match result {
        Err(CliError::Io(e)) if e.kind() == std::io::ErrorKind::BrokenPipe => {}
        other => other?,
    }
    out.finish()?;
    Ok(ExitCode::SUCCESS)
}
