//! `reflog` (SPEC §11.2): the `state.db` ref log of a ref, newest first, in git's
//! `<abbrev> <ref>@{<n>}: <message>` form; `reflog exists <ref>`.
//!
//! `HEAD` has no log of its own: it shows the log of the branch it points at. `--remote` (the
//! server's log) and `expire`/`delete` are not served.

use std::ffi::OsString;
use std::io::Write;
use std::process::ExitCode;

use asc_core::id::RefTarget;
use asc_core::path::RefName;
use asc_store::refs::Refs;

use super::common::{CliError, Ctx, Result, WsCtx};
use super::output::Output;
use super::pager::PagerWant;
use super::revparse::ref_of;
use crate::msg::Msg;
use crate::native::NativeSpec;

#[derive(clap::Parser, Debug)]
#[command(name = "reflog", disable_help_flag = true, disable_version_flag = true)]
pub struct Args {
    /// `[show|exists]`, flags and the ref.
    #[arg(trailing_var_arg = true, allow_hyphen_values = true, num_args = 0..)]
    pub rest: Vec<OsString>,
}

const FLAGS: &[&str] = &["-n", "--max-count", "-<n>"];

pub const NATIVE: &[NativeSpec] = &[NativeSpec { cmd: "reflog", flags: FLAGS, placeholders: &[] }];

/// Entries read when no `-n` is given: the log is read whole, so it is bounded.
const DEFAULT_LIMIT: u32 = 100_000;

fn fatal(e: impl std::fmt::Display) -> CliError {
    CliError::Fatal(e.to_string())
}

#[derive(Default)]
struct Opts {
    exists: bool,
    max_count: Option<u32>,
    target: Option<String>,
}

fn count(flag: &str, value: &str) -> Result<u32> {
    value
        .parse()
        .map_err(|_| CliError::Raw(format!("fatal: '{value}': not an integer (for {flag})"), 128))
}

impl Opts {
    fn parse(args: &[OsString]) -> Result<Self> {
        let mut o = Self::default();
        let mut words = Vec::new();
        let mut it = args.iter().map(|a| a.to_string_lossy().into_owned());
        while let Some(arg) = it.next() {
            if let Some(v) = arg.strip_prefix("--max-count=") {
                o.max_count = Some(count("--max-count", v)?);
            } else if arg == "-n" || arg == "--max-count" {
                o.max_count = Some(count(&arg, &it.next().unwrap_or_default())?);
            } else if let Some(v) = arg.strip_prefix("-n") {
                o.max_count = Some(count("-n", v)?);
            } else if arg.len() > 1 && arg[1..].bytes().all(|b| b.is_ascii_digit()) && arg.starts_with('-') {
                o.max_count = Some(count("-<n>", &arg[1..])?);
            } else {
                words.push(arg);
            }
        }
        let mut words = words.into_iter().peekable();
        match words.peek().map(String::as_str) {
            Some("exists") => {
                o.exists = true;
                words.next();
            }
            Some("show") => {
                words.next();
            }
            Some(sub @ ("expire" | "delete" | "drop" | "write")) => {
                return Err(CliError::Msg(Msg::AscOptionUnsupported { flag: sub.to_owned() }));
            }
            _ => {}
        }
        o.target = words.next();
        Ok(o)
    }
}

/// The log of `typed` (`HEAD`, a branch, or a full ref): the ref's full name.
async fn log_name(ws: &WsCtx, typed: &str) -> Result<RefName> {
    let full = match ref_of(ws, typed).await? {
        Some(full) if full != "HEAD" => full,
        Some(_) => return RefName::new_onelevel(b"HEAD".to_vec()).map_err(fatal),
        None if typed.starts_with("refs/") => typed.to_owned(),
        None => format!("refs/heads/{typed}"),
    };
    RefName::new(full.into_bytes()).map_err(fatal)
}

pub async fn run(ctx: &Ctx, a: Args) -> Result<ExitCode> {
    let o = Opts::parse(&a.rest)?;
    let ws = ctx.ws()?;
    let typed = o.target.as_deref().unwrap_or("HEAD");
    let name = log_name(ws, typed).await?;
    if o.exists {
        let found = !ws.db.refs().log(ws.repo.key, &name, 1).await?.is_empty();
        return Ok(ExitCode::from(u8::from(!found)));
    }
    let abbrev = ctx
        .config
        .get("core.abbrev")
        .and_then(|v| v.parse::<usize>().ok())
        .map_or(7, |n| n.clamp(4, 40));
    let limit = o.max_count.unwrap_or(DEFAULT_LIMIT).min(DEFAULT_LIMIT);
    let entries = ws.db.refs().log(ws.repo.key, &name, limit).await?;
    let mut out = Output::open(ctx, "reflog", PagerWant::Auto)?;
    let mut result = Ok(());
    for (i, entry) in entries.iter().enumerate() {
        let Some(RefTarget::Changeset(id)) = entry.new else { continue };
        let git = match ws.engine.git_commit_of(id).await {
            Ok(g) => g.to_string(),
            Err(e) => {
                result = Err(fatal(e));
                break;
            }
        };
        if let Err(e) = writeln!(out, "{} {typed}@{{{i}}}: {}", &git[..abbrev], entry.reason) {
            if e.kind() != std::io::ErrorKind::BrokenPipe {
                result = Err(CliError::Io(e));
            }
            break;
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

    fn parse(items: &[&str]) -> Opts {
        let v: Vec<OsString> = items.iter().map(OsString::from).collect();
        Opts::parse(&v).unwrap()
    }

    #[test]
    fn subcommand_count_and_ref() {
        let o = parse(&["show", "-n", "3", "master"]);
        assert!(!o.exists);
        assert_eq!((o.max_count, o.target.as_deref()), (Some(3), Some("master")));
        let o = parse(&["exists", "refs/heads/x"]);
        assert!(o.exists);
        assert_eq!(parse(&["-5"]).max_count, Some(5));
        assert_eq!(parse(&["--max-count=2"]).max_count, Some(2));
        assert_eq!(parse(&[]).target, None);
    }

    #[test]
    fn expire_is_not_served() {
        let v = [OsString::from("expire")];
        assert!(matches!(Opts::parse(&v), Err(CliError::Msg(Msg::AscOptionUnsupported { .. }))));
    }
}
