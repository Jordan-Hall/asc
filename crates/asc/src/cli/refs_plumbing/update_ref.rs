//! `update-ref` (SPEC §11.2, §9.4.1): one ref change through `Ctx::update_local_refs`. Moving the
//! branch HEAD names keeps the staged state, as git keeps its index.

use std::ffi::OsString;
use std::process::ExitCode;

use asc_core::id::RefTarget;
use asc_core::path::RefName;
use asc_git::projection::Head;
use asc_store::refs::{Expect, RefError, RefTxn, RefUpdate, Refs as _};

use super::args::{Cursor, Word, object_oid_of, parse_oid, resolve_target};
use super::check_ref_format::{Rules, valid};
use crate::cli::common::{CliError, Ctx, Result, StagingAfterMove, WsCtx};
use crate::cli::restore::actor;

const USAGE: &str = super::usage::UPDATE_REF;

#[derive(Default)]
struct Opts {
    reason: Option<String>,
    delete: bool,
    no_deref: bool,
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
            Word::Long(name, _) => match name.as_str() {
                "no-deref" => o.no_deref = true,
                "create-reflog" => {}
                other => return Err(cur.usage_error(&format!("unknown option `{other}'"))),
            },
        }
    }
    Ok(o)
}

fn bad_value(word: &str) -> CliError {
    CliError::Fatal(format!("{word}: not a valid SHA1"))
}

/// What a ref holds, as git prints it in "is at … but expected …".
async fn describe(ws: &WsCtx, target: Option<RefTarget>) -> Result<String> {
    match target {
        Some(t) => Ok(object_oid_of(ws, t).await?.to_string()),
        None => Ok("0".repeat(40)),
    }
}

async fn current(ws: &WsCtx, name: &RefName) -> Result<Option<RefTarget>> {
    Ok(ws.db.refs().get(ws.repo.key, name).await?.map(|e| e.target))
}

/// git's `update_ref failed for ref '<name>': <why>`.
fn failed(name: &str, why: impl std::fmt::Display) -> CliError {
    CliError::Fatal(format!("update_ref failed for ref '{name}': {why}"))
}

/// The "cannot lock ref" message for a failed compare-and-swap. `want` spells the expected id
/// when the caller knows it better than the target (an id this repository has never seen).
async fn lock_message(
    ws: &WsCtx,
    name: &RefName,
    expect: Option<Expect>,
    want: Option<&str>,
) -> String {
    let text = String::from_utf8_lossy(name.as_bytes()).into_owned();
    let now = current(ws, name).await.ok().flatten();
    let why = match (expect, now) {
        (Some(Expect::Absent), Some(_)) => "reference already exists".to_owned(),
        (_, None) => format!("unable to resolve reference '{text}'"),
        (expect, Some(have)) if want.is_some() || matches!(expect, Some(Expect::Value(_))) => {
            let have = describe(ws, Some(have)).await.unwrap_or_default();
            let want = match (want, expect) {
                (Some(w), _) => w.to_owned(),
                (None, Some(Expect::Value(w))) => describe(ws, Some(w)).await.unwrap_or_default(),
                _ => String::new(),
            };
            format!("is at {have} but expected {want}")
        }
        _ => "the reference changed concurrently".to_owned(),
    };
    format!("cannot lock ref '{text}': {why}")
}

/// A failed compare-and-swap: a fatal `update_ref failed` for an update, a plain error (exit 1)
/// for a delete, as git reports them.
async fn lock_error(
    ws: &WsCtx,
    name: &RefName,
    expect: Option<Expect>,
    want: Option<&str>,
    delete: bool,
) -> CliError {
    let message = lock_message(ws, name, expect, want).await;
    if delete {
        CliError::Raw(format!("error: {message}"), 1)
    } else {
        failed(&String::from_utf8_lossy(name.as_bytes()), message)
    }
}

pub async fn run(ctx: &Ctx, args: Vec<OsString>) -> Result<ExitCode> {
    let o = parse(&args)?;
    let ws = ctx.ws()?;
    let (name_word, new_word, old_word) = match (o.delete, o.operands.as_slice()) {
        (true, [r]) => (r, None, None),
        (true, [r, old]) => (r, None, Some(old)),
        (false, [r, new]) => (r, Some(new), None),
        (false, [r, new, old]) => (r, Some(new), Some(old)),
        _ => return Err(CliError::Usage(USAGE.to_owned())),
    };
    let mut ref_word = name_word.clone();
    if ref_word == "HEAD"
        && !o.no_deref
        && let Head::Branch(b) = ws.projection.read_head()?
    {
        ref_word = format!("refs/heads/{b}");
    }
    if ref_word != "HEAD" && !(ref_word.starts_with("refs/") && valid(&ref_word, Rules::default()))
    {
        return Err(failed(
            &ref_word,
            format!("refusing to update ref with bad name '{ref_word}'"),
        ));
    }
    let name = if ref_word == "HEAD" {
        RefName::new_onelevel(b"HEAD".to_vec())
    } else {
        RefName::new(ref_word.clone().into_bytes())
    }
    .map_err(|_| {
        failed(
            &ref_word,
            format!("refusing to update ref with bad name '{ref_word}'"),
        )
    })?;

    let zero = |w: &str| parse_oid(w).is_some_and(|o| o.as_bytes().iter().all(|&b| b == 0));
    let new = match new_word {
        Some(w) if !zero(w) => {
            let Some(target) = resolve_target(ctx, ws, w).await? else {
                return Err(match parse_oid(w) {
                    Some(_) => failed(
                        &ref_word,
                        format!("trying to write ref '{ref_word}' with nonexistent object {w}"),
                    ),
                    None => bad_value(w),
                });
            };
            if ref_word.starts_with("refs/heads/") && matches!(target, RefTarget::Tag(_)) {
                let oid = object_oid_of(ws, target).await?;
                return Err(failed(
                    &ref_word,
                    format!("trying to write non-commit object {oid} to branch '{ref_word}'"),
                ));
            }
            Some(target)
        }
        _ => None,
    };
    let expect = match old_word {
        None => Expect::Any,
        Some(w) if w.is_empty() || zero(w) => Expect::Absent,
        Some(w) => match resolve_target(ctx, ws, w).await? {
            Some(target) => Expect::Value(target),
            // An id this repository never saw cannot be what the ref holds.
            None if parse_oid(w).is_some() => {
                return Err(lock_error(ws, &name, None, Some(w), o.delete).await);
            }
            None => return Err(bad_value(w)),
        },
    };
    if new.is_none()
        && expect == Expect::Any
        && current(ws, &name).await?.is_none()
        && ref_word != "HEAD"
    {
        return Ok(ExitCode::SUCCESS);
    }
    let txn = RefTxn {
        updates: vec![RefUpdate {
            name: name.clone(),
            expect,
            new,
            expires_at: None,
        }],
        actor: actor(),
        reason: String::new(),
        op_id: uuid::Uuid::now_v7(),
    };
    let message = o.reason.clone().unwrap_or_default();
    match ctx
        .update_local_refs_as(txn, &message, StagingAfterMove::Keep, o.no_deref)
        .await
    {
        Ok(()) => Ok(ExitCode::SUCCESS),
        Err(CliError::Refs(RefError::Conflict(_))) => {
            Err(lock_error(ws, &name, Some(expect), None, o.delete).await)
        }
        Err(other) => Err(other),
    }
}
