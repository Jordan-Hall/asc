//! `asc gc` (SPEC §3.5, §3.6): evicts the local cache down to 80% of `asc.cache.maxSize`.
//!
//! `store/` holds the objects this machine wrote and nothing else has them, so eviction never
//! touches it. The one removal is the hidden-draft expiry of SPEC §21.2.4
//! ([`crate::branchless::local_gc`]): objects reachable only from hidden drafts older than
//! `asc.draft.hiddenDays`. With `--store <url>` the command collects a bucket-mode repository instead
//! ([`super::gc_store`]). `--dry-run` reports what an eviction would free and changes nothing.
//! Git's `--auto` and `--quiet` are accepted; git's other gc flags (`--prune`, `--aggressive`,
//! ...) have nothing to do in a layout without loose objects or packs and are refused.

use std::ffi::OsString;
use std::path::PathBuf;
use std::process::ExitCode;

use asc_state::AscHome;
use asc_store::cachedir::CacheDir;

use super::common::{CliError, Ctx, Result};
use super::doctor::{dir_size, human};
use super::gc_store::{self, StoreGcArgs};
use crate::branchless::{local_gc, mode::Settings};
use crate::daemon::scheduler;
use crate::native::NativeSpec;

const USAGE: &str = "usage: asc gc [--dry-run] [--auto] [-q | --quiet]
   or: asc gc --store <url> [--dry-run] [--pitr-days <n>]";

#[derive(clap::Parser, Debug)]
#[command(name = "gc", disable_help_flag = true, disable_version_flag = true)]
pub struct Args {
    #[arg(trailing_var_arg = true, allow_hyphen_values = true, num_args = 0..)]
    pub rest: Vec<OsString>,
}

pub const NATIVE: &[NativeSpec] = &[NativeSpec {
    cmd: "gc",
    flags: &[
        "--store",
        "--dry-run",
        "-n",
        "--pitr-days",
        "--auto",
        "-q",
        "--quiet",
    ],
    placeholders: &[],
}];

/// Parsed arguments of the local form.
#[derive(Debug, Default, PartialEq, Eq)]
struct LocalArgs {
    dry_run: bool,
    quiet: bool,
}

fn usage(msg: &str) -> CliError {
    CliError::Usage(format!("error: {msg}\n{USAGE}"))
}

fn parse_local(rest: &[OsString]) -> Result<LocalArgs> {
    let mut out = LocalArgs::default();
    for a in rest {
        match a.to_str() {
            Some("--dry-run" | "-n") => out.dry_run = true,
            Some("-q" | "--quiet") => out.quiet = true,
            Some("--auto") => {}
            Some(other) => return Err(usage(&format!("unknown argument {other:?}"))),
            None => return Err(usage("arguments must be UTF-8")),
        }
    }
    Ok(out)
}

fn has_store_flag(rest: &[OsString]) -> bool {
    rest.iter().any(|a| {
        a.to_str()
            .is_some_and(|s| s == "--store" || s.starts_with("--store="))
    })
}

/// The cache directory: `asc.cache.dir` (with `~/`), else `<home>/cache`.
fn cache_root(ctx: &Ctx, home: &AscHome) -> PathBuf {
    match ctx.config.get("asc.cache.dir") {
        Some(p) => match (p.strip_prefix("~/"), std::env::home_dir()) {
            (Some(rest), Some(h)) => h.join(rest),
            _ => PathBuf::from(p),
        },
        None => home.cache(),
    }
}

/// `asc.cache.maxSize`, else the daemon's default.
fn cache_limit(ctx: &Ctx) -> Result<u64> {
    let text = ctx
        .config
        .get("asc.cache.maxsize")
        .or_else(|| ctx.config.get("asc.cache.maxSize"));
    match text {
        Some(t) => scheduler::parse_size(&t).ok_or_else(|| {
            CliError::Fatal(format!(
                "bad numeric config value '{t}' for 'asc.cache.maxsize'"
            ))
        }),
        None => Ok(scheduler::Settings::new(PathBuf::new()).cache_max_bytes),
    }
}

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

/// The expiry of hidden, un-uploaded drafts; only inside a workspace (it needs `state.db`).
async fn expire_drafts(ctx: &Ctx, local: &LocalArgs) -> Result<()> {
    let Some(ws) = ctx.ws_opt() else {
        return Ok(());
    };
    let getenv = |k: &str| std::env::var(k).ok();
    let days = Settings::resolve(&ctx.config, &ctx.env, &getenv)
        .map_err(CliError::Fatal)?
        .hidden_days;
    let r = local_gc::collect(ws, days, unix_now(), local.dry_run).await?;
    if !local.quiet && r.expired_heads > 0 {
        if local.dry_run {
            println!(
                "drafts: {} hidden heads past {days} days; would remove {} objects; nothing was deleted",
                r.expired_heads, r.collectable
            );
        } else {
            println!(
                "drafts: removed {} objects of {} hidden heads past {days} days",
                r.deleted, r.rows_dropped
            );
        }
    }
    Ok(())
}

pub async fn run(ctx: &Ctx, args: Args) -> Result<ExitCode> {
    if has_store_flag(&args.rest) {
        let default_days = ctx
            .config
            .get("asc.gc.pitrDays")
            .and_then(|v| v.trim().parse().ok());
        let parsed = StoreGcArgs::parse(&args.rest, default_days)?;
        return gc_store::run(&parsed).await;
    }
    let local = parse_local(&args.rest)?;
    let home = match ctx.ws_opt() {
        Some(ws) => ws.home.clone(),
        None => AscHome::locate()?,
    };
    let root = cache_root(ctx, &home);
    let limit = cache_limit(ctx)?;
    let before = dir_size(&root);
    let target = limit / 10 * 8;
    if local.dry_run {
        let would = if before.bytes > limit {
            before.bytes.saturating_sub(target)
        } else {
            0
        };
        if !local.quiet {
            println!(
                "cache: {} bytes ({}) in {} files, limit {} bytes ({})",
                before.bytes,
                human(before.bytes),
                before.files,
                limit,
                human(limit)
            );
            println!(
                "would evict about {would} bytes ({}); nothing was deleted",
                human(would)
            );
        }
        expire_drafts(ctx, &local).await?;
        return Ok(ExitCode::SUCCESS);
    }
    let cache = CacheDir::open(&root, home.tmp()).map_err(CliError::Objects)?;
    let freed = cache.evict_to(limit).await.map_err(CliError::Objects)?;
    if !local.quiet {
        let after = dir_size(&root);
        println!(
            "cache: evicted {freed} bytes ({}); {} bytes ({}) in {} files left, limit {} bytes ({})",
            human(freed),
            after.bytes,
            human(after.bytes),
            after.files,
            limit,
            human(limit)
        );
    }
    expire_drafts(ctx, &local).await?;
    Ok(ExitCode::SUCCESS)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn os(a: &[&str]) -> Vec<OsString> {
        a.iter().map(OsString::from).collect()
    }

    #[test]
    fn local_flags() {
        assert_eq!(
            parse_local(&os(&["--dry-run", "-q", "--auto"])).unwrap(),
            LocalArgs {
                dry_run: true,
                quiet: true
            }
        );
        assert!(parse_local(&os(&["--prune=now"])).is_err());
        assert!(parse_local(&os(&["--dry-run=yes"])).is_err());
    }

    #[test]
    fn store_flag_is_detected() {
        assert!(has_store_flag(&os(&["--store", "s3://b"])));
        assert!(has_store_flag(&os(&["--store=s3://b"])));
        assert!(!has_store_flag(&os(&["--dry-run"])));
    }
}
