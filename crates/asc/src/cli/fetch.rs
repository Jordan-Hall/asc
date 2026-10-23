//! `fetch [<remote> [<refspec>…]]` for bucket remotes (SPEC §6.3, §11.2): `--prune`, `--all`,
//! `--tags`, `--deepen=<n>`, `--unshallow`, `--prefetch <pathspec>`.
//!
//! The engine never writes a local ref (SPEC §9.4.1): its `local_updates` (and the prune
//! deletions) are applied here through `Ctx::update_local_refs`, so the `reference-transaction`
//! hook sees them as it does with git. Output is git's: `From <url>` and one aligned line per
//! updated ref, on stderr.

use std::ffi::OsString;
use std::process::ExitCode;

use asc_core::id::{ChangesetId, GitOid, RefTarget};
use asc_engine::history::HistoryApi;
use asc_engine::sync::{Error as SyncError, FetchReport, Refspec};
use asc_engine::tree::Pathspec;
use asc_git::projection::Head;
use asc_state::state::git_map;
use asc_store::refs::{Expect, RefUpdate};

use super::common::{CliError, Ctx, Result, WsCtx};
use super::conflict;
use super::remote::{apply_refs, tracking_refs};
use super::switch::{abbrev_len, ref_name, reverse_refspec};
use crate::native::NativeSpec;
use crate::remote::url::display_from;
use crate::remote::{self, Intent, Opened, StateBoundaries};

#[derive(clap::Parser, Debug)]
#[command(name = "fetch", disable_help_flag = true, disable_version_flag = true)]
pub struct Args {
    #[arg(trailing_var_arg = true, allow_hyphen_values = true, num_args = 0..)]
    pub rest: Vec<OsString>,
}

pub const NATIVE: &[NativeSpec] = &[NativeSpec {
    cmd: "fetch",
    flags: &[
        "-q", "--quiet", "-v", "--verbose", "-p", "--prune", "--all", "-t", "--tags", "-n", "--no-tags", "--deepen",
        "--unshallow", "--prefetch", "-f", "--force", "--progress", "--no-progress", "--no-recurse-submodules",
        "--no-auto-gc", "--no-write-fetch-head", "--atomic",
    ],
    placeholders: &[],
}];

const USAGE: &str = "usage: git fetch [<options>] [<repository> [<refspec>...]]\n   or: git fetch [<options>] <group>\n   or: git fetch --multiple [<options>] [(<repository> | <group>)...]\n   or: git fetch --all [<options>]";

pub(crate) fn fatal(e: impl std::fmt::Display) -> CliError {
    CliError::Fatal(format!("asc: {e}"))
}

/// An engine sync failure as the CLI reports it.
pub(crate) fn sync_error(e: SyncError) -> CliError {
    match e {
        SyncError::RawGit(_) => CliError::Fatal("asc: import this history on the server (asc serve admin import)".into()),
        other => fatal(other),
    }
}

#[derive(Clone, Debug, Default)]
pub(crate) struct Opts {
    pub quiet: bool,
    pub verbose: bool,
    pub prune: Option<bool>,
    pub all: bool,
    pub tags: Option<bool>,
    pub deepen: Option<u32>,
    pub unshallow: bool,
    pub prefetch: Vec<String>,
    pub words: Vec<String>,
}

fn parse(argv: &[OsString]) -> Result<Opts> {
    let mut o = Opts::default();
    let mut it = argv.iter().map(|a| a.to_string_lossy().into_owned()).peekable();
    let mut ended = false;
    while let Some(a) = it.next() {
        if ended || !a.starts_with('-') || a == "-" {
            o.words.push(a);
            continue;
        }
        match a.as_str() {
            "--" => ended = true,
            "-q" | "--quiet" => o.quiet = true,
            "-v" | "--verbose" => o.verbose = true,
            "-p" | "--prune" => o.prune = Some(true),
            "--all" => o.all = true,
            "-t" | "--tags" => o.tags = Some(true),
            "-n" | "--no-tags" => o.tags = Some(false),
            "--unshallow" => o.unshallow = true,
            "--deepen" => {
                let v = it.next().ok_or_else(|| CliError::Usage(USAGE.to_owned()))?;
                o.deepen = Some(v.parse().map_err(|_| CliError::Fatal(format!("depth {v} is not a positive number")))?);
            }
            "--prefetch" => o.prefetch.push(it.next().ok_or_else(|| CliError::Usage(USAGE.to_owned()))?),
            "-f" | "--force" | "--progress" | "--no-progress" | "--no-recurse-submodules" | "--no-auto-gc"
            | "--no-write-fetch-head" | "--atomic" => {}
            w if w.starts_with("--deepen=") => {
                let v = &w["--deepen=".len()..];
                o.deepen = Some(v.parse().map_err(|_| CliError::Fatal(format!("depth {v} is not a positive number")))?);
            }
            w if w.starts_with("--prefetch=") => o.prefetch.push(w["--prefetch=".len()..].to_owned()),
            _ => return Err(CliError::Usage(USAGE.to_owned())),
        }
    }
    Ok(o)
}

/// The branch HEAD names, if any.
pub(crate) fn current_branch(ws: &WsCtx) -> Result<Option<String>> {
    Ok(match ws.projection.read_head()? {
        Head::Branch(b) => Some(b),
        Head::Detached(_) => None,
    })
}

/// The remote a bare `fetch`/`pull` uses: the branch's, else `origin`.
pub(crate) fn default_remote(ctx: &Ctx, ws: &WsCtx) -> Result<String> {
    if let Some(b) = current_branch(ws)?
        && let Some(r) = ctx.config.get(&format!("branch.{b}.remote"))
        && r != "."
    {
        return Ok(r);
    }
    Ok("origin".to_owned())
}

/// The git id of a ref target, shortened to `abbrev` digits; zeros when it cannot be derived.
pub(crate) async fn git_short(ws: &WsCtx, target: Option<RefTarget>, abbrev: usize) -> String {
    let full = match target {
        Some(RefTarget::Changeset(c)) => conflict::git_id(ws, c).await.ok(),
        Some(RefTarget::Tag(t)) => git_map::asc_to_git(&ws.db, ws.repo.key, t.as_bytes())
            .await
            .ok()
            .and_then(|rows| rows.into_iter().next().map(|(o, _)| o)),
        None => None,
    };
    let text = full.unwrap_or_else(|| GitOid::from_bytes([0; 20])).to_string();
    text[..abbrev.min(text.len())].to_owned()
}

/// One line of the ref table.
pub(crate) struct Line {
    pub flag: char,
    pub summary: String,
    pub from: String,
    pub to: String,
    pub reason: Option<String>,
}

/// Renders the table the way git's transport does. `pad_from` is fetch's aligned form.
pub(crate) fn render(lines: &[Line], abbrev: usize, pad_from: bool) -> String {
    let width = 2 * abbrev + 3;
    let refcol = lines.iter().map(|l| l.from.chars().count()).max().unwrap_or(0).max(10);
    let mut out = String::new();
    for l in lines {
        let from = if pad_from { format!("{:<refcol$}", l.from) } else { l.from.clone() };
        let to = if l.to.is_empty() { String::new() } else { format!(" -> {}", l.to) };
        let reason = l.reason.as_ref().map(|r| format!(" ({r})")).unwrap_or_default();
        let line = if l.from.is_empty() {
            format!(" {} {}", l.flag, l.summary)
        } else {
            format!(" {} {:<width$} {from}{to}{reason}", l.flag, l.summary)
        };
        out.push_str(&line);
        out.push('\n');
    }
    out
}

fn short_remote_side(full: &str) -> String {
    full.strip_prefix("refs/heads/")
        .or_else(|| full.strip_prefix("refs/tags/"))
        .unwrap_or(full)
        .to_owned()
}

fn short_local_side(full: &str) -> String {
    full.strip_prefix("refs/remotes/")
        .or_else(|| full.strip_prefix("refs/tags/"))
        .or_else(|| full.strip_prefix("refs/heads/"))
        .unwrap_or(full)
        .to_owned()
}

/// The remote ref a tracking ref `dst` comes from under `specs` (`+src:dst` and `src:dst`).
fn source_of(specs: &[String], dst: &str) -> Option<String> {
    specs.iter().filter(|s| !s.starts_with('^')).find_map(|s| reverse_refspec(s, dst))
}

fn excluded(specs: &[String], src: &str) -> bool {
    specs.iter().filter_map(|s| s.strip_prefix('^')).any(|pat| match pat.split_once('*') {
        None => pat == src,
        Some((pre, post)) => src.len() >= pre.len() + post.len() && src.starts_with(pre) && src.ends_with(post),
    })
}

/// Tracking refs of `opened` whose remote counterpart is gone (`--prune`, `remote prune`).
pub(crate) async fn stale_tracking(ctx: &Ctx, ws: &WsCtx, opened: &Opened) -> Result<Vec<(String, RefTarget)>> {
    let specs = remote::fetch_refspecs(ctx, &opened.name);
    let present: std::collections::HashSet<String> = opened
        .remote
        .list_refs()
        .await
        .map_err(sync_error)?
        .into_iter()
        .map(|r| String::from_utf8_lossy(r.name.as_bytes()).into_owned())
        .collect();
    let mut stale = Vec::new();
    for (full, target) in tracking_refs(ws, &opened.name).await? {
        if let Some(src) = source_of(&specs, &full)
            && !present.contains(&src)
            && !excluded(&specs, &src)
        {
            stale.push((full, target));
        }
    }
    Ok(stale)
}

/// Turns the words after the remote into refspec strings: `a:b` and `+a:b` as given, a bare
/// name as the branch it names with git's opportunistic remote-tracking update.
fn cli_specs(name: &str, words: &[String]) -> Vec<String> {
    words
        .iter()
        .map(|w| {
            if w.contains(':') {
                w.clone()
            } else if w.starts_with("refs/") {
                format!("+{w}:{}", w.replacen("refs/heads/", &format!("refs/remotes/{name}/"), 1))
            } else {
                format!("+refs/heads/{w}:refs/remotes/{name}/{w}")
            }
        })
        .collect()
}

/// What one fetch did, for the caller (`pull` merges the tracking refs).
pub(crate) struct Fetched {
    pub opened: Opened,
    pub report: FetchReport,
    pub rejected: bool,
}

/// Fetches from `target` (a remote name or URL) and prints git's table.
pub(crate) async fn fetch_one(ctx: &Ctx, ws: &WsCtx, target: &str, o: &Opts, words: &[String]) -> Result<Fetched> {
    let opened = remote::open(ctx, ws, target, Intent::Read).await?;
    let mut specs: Vec<String> = if words.is_empty() {
        if opened.configured {
            remote::fetch_refspecs(ctx, &opened.name)
        } else {
            return Err(CliError::Fatal("asc: fetching from a URL needs a refspec".into()));
        }
    } else {
        cli_specs(&opened.name, words)
    };
    let tag_opt = ctx.config.get(&format!("remote.{}.tagOpt", opened.name));
    let tags = o.tags.unwrap_or_else(|| tag_opt.as_deref() == Some("--tags"));
    if tags && !specs.iter().any(|s| s.ends_with("refs/tags/*:refs/tags/*")) {
        specs.push("+refs/tags/*:refs/tags/*".to_owned());
    }
    let parsed: Vec<Refspec> = specs.iter().map(|s| Refspec::parse(s)).collect::<std::result::Result<_, _>>().map_err(sync_error)?;
    let boundaries = StateBoundaries::new(ws);
    let opts = remote::options(ctx, &boundaries);
    let mut report = ws.engine.fetch(&*opened.remote, &parsed, &opts).await.map_err(sync_error)?;

    let prune = o
        .prune
        .or_else(|| ctx.config.bool(&format!("remote.{}.prune", opened.name)))
        .or_else(|| ctx.config.bool("fetch.prune"))
        .unwrap_or(false);
    let mut deleted = Vec::new();
    if prune && opened.configured {
        for (full, target) in stale_tracking(ctx, ws, &opened).await? {
            report.local_updates.updates.push(RefUpdate {
                name: ref_name(&full)?,
                expect: Expect::Value(target),
                new: None,
                expires_at: None,
            });
            deleted.push((full, target));
        }
    }

    let abbrev = abbrev_len(ctx);
    let mut lines = Vec::new();
    for c in &report.updated {
        let full = String::from_utf8_lossy(c.name.as_bytes()).into_owned();
        let from = source_of(&specs, &full).map_or_else(|| short_local_side(&full), |s| short_remote_side(&s));
        let to = short_local_side(&full);
        let is_tag = full.starts_with("refs/tags/");
        let (flag, summary) = match c.old {
            None if is_tag => ('*', "[new tag]".to_owned()),
            None if full.starts_with("refs/remotes/") || full.starts_with("refs/heads/") => ('*', "[new branch]".to_owned()),
            None => ('*', "[new ref]".to_owned()),
            Some(old) => {
                let ff = match (old, c.new) {
                    (RefTarget::Changeset(a), RefTarget::Changeset(b)) => {
                        HistoryApi::is_ancestor(&ws.engine, a, b).await.unwrap_or(false)
                    }
                    _ => false,
                };
                let (a, b) = (git_short(ws, Some(old), abbrev).await, git_short(ws, Some(c.new), abbrev).await);
                if is_tag {
                    ('t', "[tag update]".to_owned())
                } else if ff {
                    (' ', format!("{a}..{b}"))
                } else {
                    ('+', format!("{a}...{b}"))
                }
            }
        };
        let reason = (flag == '+').then(|| "forced update".to_owned());
        lines.push(Line { flag: if flag == 't' { 't' } else { flag }, summary, from, to, reason });
    }
    let mut rejected = false;
    for r in &report.rejected {
        rejected = true;
        let full = String::from_utf8_lossy(r.as_bytes()).into_owned();
        lines.push(Line {
            flag: '!',
            summary: "[rejected]".into(),
            from: source_of(&specs, &full).map_or_else(|| short_local_side(&full), |s| short_remote_side(&s)),
            to: short_local_side(&full),
            reason: Some("non-fast-forward".into()),
        });
    }
    for (full, _) in &deleted {
        lines.push(Line {
            flag: '-',
            summary: "[deleted]".into(),
            from: "(none)".into(),
            to: short_local_side(full),
            reason: None,
        });
    }

    let local = std::mem::take(&mut report.local_updates.updates);
    apply_refs(ctx, local.clone(), &format!("fetch {}", opened.name)).await?;
    report.local_updates.updates = local;

    if !lines.is_empty() && !o.quiet {
        eprintln!("From {}", display_from(&opened.url));
        eprint!("{}", render(&lines, abbrev, true));
    }

    if let Some(n) = o.deepen {
        ws.engine.deepen(&*opened.remote, &opts, Some(n)).await.map_err(sync_error)?;
    }
    if o.unshallow {
        ws.engine.unshallow(&*opened.remote, &opts).await.map_err(sync_error)?;
    }
    if !o.prefetch.is_empty() {
        let spec = Pathspec::parse(o.prefetch.iter().map(String::as_bytes)).map_err(fatal)?;
        for c in &report.updated {
            if let RefTarget::Changeset(cs) = c.new {
                let root = ws.engine.get_changeset(cs).await?.root;
                ws.engine.prefetch(&*opened.remote, root, Some(&spec)).await.map_err(sync_error)?;
            }
        }
    }
    Ok(Fetched { opened, report, rejected })
}

pub async fn run(ctx: &Ctx, a: Args) -> Result<ExitCode> {
    let ws = ctx.ws()?;
    let o = parse(&a.rest)?;
    let mut failed = false;
    if o.all {
        if !o.words.is_empty() {
            return Err(CliError::Fatal("fetch --all does not take a repository argument".into()));
        }
        for name in remote::remote_names(ctx) {
            failed |= fetch_one(ctx, ws, &name, &o, &[]).await?.rejected;
        }
    } else {
        let (target, words) = match o.words.split_first() {
            Some((first, rest)) => (first.clone(), rest.to_vec()),
            None => (default_remote(ctx, ws)?, Vec::new()),
        };
        failed = fetch_one(ctx, ws, &target, &o, &words).await?.rejected;
    }
    Ok(if failed { ExitCode::from(1) } else { ExitCode::SUCCESS })
}

/// The changeset a ref names, when it exists locally.
pub(crate) async fn tip_of(ws: &WsCtx, full: &str) -> Result<Option<ChangesetId>> {
    Ok(match super::switch::ref_target(ws, full).await? {
        Some(RefTarget::Changeset(c)) => Some(c),
        _ => None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(flag: char, summary: &str, from: &str, to: &str) -> Line {
        Line { flag, summary: summary.into(), from: from.into(), to: to.into(), reason: None }
    }

    #[test]
    fn fetch_table_is_git_aligned() {
        let text = render(
            &[line('*', "[new branch]", "master", "origin/master"), line(' ', "1234567..89abcde", "dev", "origin/dev")],
            7,
            true,
        );
        assert_eq!(
            text,
            " * [new branch]      master     -> origin/master\n   1234567..89abcde  dev        -> origin/dev\n"
        );
    }

    #[test]
    fn push_table_is_not_padded() {
        let mut l = line('!', "[rejected]", "master", "master");
        l.reason = Some("fetch first".into());
        assert_eq!(render(&[l], 7, false), " ! [rejected]        master -> master (fetch first)\n");
    }

    #[test]
    fn specs_from_the_command_line() {
        assert_eq!(cli_specs("o", &["m".into()]), vec!["+refs/heads/m:refs/remotes/o/m"]);
        assert_eq!(cli_specs("o", &["a:b".into()]), vec!["a:b"]);
    }

    #[test]
    fn negative_specs_exclude() {
        let specs = vec!["+refs/heads/*:refs/remotes/o/*".to_owned(), "^refs/heads/agent/*".to_owned()];
        assert!(excluded(&specs, "refs/heads/agent/x"));
        assert!(!excluded(&specs, "refs/heads/master"));
        assert_eq!(source_of(&specs, "refs/remotes/o/dev").as_deref(), Some("refs/heads/dev"));
    }
}
