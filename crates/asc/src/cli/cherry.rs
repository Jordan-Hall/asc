//! `cherry` (SPEC §11.2): the commits of `<head>` that `<upstream>` lacks, oldest first, marked
//! `-` when an upstream commit made the same change and `+` when none did. Two commits make
//! the same change when their patches are the same apart from line numbers and whitespace
//! (git's patch id); merge commits are not listed or compared.

use std::collections::HashSet;
use std::ffi::OsString;
use std::io::Write as _;
use std::process::ExitCode;

use asc_core::id::{ChangesetId, TreeId};
use sha2::{Digest, Sha256};

use super::commit::message::subject;
use super::common::{CliEngine, CliError, Ctx, Result, WsCtx};
use super::conflict;
use super::log::opts::Opts;
use super::log::revs;
use super::log::walk::{Hist, Walker};
use super::range_diff::file_sections;
use super::refs_plumbing::args::{Cursor, Word};
use super::revparse::commit_of;
use crate::native::NativeSpec;

/// Most commits compared on either side.
const MAX_COMMITS: usize = 200_000;

const USAGE: &str = "usage: git cherry [-v] [<upstream> [<head> [<limit>]]]

    --[no-]abbrev[=<n>]   use <n> digits to display object names
    -v, --[no-]verbose    be verbose
";

#[derive(clap::Parser, Debug)]
#[command(name = "cherry", disable_help_flag = true, disable_version_flag = true)]
pub struct Args {
    #[arg(trailing_var_arg = true, allow_hyphen_values = true, num_args = 0..)]
    pub rest: Vec<OsString>,
}

pub const NATIVE: &[NativeSpec] = &[NativeSpec {
    cmd: "cherry",
    flags: &["-v", "--verbose", "--no-verbose", "--abbrev", "--no-abbrev"],
    placeholders: &[],
}];

fn fatal(text: impl Into<String>) -> CliError {
    CliError::Fatal(text.into())
}

#[derive(Default)]
struct Opt {
    verbose: bool,
    abbrev: Option<usize>,
    words: Vec<String>,
}

fn parse(args: &[OsString]) -> Result<Opt> {
    let mut cur = Cursor::new(args, USAGE);
    let mut o = Opt::default();
    while let Some(word) = cur.next() {
        match word {
            Word::End => o.words.extend(cur.rest()),
            Word::Operand(p) => o.words.push(p),
            Word::Short(flags) => {
                for c in flags.chars() {
                    if c == 'v' {
                        o.verbose = true;
                    } else {
                        return Err(cur.usage_error(&format!("unknown switch `{c}'")));
                    }
                }
            }
            Word::Long(name, inline) => match name.as_str() {
                "verbose" => o.verbose = true,
                "no-verbose" => o.verbose = false,
                "abbrev" => {
                    o.abbrev = Some(match inline {
                        None => 7,
                        Some(v) => v.parse::<usize>().map_err(|_| {
                            CliError::Raw(
                                "error: option `abbrev' expects a numerical value".into(),
                                129,
                            )
                        })?,
                    });
                }
                "no-abbrev" => o.abbrev = Some(40),
                other => return Err(cur.usage_error(&format!("unknown option `{other}'"))),
            },
        }
    }
    Ok(o)
}

/// A digest of the change a commit makes: the files, and the hunk lines with whitespace and
/// line numbers left out.
async fn patch_id(ws: &WsCtx, id: ChangesetId) -> Result<[u8; 32]> {
    let cs = ws.engine.get_changeset(id).await?;
    let old: TreeId = match cs.parents.first() {
        Some(&p) => ws.engine.get_changeset(p).await?.root,
        None => CliEngine::empty_tree_id(),
    };
    let sections = file_sections(ws, old, cs.root, 40, false).await?;
    let mut h = Sha256::new();
    for s in &sections {
        h.update(b"file ");
        h.update(s.heading.as_bytes());
        h.update(b"\n");
        for line in s.body.split(|&b| b == b'\n') {
            if line.starts_with(b"@@") {
                continue;
            }
            let stripped: Vec<u8> = line
                .iter()
                .copied()
                .filter(|b| !b.is_ascii_whitespace())
                .collect();
            if !stripped.is_empty() {
                h.update(&stripped);
                h.update(b"\n");
            }
        }
    }
    Ok(h.finalize().into())
}

async fn walk(ctx: &Ctx, ws: &WsCtx, words: &[&str]) -> Result<Vec<super::log::walk::Entry>> {
    let mut args: Vec<OsString> = vec!["--no-merges".into()];
    args.extend(words.iter().map(OsString::from));
    let o = Opts::parse(&args)?;
    let hist = Hist::select(ws);
    let sel = revs::select(ctx, hist.api(ws), &o).await?;
    let mut walker = Walker::new(ws, &hist, &sel, &o).await?;
    let mut out = Vec::new();
    while let Some(e) = walker.next().await? {
        if out.len() >= MAX_COMMITS {
            return Err(fatal("asc: cherry: too many commits"));
        }
        out.push(e);
    }
    Ok(out)
}

pub async fn run(ctx: &Ctx, a: Args) -> Result<ExitCode> {
    let o = parse(&a.rest)?;
    let ws = ctx.ws()?;
    // git reads three arguments at most; with more it falls back to the defaults.
    let o = if o.words.len() > 3 {
        Opt {
            words: Vec::new(),
            ..o
        }
    } else {
        o
    };
    let upstream = match o.words.first() {
        Some(u) => u.clone(),
        None => {
            if commit_of(ctx, "@{upstream}").await.ok().flatten().is_none() {
                return Err(CliError::Usage(format!(
                    "Could not find a tracked remote branch, please specify <upstream> manually.\n{USAGE}"
                )));
            }
            "@{upstream}".to_owned()
        }
    };
    let head = o.words.get(1).cloned().unwrap_or_else(|| "HEAD".to_owned());
    let limit = o.words.get(2).cloned();
    // Each end as a full id, so that the ranges below name them exactly.
    let mut ends: Vec<String> = Vec::new();
    for word in std::iter::once(&head)
        .chain(std::iter::once(&upstream))
        .chain(limit.iter())
    {
        let Some(id) = commit_of(ctx, word).await? else {
            return Err(fatal(format!("unknown commit {word}")));
        };
        ends.push(conflict::git_id(ws, id).await?.to_string());
    }
    let (head, upstream) = (ends[0].clone(), ends[1].clone());
    let limit = ends.get(2).cloned();

    // The patch ids of what upstream has and head lacks.
    let theirs = walk(ctx, ws, &[&format!("{head}..{upstream}")]).await?;
    let mut seen: HashSet<[u8; 32]> = HashSet::with_capacity(theirs.len());
    for e in &theirs {
        seen.insert(patch_id(ws, e.id).await?);
    }
    let mut words = vec![format!("{upstream}..{head}")];
    if let Some(l) = &limit {
        words.push(format!("^{l}"));
    }
    let refs: Vec<&str> = words.iter().map(String::as_str).collect();
    let mut mine = walk(ctx, ws, &refs).await?;
    // Oldest first.
    mine.reverse();

    let mut out = String::new();
    for e in &mine {
        let sign = if seen.contains(&patch_id(ws, e.id).await?) {
            '-'
        } else {
            '+'
        };
        let full = conflict::git_id(ws, e.id).await?.to_string();
        let shown = match o.abbrev {
            Some(n) => &full[..n.clamp(4, 40)],
            None => &full[..],
        };
        out.push(sign);
        out.push(' ');
        out.push_str(shown);
        if o.verbose {
            out.push(' ');
            out.push_str(&subject(&e.cs.message));
        }
        out.push('\n');
    }
    let mut stdout = std::io::stdout().lock();
    match stdout
        .write_all(out.as_bytes())
        .and_then(|()| stdout.flush())
    {
        Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => {}
        other => other?,
    }
    Ok(ExitCode::SUCCESS)
}
