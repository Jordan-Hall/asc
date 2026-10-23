//! `branch` (SPEC §11.2): list (through ref-filter, so `--format`, `--sort`, `--contains`,
//! `--merged` and `--points-at` take the whole atom set), create, delete, rename, copy and the
//! upstream settings. Every ref change goes through `Ctx::update_local_refs`; a branch another
//! workspace has checked out is refused as git refuses it (SPEC §9.3).

use std::ffi::OsString;
use std::io::Write as _;
use std::process::ExitCode;

use asc_core::id::{ChangesetId, RefTarget};
use asc_engine::history::HistoryApi;
use asc_git::projection::Head;
use asc_store::refs::{Expect, RefTxn};

use super::common::{CliError, Ctx, Result, StagingAfterMove, WsCtx};
use super::output::Output;
use super::pager::PagerWant;
use super::ref_filter::{self, Filters, Format, Query, Quote, Request, SortKey};
use super::refs_plumbing::args::{Cursor, Word, resolve_oid, resolve_target};
use super::restore::{actor, tool};
use super::switch::{
    Rev, RevKind, Saved, TrackOpt, abbrev_len, bad_branch_name, branch_tip, ensure_mapped, fatal,
    held_anywhere, held_elsewhere, peel, point_head, ref_target, remove_branch_section,
    resolve_rev, restore_head, reverse_refspec, set_config, track_start, unset_config,
    upstream_ref, valid_branch_name,
};
use crate::ext;
use crate::native::NativeSpec;

#[derive(clap::Parser, Debug)]
#[command(name = "branch", disable_help_flag = true, disable_version_flag = true)]
pub struct Args {
    /// Every word before `--`, options included: the command reads its own flags.
    #[arg(allow_hyphen_values = true, num_args = 0..)]
    pub rest: Vec<OsString>,
    /// The words after `--`.
    #[arg(last = true)]
    pub after: Vec<OsString>,
}

impl Args {
    /// The words as typed, `--` restored (clap consumes it).
    pub(crate) fn argv(&self) -> Vec<OsString> {
        let mut all = self.rest.clone();
        if !self.after.is_empty() {
            all.push(OsString::from("--"));
            all.extend(self.after.iter().cloned());
        }
        all
    }
}

pub const NATIVE: &[NativeSpec] = &[NativeSpec {
    cmd: "branch",
    flags: &[
        "-a",
        "--all",
        "-r",
        "--remotes",
        "-v",
        "--verbose",
        "-q",
        "--quiet",
        "-f",
        "--force",
        "-d",
        "--delete",
        "-D",
        "-m",
        "--move",
        "-M",
        "-c",
        "--copy",
        "-C",
        "-l",
        "--list",
        "-u",
        "--set-upstream-to",
        "--unset-upstream",
        "-t",
        "--track",
        "--no-track",
        "--show-current",
        "--contains",
        "--no-contains",
        "--merged",
        "--no-merged",
        "--points-at",
        "--format",
        "--sort",
        "--abbrev",
        "--no-abbrev",
        "--color",
        "--no-color",
        "-i",
        "--ignore-case",
    ],
    placeholders: ref_filter::ATOMS,
}];

const USAGE: &str = "usage: git branch [<options>] [-r | -a] [--merged] [--no-merged]\n   or: git branch [<options>] [-f] <branch-name> [<start-point>]\n   or: git branch [<options>] [-l] [<pattern>...]\n   or: git branch [<options>] [-r] (-d | -D) <branch-name>...\n   or: git branch [<options>] (-m | -M) [<old-branch>] <new-branch>\n   or: git branch [<options>] (-c | -C) [<old-branch>] <new-branch>\n   or: git branch [<options>] [-r | -a] [--points-at]\n   or: git branch [<options>] [-r | -a] [--format]";

/// Names the colours `branch` uses (git's defaults), as escape sequences.
struct Colors {
    on: bool,
}

impl Colors {
    fn code(&self, code: &str) -> String {
        if self.on {
            format!("\x1b[{code}m")
        } else {
            String::new()
        }
    }
    fn reset(&self) -> String {
        self.code("")
    }
}

#[derive(Default)]
struct Opts {
    all: bool,
    remotes: bool,
    list: bool,
    verbose: u8,
    quiet: bool,
    force: bool,
    /// 1 for `-d`, 2 for `-D`.
    delete: u8,
    rename: u8,
    copy: u8,
    set_upstream: Option<String>,
    unset_upstream: bool,
    track: TrackOpt,
    show_current: bool,
    contains: Vec<String>,
    no_contains: Vec<String>,
    merged: Vec<String>,
    no_merged: Vec<String>,
    points_at: Vec<String>,
    format: Option<String>,
    sorts: Vec<String>,
    /// `None`: the default; `Some(0)`: `--no-abbrev`.
    abbrev: Option<usize>,
    color: Option<bool>,
    ignore_case: bool,
    words: Vec<String>,
}

fn parse_color(value: &str) -> Result<Option<bool>> {
    Ok(match value {
        "always" | "true" | "yes" | "on" | "1" | "" => Some(true),
        "never" | "false" | "no" | "off" | "0" => Some(false),
        "auto" => None,
        _ => {
            return Err(CliError::Fatal(format!(
                "bad boolean config value '{value}' for 'color'"
            )));
        }
    })
}

fn parse(args: &[OsString]) -> Result<Opts> {
    let mut cur = Cursor::new(args, USAGE);
    let mut o = Opts::default();
    while let Some(word) = cur.next() {
        match word {
            Word::End => o.words.extend(cur.rest()),
            Word::Operand(w) => o.words.push(w),
            Word::Short(flags) => {
                let mut chars = flags.chars();
                while let Some(c) = chars.next() {
                    match c {
                        'a' => o.all = true,
                        'r' => o.remotes = true,
                        'v' => o.verbose = o.verbose.saturating_add(1),
                        'q' => o.quiet = true,
                        'f' => o.force = true,
                        'd' => o.delete = o.delete.max(1),
                        'D' => {
                            o.delete = 2;
                            o.force = true;
                        }
                        'm' => o.rename = o.rename.max(1),
                        'M' => {
                            o.rename = 2;
                            o.force = true;
                        }
                        'c' => o.copy = o.copy.max(1),
                        'C' => {
                            o.copy = 2;
                            o.force = true;
                        }
                        'l' => o.list = true,
                        't' => o.track = TrackOpt::Direct,
                        'i' => o.ignore_case = true,
                        'u' => {
                            let rest: String = chars.by_ref().collect();
                            o.set_upstream = Some(if rest.is_empty() {
                                cur.value("u", None)?
                            } else {
                                rest
                            });
                        }
                        other => return Err(cur.usage_error(&format!("unknown switch `{other}'"))),
                    }
                }
            }
            Word::Long(name, inline) => match name.as_str() {
                "all" => o.all = true,
                "remotes" => o.remotes = true,
                "verbose" => o.verbose = o.verbose.saturating_add(1),
                "quiet" => o.quiet = true,
                "force" => o.force = true,
                "delete" => o.delete = o.delete.max(1),
                "move" => o.rename = o.rename.max(1),
                "copy" => o.copy = o.copy.max(1),
                "list" => o.list = true,
                "set-upstream-to" => o.set_upstream = Some(cur.value("set-upstream-to", inline)?),
                "unset-upstream" => o.unset_upstream = true,
                "track" => {
                    o.track = match inline.as_deref() {
                        None | Some("direct") => TrackOpt::Direct,
                        Some("inherit") => TrackOpt::Inherit,
                        Some(other) => {
                            return Err(CliError::Fatal(format!(
                                "malformed value for --track: {other}"
                            )));
                        }
                    };
                }
                "no-track" => o.track = TrackOpt::No,
                "show-current" => o.show_current = true,
                "contains" => o.contains.push(cur.value_or(inline, "HEAD")),
                "no-contains" => o.no_contains.push(cur.value_or(inline, "HEAD")),
                "merged" => o.merged.push(cur.value_or(inline, "HEAD")),
                "no-merged" => o.no_merged.push(cur.value_or(inline, "HEAD")),
                "points-at" => o.points_at.push(cur.value("points-at", inline)?),
                "format" => o.format = Some(cur.value("format", inline)?),
                "sort" => o.sorts.push(cur.value("sort", inline)?),
                "abbrev" => {
                    o.abbrev = Some(match inline {
                        Some(v) => v.parse::<usize>().map_err(|_| {
                            cur.usage_error("option `abbrev' expects a numerical value")
                        })?,
                        None => 7,
                    });
                }
                "no-abbrev" => o.abbrev = Some(0),
                "color" => o.color = parse_color(inline.as_deref().unwrap_or("always"))?,
                "no-color" => o.color = Some(false),
                "ignore-case" => o.ignore_case = true,
                other => return Err(cur.usage_error(&format!("unknown option `{other}'"))),
            },
        }
    }
    Ok(o)
}

/// The commits `words` name; a tag is peeled. `fatal` makes a bad name exit 128 (the options
/// git reads with a callback exit 129).
async fn commits(
    ctx: &Ctx,
    ws: &WsCtx,
    words: &[String],
    fatal_exit: bool,
) -> Result<Vec<ChangesetId>> {
    let mut out = Vec::new();
    for w in words {
        let found = match resolve_target(ctx, ws, w).await? {
            Some(t) => Some(peel(ws, t).await?),
            None => None,
        };
        out.push(found.ok_or_else(|| {
            if fatal_exit {
                CliError::Fatal(format!("malformed object name {w}"))
            } else {
                CliError::Raw(format!("error: malformed object name {w}"), 129)
            }
        })?);
    }
    Ok(out)
}

// ------------------------------------------------------------------------------------ listing

/// The text of the detached-HEAD pseudo entry, `(HEAD detached at abc1234)`.
async fn detached_description(
    ctx: &Ctx,
    ws: &WsCtx,
) -> Result<Option<(String, Option<ChangesetId>)>> {
    let Head::Detached(oid) = ws.projection.read_head()? else {
        return Ok(None);
    };
    let text = oid.to_string();
    Ok(Some((
        format!("(HEAD detached at {})", &text[..abbrev_len(ctx)]),
        ws.head_commit().await?,
    )))
}

fn width(text: &str) -> usize {
    text.chars().count()
}

async fn list(ctx: &Ctx, ws: &WsCtx, o: &Opts) -> Result<ExitCode> {
    let local = !o.remotes || o.all;
    let remote = o.remotes || o.all;
    let mut prefixes = Vec::new();
    if local {
        prefixes.push("refs/heads/");
    }
    if remote {
        prefixes.push("refs/remotes/");
    }
    let mut patterns = Vec::new();
    for prefix in &prefixes {
        if o.words.is_empty() {
            patterns.push(format!("{prefix}*"));
        } else {
            patterns.extend(o.words.iter().map(|p| format!("{prefix}{p}")));
        }
    }
    let mut points_at = Vec::new();
    for w in &o.points_at {
        points_at.push(
            resolve_oid(ctx, ws, w)
                .await?
                .ok_or_else(|| CliError::Raw(format!("error: malformed object name '{w}'"), 129))?,
        );
    }
    let filters = Filters {
        points_at,
        merged: commits(ctx, ws, &o.merged, true).await?,
        no_merged: commits(ctx, ws, &o.no_merged, true).await?,
        contains: commits(ctx, ws, &o.contains, false).await?,
        no_contains: commits(ctx, ws, &o.no_contains, false).await?,
    };
    let mut out = Output::open(ctx, "branch", PagerWant::Auto)?;
    let color = o
        .color
        .unwrap_or_else(|| match ctx.config.get("color.branch").as_deref() {
            Some(v) => parse_color(v).ok().flatten().unwrap_or_else(|| out.color()),
            None => out.color(),
        });
    let colors = Colors { on: color };
    let remote_prefix = if o.all { "remotes/" } else { "" };

    // The format: git's own unless `--format` replaces it. The two formats (local and remote
    // refs) are parsed apart; a third one made of both loads every atom either needs.
    let (local_text, remote_text) = match &o.format {
        Some(f) => (f.clone(), f.clone()),
        None => default_formats(o, &colors, remote_prefix, 0),
    };
    let sorts: Vec<String> = if o.sorts.is_empty() {
        ctx.config.get_all("branch.sort")
    } else {
        o.sorts.clone()
    };
    let sorts = sorts
        .iter()
        .map(|s| ref_filter::parse_sort_key(s).map_err(ref_filter::to_cli))
        .collect::<Result<Vec<SortKey>>>()?;
    let load = Format::parse(format!("{local_text}{remote_text}").as_bytes(), Quote::None)
        .map_err(ref_filter::to_cli)?;
    let req = Request {
        query: Query {
            patterns,
            excludes: Vec::new(),
            ignore_case: o.ignore_case,
            as_path: false,
            include_root: false,
            filters,
        },
        format: &load,
        sorts,
        color,
    };
    let (items, env) = ref_filter::list(ctx, ws, &req).await?;
    let detached = if local
        && o.format.is_none()
        && (o.words.is_empty() || o.words.iter().any(|p| p == "HEAD"))
    {
        detached_description(ctx, ws).await?
    } else {
        None
    };

    // Verbose lines align the names to the longest one.
    let maxwidth = if o.verbose > 0 && o.format.is_none() {
        let names = items
            .iter()
            .map(|i| match i.name.strip_prefix("refs/heads/") {
                Some(n) => width(n),
                None => {
                    width(i.name.strip_prefix("refs/remotes/").unwrap_or(&i.name))
                        + remote_prefix.len()
                }
            });
        names
            .chain(detached.iter().map(|(d, _)| width(d)))
            .max()
            .unwrap_or(0)
    } else {
        0
    };
    let (local_text, remote_text) = match &o.format {
        Some(_) => (local_text, remote_text),
        None => default_formats(o, &colors, remote_prefix, maxwidth),
    };
    let local_format =
        Format::parse(local_text.as_bytes(), Quote::None).map_err(ref_filter::to_cli)?;
    let remote_format =
        Format::parse(remote_text.as_bytes(), Quote::None).map_err(ref_filter::to_cli)?;

    let result = (|| -> Result<()> {
        if let Some((desc, _)) = &detached {
            out.write_all(b"* ")?;
            out.write_all(colors.code("32").as_bytes())?;
            if o.verbose > 0 {
                out.write_all(format!("{desc:<maxwidth$}").as_bytes())?;
            } else {
                out.write_all(desc.as_bytes())?;
            }
            out.write_all(colors.reset().as_bytes())?;
        }
        Ok(())
    })();
    result?;
    if let Some((_, head)) = &detached {
        if o.verbose > 0 {
            let line = match head {
                Some(cs) => super::switch::commit_line(ctx, ws, *cs).await?,
                None => String::new(),
            };
            out.write_all(format!(" {line}").as_bytes())?;
        }
        out.write_all(b"\n")?;
    }
    let result = (|| -> Result<()> {
        for item in &items {
            let f = if item.name.starts_with("refs/remotes/") {
                &remote_format
            } else {
                &local_format
            };
            let line = f.render(item, &env).map_err(ref_filter::to_cli)?;
            out.write_all(&line)?;
            out.write_all(b"\n")?;
        }
        Ok(())
    })();
    match result {
        Err(CliError::Io(e)) if e.kind() == std::io::ErrorKind::BrokenPipe => {}
        other => other?,
    }
    out.finish()?;
    Ok(ExitCode::SUCCESS)
}

/// git's `build_format` for local and remote branches.
fn default_formats(o: &Opts, c: &Colors, remote_prefix: &str, maxwidth: usize) -> (String, String) {
    let (green, red, blue, cyan, reset) = (
        c.code("32"),
        c.code("31"),
        c.code("34"),
        c.code("36"),
        c.reset(),
    );
    let head = format!(
        "%(if)%(HEAD)%(then)* {green}%(else)%(if)%(worktreepath)%(then)+ {cyan}%(else)  %(end)%(end)"
    );
    if o.verbose == 0 {
        let local = format!("{head}%(refname:lstrip=2){reset}");
        let remote = format!(
            "  {red}{remote_prefix}%(refname:lstrip=2){reset}%(if)%(symref)%(then) -> %(symref:short)%(end)"
        );
        return (local, remote);
    }
    let obname = match o.abbrev {
        None => "%(objectname:short)".to_owned(),
        Some(0) => "%(objectname)".to_owned(),
        Some(n) => format!("%(objectname:short={n})"),
    };
    let mut local =
        format!("{head}%(align:{maxwidth},left)%(refname:lstrip=2)%(end){reset} {obname} ");
    let mut remote = format!(
        "  {red}%(align:{maxwidth},left){remote_prefix}%(refname:lstrip=2)%(end){reset} {obname} "
    );
    if o.verbose > 1 {
        local.push_str(&format!(
            "%(if:notequals=*)%(HEAD)%(then)%(if)%(worktreepath)%(then)({cyan}%(worktreepath){reset}) %(end)%(end)\
             %(if)%(upstream)%(then)[{blue}%(upstream:short){reset}%(if)%(upstream:track)%(then): %(upstream:track,nobracket)%(end)] %(end)%(contents:subject)"
        ));
    } else {
        local.push_str("%(contents:subject)");
    }
    remote.push_str("%(if)%(symref)%(then)-> %(symref:short)%(else)%(contents:subject)%(end)");
    (local, remote)
}

// ------------------------------------------------------------------------- create and friends

fn txn(updates: Vec<asc_store::refs::RefUpdate>) -> RefTxn {
    RefTxn {
        updates,
        actor: actor(),
        reason: String::new(),
        op_id: ext::OpId::new().0,
    }
}

fn update(name: &str, expect: Expect, new: Option<RefTarget>) -> Result<asc_store::refs::RefUpdate> {
    Ok(asc_store::refs::RefUpdate {
        name: super::switch::ref_name(name)?,
        expect,
        new,
        expires_at: None,
    })
}

fn in_use(branch: &str, path: &str, what: &str) -> CliError {
    CliError::Fatal(format!(
        "cannot {what} the branch '{branch}' used by worktree at '{path}'"
    ))
}

fn current_branch(ws: &WsCtx) -> Result<Option<String>> {
    Ok(match ws.projection.read_head()? {
        Head::Branch(b) => Some(b),
        Head::Detached(_) => None,
    })
}

async fn create(ctx: &Ctx, ws: &WsCtx, o: &Opts) -> Result<ExitCode> {
    let (name, start_word) = match o.words.as_slice() {
        [name] => (name, None),
        [name, start] => (name, Some(start)),
        _ => return Err(CliError::Usage(USAGE.to_owned())),
    };
    if !valid_branch_name(name) {
        return Err(bad_branch_name(ctx, name));
    }
    let existing = ref_target(ws, &format!("refs/heads/{name}")).await?;
    if existing.is_some() {
        if !o.force {
            return Err(CliError::Fatal(format!(
                "a branch named '{name}' already exists"
            )));
        }
        if let Some(path) = held_anywhere(ws, name).await? {
            return Err(in_use(name, &path, "force update"));
        }
    }
    let start = match start_word {
        Some(w) => resolve_rev(ctx, ws, w)
            .await?
            .ok_or_else(|| CliError::Fatal(format!("not a valid object name: '{w}'")))?,
        None => match ws.head_commit().await? {
            Some(commit) => Rev {
                label: "HEAD".into(),
                commit,
                kind: RevKind::Other,
            },
            None => {
                let shown = current_branch(ws)?.unwrap_or_else(|| "HEAD".into());
                return Err(CliError::Fatal(format!(
                    "not a valid object name: '{shown}'"
                )));
            }
        },
    };
    ensure_mapped(ws, start.commit).await?;
    let expect = existing.map_or(Expect::Absent, Expect::Value);
    let reason = if existing.is_some() {
        format!("branch: Reset to {}", start.label)
    } else {
        format!("branch: Created from {}", start.label)
    };
    let u = update(
        &format!("refs/heads/{name}"),
        expect,
        Some(RefTarget::Changeset(start.commit)),
    )?;
    ctx.update_local_refs(txn(vec![u]), &reason, StagingAfterMove::Keep)
        .await?;
    track_start(ctx, name, &start, o.track, o.quiet).await?;
    Ok(ExitCode::SUCCESS)
}

/// The commit git calls "merged enough" to delete `branch` safely with `-d`: reachable from its
/// upstream when it has one, else from HEAD.
async fn merged_for_delete(ctx: &Ctx, ws: &WsCtx, branch: &str, tip: ChangesetId) -> Result<bool> {
    let head = ws.head_commit().await?;
    let upstream = match upstream_ref(ctx, branch) {
        Some((_, tracking)) => match ref_target(ws, &tracking).await? {
            Some(t) => Some((tracking, peel(ws, t).await?)),
            None => None,
        },
        None => None,
    };
    let reaches = |a: ChangesetId, b: Option<ChangesetId>| async move {
        match b {
            Some(b) => HistoryApi::is_ancestor(&ws.engine, a, b)
                .await
                .map_err(fatal),
            None => Ok(false),
        }
    };
    let (reference, name) = match &upstream {
        Some((name, c)) => (Some(*c), Some(name.clone())),
        None => (head, None),
    };
    let merged = reaches(tip, reference).await?;
    if let Some(name) = name {
        let in_head = reaches(tip, head).await?;
        if head.is_some() && in_head != merged {
            if merged {
                eprintln!(
                    "warning: deleting branch '{branch}' that has been merged to\n         '{name}', but not yet merged to HEAD"
                );
            } else {
                eprintln!(
                    "warning: not deleting branch '{branch}' that is not yet merged to\n         '{name}', even though it is merged to HEAD"
                );
            }
        }
    }
    Ok(merged)
}

async fn delete(ctx: &Ctx, ws: &WsCtx, o: &Opts) -> Result<ExitCode> {
    if o.words.is_empty() {
        return Err(CliError::Fatal("branch name required".into()));
    }
    let prefix = if o.remotes {
        "refs/remotes/"
    } else {
        "refs/heads/"
    };
    let mut status = 0u8;
    for name in &o.words {
        let full = format!("{prefix}{name}");
        if !o.remotes
            && let Some(path) = held_anywhere(ws, name).await?
        {
            eprintln!("error: cannot delete branch '{name}' used by worktree at '{path}'");
            status = 1;
            continue;
        }
        let Some(target) = ref_target(ws, &full).await? else {
            let what = if o.remotes {
                "remote-tracking branch"
            } else {
                "branch"
            };
            eprintln!("error: {what} '{name}' not found");
            status = 1;
            continue;
        };
        let tip = peel(ws, target).await?;
        if o.delete == 1 && !o.remotes && !merged_for_delete(ctx, ws, name, tip).await? {
            eprintln!("error: the branch '{name}' is not fully merged");
            if ctx.config.bool("advice.forceDeleteBranch") != Some(false) {
                let bin = tool(ctx);
                eprintln!(
                    "hint: If you are sure you want to delete it, run '{bin} branch -D {name}'\n\
                     hint: Disable this message with \"{bin} config set advice.forceDeleteBranch false\""
                );
            }
            status = 1;
            continue;
        }
        let u = update(&full, Expect::Value(target), None)?;
        ctx.update_local_refs(txn(vec![u]), "branch: Deleted", StagingAfterMove::Keep)
            .await?;
        if !o.remotes {
            remove_branch_section(ctx, name).await;
        }
        if !o.quiet {
            let git = ws
                .engine
                .git_commit_of(tip)
                .await
                .map_err(fatal)?
                .to_string();
            let what = if o.remotes {
                "remote-tracking branch"
            } else {
                "branch"
            };
            println!("Deleted {what} {name} (was {}).", &git[..abbrev_len(ctx)]);
        }
    }
    Ok(ExitCode::from(status))
}

/// Copies the `branch.<old>.*` settings git keeps with a branch.
async fn copy_config(ctx: &Ctx, old: &str, new: &str) -> Result<()> {
    for key in ["remote", "merge", "rebase", "pushRemote"] {
        if let Some(v) = ctx.config.get(&format!("branch.{old}.{key}")) {
            set_config(ctx, &format!("branch.{new}.{key}"), &v).await?;
        }
    }
    Ok(())
}

async fn rename_or_copy(ctx: &Ctx, ws: &WsCtx, o: &Opts, copy: bool) -> Result<ExitCode> {
    let current = current_branch(ws)?;
    if o.words.is_empty() {
        return Err(CliError::Fatal("branch name required".into()));
    }
    let (old, new) = match o.words.as_slice() {
        [new] => match &current {
            Some(c) => (c.clone(), new.clone()),
            None => {
                return Err(CliError::Fatal(format!(
                    "cannot {} the current branch while not on any.",
                    if copy { "copy" } else { "rename" }
                )));
            }
        },
        [old, new] => (old.clone(), new.clone()),
        _ => return Err(CliError::Usage(USAGE.to_owned())),
    };
    if !valid_branch_name(&new) {
        return Err(bad_branch_name(ctx, &new));
    }
    let old_target = ref_target(ws, &format!("refs/heads/{old}")).await?;
    let unborn_current = old_target.is_none() && current.as_deref() == Some(old.as_str());
    if old_target.is_none() && !unborn_current {
        return Err(CliError::Fatal(format!("no branch named '{old}'")));
    }
    let new_target = ref_target(ws, &format!("refs/heads/{new}")).await?;
    if old == new && !copy {
        return Ok(ExitCode::SUCCESS);
    }
    if new_target.is_some() {
        if !o.force {
            return Err(CliError::Fatal(format!(
                "a branch named '{new}' already exists"
            )));
        }
        if let Some(path) = held_anywhere(ws, &new).await? {
            return Err(in_use(&new, &path, "force update"));
        }
    }
    if !copy && let Some(path) = held_elsewhere(ws, &old).await? {
        return Err(in_use(&old, &path, "force update"));
    }
    let new_ref = format!("refs/heads/{new}");
    let old_ref = format!("refs/heads/{old}");
    if unborn_current {
        // Nothing to move: HEAD names the new unborn branch.
        let saved = Saved::capture(ws).await?;
        point_head(ws, &Head::Branch(new.clone()), &new_ref, saved.base).await?;
        return Ok(ExitCode::SUCCESS);
    }
    let old_target = old_target.ok_or_else(|| fatal("asc: branch vanished"))?;
    let old_tip = peel(ws, old_target).await?;
    ensure_mapped(ws, old_tip).await?;
    let mut updates = vec![update(
        &new_ref,
        new_target.map_or(Expect::Absent, Expect::Value),
        Some(old_target),
    )?];
    let follows_head = !copy && current.as_deref() == Some(old.as_str());
    let reason = if copy {
        format!("branch: Created from {old}")
    } else {
        updates.push(update(&old_ref, Expect::Value(old_target), None)?);
        format!("Branch: renamed {old_ref} to {new_ref}")
    };
    let saved = Saved::capture(ws).await?;
    if follows_head {
        point_head(ws, &Head::Branch(new.clone()), &new_ref, saved.base).await?;
    }
    if let Err(e) = ctx
        .update_local_refs(txn(updates), &reason, StagingAfterMove::Keep)
        .await
    {
        if follows_head && branch_tip(ws, &new).await? != Some(old_tip) {
            restore_head(ws, &saved).await;
        }
        return Err(e);
    }
    if copy {
        copy_config(ctx, &old, &new).await?;
    } else {
        let rest = vec![
            OsString::from("--local"),
            OsString::from("--rename-section"),
            OsString::from(format!("branch.{old}")),
            OsString::from(format!("branch.{new}")),
        ];
        // A branch without settings has no section to rename.
        let _ = super::config::run(ctx, super::config::Args { rest }).await;
    }
    Ok(ExitCode::SUCCESS)
}

async fn set_upstream(ctx: &Ctx, ws: &WsCtx, o: &Opts, upstream: &str) -> Result<ExitCode> {
    let branch = match (o.words.as_slice(), current_branch(ws)?) {
        ([b], _) => b.clone(),
        ([], Some(b)) => b,
        ([], None) => {
            return Err(CliError::Fatal(format!(
                "could not set upstream of HEAD to '{upstream}' when it does not point to any branch."
            )));
        }
        _ => return Err(CliError::Usage(USAGE.to_owned())),
    };
    if ref_target(ws, &format!("refs/heads/{branch}"))
        .await?
        .is_none()
    {
        let unborn = current_branch(ws)?.as_deref() == Some(branch.as_str());
        return Err(CliError::Fatal(if unborn {
            format!("No commit on branch '{branch}' yet.")
        } else {
            format!("branch '{branch}' does not exist")
        }));
    }
    let start = match resolve_rev(ctx, ws, upstream).await? {
        Some(r) if matches!(r.kind, RevKind::Branch(_) | RevKind::Remote(_)) => r,
        _ => {
            let mut text =
                format!("fatal: the requested upstream branch '{upstream}' does not exist");
            if ctx.config.bool("advice.setUpstreamFailure") != Some(false) {
                let bin = tool(ctx);
                text.push_str(&format!(
                    "\nhint:\nhint: If you are planning on basing your work on an upstream\n\
                     hint: branch that already exists at the remote, you may need to\n\
                     hint: run \"{bin} fetch\" to retrieve it.\nhint:\n\
                     hint: If you are planning to push out a new local branch that\n\
                     hint: will track its remote counterpart, you may want to use\n\
                     hint: \"{bin} push -u\" to set the upstream config as you push.\n\
                     hint: Disable this message with \"{bin} config set advice.setUpstreamFailure false\""
                ));
            }
            return Err(CliError::Raw(text, 128));
        }
    };
    if matches!(&start.kind, RevKind::Branch(b) if *b == branch) {
        eprintln!("warning: not setting branch '{branch}' as its own upstream");
        return Ok(ExitCode::SUCCESS);
    }
    let (remote, merge, shown) = match &start.kind {
        RevKind::Remote(full) => {
            let rest = full.strip_prefix("refs/remotes/").unwrap_or(full);
            let Some((remote, short)) = rest.split_once('/') else {
                return Err(CliError::Fatal(format!(
                    "the requested upstream branch '{upstream}' does not exist"
                )));
            };
            let merge = ctx
                .config
                .get_all(&format!("remote.{remote}.fetch"))
                .iter()
                .find_map(|spec| reverse_refspec(spec, full))
                .unwrap_or_else(|| format!("refs/heads/{short}"));
            (remote.to_owned(), merge, rest.to_owned())
        }
        RevKind::Branch(local) => (".".to_owned(), format!("refs/heads/{local}"), local.clone()),
        _ => {
            return Err(CliError::Fatal(format!(
                "the requested upstream branch '{upstream}' does not exist"
            )));
        }
    };
    set_config(ctx, &format!("branch.{branch}.remote"), &remote).await?;
    set_config(ctx, &format!("branch.{branch}.merge"), &merge).await?;
    if !o.quiet {
        println!("branch '{branch}' set up to track '{shown}'.");
    }
    Ok(ExitCode::SUCCESS)
}

async fn unset_upstream(ctx: &Ctx, ws: &WsCtx, o: &Opts) -> Result<ExitCode> {
    let branch = match (o.words.as_slice(), current_branch(ws)?) {
        ([b], _) => b.clone(),
        ([], Some(b)) => b,
        ([], None) => {
            return Err(CliError::Fatal(
                "could not unset upstream of HEAD when it does not point to any branch.".into(),
            ));
        }
        _ => return Err(CliError::Usage(USAGE.to_owned())),
    };
    if ref_target(ws, &format!("refs/heads/{branch}"))
        .await?
        .is_none()
    {
        return Err(CliError::Fatal(format!("branch '{branch}' does not exist")));
    }
    let remote = ctx.config.get(&format!("branch.{branch}.remote"));
    let merge = ctx.config.get(&format!("branch.{branch}.merge"));
    if remote.is_none() && merge.is_none() {
        return Err(CliError::Fatal(format!(
            "branch '{branch}' has no upstream information"
        )));
    }
    unset_config(ctx, &format!("branch.{branch}.remote")).await;
    unset_config(ctx, &format!("branch.{branch}.merge")).await;
    Ok(ExitCode::SUCCESS)
}

pub async fn run(ctx: &Ctx, a: Args) -> Result<ExitCode> {
    let ws = ctx.ws()?;
    let o = parse(&a.argv())?;
    let modes = [
        o.delete > 0,
        o.rename > 0,
        o.copy > 0,
        o.set_upstream.is_some(),
        o.unset_upstream,
        o.show_current,
        o.list,
    ];
    if modes.iter().filter(|m| **m).count() > 1 {
        return Err(CliError::Fatal(
            "options are incompatible with each other".into(),
        ));
    }
    let filtered = !(o.contains.is_empty()
        && o.no_contains.is_empty()
        && o.merged.is_empty()
        && o.no_merged.is_empty()
        && o.points_at.is_empty());
    if o.delete > 0 {
        return delete(ctx, ws, &o).await;
    }
    if o.rename > 0 {
        return rename_or_copy(ctx, ws, &o, false).await;
    }
    if o.copy > 0 {
        return rename_or_copy(ctx, ws, &o, true).await;
    }
    if let Some(up) = o.set_upstream.clone() {
        return set_upstream(ctx, ws, &o, &up).await;
    }
    if o.unset_upstream {
        return unset_upstream(ctx, ws, &o).await;
    }
    if o.show_current {
        if let Some(b) = current_branch(ws)? {
            println!("{b}");
        }
        return Ok(ExitCode::SUCCESS);
    }
    if o.list || filtered || o.words.is_empty() {
        return list(ctx, ws, &o).await;
    }
    if o.all || o.remotes {
        return Err(CliError::Fatal(
            "the -a and -r options are only valid for listing".into(),
        ));
    }
    create(ctx, ws, &o).await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts(items: &[&str]) -> Opts {
        parse(&items.iter().map(OsString::from).collect::<Vec<_>>()).unwrap()
    }

    #[test]
    fn verbose_counts_and_clusters() {
        let o = opts(&["-vva", "x*"]);
        assert_eq!((o.verbose, o.all), (2, true));
        assert_eq!(o.words, ["x*"]);
        let o = opts(&["-fD", "a", "b"]);
        assert_eq!((o.delete, o.force), (2, true));
        assert_eq!(o.words, ["a", "b"]);
    }

    #[test]
    fn optional_values_default_to_head() {
        let o = opts(&["--merged=topic", "--no-merged", "--contains"]);
        assert_eq!(o.merged, ["topic"]);
        assert_eq!(
            o.no_merged,
            ["--contains"],
            "a following word is the value, as in git"
        );
        let o = opts(&["--merged=topic", "--contains"]);
        assert_eq!(o.contains, ["HEAD"]);
        let o = opts(&["--contains", "v1", "-a"]);
        assert_eq!(
            (o.contains.as_slice(), o.all),
            (["v1".to_owned()].as_slice(), true)
        );
    }

    #[test]
    fn upstream_values_attach_or_follow() {
        assert_eq!(
            opts(&["-uorigin/x"]).set_upstream.as_deref(),
            Some("origin/x")
        );
        assert_eq!(
            opts(&["-u", "origin/x", "b"]).set_upstream.as_deref(),
            Some("origin/x")
        );
        assert_eq!(
            opts(&["--set-upstream-to=o/y"]).set_upstream.as_deref(),
            Some("o/y")
        );
        assert!(opts(&["--no-abbrev"]).abbrev == Some(0));
    }

    #[test]
    fn colour_values() {
        assert_eq!(parse_color("always").unwrap(), Some(true));
        assert_eq!(parse_color("never").unwrap(), Some(false));
        assert_eq!(parse_color("auto").unwrap(), None);
        assert!(parse_color("sometimes").is_err());
    }

    #[test]
    fn the_default_format_pads_verbose_names() {
        let c = Colors { on: false };
        let o = Opts {
            verbose: 1,
            ..Opts::default()
        };
        let (local, remote) = default_formats(&o, &c, "remotes/", 12);
        assert!(local.contains("%(align:12,left)%(refname:lstrip=2)%(end)"));
        assert!(local.ends_with("%(contents:subject)"));
        assert!(remote.contains("remotes/%(refname:lstrip=2)"));
    }
}
