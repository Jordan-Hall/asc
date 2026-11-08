//! `asc gc` (SPEC §3.5, §3.6): evicts the local cache down to 80% of `asc.cache.maxSize`.
//!
//! `store/` holds the objects this machine wrote and nothing else has them, so eviction never
//! touches it. The one removal is the hidden-draft expiry of SPEC §21.2.4
//! ([`crate::branchless::local_gc`]): objects reachable only from hidden drafts older than
//! `asc.draft.hiddenDays`. With `--store <url>` the command collects a bucket-mode repository instead
//! ([`super::gc_store`]). `--dry-run` reports what an eviction would free and changes nothing.
//! The shared blob area of link-mode checkouts (`asc.worktree.link = symlink`) loses every file no
//! link in a registered plain workspace names; a file sealed in the last hour (`asc.gc.sharedGrace`) is kept, as a
//! checkout may not have linked it yet, and nothing is pruned while a plain workspace's directory
//! cannot be read. Git's `--auto` and `--quiet` are accepted; git's other gc flags (`--prune`, `--aggressive`,
//! ...) have nothing to do in a layout without loose objects or packs and are refused.

use std::ffi::OsString;
use std::path::PathBuf;
use std::process::ExitCode;

use asc_state::AscHome;
use asc_state::state::{StateDb, workspaces};
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

/// A shared area file younger than this is kept: a running checkout may not have linked it yet.
const SHARED_GRACE: std::time::Duration = std::time::Duration::from_secs(3600);

/// A duration such as `90s` or `2h` in seconds; unlike a `--ttl`, zero (`0`, `0s`) is allowed.
fn zero_or_ttl(t: &str) -> Option<u64> {
    let digits = t.trim_end_matches(['s', 'm', 'h', 'd', 'w']);
    if !digits.is_empty() && t.len() - digits.len() <= 1 && digits.bytes().all(|b| b == b'0') {
        return Some(0);
    }
    asc_wc::worktree::parse_ttl(t)
}

/// `asc.gc.sharedGrace` (`0s`, `30m`, `2h`, ...), else [`SHARED_GRACE`].
fn shared_grace(ctx: &Ctx) -> Result<std::time::Duration> {
    let text = ctx
        .config
        .get("asc.gc.sharedgrace")
        .or_else(|| ctx.config.get("asc.gc.sharedGrace"));
    match text {
        None => Ok(SHARED_GRACE),
        Some(t) => zero_or_ttl(t.trim())
            .map(std::time::Duration::from_secs)
            .ok_or_else(|| {
                CliError::Fatal(format!(
                    "asc: invalid asc.gc.sharedGrace '{t}'; use a number and s, m, h, d or w"
                ))
            }),
    }
}

/// Prunes the shared blob area down to the files that links in registered plain workspaces name.
async fn prune_shared(ctx: &Ctx, home: &AscHome, local: &LocalArgs) -> Result<()> {
    let grace = shared_grace(ctx)?;
    let area = home.shared_blobs();
    if !area.is_dir() {
        return Ok(());
    }
    let opened;
    let db = match ctx.ws_opt() {
        Some(ws) => &ws.db,
        None => {
            opened = StateDb::open(home).await?;
            &opened
        }
    };
    let mut used = std::collections::HashSet::new();
    for w in workspaces::list(db, None).await? {
        if w.ws.mode != workspaces::Mode::Plain {
            continue;
        }
        let mount = PathBuf::from(&w.ws.mount);
        if let Err(e) = asc_wc::shared::links_under(&mount, &mut used) {
            if !local.quiet {
                println!(
                    "shared: kept every file; cannot read workspace {}: {e}",
                    mount.display()
                );
            }
            return Ok(());
        }
    }
    let done = asc_wc::shared::prune(&area, &used, grace, !local.dry_run)
        .map_err(|e| CliError::Fatal(format!("asc: cannot prune {}: {e}", area.display())))?;
    if !local.quiet {
        if local.dry_run {
            println!(
                "shared: would remove {} unlinked files ({}); {} linked; nothing was deleted",
                done.files,
                human(done.bytes),
                used.len()
            );
        } else {
            println!(
                "shared: removed {} unlinked files ({}); {} linked",
                done.files,
                human(done.bytes),
                used.len()
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
        prune_shared(ctx, &home, &local).await?;
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
    prune_shared(ctx, &home, &local).await?;
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
    fn shared_grace_accepts_zero() {
        assert_eq!(zero_or_ttl("0"), Some(0));
        assert_eq!(zero_or_ttl("0s"), Some(0));
        assert_eq!(zero_or_ttl("2h"), Some(7200));
        assert_eq!(zero_or_ttl("0ss"), None);
        assert_eq!(zero_or_ttl("s"), None);
        assert_eq!(zero_or_ttl("soon"), None);
    }

    #[test]
    fn store_flag_is_detected() {
        assert!(has_store_flag(&os(&["--store", "s3://b"])));
        assert!(has_store_flag(&os(&["--store=s3://b"])));
        assert!(!has_store_flag(&os(&["--dry-run"])));
    }
}
