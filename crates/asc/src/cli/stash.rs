//! `asc stash` (SPEC §11.5): git's stash structure on `refs/stash`.
//!
//! `push|save [-u] [-k] [-q] [-m <msg>] [-- <pathspec>…]`, `pop|apply [--index] [-q] [<stash>]`,
//! `list` (the `log -g` options), `show` (the `diff` options), `drop`, `clear`, `branch`, and
//! the plumbing forms `create` and `store` that lint-staged and the hooks use. A stash is the
//! commit W with parents `[HEAD, I]` (index state) and optionally `U` (untracked files), the
//! stack is the reflog of `refs/stash` (`stash/stack.rs`), and every ref move goes through
//! `Ctx::update_local_refs`. `drop` and `clear` save a working-copy snapshot first (SPEC §20.3).
//! Stash commits are not recorded through `RewriteSink`.
//!
//! Not served, and refused with a message: `-p`/`--patch`, `-S`/`--staged`, `-a`/`--all`,
//! `--pathspec-from-file`, and `show --include-untracked`.

mod apply;
pub(crate) mod save;
pub(crate) mod stack;

use std::ffi::OsString;
use std::process::ExitCode;

use asc_core::git::ObjectKind;
use asc_core::id::{ChangesetId, GitOid};
use asc_state::state::git_map;

use crate::cli::common::{CliError, Ctx, Result};
use crate::cli::conflict::{self, fatal};
use crate::cli::log::PLACEHOLDERS;
use crate::cli::restore::{resolve_commit, tool};
use crate::ext;
use crate::native::NativeSpec;

#[derive(clap::Parser, Debug)]
#[command(name = "stash", disable_help_flag = true, disable_version_flag = true)]
pub struct Args {
    /// The subcommand and its words, as typed.
    #[arg(trailing_var_arg = true, allow_hyphen_values = true, num_args = 0..)]
    pub rest: Vec<OsString>,
}

pub const NATIVE: &[NativeSpec] = &[NativeSpec {
    cmd: "stash",
    flags: &[
        "-u",
        "--include-untracked",
        "--no-include-untracked",
        "-k",
        "--keep-index",
        "--no-keep-index",
        "-q",
        "--quiet",
        "-m",
        "--message",
        "--index",
        "--no-index",
        "-z",
        "-p",
        "--patch",
        "-s",
        "--no-patch",
        "--stat",
        "--numstat",
        "--shortstat",
        "--name-only",
        "--name-status",
        "--raw",
        "--summary",
        "--pretty",
        "--format",
        "--oneline",
        "--abbrev-commit",
        "--no-abbrev-commit",
        "--date",
        "--end-of-options",
        "-U",
        "--unified",
    ],
    placeholders: PLACEHOLDERS,
}];

fn text(arg: &OsString) -> String {
    arg.to_string_lossy().into_owned()
}

fn unsupported(what: &str) -> CliError {
    fatal(format!("asc: stash {what} is not supported"))
}

/// The options of `push` and `save`, and the words left over.
fn parse_push(tokens: &[OsString], save: bool) -> Result<(save::Opts, Vec<OsString>)> {
    let mut o = save::Opts::default();
    let mut rest = Vec::new();
    let mut it = tokens.iter().peekable();
    while let Some(tok) = it.next() {
        let Some(t) = tok.to_str() else {
            rest.push(tok.clone());
            continue;
        };
        if t == "--" {
            if save {
                rest.extend(it.by_ref().cloned());
            } else {
                o.paths.extend(it.by_ref().cloned());
            }
            break;
        }
        let missing =
            |name: &str| CliError::Usage(format!("error: switch `{name}' requires a value"));
        if let Some(long) = t.strip_prefix("--") {
            let (name, inline) = long
                .split_once('=')
                .map_or((long, None), |(n, v)| (n, Some(v.to_owned())));
            match name {
                "include-untracked" => o.untracked = true,
                "no-include-untracked" => o.untracked = false,
                "keep-index" => o.keep_index = true,
                "no-keep-index" => o.keep_index = false,
                "quiet" => o.quiet = true,
                "message" => {
                    let v = match inline {
                        Some(v) => v,
                        None => text(it.next().ok_or_else(|| missing("message"))?),
                    };
                    o.message = Some(v);
                }
                "patch" => return Err(unsupported("--patch")),
                "staged" => return Err(unsupported("--staged")),
                "all" => return Err(unsupported("--all")),
                "pathspec-from-file" | "pathspec-file-nul" => {
                    return Err(unsupported("--pathspec-from-file"));
                }
                _ => return Err(CliError::Usage(format!("error: unknown option `{name}'"))),
            }
            continue;
        }
        if t.len() > 1 && t.starts_with('-') {
            let mut chars = t[1..].chars();
            while let Some(c) = chars.next() {
                match c {
                    'u' => o.untracked = true,
                    'k' => o.keep_index = true,
                    'q' => o.quiet = true,
                    'm' => {
                        let attached: String = chars.by_ref().collect();
                        let v = if attached.is_empty() {
                            text(it.next().ok_or_else(|| missing("m"))?)
                        } else {
                            attached
                        };
                        o.message = Some(v);
                    }
                    'p' => return Err(unsupported("--patch")),
                    'S' => return Err(unsupported("--staged")),
                    'a' => return Err(unsupported("--all")),
                    other => {
                        return Err(CliError::Usage(format!("error: unknown switch `{other}'")));
                    }
                }
            }
            continue;
        }
        if save {
            rest.push(tok.clone());
        } else {
            o.paths.push(tok.clone());
        }
    }
    Ok((o, rest))
}

pub async fn run(ctx: &Ctx, a: Args) -> Result<ExitCode> {
    let rest = a.rest;
    let first = rest.first().and_then(|t| t.to_str()).map(str::to_owned);
    let args = rest.get(1..).unwrap_or_default();
    match first.as_deref() {
        None => save::push(ctx, &save::Opts::default()).await,
        Some("push") => {
            let (o, _) = parse_push(args, false)?;
            save::push(ctx, &o).await
        }
        Some("save") => {
            let (mut o, words) = parse_push(args, true)?;
            if !words.is_empty() {
                o.message = Some(words.iter().map(text).collect::<Vec<_>>().join(" "));
            }
            save::push(ctx, &o).await
        }
        Some("pop") => pop(ctx, args).await,
        Some("apply") => apply_cmd(ctx, args).await,
        Some("list") => list(ctx, args).await,
        Some("show") => show(ctx, args).await,
        Some("drop") => drop_cmd(ctx, args).await,
        Some("clear") => clear(ctx, args).await,
        Some("branch") => branch(ctx, args).await,
        Some("create") => {
            let message =
                (!args.is_empty()).then(|| args.iter().map(text).collect::<Vec<_>>().join(" "));
            save::create(ctx, message).await
        }
        Some("store") => store(ctx, args).await,
        Some(t) if t.starts_with('-') => {
            let (o, _) = parse_push(&rest, false)?;
            save::push(ctx, &o).await
        }
        Some(t) => Err(fatal(format!(
            "subcommand wasn't specified; 'push' can't be assumed due to unexpected token '{t}'"
        ))),
    }
}

// ---------------------------------------------------------------- shared parsing

/// `[-q] [--index] [<stash>]`.
fn parse_apply(args: &[OsString]) -> Result<(apply::Opts, Option<String>)> {
    let mut o = apply::Opts::default();
    let mut which = None;
    for arg in args {
        match arg.to_str() {
            Some("-q" | "--quiet") => o.quiet = true,
            Some("--index") => o.index = true,
            Some("--no-index") => o.index = false,
            Some(t) if t.starts_with('-') && t != "-" => {
                return Err(CliError::Usage(format!(
                    "error: unknown option `{}'",
                    t.trim_start_matches('-')
                )));
            }
            _ if which.is_none() => which = Some(text(arg)),
            _ => return Err(fatal("asc: too many arguments")),
        }
    }
    Ok((o, which))
}

/// A stash named by `<stash>` or, for `apply`, `show` and `branch`, by any revision that has a
/// stash's shape.
async fn pick_any(ctx: &Ctx, stack: &[stack::Entry], arg: Option<&str>) -> Result<stack::Picked> {
    let ws = ctx.ws()?;
    match stack::pick(ws, stack, arg).await {
        Err(CliError::Raw(msg, 1)) if msg.ends_with("is not a valid reference") => {
            let Some(arg) = arg else {
                return Err(CliError::Raw(msg, 1));
            };
            match resolve_rev(ctx, arg).await? {
                Some(c) if apply::is_stash_like(ws, c).await? => Ok(stack::Picked {
                    index: None,
                    commit: c,
                    text: arg.to_owned(),
                }),
                _ => Err(CliError::Raw(msg, 1)),
            }
        }
        other => other,
    }
}

/// A revision as `resolve_commit` reads it, plus a full git id through the git map (which
/// `resolve_commit` does not read).
async fn resolve_rev(ctx: &Ctx, text: &str) -> Result<Option<ChangesetId>> {
    let ws = ctx.ws()?;
    if text.len() == 40
        && let Ok(oid) = text.parse::<GitOid>()
        && let Some(asc) = git_map::git_to_asc(&ws.db, ws.repo.key, oid, ObjectKind::Commit).await?
    {
        return Ok(Some(ChangesetId::from_bytes(asc)));
    }
    resolve_commit(ctx, text).await
}

fn not_a_reference(tool: &str, arg: &str) -> CliError {
    let _ = tool;
    CliError::Raw(format!("error: '{arg}' is not a stash reference"), 1)
}

// ---------------------------------------------------------------- apply, pop, drop, branch

async fn apply_cmd(ctx: &Ctx, args: &[OsString]) -> Result<ExitCode> {
    let ws = ctx.ws()?;
    let (o, which) = parse_apply(args)?;
    let stack = stack::load(ws).await?;
    let picked = pick_any(ctx, &stack, which.as_deref()).await?;
    let ok = apply::apply(ctx, ws, picked.commit, o).await?;
    Ok(ExitCode::from(u8::from(!ok)))
}

async fn pop(ctx: &Ctx, args: &[OsString]) -> Result<ExitCode> {
    let ws = ctx.ws()?;
    let (o, which) = parse_apply(args)?;
    let stack = stack::load(ws).await?;
    let picked = stack::pick(ws, &stack, which.as_deref()).await?;
    let Some(index) = picked.index else {
        return Err(not_a_reference(
            tool(ctx),
            which.as_deref().unwrap_or_default(),
        ));
    };
    if !apply::apply(ctx, ws, picked.commit, o).await? {
        println!("The stash entry is kept in case you need it again.");
        return Ok(ExitCode::FAILURE);
    }
    stack::drop_at(ctx, ws, &stack, index).await?;
    announce_drop(ctx, &picked, o.quiet).await?;
    Ok(ExitCode::SUCCESS)
}

async fn announce_drop(ctx: &Ctx, picked: &stack::Picked, quiet: bool) -> Result<()> {
    if !quiet {
        let ws = ctx.ws()?;
        println!(
            "Dropped {} ({})",
            picked.text,
            conflict::git_id(ws, picked.commit).await?
        );
    }
    Ok(())
}

async fn drop_cmd(ctx: &Ctx, args: &[OsString]) -> Result<ExitCode> {
    let ws = ctx.ws()?;
    let (o, which) = parse_apply(args)?;
    let stack = stack::load(ws).await?;
    let picked = stack::pick(ws, &stack, which.as_deref()).await?;
    let Some(index) = picked.index else {
        return Err(not_a_reference(
            tool(ctx),
            which.as_deref().unwrap_or_default(),
        ));
    };
    ext::op_sink().snapshot_wc();
    stack::drop_at(ctx, ws, &stack, index).await?;
    announce_drop(ctx, &picked, o.quiet).await?;
    Ok(ExitCode::SUCCESS)
}

async fn clear(ctx: &Ctx, args: &[OsString]) -> Result<ExitCode> {
    let ws = ctx.ws()?;
    if let Some(extra) = args.first() {
        return Err(CliError::Usage(format!(
            "error: unknown option `{}'",
            text(extra).trim_start_matches('-')
        )));
    }
    if stack::load(ws).await?.is_empty() {
        return Ok(ExitCode::SUCCESS);
    }
    ext::op_sink().snapshot_wc();
    stack::move_ref(ctx, ws, None, stack::VIA_CLEAR, "clear").await?;
    Ok(ExitCode::SUCCESS)
}

async fn branch(ctx: &Ctx, args: &[OsString]) -> Result<ExitCode> {
    let ws = ctx.ws()?;
    let Some(name) = args.first() else {
        return Err(CliError::Raw("No branch name specified".into(), 1));
    };
    if args.len() > 2 {
        return Err(fatal("asc: too many arguments"));
    }
    let which = args.get(1).map(text);
    let stack = stack::load(ws).await?;
    let picked = pick_any(ctx, &stack, which.as_deref()).await?;
    let w = ws.engine.get_changeset(picked.commit).await?;
    let base = *w
        .parents
        .first()
        .ok_or_else(|| fatal("not a stash-like commit"))?;
    let base_git = conflict::git_id(ws, base).await?;
    let switch_args = super::switch::Args {
        rest: vec![
            OsString::from("-c"),
            name.clone(),
            OsString::from(base_git.to_string()),
        ],
        after: Vec::new(),
    };
    let code = super::switch::run(ctx, switch_args).await?;
    if code != ExitCode::SUCCESS {
        return Ok(code);
    }
    let ok = apply::apply(
        ctx,
        ws,
        picked.commit,
        apply::Opts {
            index: true,
            quiet: false,
        },
    )
    .await?;
    if !ok {
        return Ok(ExitCode::FAILURE);
    }
    if let Some(index) = picked.index {
        stack::drop_at(ctx, ws, &stack, index).await?;
        announce_drop(ctx, &picked, false).await?;
    }
    Ok(ExitCode::SUCCESS)
}

// ---------------------------------------------------------------- list, show

async fn list(ctx: &Ctx, args: &[OsString]) -> Result<ExitCode> {
    if stack::load(ctx.ws()?).await?.is_empty() {
        return Ok(ExitCode::SUCCESS);
    }
    let mut rest: Vec<OsString> = vec!["-g".into(), "--format=%gd: %gs".into()];
    rest.extend(args.iter().cloned());
    rest.push("refs/stash".into());
    super::log::run(ctx, super::log::Args { rest }).await
}

/// Diff options that choose an output form: the default `--stat` is dropped when one is present.
fn has_format(flags: &[OsString]) -> bool {
    flags.iter().filter_map(|f| f.to_str()).any(|f| {
        matches!(
            f,
            "-p" | "--patch"
                | "-s"
                | "--no-patch"
                | "--stat"
                | "--numstat"
                | "--shortstat"
                | "--name-only"
                | "--name-status"
                | "--raw"
                | "--summary"
        ) || f.starts_with("--stat=")
    })
}

async fn show(ctx: &Ctx, args: &[OsString]) -> Result<ExitCode> {
    let ws = ctx.ws()?;
    let mut flags = Vec::new();
    let mut which = None;
    for arg in args {
        match arg.to_str() {
            Some("-u" | "--include-untracked" | "--only-untracked") => {
                return Err(unsupported("show --include-untracked"));
            }
            Some(t) if t.starts_with('-') && t != "-" => flags.push(arg.clone()),
            _ if which.is_none() => which = Some(text(arg)),
            _ => return Err(fatal("asc: too many arguments")),
        }
    }
    let stack = stack::load(ws).await?;
    let picked = pick_any(ctx, &stack, which.as_deref()).await?;
    let w = ws.engine.get_changeset(picked.commit).await?;
    let base = *w
        .parents
        .first()
        .ok_or_else(|| fatal("not a stash-like commit"))?;
    if !has_format(&flags) {
        flags.push("--stat".into());
    }
    // The changeset ids: the engine's revision resolver takes them where it does not take a
    // full git id.
    flags.push(OsString::from(base.to_string()));
    flags.push(OsString::from(picked.commit.to_string()));
    super::diff::run(
        ctx,
        super::diff::opts::RawArgs {
            cmd: "diff".into(),
            rest: flags,
        },
    )
    .await
}

// ---------------------------------------------------------------- store

async fn store(ctx: &Ctx, args: &[OsString]) -> Result<ExitCode> {
    let ws = ctx.ws()?;
    let mut message: Option<String> = None;
    let mut commit: Option<String> = None;
    let mut extra = false;
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.to_str() {
            Some("-q" | "--quiet") => {}
            Some("-m" | "--message") => {
                message = Some(text(it.next().ok_or_else(|| {
                    CliError::Usage("error: switch `m' requires a value".into())
                })?));
            }
            Some(t) if t.starts_with("--message=") => {
                message = Some(t["--message=".len()..].to_owned())
            }
            Some(t) if t.starts_with('-') && t != "-" => {
                return Err(CliError::Usage(format!(
                    "error: unknown option `{}'",
                    t.trim_start_matches('-')
                )));
            }
            _ if commit.is_none() && !extra => commit = Some(text(arg)),
            _ => extra = true,
        }
    }
    let Some(arg) = commit.filter(|_| !extra) else {
        return Err(CliError::Raw(
            format!(
                "\"{} stash store\" requires one <commit> argument",
                tool(ctx)
            ),
            1,
        ));
    };
    let Some(c) = resolve_rev(ctx, &arg).await? else {
        return Err(fatal(format!("'{arg}' is not a stash-like commit")));
    };
    if !apply::is_stash_like(ws, c).await? {
        return Err(fatal(format!(
            "'{}' is not a stash-like commit",
            conflict::git_id(ws, c).await?
        )));
    }
    let message = message.unwrap_or_else(|| format!("Created via \"{} stash store\".", tool(ctx)));
    // Like a ref update to the value the ref already has: nothing is logged.
    if stack::load(ws).await?.first().map(|e| e.commit) != Some(c) {
        stack::push(ctx, ws, c, &message).await?;
    }
    Ok(ExitCode::SUCCESS)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(items: &[&str]) -> Vec<OsString> {
        items.iter().map(OsString::from).collect()
    }

    #[test]
    fn push_options_cluster_and_take_values() {
        let (o, _) = parse_push(&words(&["-uk", "-m", "msg", "--", "a", "b"]), false).unwrap();
        assert!(o.untracked && o.keep_index && !o.quiet);
        assert_eq!(o.message.as_deref(), Some("msg"));
        assert_eq!(o.paths, words(&["a", "b"]));
        let (o, _) = parse_push(&words(&["--message=x y", "-q", "p"]), false).unwrap();
        assert_eq!(
            (o.message.as_deref(), o.quiet, o.paths),
            (Some("x y"), true, words(&["p"]))
        );
        let (o, _) = parse_push(&words(&["-mattached"]), false).unwrap();
        assert_eq!(o.message.as_deref(), Some("attached"));
    }

    #[test]
    fn a_missing_message_is_a_usage_error() {
        assert!(matches!(
            parse_push(&words(&["-m"]), false),
            Err(CliError::Usage(_))
        ));
        assert!(matches!(
            parse_push(&words(&["--message"]), false),
            Err(CliError::Usage(_))
        ));
    }

    #[test]
    fn save_joins_the_words_and_unserved_forms_are_refused() {
        let (_, rest) = parse_push(&words(&["-u", "a", "b"]), true).unwrap();
        assert_eq!(rest, words(&["a", "b"]));
        for flag in ["-p", "--patch", "-S", "--staged", "-a", "--all"] {
            assert!(parse_push(&words(&[flag]), false).is_err(), "{flag}");
        }
        assert!(matches!(
            parse_push(&words(&["--bogus"]), false),
            Err(CliError::Usage(_))
        ));
    }

    #[test]
    fn show_keeps_an_explicit_output_form() {
        assert!(has_format(&words(&["-p"])));
        assert!(has_format(&words(&["--stat=80"])));
        assert!(!has_format(&words(&["--color"])));
    }

    #[test]
    fn apply_options_take_one_stash() {
        let (o, which) = parse_apply(&words(&["--index", "-q", "stash@{2}"])).unwrap();
        assert!(o.index && o.quiet);
        assert_eq!(which.as_deref(), Some("stash@{2}"));
        assert!(parse_apply(&words(&["a", "b"])).is_err());
        assert!(parse_apply(&words(&["--wat"])).is_err());
    }
}
