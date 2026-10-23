//! `show-ref` (SPEC §11.2): `<id> <refname>` for the refs that match, `--verify` for exact names,
//! `--exists` for a quiet existence test.

use std::ffi::OsString;
use std::io::Write;
use std::process::ExitCode;

use asc_core::id::RefTarget;
use asc_core::path::RefName;
use asc_store::refs::Refs as _;

use super::args::{Cursor, Word, object_oid_of};
use crate::cli::common::{CliError, Ctx, Result, WsCtx};
use crate::cli::output::Output;
use crate::cli::pager::PagerWant;

const USAGE: &str = super::usage::SHOW_REF;

#[derive(Default)]
struct Opts {
    head: bool,
    tags: bool,
    branches: bool,
    dereference: bool,
    hash_only: bool,
    abbrev: Option<usize>,
    quiet: bool,
    verify: bool,
    exists: bool,
    patterns: Vec<String>,
}

fn digits(cur: &Cursor, value: Option<String>, default: usize) -> Result<usize> {
    match value {
        None => Ok(default),
        Some(v) => v
            .parse::<usize>()
            .map(|n| n.clamp(4, 40))
            .map_err(|_| cur.usage_error("option `abbrev' expects a numerical value")),
    }
}

fn parse(args: &[OsString]) -> Result<Opts> {
    let mut cur = Cursor::new(args, USAGE);
    let mut o = Opts::default();
    while let Some(word) = cur.next() {
        match word {
            Word::End => o.patterns.extend(cur.rest()),
            Word::Operand(p) => o.patterns.push(p),
            Word::Short(flags) => {
                for c in flags.chars() {
                    match c {
                        'd' => o.dereference = true,
                        's' => {
                            o.hash_only = true;
                            o.abbrev = o.abbrev.or(Some(40));
                        }
                        'q' => o.quiet = true,
                        other => return Err(cur.usage_error(&format!("unknown switch `{other}'"))),
                    }
                }
            }
            Word::Long(name, inline) => match name.as_str() {
                "head" => o.head = true,
                "tags" => o.tags = true,
                "branches" | "heads" => o.branches = true,
                "dereference" => o.dereference = true,
                "hash" => {
                    o.hash_only = true;
                    o.abbrev = Some(digits(&cur, inline, 40)?);
                }
                "abbrev" => o.abbrev = Some(digits(&cur, inline, 7)?),
                "quiet" => o.quiet = true,
                "verify" => o.verify = true,
                "exists" => o.exists = true,
                other => return Err(cur.usage_error(&format!("unknown option `{other}'"))),
            },
        }
    }
    Ok(o)
}

/// Whether `refname` matches one of the patterns: equal, or ending in `/<pattern>`.
fn matches(patterns: &[String], refname: &str, verify: bool) -> bool {
    patterns.iter().any(|p| {
        refname == p || (!verify && refname.len() > p.len() && refname.ends_with(p.as_str()) && refname.as_bytes()[refname.len() - p.len() - 1] == b'/')
    })
}

async fn show(ws: &WsCtx, o: &Opts, out: &mut Output, name: &str, target: RefTarget) -> Result<()> {
    if o.quiet {
        return Ok(());
    }
    let hex = |oid: asc_core::id::GitOid| {
        let text = oid.to_string();
        text[..o.abbrev.unwrap_or(40).min(40)].to_owned()
    };
    let oid = object_oid_of(ws, target).await?;
    if o.hash_only {
        writeln!(out, "{}", hex(oid))?;
    } else {
        writeln!(out, "{} {name}", hex(oid))?;
    }
    if o.dereference
        && let RefTarget::Tag(t) = target {
            let peeled = crate::cli::log::refs::tag_record(ws, t).await?.target;
            let peeled = object_oid_of(ws, RefTarget::Changeset(peeled)).await?;
            writeln!(out, "{} {name}^{{}}", hex(peeled))?;
        }
    Ok(())
}

async fn read(ws: &WsCtx, name: &str) -> Result<Option<RefTarget>> {
    if name == "HEAD" {
        return Ok(ws.head_commit().await?.map(RefTarget::Changeset));
    }
    let Ok(rn) = RefName::new(name.as_bytes().to_vec()) else { return Ok(None) };
    Ok(ws.db.refs().get(ws.repo.key, &rn).await?.map(|e| e.target))
}

pub async fn run(ctx: &Ctx, args: Vec<OsString>) -> Result<ExitCode> {
    let o = parse(&args)?;
    let ws = ctx.ws()?;
    if o.exists {
        let [name] = o.patterns.as_slice() else {
            return Err(CliError::Fatal("--exists requires a reference".into()));
        };
        return Ok(if read(ws, name).await?.is_some() || name == "HEAD" {
            ExitCode::SUCCESS
        } else {
            eprintln!("error: reference does not exist");
            ExitCode::from(2)
        });
    }
    let mut out = Output::open(ctx, "show-ref", PagerWant::Never)?;
    let mut found = 0usize;
    let result = async {
        if o.verify {
            if o.patterns.is_empty() {
                return Err(CliError::Fatal("--verify requires a reference".into()));
            }
            for p in &o.patterns {
                let known = if p.starts_with("refs/") || p == "HEAD" { read(ws, p).await? } else { None };
                match known {
                    Some(target) => {
                        found += 1;
                        show(ws, &o, &mut out, p, target).await?;
                    }
                    None if o.quiet => return Ok(ExitCode::from(1)),
                    None => return Err(CliError::Fatal(format!("'{p}' - not a valid ref"))),
                }
            }
            return Ok(ExitCode::SUCCESS);
        }
        let filtered = o.tags || o.branches;
        if o.head
            && let Some(target) = read(ws, "HEAD").await? {
                found += 1;
                show(ws, &o, &mut out, "HEAD", target).await?;
            }
        for entry in ws.db.refs().list(ws.repo.key, "refs/").await? {
            let name = String::from_utf8_lossy(entry.name.as_bytes()).into_owned();
            let kind_ok = (!filtered)
                || (o.tags && name.starts_with("refs/tags/"))
                || (o.branches && name.starts_with("refs/heads/"));
            if !kind_ok || (!o.patterns.is_empty() && !matches(&o.patterns, &name, false)) {
                continue;
            }
            found += 1;
            show(ws, &o, &mut out, &name, entry.target).await?;
        }
        Ok(if found > 0 { ExitCode::SUCCESS } else { ExitCode::from(1) })
    }
    .await;
    match result {
        Err(CliError::Io(e)) if e.kind() == std::io::ErrorKind::BrokenPipe => Ok(ExitCode::SUCCESS),
        other => {
            out.finish()?;
            other
        }
    }
}
