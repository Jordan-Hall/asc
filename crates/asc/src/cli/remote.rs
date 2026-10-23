//! `remote [-v]`, `remote add|remove|rm|rename|set-url|get-url|show|prune` (SPEC §11.2, §3.4).
//!
//! Remotes live in git config: `remote.<n>.url` (as the user typed it), `.fetch`, `.pushurl`
//! and, for a bucket, `.ascRefs` (`local` = single-writer refs in `state.db`, SPEC §3.4).
//! Output and errors follow git 2.55.

use std::ffi::OsString;
use std::process::ExitCode;

use asc_core::id::RefTarget;
use asc_store::refs::{Expect, RefTxn, RefUpdate, Refs};

use super::common::{CliError, Ctx, Result, StagingAfterMove, WsCtx};
use super::restore::actor;
use super::switch::{ref_name, set_config, unset_config};
use crate::ext;
use crate::native::NativeSpec;
use crate::remote::url::{self, RefsMode, RemoteSpec};
use crate::remote::{self, Intent};

#[derive(clap::Parser, Debug)]
#[command(name = "remote", disable_help_flag = true, disable_version_flag = true)]
pub struct Args {
    #[arg(trailing_var_arg = true, allow_hyphen_values = true, num_args = 0..)]
    pub rest: Vec<OsString>,
}

pub const NATIVE: &[NativeSpec] = &[NativeSpec {
    cmd: "remote",
    flags: &[
        "-v", "--verbose", "--add", "--delete", "--push", "--all", "-n", "--dry-run", "--refs", "--tags",
        "--no-tags",
    ],
    placeholders: &[],
}];

const USAGE: &str = "usage: git remote [-v | --verbose]\n   or: git remote add [-t <branch>] [-m <master>] [-f] [--tags | --no-tags] [--mirror=<fetch|push>] <name> <url>\n   or: git remote rename [--[no-]progress] <old> <new>\n   or: git remote remove <name>\n   or: git remote set-url [--push] <name> <newurl> [<oldurl>]\n   or: git remote set-url --add [--push] <name> <newurl>\n   or: git remote set-url --delete [--push] <name> <url>\n   or: git remote get-url [--push] [--all] <name>\n   or: git remote [-v | --verbose] show [-n] <name>\n   or: git remote prune [-n | --dry-run] <name>";

fn usage() -> CliError {
    CliError::Usage(USAGE.to_owned())
}

fn text(a: &OsString) -> Result<String> {
    a.to_str().map(str::to_owned).ok_or_else(|| CliError::Fatal("asc: argument is not valid UTF-8".into()))
}

/// `error: <text>` with git's exit code.
fn error(text: String, code: u8) -> CliError {
    CliError::Raw(format!("error: {text}"), code)
}

fn no_such(name: &str) -> CliError {
    error(format!("No such remote: '{name}'"), 2)
}

/// A remote name that cannot corrupt a ref name or a config key.
pub(crate) fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('-')
        && !name.starts_with('.')
        && !name.ends_with('.')
        && !name.contains("..")
        && name.len() <= 255
        && ref_name(&format!("refs/remotes/{name}/x")).is_ok()
        && !name.chars().any(|c| c.is_whitespace() || c == '\\')
}

async fn config_add(ctx: &Ctx, key: &str, value: &str) -> Result<()> {
    let rest = vec![
        OsString::from("--local"),
        OsString::from("--add"),
        OsString::from(key),
        OsString::from(value),
    ];
    super::config::run(ctx, super::config::Args { rest }).await?;
    Ok(())
}

async fn config_section(ctx: &Ctx, flag: &str, a: &str, b: Option<&str>) -> Result<()> {
    let mut rest = vec![OsString::from("--local"), OsString::from(flag), OsString::from(a)];
    rest.extend(b.map(OsString::from));
    super::config::run(ctx, super::config::Args { rest }).await?;
    Ok(())
}

fn txn(updates: Vec<RefUpdate>, reason: &str) -> RefTxn {
    RefTxn { updates, actor: actor(), reason: reason.to_owned(), op_id: ext::OpId::new().0 }
}

/// Applies a remote-tracking ref transaction through the single local ref path.
pub(crate) async fn apply_refs(ctx: &Ctx, updates: Vec<RefUpdate>, reason: &str) -> Result<()> {
    if updates.is_empty() {
        return Ok(());
    }
    ctx.update_local_refs(txn(updates, reason), reason, StagingAfterMove::Keep).await
}

/// The remote-tracking refs of `name` as `(full ref, target)`.
pub(crate) async fn tracking_refs(ws: &WsCtx, name: &str) -> Result<Vec<(String, RefTarget)>> {
    let prefix = format!("refs/remotes/{name}/");
    let entries = ws.db.refs().list(ws.repo.key, &prefix).await?;
    Ok(entries
        .into_iter()
        .map(|e| (String::from_utf8_lossy(e.name.as_bytes()).into_owned(), e.target))
        .collect())
}

fn urls_of(ctx: &Ctx, name: &str, push: bool) -> Vec<String> {
    if push {
        let own = ctx.config.get_all(&format!("remote.{name}.pushurl"));
        if !own.is_empty() {
            return own;
        }
    }
    ctx.config.get_all(&format!("remote.{name}.url"))
}

fn label(ctx: &Ctx, name: &str, shown: &str) -> String {
    match url::parse(shown, remote::refs_mode(ctx, name)) {
        Ok(RemoteSpec::Bucket { refs: RefsMode::Local, .. }) => " [bucket, refs local to this machine]".to_owned(),
        Ok(RemoteSpec::Bucket { refs: RefsMode::Shared, .. }) => " [bucket, shared refs]".to_owned(),
        _ => String::new(),
    }
}

fn list(ctx: &Ctx, verbose: bool) -> Result<ExitCode> {
    let mut out = String::new();
    for name in remote::remote_names(ctx) {
        if !verbose {
            out.push_str(&format!("{name}\n"));
            continue;
        }
        for u in urls_of(ctx, &name, false).iter().take(1) {
            out.push_str(&format!("{name}\t{u} (fetch){}\n", label(ctx, &name, u)));
        }
        for u in urls_of(ctx, &name, true) {
            out.push_str(&format!("{name}\t{u} (push){}\n", label(ctx, &name, &u)));
        }
    }
    print!("{out}");
    Ok(ExitCode::SUCCESS)
}

async fn add(ctx: &Ctx, args: &[String]) -> Result<ExitCode> {
    let mut refs: Option<RefsMode> = None;
    let mut tags: Option<bool> = None;
    let mut words = Vec::new();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--refs" => {
                refs = Some(match it.next().map(String::as_str) {
                    Some("local") => RefsMode::Local,
                    Some("shared") => RefsMode::Shared,
                    _ => return Err(usage()),
                });
            }
            "--tags" => tags = Some(true),
            "--no-tags" => tags = Some(false),
            w if w.starts_with('-') => return Err(usage()),
            w => words.push(w.to_owned()),
        }
    }
    let [name, location] = words.as_slice() else { return Err(usage()) };
    ctx.ws()?;
    if !valid_name(name) {
        return Err(CliError::Fatal(format!("'{name}' is not a valid remote name")));
    }
    if ctx.config.get(&format!("remote.{name}.url")).is_some() {
        return Err(error(format!("remote {name} already exists."), 3));
    }
    let spec = url::parse(location, refs.unwrap_or(RefsMode::Local)).map_err(|e| CliError::Fatal(format!("asc: {e}")))?;
    // The user's URL as given; a plain directory is stored as the file URL it names.
    let stored = match &spec {
        RemoteSpec::Bucket { url, .. } if !location.contains("://") => url.clone(),
        _ => location.clone(),
    };
    set_config(ctx, &format!("remote.{name}.url"), &stored).await?;
    set_config(ctx, &format!("remote.{name}.fetch"), &format!("+refs/heads/*:refs/remotes/{name}/*")).await?;
    if let RemoteSpec::Bucket { refs, .. } = &spec {
        let word = if *refs == RefsMode::Shared { "shared" } else { "local" };
        set_config(ctx, &format!("remote.{name}.ascRefs"), word).await?;
    }
    match tags {
        Some(true) => set_config(ctx, &format!("remote.{name}.tagOpt"), "--tags").await?,
        Some(false) => set_config(ctx, &format!("remote.{name}.tagOpt"), "--no-tags").await?,
        None => {}
    }
    Ok(ExitCode::SUCCESS)
}

async fn remove(ctx: &Ctx, args: &[String]) -> Result<ExitCode> {
    let [name] = args else { return Err(usage()) };
    let ws = ctx.ws()?;
    if ctx.config.get(&format!("remote.{name}.url")).is_none() {
        return Err(no_such(name));
    }
    let gone: Vec<RefUpdate> = tracking_refs(ws, name)
        .await?
        .into_iter()
        .map(|(full, target)| {
            Ok(RefUpdate { name: ref_name(&full)?, expect: Expect::Value(target), new: None, expires_at: None })
        })
        .collect::<Result<_>>()?;
    apply_refs(ctx, gone, &format!("remote: remove {name}")).await?;
    // Branches that tracked it lose their upstream, as in git.
    for section in ctx.config.file().sections_by_name("branch").into_iter().flatten() {
        let Some(branch) = section.header().subsection_name().map(|b| b.to_string()) else { continue };
        if ctx.config.get(&format!("branch.{branch}.remote")).as_deref() == Some(name.as_str()) {
            unset_config(ctx, &format!("branch.{branch}.remote")).await;
            unset_config(ctx, &format!("branch.{branch}.merge")).await;
        }
    }
    config_section(ctx, "--remove-section", &format!("remote.{name}"), None).await?;
    Ok(ExitCode::SUCCESS)
}

async fn rename(ctx: &Ctx, args: &[String]) -> Result<ExitCode> {
    let [old, new] = args else { return Err(usage()) };
    let ws = ctx.ws()?;
    if ctx.config.get(&format!("remote.{old}.url")).is_none() {
        return Err(no_such(old));
    }
    if ctx.config.get(&format!("remote.{new}.url")).is_some() {
        return Err(error(format!("remote {new} already exists."), 3));
    }
    if !valid_name(new) {
        return Err(CliError::Fatal(format!("'{new}' is not a valid remote name")));
    }
    let mut moves = Vec::new();
    for (full, target) in tracking_refs(ws, old).await? {
        let short = full.strip_prefix(&format!("refs/remotes/{old}/")).unwrap_or(&full).to_owned();
        let to = ref_name(&format!("refs/remotes/{new}/{short}"))?;
        moves.push(RefUpdate { name: to, expect: Expect::Absent, new: Some(target), expires_at: None });
        moves.push(RefUpdate { name: ref_name(&full)?, expect: Expect::Value(target), new: None, expires_at: None });
    }
    config_section(ctx, "--rename-section", &format!("remote.{old}"), Some(&format!("remote.{new}"))).await?;
    // The default fetch refspec follows the name.
    let specs = ctx.config.get_all(&format!("remote.{new}.fetch"));
    let specs = if specs.is_empty() { ctx.config.get_all(&format!("remote.{old}.fetch")) } else { specs };
    let default_old = format!("+refs/heads/*:refs/remotes/{old}/*");
    if specs.contains(&default_old) {
        let rest = vec![
            OsString::from("--local"),
            OsString::from("--replace-all"),
            OsString::from(format!("remote.{new}.fetch")),
            OsString::from(format!("+refs/heads/*:refs/remotes/{new}/*")),
            OsString::from(regex_escape(&default_old)),
        ];
        super::config::run(ctx, super::config::Args { rest }).await?;
    }
    for section in ctx.config.file().sections_by_name("branch").into_iter().flatten() {
        let Some(branch) = section.header().subsection_name().map(|b| b.to_string()) else { continue };
        if ctx.config.get(&format!("branch.{branch}.remote")).as_deref() == Some(old.as_str()) {
            set_config(ctx, &format!("branch.{branch}.remote"), new).await?;
        }
    }
    apply_refs(ctx, moves, &format!("remote: renamed {old} to {new}")).await?;
    Ok(ExitCode::SUCCESS)
}

fn regex_escape(text: &str) -> String {
    let mut out = String::new();
    for c in text.chars() {
        if "\\^$.|?*+()[]{}".contains(c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

async fn set_url(ctx: &Ctx, args: &[String]) -> Result<ExitCode> {
    let (mut add_mode, mut delete, mut push) = (false, false, false);
    let mut words = Vec::new();
    for a in args {
        match a.as_str() {
            "--add" => add_mode = true,
            "--delete" => delete = true,
            "--push" => push = true,
            w if w.starts_with('-') => return Err(usage()),
            w => words.push(w.to_owned()),
        }
    }
    if add_mode && delete {
        return Err(CliError::Fatal("--add --delete doesn't make sense".into()));
    }
    ctx.ws()?;
    let key_of = |name: &str| format!("remote.{name}.{}", if push { "pushurl" } else { "url" });
    match (add_mode, delete, words.as_slice()) {
        (true, false, [name, newurl]) => {
            if ctx.config.get(&format!("remote.{name}.url")).is_none() {
                return Err(no_such(name));
            }
            check_url(newurl)?;
            config_add(ctx, &key_of(name), newurl).await?;
        }
        (false, true, [name, pattern]) => {
            if ctx.config.get(&format!("remote.{name}.url")).is_none() {
                return Err(no_such(name));
            }
            let current = urls_of(ctx, name, push);
            let re = regex::Regex::new(pattern).map_err(|_| CliError::Fatal(format!("Invalid old URL pattern: {pattern}")))?;
            if !current.iter().any(|u| re.is_match(u)) {
                return Err(CliError::Fatal(format!("No such URL found: {pattern}")));
            }
            if !push && current.iter().filter(|u| !re.is_match(u)).count() == 0 {
                return Err(CliError::Fatal("Will not delete all non-push URLs".into()));
            }
            let rest = vec![
                OsString::from("--local"),
                OsString::from("--unset"),
                OsString::from(key_of(name)),
                OsString::from(pattern),
            ];
            super::config::run(ctx, super::config::Args { rest }).await?;
        }
        (false, false, [name, newurl, rest @ ..]) if rest.len() <= 1 => {
            if ctx.config.get(&format!("remote.{name}.url")).is_none() {
                return Err(no_such(name));
            }
            check_url(newurl)?;
            let mut argv = vec![OsString::from("--local"), OsString::from("--replace-all"), OsString::from(key_of(name)), OsString::from(newurl)];
            if let Some(old) = rest.first() {
                argv.push(OsString::from(regex_escape(old)));
            }
            super::config::run(ctx, super::config::Args { rest: argv }).await?;
        }
        _ => return Err(usage()),
    }
    Ok(ExitCode::SUCCESS)
}

fn check_url(u: &str) -> Result<()> {
    url::parse(u, RefsMode::Local).map(|_| ()).map_err(|e| CliError::Fatal(format!("asc: {e}")))
}

fn get_url(ctx: &Ctx, args: &[String]) -> Result<ExitCode> {
    let (mut push, mut all) = (false, false);
    let mut words = Vec::new();
    for a in args {
        match a.as_str() {
            "--push" => push = true,
            "--all" => all = true,
            w if w.starts_with('-') => return Err(usage()),
            w => words.push(w.to_owned()),
        }
    }
    let [name] = words.as_slice() else { return Err(usage()) };
    let urls = urls_of(ctx, name, push);
    if ctx.config.get(&format!("remote.{name}.url")).is_none() {
        return Err(no_such(name));
    }
    if all {
        for u in &urls {
            println!("{u}");
        }
    } else if let Some(u) = urls.first() {
        println!("{u}");
    }
    Ok(ExitCode::SUCCESS)
}

async fn show(ctx: &Ctx, args: &[String]) -> Result<ExitCode> {
    let mut no_query = false;
    let mut words = Vec::new();
    for a in args {
        match a.as_str() {
            "-n" => no_query = true,
            w if w.starts_with('-') => return Err(usage()),
            w => words.push(w.to_owned()),
        }
    }
    let [name] = words.as_slice() else { return Err(usage()) };
    let ws = ctx.ws()?;
    if ctx.config.get(&format!("remote.{name}.url")).is_none() {
        return Err(no_such(name));
    }
    println!("* remote {name}");
    let fetch_urls = urls_of(ctx, name, false);
    println!("  Fetch URL: {}", fetch_urls.first().cloned().unwrap_or_default());
    for u in urls_of(ctx, name, true) {
        println!("  Push  URL: {u}");
    }
    let mut branches: Vec<String> = Vec::new();
    if no_query {
        println!("  HEAD branch: (not queried)");
    } else {
        let opened = remote::open(ctx, ws, name, Intent::Read).await?;
        let refs = opened.remote.list_refs().await.map_err(|e| CliError::Fatal(format!("asc: {e}")))?;
        for r in refs {
            let full = String::from_utf8_lossy(r.name.as_bytes()).into_owned();
            if let Some(b) = full.strip_prefix("refs/heads/") {
                branches.push(b.to_owned());
            }
        }
        branches.sort();
        println!("  HEAD branch: (unknown)");
    }
    if !branches.is_empty() {
        println!("  Remote branch{}:", if branches.len() == 1 { "" } else { "es" });
        let tracked = tracking_refs(ws, name).await?;
        for b in &branches {
            let is = tracked.iter().any(|(f, _)| f == &format!("refs/remotes/{name}/{b}"));
            if is {
                println!("    {b} tracked");
            } else {
                println!("    {b} new (next fetch will store in remotes/{name})");
            }
        }
    }
    Ok(ExitCode::SUCCESS)
}

async fn prune(ctx: &Ctx, args: &[String]) -> Result<ExitCode> {
    let mut dry = false;
    let mut words = Vec::new();
    for a in args {
        match a.as_str() {
            "-n" | "--dry-run" => dry = true,
            w if w.starts_with('-') => return Err(usage()),
            w => words.push(w.to_owned()),
        }
    }
    let [name] = words.as_slice() else { return Err(usage()) };
    let ws = ctx.ws()?;
    if ctx.config.get(&format!("remote.{name}.url")).is_none() {
        return Err(no_such(name));
    }
    let opened = remote::open(ctx, ws, name, Intent::Read).await?;
    let stale = super::fetch::stale_tracking(ctx, ws, &opened).await?;
    if stale.is_empty() {
        return Ok(ExitCode::SUCCESS);
    }
    println!("Pruning {name}");
    println!("URL: {}", super::url_display::anonymize(&opened.url));
    let mut updates = Vec::new();
    for (full, target) in stale {
        let short = full.strip_prefix("refs/remotes/").unwrap_or(&full);
        println!(" * [{}] {short}", if dry { "would prune" } else { "pruned" });
        updates.push(RefUpdate { name: ref_name(&full)?, expect: Expect::Value(target), new: None, expires_at: None });
    }
    if !dry {
        apply_refs(ctx, updates, &format!("remote: prune {name}")).await?;
    }
    Ok(ExitCode::SUCCESS)
}

pub async fn run(ctx: &Ctx, a: Args) -> Result<ExitCode> {
    let argv: Vec<String> = a.rest.iter().map(text).collect::<Result<_>>()?;
    let verbose = argv.iter().take_while(|w| w.starts_with('-')).any(|w| w == "-v" || w == "--verbose");
    let rest: Vec<String> = argv.iter().filter(|w| !(*w == "-v" || *w == "--verbose")).cloned().collect();
    match rest.split_first() {
        None => {
            ctx.ws()?;
            list(ctx, verbose)
        }
        Some((cmd, tail)) => match cmd.as_str() {
            "add" => add(ctx, tail).await,
            "remove" | "rm" => remove(ctx, tail).await,
            "rename" => rename(ctx, tail).await,
            "set-url" => set_url(ctx, tail).await,
            "get-url" => get_url(ctx, tail),
            "show" => show(ctx, tail).await,
            "prune" => prune(ctx, tail).await,
            _ => Err(usage()),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_names_are_checked() {
        assert!(valid_name("origin"));
        assert!(valid_name("up-stream_2"));
        for bad in ["", "-x", ".x", "a..b", "a b", "a\\b", "a/", "x.lock"] {
            assert!(!valid_name(bad), "{bad:?}");
        }
    }

    #[test]
    fn regex_escape_makes_a_literal() {
        let re = regex::Regex::new(&regex_escape("+refs/heads/*:refs/remotes/a/*")).unwrap();
        assert!(re.is_match("+refs/heads/*:refs/remotes/a/*"));
        assert!(!re.is_match("+refs/heads/x:refs/remotes/a/x"));
    }
}
