//! `switch` (SPEC §11.2) and the head-moving core that `checkout` shares.
//!
//! A move has three parts, in this order: the working tree (written only through `asc_wc`:
//! `plain::checkout` in `Safe` mode between two small trees, or the forced sync `reset --hard`
//! uses), the ref and `HEAD` change (through `Ctx::update_local_refs`, which runs the
//! `reference-transaction` hook, writes the reflog and fixes the stage), and the report plus the
//! `post-checkout` hook.
//!
//! Safe mode refuses to overwrite local changes the way git does. A path the move changes must be
//! clean in the index (or already staged as the target has it) and in the working tree, and no
//! untracked file may be in the way. Nothing is written before every check has passed.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::ffi::OsString;
use std::fs;
use std::path::PathBuf;
use std::process::ExitCode;

use asc_core::git::ObjectKind;
use asc_core::id::{ChangesetId, FileId, GitOid, RefTarget, TreeId};
use asc_core::object::{Change, Entry, Kind};
use asc_core::path::{RefName, RepoPath};
use asc_engine::graph::{EngineSource, Graph, LogQuery};
use asc_git::index::{FileStat, StatSource};
use asc_git::projection::Head;
use asc_state::state::git_map;
use asc_state::state::stage::{self, Leaf, StageRow, StageSet};
use asc_state::state::workspaces;
use asc_store::refs::{Expect, RefTxn, RefUpdate, Refs as _};
use asc_wc::plain::{self, Mode};
use asc_wc::status;
use futures::TryStreamExt;

use super::common::{CliEngine, CliError, Ctx, Result, StagingAfterMove, WsCtx};
use super::refs_plumbing::args::{Cursor, Word};
use super::restore::{WcParts, actor, index_tree, root_of, sync_worktree, tool};
use super::revparse::commit_of;
use crate::ext;
use crate::hooks_runner::{Hook, HookEnv, Outcome, Runner, null_oid};
use crate::native::NativeSpec;

/// Commits listed in the "leaving commits behind" warning; the walk stops one past it.
const ORPHAN_LIMIT: usize = 1000;
/// The file in the gitdir that remembers where HEAD was before the last switch (`-`).
const PREV_FILE: &str = "asc-prev-head";

#[derive(clap::Parser, Debug)]
#[command(name = "switch", disable_help_flag = true, disable_version_flag = true)]
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
    cmd: "switch",
    flags: &[
        "-c", "--create", "-C", "--force-create", "-d", "--detach", "-f", "--force",
        "--discard-changes", "-q", "--quiet", "-t", "--track", "--no-track", "--guess", "--no-guess",
        "--orphan", "--ignore-other-worktrees", "--progress", "--no-progress",
    ],
    placeholders: &[],
}];

pub(crate) const USAGE_SWITCH: &str = "usage: git switch [<options>] [<branch>]\n\n    -c, --create <branch>\n                          create and switch to a new branch\n    -C, --force-create <branch>\n                          create/reset and switch to a branch\n    --guess               second guess 'git switch <no-such-branch>'\n    --discard-changes     throw away local modifications\n    -q, --quiet           suppress progress reporting\n    --recurse-submodules[=<checkout>]\n                          control recursive updating of submodules\n    --progress            force progress reporting\n    -m, --merge           perform a 3-way merge with the new branch\n    --conflict <style>    conflict style (merge, diff3, or zdiff3)\n    -d, --detach          detach HEAD at named commit\n    -t, --track[=(direct|inherit)]\n                          set branch tracking configuration\n    -f, --force           force checkout (throw away local modifications)\n    --orphan <new-branch> new unparented branch\n    --overwrite-ignore    update ignored files (default)\n    --ignore-other-worktrees\n                          do not check if another worktree is holding the given ref";

pub(crate) fn fatal(e: impl std::fmt::Display) -> CliError {
    CliError::Fatal(e.to_string())
}

// ---------------------------------------------------------------------------------- options

/// How a new branch is told to track its start point.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub(crate) enum TrackOpt {
    /// `branch.autoSetupMerge` decides.
    #[default]
    Auto,
    Direct,
    Inherit,
    No,
}

/// The options `switch` and `checkout` share.
#[derive(Clone, Debug, Default)]
pub(crate) struct Opts {
    /// `-c` / `-b`.
    pub create: Option<String>,
    /// `-C` / `-B`.
    pub force_create: Option<String>,
    pub orphan: Option<String>,
    pub detach: bool,
    pub force: bool,
    pub quiet: bool,
    pub track: TrackOpt,
    pub guess: Option<bool>,
    pub ignore_other_worktrees: bool,
}

/// Which command parses.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Cmd {
    Switch,
    Checkout,
}

/// What the command line holds besides the options.
pub(crate) struct Parsed {
    pub opts: Opts,
    /// The words before `--` (or all of them without one).
    pub words: Vec<String>,
    /// `Some` when `--` was given: the words after it.
    pub after: Option<Vec<String>>,
}

fn track_mode(value: Option<&str>) -> Result<TrackOpt> {
    match value {
        None | Some("direct") => Ok(TrackOpt::Direct),
        Some("inherit") => Ok(TrackOpt::Inherit),
        Some(other) => Err(CliError::Fatal(format!("malformed value for --track: {other}"))),
    }
}

/// Parses `args` for `cmd`. Clustered short flags and attached values (`-cfoo`) work as in git.
pub(crate) fn parse(args: &[OsString], cmd: Cmd, usage: &'static str) -> Result<Parsed> {
    let mut cur = Cursor::new(args, usage);
    let mut o = Opts::default();
    let mut words = Vec::new();
    let mut after = None;
    while let Some(word) = cur.next() {
        match word {
            Word::End => {
                after = Some(cur.rest());
            }
            Word::Operand(w) => words.push(w),
            Word::Short(flags) => {
                let mut chars = flags.chars();
                while let Some(c) = chars.next() {
                    let value = |name: &str, chars: &mut std::str::Chars<'_>, cur: &mut Cursor| {
                        let rest: String = chars.by_ref().collect();
                        if rest.is_empty() { cur.value(name, None) } else { Ok(rest) }
                    };
                    match (c, cmd) {
                        ('c', Cmd::Switch) | ('b', Cmd::Checkout) => {
                            o.create = Some(value(&c.to_string(), &mut chars, &mut cur)?);
                        }
                        ('C', Cmd::Switch) | ('B', Cmd::Checkout) => {
                            o.force_create = Some(value(&c.to_string(), &mut chars, &mut cur)?);
                        }
                        ('d', _) => o.detach = true,
                        ('f', _) => o.force = true,
                        ('q', _) => o.quiet = true,
                        ('t', _) => o.track = TrackOpt::Direct,
                        (other, _) => return Err(cur.usage_error(&format!("unknown switch `{other}'"))),
                    }
                }
            }
            Word::Long(name, inline) => match name.as_str() {
                "create" if cmd == Cmd::Switch => o.create = Some(cur.value("create", inline)?),
                "force-create" if cmd == Cmd::Switch => {
                    o.force_create = Some(cur.value("force-create", inline)?);
                }
                "detach" => o.detach = true,
                "force" | "discard-changes" => o.force = true,
                "quiet" => o.quiet = true,
                "track" => o.track = track_mode(inline.as_deref())?,
                "no-track" => o.track = TrackOpt::No,
                "guess" => o.guess = Some(true),
                "no-guess" => o.guess = Some(false),
                "orphan" => o.orphan = Some(cur.value("orphan", inline)?),
                "ignore-other-worktrees" => o.ignore_other_worktrees = true,
                "progress" | "no-progress" => {}
                other => return Err(cur.usage_error(&format!("unknown option `{other}'"))),
            },
        }
    }
    Ok(Parsed { opts: o, words, after })
}

// ---------------------------------------------------------------------------------- the move

/// The commit a start point names, with how it was named.
#[derive(Clone, Debug)]
pub(crate) struct Rev {
    /// The text as typed (for `Note: switching to '<label>'`).
    pub label: String,
    pub commit: ChangesetId,
    pub kind: RevKind,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum RevKind {
    /// `refs/heads/<name>`.
    Branch(String),
    /// The full name of a remote-tracking ref.
    Remote(String),
    Tag,
    Other,
}

/// Where HEAD goes.
#[derive(Clone, Debug)]
pub(crate) enum Dest {
    /// A branch that exists (`tip` `None`: the unborn current branch).
    Existing { branch: String, tip: Option<ChangesetId> },
    /// A new branch (`force`: reset it when it exists).
    Create { branch: String, start: Option<Rev>, force: bool },
    Detach { commit: ChangesetId },
    Orphan { branch: String },
}

/// One head move.
#[derive(Clone, Debug)]
pub(crate) struct Move {
    pub dest: Dest,
    pub opts: Opts,
    /// The word typed for the destination (`Note: switching to '<typed>'`).
    pub typed: String,
    /// `checkout --orphan` keeps the index and the files; `switch --orphan` empties them.
    pub keep_tree: bool,
}

fn heads(name: &str) -> String {
    format!("refs/heads/{name}")
}

pub(crate) fn ref_name(full: &str) -> Result<RefName> {
    RefName::new(full.as_bytes().to_vec()).map_err(fatal)
}

/// The target of `full`, when the ref exists.
pub(crate) async fn ref_target(ws: &WsCtx, full: &str) -> Result<Option<RefTarget>> {
    let Ok(name) = RefName::new(full.as_bytes().to_vec()) else { return Ok(None) };
    Ok(ws.db.refs().get(ws.repo.key, &name).await?.map(|e| e.target))
}

/// The commit `refs/heads/<name>` is at.
pub(crate) async fn branch_tip(ws: &WsCtx, name: &str) -> Result<Option<ChangesetId>> {
    Ok(match ref_target(ws, &heads(name)).await? {
        Some(RefTarget::Changeset(c)) => Some(c),
        _ => None,
    })
}

/// The commit a ref target leads to.
pub(crate) async fn peel(ws: &WsCtx, target: RefTarget) -> Result<ChangesetId> {
    match target {
        RefTarget::Changeset(c) => Ok(c),
        RefTarget::Tag(t) => Ok(super::log::refs::tag_record(ws, t).await?.target),
    }
}

/// Makes sure the git map knows `commit`'s git id. The chokepoint and the `reference-transaction`
/// hook read it back, and a changeset nothing has exported yet has no row.
pub(crate) async fn ensure_mapped(ws: &WsCtx, commit: ChangesetId) -> Result<GitOid> {
    let git = ws.engine.git_commit_of(commit).await.map_err(fatal)?;
    let row = asc_engine::derive::GitMapRow { git, kind: ObjectKind::Commit, asc: *commit.as_bytes() };
    git_map::insert_many(&ws.db, ws.repo.key, &[row]).await?;
    Ok(git)
}

/// Abbreviation length: `core.abbrev`, default 7.
pub(crate) fn abbrev_len(ctx: &Ctx) -> usize {
    ctx.config.get("core.abbrev").and_then(|v| v.parse::<usize>().ok()).map_or(7, |n| n.clamp(4, 40))
}

/// `<abbrev> <subject>` of a commit, as `HEAD is now at …` prints it.
pub(crate) async fn commit_line(ctx: &Ctx, ws: &WsCtx, cs: ChangesetId) -> Result<String> {
    let git = ws.engine.git_commit_of(cs).await.map_err(fatal)?;
    let message = ws.engine.get_changeset(cs).await?.message.clone();
    let text = String::from_utf8_lossy(&message).into_owned();
    let subject: Vec<&str> = text.lines().take_while(|l| !l.trim().is_empty()).collect();
    Ok(format!("{} {}", &git.to_string()[..abbrev_len(ctx)], subject.join(" ")))
}

/// The path (with `/`) of another workspace that has `branch` checked out.
pub(crate) async fn held_elsewhere(ws: &WsCtx, branch: &str) -> Result<Option<String>> {
    Ok(match workspaces::holding_branch(&ws.db, ws.repo.key, branch).await? {
        Some(w) if w.ws.id != ws.workspace.ws.id => Some(w.ws.path.replace('\\', "/")),
        _ => None,
    })
}

/// The path of any workspace (this one included) that has `branch` checked out.
pub(crate) async fn held_anywhere(ws: &WsCtx, branch: &str) -> Result<Option<String>> {
    Ok(workspaces::holding_branch(&ws.db, ws.repo.key, branch)
        .await?
        .map(|w| w.ws.path.replace('\\', "/")))
}

/// Whether `name` can name a branch (`git check-ref-format --branch`).
pub(crate) fn valid_branch_name(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('-')
        && name != "HEAD"
        && RefName::new(heads(name).into_bytes()).is_ok()
}

/// `fatal: '<name>' is not a valid branch name` with git's hint.
pub(crate) fn bad_branch_name(ctx: &Ctx, name: &str) -> CliError {
    let mut text = format!("fatal: '{name}' is not a valid branch name");
    if ctx.config.bool("advice.refSyntax") != Some(false) {
        text.push_str(&format!(
            "\nhint: See '{} help check-ref-format'\nhint: Disable this message with \"{} config set advice.refSyntax false\"",
            tool(ctx),
            tool(ctx)
        ));
    }
    CliError::Raw(text, 128)
}

// -------------------------------------------------------------------------- previous position

/// Where HEAD was before the last switch.
pub(crate) enum Prev {
    Branch(String),
    Commit(GitOid),
}

fn prev_path(ws: &WsCtx) -> PathBuf {
    ws.projection.git_dir().join(PREV_FILE)
}

pub(crate) fn read_prev(ws: &WsCtx) -> Option<Prev> {
    let text = fs::read_to_string(prev_path(ws)).ok()?;
    let line = text.lines().next()?;
    match line.split_once(' ')? {
        ("branch", name) if valid_branch_name(name) => Some(Prev::Branch(name.to_owned())),
        ("commit", sha) => sha.parse().ok().map(Prev::Commit),
        _ => None,
    }
}

fn write_prev(ws: &WsCtx, head: &Head) {
    let line = match head {
        Head::Branch(b) => format!("branch {b}\n"),
        Head::Detached(oid) => format!("commit {oid}\n"),
    };
    // Best effort: losing it only makes `-` report "invalid reference".
    let _ = fs::write(prev_path(ws), line);
}

// ------------------------------------------------------------------------ revision lookup

/// `word` as a start point: a branch, a tag, a remote-tracking branch, or any revision. `-` is
/// the position before the last switch.
pub(crate) async fn resolve_rev(ctx: &Ctx, ws: &WsCtx, word: &str) -> Result<Option<Rev>> {
    let label = word.to_owned();
    if word == "-" {
        return Ok(match read_prev(ws) {
            Some(Prev::Branch(name)) => branch_tip(ws, &name)
                .await?
                .map(|commit| Rev { label: name.clone(), commit, kind: RevKind::Branch(name) }),
            Some(Prev::Commit(oid)) => git_map::git_to_asc(&ws.db, ws.repo.key, oid, ObjectKind::Commit)
                .await?
                .map(|asc| Rev { label: oid.to_string(), commit: ChangesetId::from_bytes(asc), kind: RevKind::Other }),
            None => None,
        });
    }
    if !word.is_empty() && !word.contains(['~', '^', '@', ':']) {
        for (prefix, kind) in [("refs/heads/", 0u8), ("refs/tags/", 1), ("refs/remotes/", 2)] {
            let full = format!("{prefix}{word}");
            if let Some(target) = ref_target(ws, &full).await? {
                let commit = peel(ws, target).await?;
                let kind = match kind {
                    0 => RevKind::Branch(word.to_owned()),
                    1 => RevKind::Tag,
                    _ => RevKind::Remote(full),
                };
                return Ok(Some(Rev { label, commit, kind }));
            }
        }
    }
    Ok(commit_of(ctx, word).await?.map(|commit| Rev { label, commit, kind: RevKind::Other }))
}

/// The remote-tracking refs `refs/remotes/<remote>/<name>` that exist, as `(remote, full)`.
pub(crate) async fn remote_matches(ws: &WsCtx, name: &str) -> Result<Vec<(String, String)>> {
    let mut out = Vec::new();
    for entry in ws.db.refs().list(ws.repo.key, "refs/remotes/").await? {
        let full = String::from_utf8_lossy(entry.name.as_bytes()).into_owned();
        let Some(rest) = full.strip_prefix("refs/remotes/") else { continue };
        if let Some((remote, branch)) = rest.split_once('/')
            && branch == name
        {
            out.push((remote.to_owned(), full.clone()));
        }
    }
    Ok(out)
}

/// `checkout.defaultRemote` aside, the one remote-tracking branch named `name`, if exactly one
/// remote has it. More than one is git's "matched multiple" error.
pub(crate) async fn guess_remote(ctx: &Ctx, ws: &WsCtx, name: &str) -> Result<Option<Rev>> {
    let found = remote_matches(ws, name).await?;
    let pick = match found.as_slice() {
        [] => return Ok(None),
        [one] => one.clone(),
        many => {
            let preferred = ctx.config.get("checkout.defaultRemote");
            match many.iter().find(|(r, _)| Some(r) == preferred.as_ref()) {
                Some(p) => p.clone(),
                None => {
                    let mut text = String::new();
                    if ctx.config.bool("advice.checkoutAmbiguousRemoteBranchName") != Some(false) {
                        let bin = tool(ctx);
                        text.push_str(&format!(
                            "hint: If you meant to check out a remote tracking branch on, e.g. 'origin',\n\
                             hint: you can do so by fully qualifying the name with the --track option:\n\
                             hint:\n\
                             hint:     {bin} checkout --track origin/<name>\n\
                             hint:\n\
                             hint: If you'd like to always have checkouts of an ambiguous <name> prefer\n\
                             hint: one remote, e.g. the 'origin' remote, consider setting\n\
                             hint: checkout.defaultRemote=origin in your config.\n"
                        ));
                    }
                    text.push_str(&format!(
                        "fatal: '{name}' matched multiple ({}) remote tracking branches",
                        many.len()
                    ));
                    return Err(CliError::Raw(text, 128));
                }
            }
        }
    };
    let Some(target) = ref_target(ws, &pick.1).await? else { return Ok(None) };
    let commit = peel(ws, target).await?;
    Ok(Some(Rev { label: format!("{}/{name}", pick.0), commit, kind: RevKind::Remote(pick.1) }))
}

// ------------------------------------------------------------------------------ tree moves

/// Stat data that never matches a file, so every probed path is hashed.
struct ColdStat;

impl StatSource for ColdStat {
    fn stat(&self, _: &RepoPath, entry: &Entry) -> FileStat {
        FileStat { size: entry.size as u32, ..FileStat::default() }
    }
}

/// A scratch index file that is removed when dropped.
struct Scratch(PathBuf);

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
        let mut lock = self.0.clone().into_os_string();
        lock.push(".lock");
        let _ = fs::remove_file(lock);
    }
}

fn leaf_of_change(change: &Change) -> Option<Leaf> {
    match change {
        Change::Write { kind, file, size, git, .. } => {
            Some(Leaf { kind: *kind, file: *file, size: *size, git: *git })
        }
        Change::Gitlink { commit } => {
            Some(Leaf { kind: Kind::Gitlink, file: FileId::from_bytes([0; 32]), size: 0, git: *commit })
        }
        Change::Delete => None,
    }
}

/// Whether the row stages exactly `leaf` (`None`: a staged delete).
fn row_is(row: &StageRow, leaf: Option<&Leaf>) -> bool {
    if row.stage != 0 || row.intent_to_add {
        return false;
    }
    match (row.file, leaf) {
        (None, None) => true,
        (Some(f), Some(l)) => f == l.file && row.kind == l.kind && row.size == l.size && row.git == Some(l.git),
        _ => false,
    }
}

fn leaf_of_entry(e: &Entry) -> Leaf {
    Leaf { kind: e.kind, file: FileId::from_bytes(e.id), size: e.size, git: e.git }
}

/// The tree entry a write change makes.
fn entry_of_change(path: &RepoPath, change: &Change) -> Option<Entry> {
    match change {
        Change::Write { kind, file, size, git, .. } => Some(Entry {
            name: path.file_name(),
            kind: *kind,
            id: *file.as_bytes(),
            size: *size,
            git: *git,
        }),
        Change::Gitlink { commit } => Some(Entry {
            name: path.file_name(),
            kind: Kind::Gitlink,
            id: [0; 32],
            size: 0,
            git: *commit,
        }),
        Change::Delete => None,
    }
}

fn path_text(p: &RepoPath) -> String {
    String::from_utf8_lossy(p.as_bytes()).into_owned()
}

/// git's refusal text for local changes (`tracked`) and untracked files in the way.
fn would_overwrite(tracked: &[RepoPath], untracked: &[RepoPath]) -> CliError {
    let mut text = String::new();
    if !tracked.is_empty() {
        text.push_str("error: Your local changes to the following files would be overwritten by checkout:\n");
        for p in tracked {
            text.push_str(&format!("\t{}\n", path_text(p)));
        }
        text.push_str("Please commit your changes or stash them before you switch branches.\n");
    }
    if !untracked.is_empty() {
        text.push_str("error: The following untracked working tree files would be overwritten by checkout:\n");
        for p in untracked {
            text.push_str(&format!("\t{}\n", path_text(p)));
        }
        text.push_str("Please move or remove them before you switch branches.\n");
    }
    text.push_str("Aborting");
    CliError::Raw(text, 1)
}

/// Moves the working tree from HEAD's tree to `target` without touching local changes. Returns
/// the stage rows the move leaves (the staged changes it carries across).
async fn safe_move(
    ctx: &Ctx,
    ws: &WsCtx,
    head_root: TreeId,
    target_root: TreeId,
    rows: Vec<StageRow>,
) -> Result<Vec<StageRow>> {
    if rows.iter().any(|r| r.stage > 0) {
        return Err(CliError::Raw("error: you need to resolve your current index first".into(), 1));
    }
    let diff = ws.engine.leafdiff(head_root, target_root, None).await?;
    if diff.is_empty() {
        return Ok(rows);
    }
    // What HEAD holds at each changed path: the change that turns the target back into HEAD.
    let back: HashMap<RepoPath, Change> =
        ws.engine.leafdiff(target_root, head_root, None).await?.into_iter().collect();
    let staged: HashMap<&[u8], &StageRow> = rows.iter().map(|r| (r.path.as_slice(), r)).collect();
    let mut touched: HashSet<Vec<u8>> = HashSet::new();
    let mut stale = Vec::new();
    let mut work: Vec<(RepoPath, Option<Change>, Change)> = Vec::new();
    for (path, to) in &diff {
        touched.insert(path.as_bytes().to_vec());
        match staged.get(path.as_bytes()) {
            // Staged exactly as the target has it: nothing to do at this path.
            Some(row) if row_is(row, leaf_of_change(to).as_ref()) => {}
            Some(_) => stale.push(path.clone()),
            None => {
                let from = back.get(path).filter(|c| !matches!(c, Change::Delete)).cloned();
                work.push((path.clone(), from, to.clone()));
            }
        }
    }
    if !stale.is_empty() {
        return Err(would_overwrite(&stale, &[]));
    }
    write_safe(ctx, ws, &work).await?;
    Ok(rows.into_iter().filter(|r| !touched.contains(&r.path)).collect())
}

/// Writes `work` (path, what HEAD holds, what the target holds) through `plain::checkout` in
/// `Safe` mode. The stat cache is a scratch index holding HEAD's entries at those paths with no
/// recorded stat, so every file is hashed and local edits are always seen.
async fn write_safe(ctx: &Ctx, ws: &WsCtx, work: &[(RepoPath, Option<Change>, Change)]) -> Result<()> {
    if work.is_empty() {
        return Ok(());
    }
    let empty = CliEngine::empty_tree_id();
    let (mut from, mut to, mut entries) = (Vec::new(), Vec::new(), Vec::new());
    let mut tracked: HashSet<&RepoPath> = HashSet::new();
    for (path, old, new) in work {
        if let Some(change) = old {
            from.push((path.clone(), change.clone()));
            if let Some(e) = entry_of_change(path, change) {
                entries.push((path.clone(), e));
                tracked.insert(path);
            }
        }
        if !matches!(new, Change::Delete) {
            to.push((path.clone(), new.clone()));
        }
    }
    let from_tree = ws.engine.apply(empty, &from).await?;
    let to_tree = ws.engine.apply(empty, &to).await?;
    let scratch = Scratch(ws.home.tmp().join(format!("switch-index-{}", std::process::id())));
    let rendered = asc_git::index::render(
        entries.into_iter(),
        &ColdStat,
        &asc_git::index::RenderOptions { cache_tree: false, ..Default::default() },
    );
    asc_git::index::write(&scratch.0, &rendered).map_err(fatal)?;
    let parts = WcParts::new(ctx, ws, scratch.0.clone())?;
    let wc = parts.wc(&ws.engine);
    match plain::checkout(&wc, from_tree, to_tree, Mode::Safe).await {
        Ok(report) => {
            for p in report.skipped.iter().take(10) {
                eprintln!(
                    "warning: asc: '{}' cannot be created on this file system and was not written",
                    path_text(p)
                );
            }
            Ok(())
        }
        Err(plain::Error::Conflicts(paths)) => {
            let (local, other): (Vec<_>, Vec<_>) = paths.into_iter().partition(|p| tracked.contains(p));
            Err(would_overwrite(&local, &other))
        }
        Err(e) => Err(fatal(e)),
    }
}

/// Every path a forced move to `target` rewrites: where the index and the target differ, every
/// modified or deleted tracked file, and every conflicted path.
async fn force_paths(ctx: &Ctx, index: TreeId, target: TreeId) -> Result<Vec<RepoPath>> {
    let ws = ctx.ws()?;
    let mut paths: std::collections::BTreeSet<RepoPath> =
        ws.engine.leafdiff(index, target, None).await?.into_iter().map(|(p, _)| p).collect();
    let parts = WcParts::new(ctx, ws, ws.projection.git_dir().join("index"))?;
    let st = status::compute(&parts.wc(&ws.engine), &ws.db, &ws.workspace).await.map_err(fatal)?;
    paths.extend(st.unstaged.into_iter().map(|c| c.path));
    paths.extend(st.unmerged);
    Ok(paths.into_iter().collect())
}

/// The rows that make the index hold the whole current index tree over an empty HEAD (the
/// state `checkout --orphan` leaves).
async fn rows_over_empty(ws: &WsCtx, index_root: TreeId, rows: &[StageRow]) -> Result<Vec<StageRow>> {
    let mut out: BTreeMap<(Vec<u8>, u8), StageRow> = BTreeMap::new();
    let mut walk = ws.engine.walk(index_root, None);
    while let Some((path, entry)) = walk.try_next().await? {
        if entry.kind == Kind::Dir {
            continue;
        }
        let row = StageRow::from_leaf(path.as_bytes().to_vec(), Some(&leaf_of_entry(&entry)));
        out.insert((row.path.clone(), 0), row);
    }
    for r in rows.iter().filter(|r| r.stage > 0 || r.intent_to_add) {
        out.insert((r.path.clone(), r.stage), r.clone());
    }
    Ok(out.into_values().collect())
}

// ------------------------------------------------------------------------------ head moves

/// HEAD as it was, to put back when a move fails before it commits.
pub(crate) struct Saved {
    pub(crate) head: Head,
    pub(crate) row_head: String,
    pub(crate) base: Option<ChangesetId>,
}

impl Saved {
    pub(crate) async fn capture(ws: &WsCtx) -> Result<Self> {
        let row = workspaces::get(&ws.db, ws.workspace.ws.id)
            .await?
            .ok_or_else(|| CliError::Fatal("asc: this workspace is not registered in state.db".into()))?;
        Ok(Self { head: ws.projection.read_head()?, row_head: row.ws.head.clone(), base: row.ws.base_cs })
    }
}

/// Writes HEAD and the workspace row's head name (the base commit stays; the chokepoint moves it).
pub(crate) async fn point_head(ws: &WsCtx, head: &Head, row_head: &str, base: Option<ChangesetId>) -> Result<()> {
    ws.projection.write_head(head)?;
    workspaces::update_head_base(&ws.db, ws.workspace.ws.id, row_head, base).await?;
    Ok(())
}

pub(crate) async fn restore_head(ws: &WsCtx, saved: &Saved) {
    let _ = ws.projection.write_head(&saved.head);
    let _ = workspaces::update_head_base(&ws.db, ws.workspace.ws.id, &saved.row_head, saved.base).await;
}

fn update(name: &str, expect: Expect, new: Option<RefTarget>) -> Result<RefUpdate> {
    Ok(RefUpdate { name: ref_name(name)?, expect, new, expires_at: None })
}

/// Every ref tip's commit, for the "commits left behind" check.
async fn all_tips(ws: &WsCtx) -> Result<Vec<ChangesetId>> {
    let mut out = Vec::new();
    for entry in ws.db.refs().list(ws.repo.key, "refs/").await? {
        out.push(peel(ws, entry.target).await?);
    }
    Ok(out)
}

/// What leaving a detached HEAD at `old` prints: git's warning about commits no ref reaches, or
/// `Previous HEAD position was …`.
async fn leaving_detached(ctx: &Ctx, ws: &WsCtx, old: ChangesetId, new: Option<ChangesetId>) -> Result<()> {
    let mut exclude = all_tips(ws).await?;
    exclude.extend(new);
    let graph = Graph::new(EngineSource::new(&ws.engine));
    let q = LogQuery { include: vec![old], exclude, limit: Some(ORPHAN_LIMIT + 1), ..LogQuery::default() };
    // A history boundary the local graph cannot cross only loses the warning, never the switch.
    let lost: Vec<ChangesetId> = graph.log(q).try_collect().await.unwrap_or_default();
    if lost.is_empty() {
        eprintln!("Previous HEAD position was {}", commit_line(ctx, ws, old).await?);
        return Ok(());
    }
    let n = lost.len().min(ORPHAN_LIMIT);
    let noun = if n == 1 { "commit" } else { "commits" };
    let mut text = format!("Warning: you are leaving {n} {noun} behind, not connected to\nany of your branches:\n\n");
    for cs in lost.iter().take(n) {
        text.push_str(&format!("  {}\n", commit_line(ctx, ws, *cs).await?));
    }
    text.push('\n');
    if ctx.config.bool("advice.detachedHead") != Some(false) {
        let git = ws.engine.git_commit_of(old).await.map_err(fatal)?.to_string();
        let (pronoun, verb) = if n == 1 { ("it", "") } else { ("them", "") };
        text.push_str(&format!(
            "If you want to keep {pronoun} by creating a new branch, this may be a good time\nto do so with:\n\n {} branch <new-branch-name> {}{verb}\n\n",
            tool(ctx),
            &git[..abbrev_len(ctx)]
        ));
    }
    eprint!("{text}");
    Ok(())
}

fn detach_advice(ctx: &Ctx, typed: &str) {
    let bin = tool(ctx);
    eprintln!(
        "Note: switching to '{typed}'.\n\n\
         You are in 'detached HEAD' state. You can look around, make experimental\n\
         changes and commit them, and you can discard any commits you make in this\n\
         state without impacting any branches by switching back to a branch.\n\n\
         If you want to create a new branch to retain commits you create, you may\n\
         do so (now or later) by using -c with the switch command. Example:\n\n  \
         {bin} switch -c <new-branch-name>\n\n\
         Or undo this operation with:\n\n  \
         {bin} switch -\n\n\
         Turn off this advice by setting config variable advice.detachedHead to false\n"
    );
}

// -------------------------------------------------------------------------------- upstream

/// The tracking ref `branch.<b>.remote` + `.merge` map to through the remote's fetch refspecs.
pub(crate) fn upstream_ref(ctx: &Ctx, branch: &str) -> Option<(String, String)> {
    let remote = ctx.config.get(&format!("branch.{branch}.remote"))?;
    let merge = ctx.config.get(&format!("branch.{branch}.merge"))?;
    let tracking = if remote == "." {
        merge.clone()
    } else {
        ctx.config.get_all(&format!("remote.{remote}.fetch")).iter().find_map(|spec| map_refspec(spec, &merge))?
    };
    let display = tracking
        .strip_prefix("refs/remotes/")
        .or_else(|| tracking.strip_prefix("refs/heads/"))
        .unwrap_or(&tracking)
        .to_owned();
    Some((display, tracking))
}

/// `src` mapped through one `[+]<src>:<dst>` refspec (a single `*` on each side).
pub(crate) fn map_refspec(spec: &str, name: &str) -> Option<String> {
    let spec = spec.strip_prefix('+').unwrap_or(spec);
    let (src, dst) = spec.split_once(':')?;
    match (src.split_once('*'), dst.split_once('*')) {
        (Some((head, tail)), Some((dst_head, dst_tail))) => {
            let middle = name.strip_prefix(head)?.strip_suffix(tail)?;
            Some(format!("{dst_head}{middle}{dst_tail}"))
        }
        (None, None) => (src == name).then(|| dst.to_owned()),
        _ => None,
    }
}

fn plural(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

/// The "Your branch is …" report of `branch` against its upstream (stdout), as `checkout` and
/// `switch` print it.
pub(crate) async fn report_tracking(ctx: &Ctx, ws: &WsCtx, branch: &str) -> Result<()> {
    let Some((name, tracking)) = upstream_ref(ctx, branch) else { return Ok(()) };
    let hints = ctx.config.bool("advice.statusHints") != Some(false);
    let bin = tool(ctx);
    let mut text = String::new();
    let mut hint = |s: &str| {
        if hints {
            text.push_str(&format!("  ({})\n", s.replace("\"git ", &format!("\"{bin} "))));
        }
    };
    let theirs = match ref_target(ws, &tracking).await? {
        Some(t) => Some(peel(ws, t).await?),
        None => None,
    };
    let ours = branch_tip(ws, branch).await?;
    let head = match (ours, theirs) {
        (_, None) => {
            hint("use \"git branch --unset-upstream\" to fixup");
            format!("Your branch is based on '{name}', but the upstream is gone.\n")
        }
        (None, Some(_)) => return Ok(()),
        (Some(ours), Some(theirs)) => {
            let graph = Graph::new(EngineSource::new(&ws.engine));
            let count = |inc, exc| LogQuery { include: vec![inc], exclude: vec![exc], ..LogQuery::default() };
            let ahead = graph.count(count(ours, theirs)).await.map_err(fatal)?;
            let behind = graph.count(count(theirs, ours)).await.map_err(fatal)?;
            match (ahead, behind) {
                (0, 0) => format!("Your branch is up to date with '{name}'.\n"),
                (a, 0) => {
                    hint("use \"git push\" to publish your local commits");
                    format!("Your branch is ahead of '{name}' by {}.\n", plural(a, "commit", "commits"))
                }
                (0, b) => {
                    hint("use \"git pull\" to update your local branch");
                    format!(
                        "Your branch is behind '{name}' by {}, and can be fast-forwarded.\n",
                        plural(b, "commit", "commits")
                    )
                }
                (a, b) => {
                    hint("use \"git pull\" if you want to integrate the remote branch with yours");
                    format!(
                        "Your branch and '{name}' have diverged,\nand have {a} and {b} different commits each, respectively.\n"
                    )
                }
            }
        }
    };
    print!("{head}{text}");
    Ok(())
}

/// Sets `branch.<name>.remote/merge` through `asc config`'s own writer.
pub(crate) async fn set_config(ctx: &Ctx, key: &str, value: &str) -> Result<()> {
    let rest = vec![OsString::from("--local"), OsString::from(key), OsString::from(value)];
    super::config::run(ctx, super::config::Args { rest }).await?;
    Ok(())
}

/// Removes a config key; a missing key is not an error.
pub(crate) async fn unset_config(ctx: &Ctx, key: &str) {
    let rest = vec![OsString::from("--local"), OsString::from("--unset-all"), OsString::from(key)];
    let _ = super::config::run(ctx, super::config::Args { rest }).await;
}

/// Removes a whole `branch.<name>` section; a missing section is not an error.
pub(crate) async fn remove_branch_section(ctx: &Ctx, name: &str) {
    let rest = vec![
        OsString::from("--local"),
        OsString::from("--remove-section"),
        OsString::from(format!("branch.{name}")),
    ];
    let _ = super::config::run(ctx, super::config::Args { rest }).await;
}

/// Records that `branch` tracks `start` (git's `install_branch_config`) and says so. Returns the
/// display name and the ref the branch now tracks, when it does.
pub(crate) async fn track_start(
    ctx: &Ctx,
    branch: &str,
    start: &Rev,
    mode: TrackOpt,
    quiet: bool,
) -> Result<Option<(String, String)>> {
    let auto = ctx.config.get("branch.autoSetupMerge");
    let wanted = match (mode, &start.kind) {
        (TrackOpt::No, _) => false,
        (TrackOpt::Direct | TrackOpt::Inherit, _) => true,
        (TrackOpt::Auto, _) if auto.as_deref() == Some("false") => false,
        (TrackOpt::Auto, RevKind::Remote(_)) => true,
        (TrackOpt::Auto, RevKind::Branch(_)) => auto.as_deref() == Some("always"),
        (TrackOpt::Auto, _) => false,
    };
    if !wanted {
        return Ok(None);
    }
    let (remote, merge, shown, tracking) = match &start.kind {
        RevKind::Remote(full) => {
            let rest = full.strip_prefix("refs/remotes/").unwrap_or(full);
            let Some((remote, short)) = rest.split_once('/') else { return Ok(None) };
            let merge = ctx
                .config
                .get_all(&format!("remote.{remote}.fetch"))
                .iter()
                .find_map(|spec| reverse_refspec(spec, full))
                .unwrap_or_else(|| heads(short));
            (remote.to_owned(), merge, rest.to_owned(), full.clone())
        }
        RevKind::Branch(local) => {
            let inherited = (mode == TrackOpt::Inherit)
                .then(|| {
                    ctx.config
                        .get(&format!("branch.{local}.remote"))
                        .zip(ctx.config.get(&format!("branch.{local}.merge")))
                })
                .flatten();
            match inherited {
                Some((r, m)) => {
                    let tracking = upstream_ref(ctx, local).map_or_else(|| m.clone(), |(_, t)| t);
                    (r, m, local.clone(), tracking)
                }
                None => (".".to_owned(), heads(local), local.clone(), heads(local)),
            }
        }
        _ => {
            return Err(CliError::Fatal(format!(
                "cannot set up tracking information; starting point '{}' is not a branch",
                start.label
            )));
        }
    };
    set_config(ctx, &format!("branch.{branch}.remote"), &remote).await?;
    set_config(ctx, &format!("branch.{branch}.merge"), &merge).await?;
    if !quiet {
        println!("branch '{branch}' set up to track '{shown}'.");
    }
    Ok(Some((shown, tracking)))
}

/// The source side of the refspec whose destination is `tracking`.
pub(crate) fn reverse_refspec(spec: &str, tracking: &str) -> Option<String> {
    let spec = spec.strip_prefix('+').unwrap_or(spec);
    let (src, dst) = spec.split_once(':')?;
    match (src.split_once('*'), dst.split_once('*')) {
        (Some((head, tail)), Some((dst_head, dst_tail))) => {
            let middle = tracking.strip_prefix(dst_head)?.strip_suffix(dst_tail)?;
            Some(format!("{head}{middle}{tail}"))
        }
        (None, None) => (dst == tracking).then(|| src.to_owned()),
        _ => None,
    }
}

// ------------------------------------------------------------------------------- the move

/// Prints `<letter>\t<path>` for every path whose index or working file differs from `head`, as
/// git does after a switch that kept local changes.
async fn show_local_changes(ctx: &Ctx, ws: &WsCtx, head: Option<ChangesetId>) -> Result<()> {
    let head_root = root_of(ws, head).await?;
    let mut shown: BTreeMap<Vec<u8>, char> = BTreeMap::new();
    for row in stage::overlay(&ws.db, ws.workspace.ws.id, StageSet::Default).await? {
        if row.stage > 0 {
            shown.insert(row.path.clone(), 'U');
            continue;
        }
        let path = RepoPath::new(row.path.clone()).map_err(fatal)?;
        let in_head = ws.engine.lookup(head_root, &path).await?.is_some_and(|e| e.kind != Kind::Dir);
        let letter = match (row.file, in_head) {
            (None, _) => 'D',
            (Some(_), true) => 'M',
            (Some(_), false) => 'A',
        };
        shown.insert(row.path, letter);
    }
    let fresh = workspaces::get(&ws.db, ws.workspace.ws.id)
        .await?
        .ok_or_else(|| CliError::Fatal("asc: this workspace is not registered in state.db".into()))?;
    let parts = WcParts::new(ctx, ws, ws.projection.git_dir().join("index"))?;
    let st = status::compute(&parts.wc(&ws.engine), &ws.db, &fresh).await.map_err(fatal)?;
    for c in st.unstaged {
        shown.insert(c.path.as_bytes().to_vec(), c.kind.letter());
    }
    for (path, letter) in shown {
        println!("{letter}\t{}", String::from_utf8_lossy(&path));
    }
    Ok(())
}

/// Runs the move: checks, working tree, refs and HEAD, report, `post-checkout`.
pub(crate) async fn perform(ctx: &Ctx, m: Move) -> Result<ExitCode> {
    let ws = ctx.ws()?;
    let id = ws.workspace.ws.id;
    let saved = Saved::capture(ws).await?;
    let old_commit = ws.head_commit().await?;
    let quiet = m.opts.quiet;

    // Where the move ends.
    let (branch, target): (Option<String>, Option<ChangesetId>) = match &m.dest {
        Dest::Existing { branch, tip } => (Some(branch.clone()), *tip),
        Dest::Create { branch, start, .. } => (Some(branch.clone()), start.as_ref().map(|s| s.commit)),
        Dest::Detach { commit } => (None, Some(*commit)),
        Dest::Orphan { branch } => (Some(branch.clone()), None),
    };
    let mut existed = false;
    if let Some(b) = &branch {
        existed = branch_tip(ws, b).await?.is_some();
        if !m.opts.ignore_other_worktrees
            && let Some(path) = held_elsewhere(ws, b).await?
        {
            return Err(CliError::Fatal(match &m.dest {
                Dest::Create { force: true, .. } if existed => {
                    format!("cannot force update the branch '{b}' used by worktree at '{path}'")
                }
                _ => format!("'{b}' is already used by worktree at '{path}'"),
            }));
        }
        if existed && matches!(&m.dest, Dest::Create { force: false, .. } | Dest::Orphan { .. }) {
            return Err(CliError::Fatal(format!("a branch named '{b}' already exists")));
        }
    }

    let head_root = root_of(ws, old_commit).await?;
    let target_root = root_of(ws, target).await?;
    let (index_root, _) = index_tree(ws, head_root).await?;
    let rows = stage::overlay(&ws.db, id, StageSet::Default).await?;

    // The working tree and the rows the stage holds afterwards.
    let staging = if m.keep_tree {
        StagingAfterMove::Set(rows_over_empty(ws, index_root, &rows).await?)
    } else if m.opts.force {
        ext::op_sink().snapshot_wc();
        let paths = force_paths(ctx, index_root, target_root).await?;
        sync_worktree(ctx, target_root, &paths, true).await?;
        StagingAfterMove::Set(Vec::new())
    } else if old_commit == target {
        StagingAfterMove::Keep
    } else {
        StagingAfterMove::Set(safe_move(ctx, ws, head_root, target_root, rows).await?)
    };

    // Refs and HEAD.
    let target_git = match target {
        Some(t) => Some(ensure_mapped(ws, t).await?),
        None => None,
    };
    let old_label = match &saved.head {
        Head::Branch(b) => b.clone(),
        Head::Detached(_) => match old_commit {
            Some(c) => ws.engine.git_commit_of(c).await.map_err(fatal)?.to_string(),
            None => "HEAD".to_owned(),
        },
    };
    let mut updates = Vec::new();
    let reflog;
    match &m.dest {
        Dest::Existing { branch, .. } | Dest::Orphan { branch } => {
            point_head(ws, &Head::Branch(branch.clone()), &heads(branch), saved.base).await?;
            reflog = format!("checkout: moving from {old_label} to {branch}");
        }
        Dest::Create { branch, start, force } => {
            point_head(ws, &Head::Branch(branch.clone()), &heads(branch), saved.base).await?;
            if let Some(start) = start {
                let old = ref_target(ws, &heads(branch)).await?;
                let expect = match (old, *force) {
                    (Some(t), true) => Expect::Value(t),
                    _ => Expect::Absent,
                };
                if old != Some(RefTarget::Changeset(start.commit)) {
                    updates.push(update(&heads(branch), expect, Some(RefTarget::Changeset(start.commit)))?);
                }
                reflog = if existed {
                    format!("branch: Reset to {}", start.label)
                } else {
                    format!("branch: Created from {}", start.label)
                };
            } else {
                reflog = format!("checkout: moving from {old_label} to {branch}");
            }
        }
        Dest::Detach { commit } => {
            let oid = target_git.ok_or_else(|| fatal("asc: no git commit for the target"))?;
            point_head(ws, &Head::Detached(oid), "", saved.base).await?;
            let expect = old_commit.map_or(Expect::Absent, |o| Expect::Value(RefTarget::Changeset(o)));
            updates.push(RefUpdate {
                name: RefName::new_onelevel(b"HEAD".to_vec()).map_err(fatal)?,
                expect,
                new: Some(RefTarget::Changeset(*commit)),
                expires_at: None,
            });
            reflog = format!("checkout: moving from {old_label} to {}", m.typed);
        }
    }
    // Detaching where HEAD already is moves nothing.
    let in_place = matches!(m.dest, Dest::Detach { .. })
        && matches!(saved.head, Head::Detached(_))
        && old_commit == target;
    if !in_place {
        let txn = RefTxn { updates, actor: actor(), reason: String::new(), op_id: ext::OpId::new().0 };
        if let Err(e) = ctx.update_local_refs(txn, &reflog, staging).await {
            // Put HEAD back unless the move had already committed.
            let row = workspaces::get(&ws.db, id).await?;
            let committed = match &m.dest {
                Dest::Create { branch, start: Some(_), .. } => branch_tip(ws, branch).await? == target,
                _ => saved.base != target && row.is_some_and(|r| r.ws.base_cs == target),
            };
            if !committed {
                restore_head(ws, &saved).await;
            }
            return Err(e);
        }
    }
    write_prev(ws, &saved.head);

    // Tracking set up for a branch made from a remote-tracking start point.
    let fresh = match &m.dest {
        Dest::Create { branch, start: Some(start), .. } => {
            track_start(ctx, branch, start, m.opts.track, quiet).await?
        }
        _ => None,
    };

    // What the move carried across (git prints it as `diff-index --name-status`).
    if !quiet && !m.opts.force && !m.keep_tree && target.is_some() {
        show_local_changes(ctx, ws, target).await?;
    }

    // The report.
    if !quiet {
        if let (Head::Detached(_), Some(old)) = (&saved.head, old_commit)
            && target != Some(old)
        {
            leaving_detached(ctx, ws, old, target).await?;
        }
        match &m.dest {
            Dest::Detach { commit } => {
                if matches!(saved.head, Head::Branch(_))
                    && !m.opts.detach
                    && ctx.config.bool("advice.detachedHead") != Some(false)
                {
                    detach_advice(ctx, &m.typed);
                }
                eprintln!("HEAD is now at {}", commit_line(ctx, ws, *commit).await?);
            }
            Dest::Existing { branch: name, .. } | Dest::Create { branch: name, .. } | Dest::Orphan { branch: name } => {
                let same = matches!(&saved.head, Head::Branch(b) if b == name);
                match &m.dest {
                    Dest::Create { force: true, .. } if same => eprintln!("Reset branch '{name}'"),
                    Dest::Create { force: true, .. } if existed => eprintln!("Switched to and reset branch '{name}'"),
                    Dest::Create { .. } | Dest::Orphan { .. } => eprintln!("Switched to a new branch '{name}'"),
                    _ if same => eprintln!("Already on '{name}'"),
                    _ => eprintln!("Switched to branch '{name}'"),
                }
                // git reads the branch settings before it creates the tracking branch.
                if fresh.is_none() {
                    report_tracking(ctx, ws, name).await?;
                }
            }
        }
    }

    // `post-checkout`: its exit status is the command's.
    let old_git = match old_commit {
        Some(c) => ws.engine.git_commit_of(c).await.map_err(fatal)?,
        None => null_oid(),
    };
    let runner = Runner::for_ctx(ctx)?;
    let hook = Hook::PostCheckout { old: old_git, new: target_git.unwrap_or_else(null_oid), branch: true };
    match runner.run(hook, &HookEnv::of(ctx, "checkout")).await.map_err(fatal)? {
        Outcome::Exit(code) if code != 0 => Ok(ExitCode::from(u8::try_from(code).unwrap_or(1))),
        _ => Ok(ExitCode::SUCCESS),
    }
}

// ------------------------------------------------------------------------------ the command

/// git's refusal to switch branches in the middle of a merge, pick, revert, rebase or bisect.
fn refuse_mid_operation(ctx: &Ctx, ws: &WsCtx) -> Result<()> {
    let git = ws.projection.git_dir();
    let (what, quit) = if git.join("MERGE_HEAD").exists() {
        ("merging", "merge --quit")
    } else if git.join("CHERRY_PICK_HEAD").exists() {
        ("cherry-picking", "cherry-pick --quit")
    } else if git.join("REVERT_HEAD").exists() {
        ("reverting", "revert --quit")
    } else if git.join("rebase-merge").exists() || git.join("rebase-apply").exists() {
        ("rebasing", "rebase --quit")
    } else if git.join("BISECT_LOG").exists() {
        ("bisecting", "bisect reset")
    } else {
        return Ok(());
    };
    let bin = tool(ctx);
    Err(CliError::Fatal(format!(
        "cannot switch branch while {what}\nConsider \"{bin} {quit}\" or \"{bin} worktree add\"."
    )))
}

pub async fn run(ctx: &Ctx, a: Args) -> Result<ExitCode> {
    let ws = ctx.ws()?;
    let p = parse(&a.argv(), Cmd::Switch, USAGE_SWITCH)?;
    refuse_mid_operation(ctx, ws)?;
    let o = p.opts.clone();
    if p.after.is_some() {
        return Err(CliError::Fatal("only one reference expected".into()));
    }
    let given = [o.create.is_some(), o.force_create.is_some(), o.orphan.is_some()];
    if given.iter().filter(|g| **g).count() > 1 {
        return Err(CliError::Fatal("options '-c', '-C' and '--orphan' cannot be used together".into()));
    }
    if o.detach && (o.create.is_some() || o.force_create.is_some() || o.orphan.is_some()) {
        let name = if o.create.is_some() { "-c" } else if o.force_create.is_some() { "-C" } else { "--orphan" };
        return Err(CliError::Fatal(format!("options '--detach' and '{name}' cannot be used together")));
    }
    let guess = o.guess.or_else(|| ctx.config.bool("checkout.guess")).unwrap_or(true);

    // `--orphan <new>`.
    if let Some(name) = o.orphan.clone() {
        let name = &name;
        if !p.words.is_empty() {
            return Err(CliError::Fatal("'--orphan' cannot take <start-point>".into()));
        }
        if !valid_branch_name(name) {
            return Err(bad_branch_name(ctx, name));
        }
        let mv = Move { dest: Dest::Orphan { branch: name.clone() }, opts: o, typed: name.clone(), keep_tree: false };
        return perform(ctx, mv).await;
    }

    // `-c` / `-C <new> [<start>]`.
    if let Some((name, force)) = o.create.clone().map(|n| (n, false)).or_else(|| o.force_create.clone().map(|n| (n, true))) {
        if p.words.len() > 1 {
            return Err(CliError::Fatal("only one reference expected".into()));
        }
        if !valid_branch_name(&name) {
            return Err(bad_branch_name(ctx, &name));
        }
        let start = match p.words.first() {
            Some(w) => Some(resolve_start(ctx, ws, w).await?),
            None => ws.head_commit().await?.map(|c| Rev { label: "HEAD".into(), commit: c, kind: RevKind::Other }),
        };
        let mv = Move { dest: Dest::Create { branch: name.clone(), start, force }, opts: o, typed: name, keep_tree: false };
        return perform(ctx, mv).await;
    }

    // `--detach [<commit>]`, or `<branch>`.
    match (o.detach, p.words.as_slice()) {
        (_, []) if !o.detach => Err(CliError::Fatal("missing branch or commit argument".into())),
        (true, words) if words.len() <= 1 => {
            let word = words.first().map_or("HEAD", String::as_str);
            let rev = resolve_start(ctx, ws, word).await?;
            let mv = Move { dest: Dest::Detach { commit: rev.commit }, opts: o, typed: word.to_owned(), keep_tree: false };
            perform(ctx, mv).await
        }
        (_, [word]) => switch_to(ctx, ws, word, o, guess).await,
        _ => Err(CliError::Fatal("only one reference expected".into())),
    }
}

/// `word` as a revision, or git's "invalid reference".
async fn resolve_start(ctx: &Ctx, ws: &WsCtx, word: &str) -> Result<Rev> {
    resolve_rev(ctx, ws, word)
        .await?
        .ok_or_else(|| CliError::Fatal(format!("invalid reference: {}", if word == "-" { "@{-1}" } else { word })))
}

/// `switch <word>`: a branch, a remote branch to create locally, or the error for a non-branch.
async fn switch_to(ctx: &Ctx, ws: &WsCtx, word: &str, o: Opts, guess: bool) -> Result<ExitCode> {
    let rev = match resolve_rev(ctx, ws, word).await? {
        Some(r) => r,
        None => {
            if guess
                && valid_branch_name(word)
                && let Some(remote) = guess_remote(ctx, ws, word).await?
            {
                return create_tracking(ctx, word, remote, o).await;
            }
            return Err(CliError::Fatal(format!("invalid reference: {}", if word == "-" { "@{-1}" } else { word })));
        }
    };
    match rev.kind {
        RevKind::Branch(ref name) => {
            let mv = Move {
                dest: Dest::Existing { branch: name.clone(), tip: Some(rev.commit) },
                opts: o,
                typed: word.to_owned(),
                keep_tree: false,
            };
            perform(ctx, mv).await
        }
        ref kind => {
            if guess
                && matches!(kind, RevKind::Other)
                && valid_branch_name(word)
                && let Some(remote) = guess_remote(ctx, ws, word).await?
            {
                return create_tracking(ctx, word, remote, o).await;
            }
            let named = |w: &str| w.starts_with("refs/heads/") || matches!(w, "HEAD" | "@");
            let what = match kind {
                RevKind::Tag => format!("tag '{word}'"),
                RevKind::Remote(_) => format!("remote branch '{word}'"),
                _ if named(word) => match ws.projection.read_head()? {
                    Head::Branch(b) if matches!(word, "HEAD" | "@") => format!("'refs/heads/{b}'"),
                    _ => format!("'{word}'"),
                },
                _ => format!("commit '{word}'"),
            };
            let mut text = format!("fatal: a branch is expected, got {what}");
            if ctx.config.bool("advice.suggestDetachingHead") != Some(false) {
                text.push_str("\nhint: If you want to detach HEAD at the commit, try again with the --detach option.");
            }
            Err(CliError::Raw(text, 128))
        }
    }
}

/// Creates local branch `name` from a remote-tracking branch and switches to it.
pub(crate) async fn create_tracking(ctx: &Ctx, name: &str, remote: Rev, o: Opts) -> Result<ExitCode> {
    let mv = Move {
        dest: Dest::Create { branch: name.to_owned(), start: Some(remote), force: false },
        opts: o,
        typed: name.to_owned(),
        keep_tree: false,
    };
    perform(ctx, mv).await
}

// ------------------------------------------------------------------------------- unit tests

#[cfg(test)]
mod tests {
    use super::*;

    fn os(items: &[&str]) -> Vec<OsString> {
        items.iter().map(OsString::from).collect()
    }

    #[test]
    fn parses_clusters_and_attached_values() {
        let p = parse(&os(&["-fqcnew", "main"]), Cmd::Switch, "u").unwrap();
        assert!(p.opts.force && p.opts.quiet);
        assert_eq!(p.opts.create.as_deref(), Some("new"));
        assert_eq!(p.words, ["main"]);
        let p = parse(&os(&["-b", "topic", "--", "a", "b"]), Cmd::Checkout, "u").unwrap();
        assert_eq!(p.opts.create.as_deref(), Some("topic"));
        assert_eq!(p.after.unwrap(), ["a", "b"]);
    }

    #[test]
    fn track_values() {
        assert_eq!(track_mode(None).unwrap(), TrackOpt::Direct);
        assert_eq!(track_mode(Some("inherit")).unwrap(), TrackOpt::Inherit);
        assert!(track_mode(Some("nope")).is_err());
    }

    #[test]
    fn refspecs_map_both_ways() {
        let spec = "+refs/heads/*:refs/remotes/origin/*";
        assert_eq!(map_refspec(spec, "refs/heads/a/b").as_deref(), Some("refs/remotes/origin/a/b"));
        assert_eq!(reverse_refspec(spec, "refs/remotes/origin/a").as_deref(), Some("refs/heads/a"));
        assert_eq!(map_refspec(spec, "refs/tags/x"), None);
    }

    #[test]
    fn branch_names() {
        assert!(valid_branch_name("feature/x"));
        assert!(!valid_branch_name("-x"));
        assert!(!valid_branch_name("HEAD"));
        assert!(!valid_branch_name("a..b"));
        assert!(!valid_branch_name("bad name"));
    }

    #[test]
    fn overwrite_text_lists_both_kinds() {
        let p = |s: &str| RepoPath::new(s).unwrap();
        let CliError::Raw(text, code) = would_overwrite(&[p("f")], &[p("g")]) else { panic!() };
        assert_eq!(code, 1);
        assert!(text.starts_with("error: Your local changes"));
        assert!(text.contains("\tf\n") && text.contains("\tg\n") && text.ends_with("Aborting"));
    }
}
