//! `symbolic-ref` (SPEC §11.2): `HEAD` is the one symbolic ref an asc repository has. Reading it
//! prints the branch it names; pointing it elsewhere moves HEAD and keeps the staged state as git
//! does (SPEC §9.5).

use std::ffi::OsString;
use std::process::ExitCode;

use asc_core::path::RefName;
use asc_git::projection::Head;
use asc_state::state::workspaces;
use asc_store::refs::{RefTxn, Refs as _};

use super::args::{Cursor, Word};
use super::check_ref_format::{Rules, valid};
use crate::cli::common::{CliError, Ctx, Result, StagingAfterMove, WsCtx};
use crate::cli::restore::actor;
use crate::ext;

const USAGE: &str = super::usage::SYMBOLIC_REF;

#[derive(Default)]
struct Opts {
    quiet: bool,
    short: bool,
    delete: bool,
    reason: Option<String>,
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
                let mut chars = flags.chars();
                while let Some(c) = chars.next() {
                    match c {
                        'q' => o.quiet = true,
                        'd' => o.delete = true,
                        'm' => {
                            let rest: String = chars.by_ref().collect();
                            o.reason = Some(if rest.is_empty() {
                                cur.value("m", None)?
                            } else {
                                rest
                            });
                        }
                        other => return Err(cur.usage_error(&format!("unknown switch `{other}'"))),
                    }
                }
            }
            Word::Long(name, inline) => match name.as_str() {
                "quiet" => o.quiet = true,
                "short" => o.short = true,
                "delete" => o.delete = true,
                "no-recurse" | "recurse" => {}
                "message" => o.reason = Some(cur.value("message", inline)?),
                other => return Err(cur.usage_error(&format!("unknown option `{other}'"))),
            },
        }
    }
    Ok(o)
}

async fn exists(ws: &WsCtx, full: &str) -> Result<bool> {
    let Ok(name) = RefName::new(full.as_bytes().to_vec()) else {
        return Ok(false);
    };
    Ok(ws.db.refs().get(ws.repo.key, &name).await?.is_some())
}

/// `shorten_unambiguous_ref` without the strict mode: only the lookup rules before the one that
/// matched can make a short name ambiguous.
async fn shorten(ws: &WsCtx, full: &str) -> Result<String> {
    const RULES: [(&str, &str); 6] = [
        ("", ""),
        ("refs/", ""),
        ("refs/tags/", ""),
        ("refs/heads/", ""),
        ("refs/remotes/", ""),
        ("refs/remotes/", "/HEAD"),
    ];
    for i in (1..RULES.len()).rev() {
        let (pre, post) = RULES[i];
        let Some(short) = full
            .strip_prefix(pre)
            .and_then(|r| r.strip_suffix(post))
            .filter(|s| !s.is_empty())
        else {
            continue;
        };
        let mut ambiguous = false;
        for (p, q) in &RULES[..i] {
            ambiguous = ambiguous || exists(ws, &format!("{p}{short}{q}")).await?;
        }
        if !ambiguous {
            return Ok(short.to_owned());
        }
    }
    Ok(full.to_owned())
}

async fn read(ws: &WsCtx, o: &Opts, name: &str) -> Result<ExitCode> {
    if name == "HEAD" {
        return match ws.projection.read_head()? {
            Head::Branch(b) => {
                let full = format!("refs/heads/{b}");
                println!(
                    "{}",
                    if o.short {
                        shorten(ws, &full).await?
                    } else {
                        full
                    }
                );
                Ok(ExitCode::SUCCESS)
            }
            Head::Detached(_) if o.quiet => Ok(ExitCode::from(1)),
            Head::Detached(_) => Err(CliError::Fatal(format!("ref {name} is not a symbolic ref"))),
        };
    }
    // git reads a well-formed name that holds no symbolic ref the same, whether or not it exists.
    if exists(ws, name).await?
        || valid(
            name,
            Rules {
                allow_onelevel: true,
                ..Rules::default()
            },
        )
    {
        if o.quiet {
            return Ok(ExitCode::from(1));
        }
        return Err(CliError::Fatal(format!("ref {name} is not a symbolic ref")));
    }
    Err(CliError::Fatal(format!("No such ref: {name}")))
}

/// Points HEAD at `target` (a branch), keeping the staged state.
async fn point(ctx: &Ctx, ws: &WsCtx, o: &Opts, target: &str) -> Result<ExitCode> {
    if !target.starts_with("refs/") {
        return Err(CliError::Fatal(
            "Refusing to point HEAD outside of refs/".into(),
        ));
    }
    if !valid(target, Rules::default()) {
        return Err(CliError::Fatal(format!(
            "Refusing to set 'HEAD' to invalid ref '{target}'"
        )));
    }
    let Some(branch) = target.strip_prefix("refs/heads/") else {
        return Err(CliError::Fatal(
            "asc: HEAD can only name a branch under refs/heads/".into(),
        ));
    };
    let id = ws.workspace.ws.id;
    let before = ws.head_commit().await?;
    let base = workspaces::get(&ws.db, id)
        .await?
        .ok_or_else(|| CliError::Fatal("asc: this workspace is not registered in state.db".into()))?
        .ws
        .base_cs;
    ws.projection.write_head(&Head::Branch(branch.to_owned()))?;
    // The rows stay relative to `base_cs`; the chokepoint rebases them onto the new branch.
    workspaces::update_head_base(&ws.db, id, target, base).await?;
    let txn = RefTxn {
        updates: Vec::new(),
        actor: actor(),
        reason: String::new(),
        op_id: uuid::Uuid::now_v7(),
    };
    let message = o.reason.clone().unwrap_or_default();
    ctx.update_local_refs(txn, &message, StagingAfterMove::Keep)
        .await?;
    let after = ws.head_commit().await?;
    if before != after {
        ext::index_sink()
            .on_head_moved(id, before, after)
            .await
            .map_err(|e| CliError::Fatal(e.to_string()))?;
    }
    Ok(ExitCode::SUCCESS)
}

pub async fn run(ctx: &Ctx, args: Vec<OsString>) -> Result<ExitCode> {
    let o = parse(&args)?;
    let ws = ctx.ws()?;
    match (o.delete, o.operands.as_slice()) {
        (true, [_]) => Err(CliError::Fatal(
            "asc: deleting a symbolic ref is not supported".into(),
        )),
        (false, [name]) => read(ws, &o, name).await,
        (false, [name, target]) => {
            if name != "HEAD" {
                return Err(CliError::Fatal(
                    "asc: only HEAD can be a symbolic ref".into(),
                ));
            }
            point(ctx, ws, &o, target).await
        }
        _ => Err(CliError::Usage(USAGE.to_owned())),
    }
}
