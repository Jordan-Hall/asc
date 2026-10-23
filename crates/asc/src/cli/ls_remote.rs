//! `ls-remote` for a configured remote (SPEC §11.2, §11.4): lists the refs the remote holds as
//! `<id>TAB<ref>`, sorted by name, with `<id>TAB<tag>^{}` after an annotated tag. A URL or
//! path is routed to real git before it reaches here. Ids are git ids of objects this machine
//! has; a ref whose object has not been fetched is reported, not guessed.

use std::ffi::OsString;
use std::io::Write as _;
use std::process::ExitCode;

use asc_core::id::RefTarget;
use asc_git::projection::Head;

use super::common::{CliError, Ctx, Result};
use super::log::refs::tag_record;
use super::refs_plumbing::args::{Cursor, Word, object_oid_of};
use crate::native::NativeSpec;
use crate::remote::{self, Intent};

const USAGE: &str = "usage: git ls-remote [--branches] [--tags] [--refs] [--upload-pack=<exec>]
                     [-q | --quiet] [--exit-code] [--get-url] [--sort=<key>]
                     [--symref] [<repository> [<patterns>...]]

    -q, --[no-]quiet      do not print remote URL
    --[no-]upload-pack <exec>
                          path of git-upload-pack on the remote host
    -t, --[no-]tags       limit to tags
    -b, --[no-]branches   limit to branches
    -h, --[no-]heads      deprecated synonym for --branches
    --[no-]refs           do not show peeled tags
    --[no-]get-url        take url.<base>.insteadOf into account
    --[no-]sort <key>     field name to sort on
    --[no-]exit-code      exit with exit code 2 if no matching refs are found
    --[no-]symref         show underlying ref in addition to the object pointed by it
    -o, --[no-]server-option <server-specific>
                          option to transmit
";

#[derive(clap::Parser, Debug)]
#[command(
    name = "ls-remote",
    disable_help_flag = true,
    disable_version_flag = true
)]
pub struct Args {
    #[arg(trailing_var_arg = true, allow_hyphen_values = true, num_args = 0..)]
    pub rest: Vec<OsString>,
}

pub const NATIVE: &[NativeSpec] = &[NativeSpec {
    cmd: "ls-remote",
    flags: &[
        "-q",
        "--quiet",
        "-t",
        "--tags",
        "-b",
        "--branches",
        "--heads",
        "--refs",
        "--get-url",
        "--exit-code",
    ],
    placeholders: &[],
}];

fn fatal(text: impl Into<String>) -> CliError {
    CliError::Fatal(text.into())
}

#[derive(Default)]
struct Opts {
    quiet: bool,
    tags: bool,
    heads: bool,
    refs_only: bool,
    get_url: bool,
    exit_code: bool,
    operands: Vec<String>,
}

fn parse(args: &[OsString]) -> Result<Opts> {
    let mut cur = Cursor::new(args, USAGE);
    let mut o = Opts::default();
    while let Some(word) = cur.next() {
        match word {
            Word::End => o.operands.extend(cur.rest()),
            Word::Operand(p) => o.operands.push(p),
            Word::Short(flags) => {
                for c in flags.chars() {
                    match c {
                        'q' => o.quiet = true,
                        't' => o.tags = true,
                        'b' | 'h' => o.heads = true,
                        other => {
                            return Err(cur.usage_error(&format!("unknown switch `{other}'")));
                        }
                    }
                }
            }
            Word::Long(name, _) => match name.as_str() {
                "quiet" => o.quiet = true,
                "tags" => o.tags = true,
                "branches" | "heads" => o.heads = true,
                "refs" => o.refs_only = true,
                "get-url" => o.get_url = true,
                "exit-code" => o.exit_code = true,
                other => return Err(cur.usage_error(&format!("unknown option `{other}'"))),
            },
        }
    }
    Ok(o)
}

/// A wildmatch of `pat` against `text` where `*` also crosses `/`.
fn glob(pat: &[u8], text: &[u8]) -> bool {
    match pat.split_first() {
        None => text.is_empty(),
        Some((b'*', rest)) => (0..=text.len()).any(|i| glob(rest, &text[i..])),
        Some((b'?', rest)) => text.split_first().is_some_and(|(_, t)| glob(rest, t)),
        Some((c, rest)) => text
            .split_first()
            .is_some_and(|(t, tail)| t == c && glob(rest, tail)),
    }
}

/// Whether a ref name matches one of the patterns the way `ls-remote` does: the pattern is
/// matched against the whole name or any trailing run of whole path components.
fn matches(patterns: &[String], name: &str) -> bool {
    patterns.is_empty()
        || patterns.iter().any(|p| {
            glob(p.as_bytes(), name.as_bytes())
                || glob(format!("*/{p}").as_bytes(), name.as_bytes())
        })
}

pub async fn run(ctx: &Ctx, a: Args) -> Result<ExitCode> {
    let o = parse(&a.rest)?;
    let ws = ctx.ws()?;
    let (repo, patterns): (Option<&String>, &[String]) = match o.operands.split_first() {
        Some((r, rest)) => (Some(r), rest),
        None => (None, &[]),
    };
    let name = match repo {
        Some(r) => r.clone(),
        None => {
            let branch = match ws.projection.read_head()? {
                Head::Branch(b) => Some(b),
                _ => None,
            };
            branch
                .and_then(|b| ctx.config.get(&format!("branch.{b}.remote")))
                .unwrap_or_else(|| "origin".to_owned())
        }
    };
    if o.get_url {
        let url = ctx
            .config
            .get(&format!("remote.{name}.url"))
            .unwrap_or_else(|| name.clone());
        println!("{url}");
        return Ok(ExitCode::SUCCESS);
    }
    let opened = remote::open(ctx, ws, &name, Intent::Read).await?;
    if repo.is_none() && !o.quiet {
        eprintln!("From {}", crate::remote::url::display_from(&opened.url));
    }
    let entries = opened
        .remote
        .list_refs()
        .await
        .map_err(|e| fatal(format!("asc: {e}")))?;
    let mut rows: Vec<(String, RefTarget)> = entries
        .into_iter()
        .map(|e| {
            (
                String::from_utf8_lossy(e.name.as_bytes()).into_owned(),
                e.target,
            )
        })
        .collect();
    rows.sort_by(|a, b| a.0.cmp(&b.0));

    let limit = o.tags || o.heads;
    let mut out = String::new();
    let mut shown = 0usize;
    for (refname, target) in rows {
        if limit {
            let ok = (o.heads && refname.starts_with("refs/heads/"))
                || (o.tags && refname.starts_with("refs/tags/"));
            if !ok {
                continue;
            }
        }
        if !matches(patterns, &refname) {
            continue;
        }
        let oid = object_oid_of(ws, target).await.map_err(|_| {
            fatal(format!(
                "asc: {refname} points at an object this machine has not fetched; run asc fetch first"
            ))
        })?;
        out.push_str(&format!("{oid}\t{refname}\n"));
        shown += 1;
        if let RefTarget::Tag(id) = target
            && !o.refs_only
        {
            let rec = tag_record(ws, id).await?;
            let peeled = ws
                .engine
                .git_commit_of(rec.target)
                .await
                .map_err(|e| fatal(e.to_string()))?;
            out.push_str(&format!("{peeled}\t{refname}^{{}}\n"));
        }
    }
    let mut stdout = std::io::stdout().lock();
    stdout.write_all(out.as_bytes())?;
    stdout.flush()?;
    Ok(if shown == 0 && o.exit_code {
        ExitCode::from(2)
    } else {
        ExitCode::SUCCESS
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn patterns_match_trailing_components() {
        assert!(matches(&[], "refs/heads/master"));
        assert!(matches(&["master".into()], "refs/heads/master"));
        assert!(!matches(&["aster".into()], "refs/heads/master"));
        assert!(matches(&["refs/tags/v*".into()], "refs/tags/v1.0"));
        assert!(matches(&["v*".into()], "refs/tags/v1"));
    }
}
