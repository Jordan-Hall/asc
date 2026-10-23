//! `asc rebase` (SPEC §4.4): `rebase [-i] [--onto <newbase>] [<upstream> [<branch>]]` with
//! `--continue`, `--skip`, `--abort`, `--quit`, `--edit-todo`, `--autostash`, `--exec`,
//! `--autosquash`, `--update-refs`, `--rebase-merges`, `--keep-empty`, `--empty`, `--no-verify`,
//! `--force-rebase`, `-q` and `-v`.
//!
//! The state is git's `rebase-merge/` directory, written as git 2.55 writes it for a rebase on the
//! sequencer (`head-name`, `onto`, `orig-head`, `end`, `msgnum`, `done`, `git-rebase-todo` and its
//! `.backup`, `message`, `author-script`, `patch`, `stopped-sha`, `amend`, `rewritten-list`,
//! `rewritten-pending`, `message-squash`, `message-fixup`, `current-fixups`, `update-refs`,
//! `interactive`, `autostash`), plus `REBASE_HEAD`, `ORIG_HEAD`, `MERGE_MSG`, `SQUASH_MSG` and
//! `AUTO_MERGE`. Every file is untrusted input when a command resumes: size-capped, parsed
//! strictly, ids looked up in the repository before use.
//!
//! The todo list is git's (`rebase_todo.rs`): `pick reword edit squash fixup drop exec break label
//! reset merge update-ref noop`. Without `-i` the list is generated and run as it is; with `-i` it
//! goes through the todo editor first (`GIT_SEQUENCE_EDITOR`, `sequence.editor`, the commit
//! editor), spawned through `asc::child::command()`.
//!
//! A commit whose parent is already HEAD is kept as it is (git's fast-forward pick); a commit
//! whose paths are disjoint from what HEAD changed since its parent is rebuilt from metadata alone
//! (`rebase_metadata`, SPEC §4.4); every other commit goes through the engine's three-way merge.
//! `squash` and `fixup` amend HEAD with the merged tree and the message the `message-squash` file
//! collects. Refs move through `Ctx::update_local_refs`; HEAD stays detached until the last
//! command.
//!
//! Not served: `--root`, `--keep-base`, `--rebase-merges=rebase-cousins` (it acts as the default
//! mode), conflicts while recreating a merge (the rebase stops and `--abort` returns), octopus
//! merges, merge strategies other than `ort`, and git's patch-id detection of commits already
//! upstream (such a commit becomes empty on replay and is dropped, kept or stops the rebase as
//! `--empty` says, instead of being left out of the list up front). `pre-rebase`, `post-rewrite`
//! and, for the commits made through `git commit` in git (`reword`, `squash`, `fixup`),
//! `prepare-commit-msg`, `commit-msg` (reword only) and `post-commit` run; `post-checkout` runs on
//! the first and last head move. `--autostash` stores a conflicted stash under `refs/stash` when
//! that ref is free and under `refs/asc/autostash/<id>` otherwise.

use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{ExitCode, Stdio};

use asc_core::change::RewriteOp;
use asc_core::id::{ChangesetId, GitOid, RefTarget, TreeId};
use asc_core::object::{Change, Changeset};
use asc_core::path::{RefName, RepoPath};
use asc_core::sig::Signature;
use asc_engine::diff::format::write_summary;
use asc_engine::diff::{
    DiffCtx, DiffOptions, NoAttributes, RenameOptions, write_patch, write_shortstat,
};
use asc_engine::graph::LogQuery;
use asc_engine::history::HistoryApi;
use asc_engine::merge::{Labels, MergeOutcome};
use asc_git::projection::Head;
use asc_state::state::stage::{self, StageSet};
use asc_state::state::tables::ws_ops;
use asc_state::state::workspaces;
use asc_store::refs::{Expect, RefTxn, RefUpdate, Refs as _};
use asc_wc::status;

use super::commit::{ident, message, stagebuild};
use super::common::{CliEngine, CliError, Ctx, Result, StagingAfterMove, WsCtx};
use super::conflict::{self, NewCommit, Op as ReplayOp, Sides, fatal};
use super::rebase_todo::{self as todo, Commit, Fold, Item, Keep, MAX_TODO, Op};
use super::restore::{WcParts, actor, index_tree, root_of, sync_worktree, tool};
use super::seqstate;
use super::switch::{self, Dest, Move, RevKind};
use crate::ext::{self, OpId};
use crate::hooks_runner::{Gate, Hook, HookEnv, MsgSource, RewriteKind, Runner};
use crate::native::NativeSpec;

/// Longest `--exec` command accepted.
const MAX_LINE: usize = todo::MAX_LINE;

#[derive(clap::Parser, Debug)]
#[command(name = "rebase", disable_help_flag = true, disable_version_flag = true)]
pub struct Args {
    #[arg(long = "onto")]
    pub onto: Option<String>,
    #[arg(long = "continue")]
    pub cont: bool,
    #[arg(long = "skip")]
    pub skip: bool,
    #[arg(long = "abort")]
    pub abort: bool,
    #[arg(long = "quit")]
    pub quit: bool,
    #[arg(long = "edit-todo")]
    pub edit_todo: bool,
    #[arg(short = 'i', long = "interactive")]
    pub interactive: bool,
    #[arg(long = "autosquash")]
    pub autosquash: bool,
    #[arg(long = "no-autosquash")]
    pub no_autosquash: bool,
    #[arg(long = "update-refs")]
    pub update_refs: bool,
    #[arg(long = "no-update-refs")]
    pub no_update_refs: bool,
    #[arg(short = 'r', long = "rebase-merges", num_args = 0..=1, require_equals = true, default_missing_value = "no-rebase-cousins")]
    pub rebase_merges: Option<String>,
    #[arg(long = "no-rebase-merges")]
    pub no_rebase_merges: bool,
    #[arg(long = "keep-empty")]
    pub keep_empty: bool,
    #[arg(long = "no-keep-empty")]
    pub no_keep_empty: bool,
    #[arg(long = "empty")]
    pub empty: Option<String>,
    #[arg(long = "autostash")]
    pub autostash: bool,
    #[arg(long = "no-autostash")]
    pub no_autostash: bool,
    #[arg(short = 'x', long = "exec")]
    pub exec: Vec<String>,
    #[arg(short = 'q', long = "quiet")]
    pub quiet: bool,
    #[arg(short = 'v', long = "verbose")]
    pub verbose: bool,
    #[arg(long = "no-verify")]
    pub no_verify: bool,
    #[arg(long = "verify")]
    pub verify: bool,
    #[arg(short = 'f', long = "force-rebase", alias = "no-ff")]
    pub force: bool,
    #[arg(short = 'm', long = "merge")]
    pub merge: bool,
    pub words: Vec<OsString>,
}

pub const NATIVE: &[NativeSpec] = &[NativeSpec {
    cmd: "rebase",
    flags: &[
        "--onto",
        "--continue",
        "--skip",
        "--abort",
        "--quit",
        "--edit-todo",
        "-i",
        "--interactive",
        "--autosquash",
        "--no-autosquash",
        "--update-refs",
        "--no-update-refs",
        "-r",
        "--rebase-merges",
        "--no-rebase-merges",
        "--keep-empty",
        "--no-keep-empty",
        "--empty",
        "--autostash",
        "--no-autostash",
        "-x",
        "--exec",
        "-q",
        "--quiet",
        "-v",
        "--verbose",
        "--no-verify",
        "--verify",
        "-f",
        "--force-rebase",
        "--no-ff",
        "-m",
        "--merge",
    ],
    placeholders: &[],
}];

const USAGE: &str = "usage: git rebase [-i] [options] [--exec <cmd>] [--onto <newbase> | --keep-base] [<upstream> [<branch>]]
   or: git rebase [-i] [options] [--exec <cmd>] [--onto <newbase>] --root [<branch>]
   or: git rebase --continue | --abort | --skip | --edit-todo";

// ---------------------------------------------------------------- author-script

fn quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn unquote(value: &str) -> Option<String> {
    let inner = value.strip_prefix('\'')?;
    let mut out = String::new();
    let mut rest = inner;
    loop {
        let end = rest.find('\'')?;
        out.push_str(&rest[..end]);
        rest = &rest[end + 1..];
        if rest.is_empty() {
            return Some(out);
        }
        rest = rest.strip_prefix("\\''")?;
        out.push('\'');
    }
}

fn tz_text(min: i16) -> String {
    let sign = if min < 0 { '-' } else { '+' };
    let abs = min.unsigned_abs();
    format!("{sign}{:02}{:02}", abs / 60, abs % 60)
}

fn author_script(a: &Signature) -> String {
    format!(
        "GIT_AUTHOR_NAME={}\nGIT_AUTHOR_EMAIL={}\nGIT_AUTHOR_DATE={}\n",
        quote(&String::from_utf8_lossy(a.name())),
        quote(&String::from_utf8_lossy(a.email())),
        quote(&format!("@{} {}", a.time(), tz_text(a.tz_min()))),
    )
}

fn parse_author_script(text: &str) -> Result<Signature> {
    let bad = || fatal("asc: rebase-merge/author-script is malformed");
    let (mut name, mut email, mut date) = (None, None, None);
    for line in text.lines() {
        let (key, value) = line.split_once('=').ok_or_else(bad)?;
        let value = unquote(value).ok_or_else(bad)?;
        match key {
            "GIT_AUTHOR_NAME" => name = Some(value),
            "GIT_AUTHOR_EMAIL" => email = Some(value),
            "GIT_AUTHOR_DATE" => date = Some(value),
            _ => return Err(bad()),
        }
    }
    let (name, email, date) = (
        name.ok_or_else(bad)?,
        email.ok_or_else(bad)?,
        date.ok_or_else(bad)?,
    );
    let date = date.strip_prefix('@').ok_or_else(bad)?;
    let (secs, tz) = date.split_once(' ').ok_or_else(bad)?;
    let secs: i64 = secs.parse().map_err(|_| bad())?;
    let sign = match tz.as_bytes().first() {
        Some(b'+') => 1i16,
        Some(b'-') => -1,
        _ => return Err(bad()),
    };
    if tz.len() != 5 || !tz[1..].bytes().all(|b| b.is_ascii_digit()) {
        return Err(bad());
    }
    let hours: i16 = tz[1..3].parse().map_err(|_| bad())?;
    let mins: i16 = tz[3..5].parse().map_err(|_| bad())?;
    Signature::new(
        name.into_bytes(),
        email.into_bytes(),
        secs,
        sign * (hours * 60 + mins),
    )
    .map_err(|_| bad())
}

// ---------------------------------------------------------------- the state directory

/// What happens to a commit that turns empty on replay (`--empty`): git's files say which.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Empty {
    Drop,
    Keep,
    Stop,
}

/// A rebase in progress: what `rebase-merge/` holds.
struct Run {
    dir: PathBuf,
    gitdir: PathBuf,
    head_name: String,
    onto: GitOid,
    orig_head: GitOid,
    todo: Vec<Item>,
    done: Vec<Item>,
    end: usize,
    quiet: bool,
    verbose: bool,
    empty: Empty,
    /// Fast-forward picks whose parent is HEAD (off with `--force-rebase`, not persisted: git
    /// does not persist it either).
    allow_ff: bool,
    comment: u8,
    abbrev: usize,
}

fn state_dir(gitdir: &Path) -> PathBuf {
    gitdir.join("rebase-merge")
}

fn read_text(dir: &Path, name: &str) -> Result<Option<String>> {
    Ok(seqstate::read(dir, name)?.map(|b| String::from_utf8_lossy(&b).into_owned()))
}

fn need_text(dir: &Path, name: &str) -> Result<String> {
    read_text(dir, name)?.ok_or_else(|| fatal(format!("asc: rebase-merge/{name} is missing")))
}

fn need_oid(dir: &Path, name: &str) -> Result<GitOid> {
    need_text(dir, name)?
        .trim()
        .parse()
        .map_err(|_| fatal(format!("asc: rebase-merge/{name} holds a bad object id")))
}

/// git's refusal text for a todo list that does not parse.
fn parse_failure(ctx: &Ctx, errors: &[String], first_time: bool) -> CliError {
    let t = tool(ctx);
    let mut text = String::new();
    for e in errors {
        text.push_str(&format!("error: {e}\n"));
    }
    if first_time {
        text.push_str(&format!(
            "You can fix this with '{t} rebase --edit-todo' and then run '{t} rebase --continue'.\nOr you can abort the rebase with '{t} rebase --abort'."
        ));
    } else {
        text.push_str(&format!(
            "error: please fix this using '{t} rebase --edit-todo'."
        ));
    }
    CliError::Raw(text, 1)
}

/// The commits the state files of a rebase name in full: the pool an abbreviation in an edited
/// todo list is looked up in. Abbreviations of any other commit do not resolve here.
fn known_commits(dir: &Path) -> Vec<GitOid> {
    let mut out = Vec::new();
    for name in ["git-rebase-todo.backup", "done", "git-rebase-todo"] {
        let Ok(Some(text)) = read_text(dir, name) else {
            continue;
        };
        for t in todo::tokens(&text, b'#') {
            if t.len() == 40
                && let Ok(o) = t.parse::<GitOid>()
            {
                out.push(o);
            }
        }
    }
    out
}

/// The commit an abbreviated id names among `pool`, when exactly one matches.
fn by_prefix(name: &str, pool: &[GitOid]) -> Option<GitOid> {
    if name.len() < 4 || !name.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let lower = name.to_ascii_lowercase();
    let mut hits = pool.iter().filter(|o| o.to_string().starts_with(&lower));
    let first = hits.next()?;
    hits.all(|o| o == first).then_some(*first)
}

/// The commits a todo text names, each as the git id of a commit this repository knows.
async fn name_map(
    ctx: &Ctx,
    ws: &WsCtx,
    text: &str,
    pool: &[GitOid],
) -> Result<HashMap<String, GitOid>> {
    let mut map = HashMap::new();
    for name in todo::tokens(text, message::comment_char(ctx)) {
        if map.contains_key(&name) {
            continue;
        }
        let found = match name.parse::<GitOid>() {
            Ok(oid) => conflict::changeset_of(ws, oid).await.ok().map(|_| oid),
            Err(_) => match by_prefix(&name, pool) {
                Some(oid) => Some(oid),
                None => match commit_named(ctx, &name).await {
                    Ok(Some(id)) => Some(conflict::git_id(ws, id).await?),
                    _ => None,
                },
            },
        };
        if let Some(oid) = found {
            map.insert(name, oid);
        }
    }
    Ok(map)
}

/// Parses todo text; the error side is either the parse errors or a failure to look names up.
async fn parse_checked(
    ctx: &Ctx,
    ws: &WsCtx,
    dir: &Path,
    text: &str,
    check_fixups: bool,
) -> std::result::Result<Vec<Item>, Result<Vec<String>>> {
    let known = match name_map(ctx, ws, text, &known_commits(dir)).await {
        Ok(k) => k,
        Err(e) => return Err(Err(e)),
    };
    todo::parse(text, message::comment_char(ctx), &known, check_fixups).map_err(Ok)
}

/// A todo text the user edited: a `squash` or `fixup` needs a commit before it.
async fn parse_text(
    ctx: &Ctx,
    ws: &WsCtx,
    dir: &Path,
    text: &str,
) -> std::result::Result<Vec<Item>, Result<Vec<String>>> {
    parse_checked(ctx, ws, dir, text, true).await
}

/// A state file: what is left of a list may start with a `squash`.
async fn parse_state(
    ctx: &Ctx,
    ws: &WsCtx,
    dir: &Path,
    text: &str,
) -> std::result::Result<Vec<Item>, Result<Vec<String>>> {
    parse_checked(ctx, ws, dir, text, false).await
}

impl Run {
    /// Reads the state directory. `parse` is off for the commands that must work when the todo
    /// list does not parse (`--abort`, `--quit`, `--edit-todo`).
    async fn load(ctx: &Ctx, ws: &WsCtx, gitdir: &Path, parse: bool) -> Result<Self> {
        let dir = state_dir(gitdir);
        let head_name = need_text(&dir, "head-name")?.trim_end().to_owned();
        if head_name != "detached HEAD" && RefName::new(head_name.clone().into_bytes()).is_err() {
            return Err(fatal("asc: rebase-merge/head-name holds a bad ref name"));
        }
        let (mut todo_list, mut done) = (Vec::new(), Vec::new());
        if parse {
            let todo_text = read_text(&dir, "git-rebase-todo")?.unwrap_or_default();
            let done_text = read_text(&dir, "done")?.unwrap_or_default();
            todo_list = match parse_state(ctx, ws, &dir, &todo_text).await {
                Ok(l) => l,
                Err(Ok(errors)) => return Err(parse_failure(ctx, &errors, false)),
                Err(Err(e)) => return Err(e),
            };
            done = match parse_state(ctx, ws, &dir, &done_text).await {
                Ok(l) => l,
                Err(Ok(errors)) => return Err(parse_failure(ctx, &errors, false)),
                Err(Err(e)) => return Err(e),
            };
        }
        let end: usize = match read_text(&dir, "end")? {
            Some(t) => t
                .trim()
                .parse()
                .map_err(|_| fatal("asc: rebase-merge/end is not a number"))?,
            None if !parse => 0,
            None => return Err(fatal("asc: rebase-merge/end is missing")),
        };
        let empty = if seqstate::exists(&dir, "keep_redundant_commits") {
            Empty::Keep
        } else if seqstate::exists(&dir, "drop_redundant_commits") {
            Empty::Drop
        } else {
            Empty::Stop
        };
        Ok(Self {
            head_name,
            onto: need_oid(&dir, "onto")?,
            orig_head: need_oid(&dir, "orig-head")?,
            todo: todo_list,
            done,
            end,
            quiet: seqstate::exists(&dir, "quiet"),
            verbose: seqstate::exists(&dir, "verbose"),
            empty,
            allow_ff: true,
            comment: message::comment_char(ctx),
            abbrev: switch::abbrev_len(ctx),
            gitdir: gitdir.to_owned(),
            dir,
        })
    }

    fn write(&self, name: &str, text: &str) -> Result<()> {
        seqstate::write(&self.dir, name, text.as_bytes())
    }

    /// The files that change with every command.
    fn save_progress(&self) -> Result<()> {
        self.write("git-rebase-todo", &todo::render_full(&self.todo))?;
        self.write("done", &todo::render_full(&self.done))?;
        self.write("msgnum", &format!("{}\n", todo::count(&self.done)))
    }

    fn branch(&self) -> Option<&str> {
        self.head_name.strip_prefix("refs/heads/")
    }

    fn short(&self, oid: &GitOid) -> String {
        oid.to_string()[..self.abbrev.min(40)].to_owned()
    }

    fn clear_stop(&self) {
        for name in ["message", "author-script", "patch", "stopped-sha", "amend"] {
            seqstate::remove(&self.dir, name);
        }
        // REBASE_HEAD stays: the next pick removes it, and a stop on the last command leaves it.
        for name in ["MERGE_MSG", "AUTO_MERGE", "CHERRY_PICK_HEAD"] {
            seqstate::remove(&self.gitdir, name);
        }
    }

    /// Like [`Self::clear_stop`] for a stop that ends without a commit: no `git commit` ran, so
    /// `AUTO_MERGE` stays.
    fn clear_stop_uncommitted(&self) {
        for name in ["message", "author-script", "patch", "stopped-sha", "amend"] {
            seqstate::remove(&self.dir, name);
        }
        for name in ["MERGE_MSG", "CHERRY_PICK_HEAD"] {
            seqstate::remove(&self.gitdir, name);
        }
    }

    /// The last command that ran (the one a stop belongs to).
    fn current(&self) -> Option<&Item> {
        self.done.iter().rev().find(|i| i.counts())
    }

    fn next_op_folds(&self) -> bool {
        todo::peek(&self.todo, 0).is_some_and(|i| i.op.is_fixup())
    }

    /// git's `record_in_rewritten`: `old` joins the pending list; once the next command does not
    /// fold into HEAD, every pending commit maps to `new`.
    fn record_rewritten(&self, old: GitOid, new: GitOid) -> Result<()> {
        let mut pending = read_text(&self.dir, "rewritten-pending")?.unwrap_or_default();
        pending.push_str(&format!("{old}\n"));
        if self.next_op_folds() {
            return self.write("rewritten-pending", &pending);
        }
        let mut list = read_text(&self.dir, "rewritten-list")?.unwrap_or_default();
        for p in pending.lines().filter(|l| !l.is_empty()).take(MAX_TODO) {
            list.push_str(&format!("{p} {new}\n"));
        }
        self.write("rewritten-list", &list)?;
        seqstate::remove(&self.dir, "rewritten-pending");
        Ok(())
    }
}

// ---------------------------------------------------------------- entry

pub async fn run(ctx: &Ctx, a: Args) -> Result<ExitCode> {
    let ws = ctx.ws()?;
    let gitdir = ws.projection.git_dir().to_owned();
    let modes = [a.cont, a.skip, a.abort, a.quit, a.edit_todo]
        .iter()
        .filter(|&&m| m)
        .count();
    if modes > 1 {
        // git names the option it met last and the one before it.
        let seen: Vec<String> = std::env::args()
            .filter(|w| {
                ["--continue", "--skip", "--abort", "--quit", "--edit-todo"].contains(&w.as_str())
            })
            .collect();
        let (first, last) = (
            seen.first().cloned().unwrap_or_default(),
            seen.last().cloned().unwrap_or_default(),
        );
        return Err(CliError::Usage(format!(
            "error: options '{last}' and '{first}' cannot be used together"
        )));
    }
    if modes == 1 {
        if !a.words.is_empty() || a.onto.is_some() {
            return Err(CliError::Usage(USAGE.to_owned()));
        }
        if !state_dir(&gitdir).is_dir() {
            return Err(fatal("no rebase in progress"));
        }
        let mut run = Run::load(ctx, ws, &gitdir, a.cont || a.skip).await?;
        run.quiet |= a.quiet;
        return if a.cont {
            resume(ctx, ws, &mut run).await
        } else if a.skip {
            skip(ctx, ws, &mut run).await
        } else if a.abort {
            abort(ctx, ws, run).await
        } else if a.edit_todo {
            edit_todo(ctx, ws, &mut run).await
        } else {
            quit(ctx, ws, run).await
        };
    }
    if a.words.len() > 2 {
        return Err(CliError::Usage(USAGE.to_owned()));
    }
    start(ctx, ws, &gitdir, a).await
}

fn word(os: &OsString) -> String {
    os.to_string_lossy().into_owned()
}

/// git's refusal when no upstream is named and the branch tracks nothing.
fn no_upstream(branch: Option<&str>) -> CliError {
    let t = "    git rebase '<branch>'\n";
    let text = match branch {
        Some(b) => format!(
            "There is no tracking information for the current branch.\nPlease specify which branch you want to rebase against.\nSee git-rebase(1) for details.\n\n{t}\nIf you wish to set tracking information for this branch you can do so with:\n\n    git branch --set-upstream-to=<remote>/<branch> {b}\n"
        ),
        None => format!(
            "You are not currently on a branch.\nPlease specify which branch you want to rebase against.\nSee git-rebase(1) for details.\n\n{t}"
        ),
    };
    CliError::Raw(text, 1)
}

async fn commit_named(ctx: &Ctx, spec: &str) -> Result<Option<ChangesetId>> {
    super::restore::resolve_commit(ctx, spec).await
}

/// Whether the work tree or the stage differs from HEAD, and git's refusal text for it.
async fn dirty_state(ctx: &Ctx, ws: &WsCtx, head_root: TreeId) -> Result<(bool, bool)> {
    let fresh = workspaces::get(&ws.db, ws.workspace.ws.id)
        .await?
        .ok_or_else(|| fatal("asc: this workspace is not registered in state.db"))?;
    let parts = WcParts::new(ctx, ws, ws.projection.git_dir().join("index"))?;
    let st = status::compute(&parts.wc(&ws.engine), &ws.db, &fresh)
        .await
        .map_err(fatal)?;
    let (index, unmerged) = index_tree(ws, head_root).await?;
    let unstaged = !st.unstaged.is_empty() || !unmerged.is_empty() || !st.unmerged.is_empty();
    Ok((unstaged, index != head_root))
}

fn refuse_dirty(unstaged: bool, staged: bool) -> CliError {
    let first = if unstaged {
        "error: cannot rebase: You have unstaged changes."
    } else {
        "error: cannot rebase: Your index contains uncommitted changes."
    };
    let second = if unstaged && staged {
        "error: Additionally, your index contains uncommitted changes.\n"
    } else {
        ""
    };
    CliError::Raw(
        format!("{first}\n{second}error: Please commit or stash them."),
        1,
    )
}

/// What `--empty=<mode>` says; `None` for an unknown word.
fn empty_mode(word: &str) -> Option<Empty> {
    match word {
        "drop" => Some(Empty::Drop),
        "keep" => Some(Empty::Keep),
        "stop" | "ask" => Some(Empty::Stop),
        _ => None,
    }
}

async fn start(ctx: &Ctx, ws: &WsCtx, gitdir: &Path, a: Args) -> Result<ExitCode> {
    if state_dir(gitdir).exists() {
        return Err(already(gitdir));
    }
    let explicit = a.interactive;
    let empty_arg = match a.empty.as_deref() {
        Some(w) => Some(empty_mode(w).ok_or_else(|| {
            fatal(format!(
                "unrecognized empty type '{w}'; valid values are \"drop\", \"keep\", and \"stop\"."
            ))
        })?),
        None => None,
    };
    let comment = message::comment_char(ctx);
    let head_state = ws.projection.read_head()?;
    let cur_branch = match &head_state {
        Head::Branch(b) => Some(b.clone()),
        Head::Detached(_) => None,
    };
    let Some(head) = ws.head_commit().await? else {
        return Err(fatal("asc: rebase on an unborn branch is not supported yet"));
    };
    let upstream_text = match a.words.first() {
        Some(w) => word(w),
        None => {
            let tracked = cur_branch
                .as_deref()
                .and_then(|b| switch::upstream_ref(ctx, b));
            match tracked {
                Some((_, tracking)) => tracking,
                None => return Err(no_upstream(cur_branch.as_deref())),
            }
        }
    };
    let upstream = commit_named(ctx, &upstream_text)
        .await?
        .ok_or_else(|| fatal(format!("invalid upstream '{upstream_text}'")))?;
    let onto_text = a.onto.clone().unwrap_or_else(|| upstream_text.clone());
    let onto = commit_named(ctx, &onto_text)
        .await?
        .ok_or_else(|| fatal(format!("Does not point to a valid commit '{onto_text}'")))?;
    // The branch to rebase: the argument, or the current one.
    let branch_arg = a.words.get(1).map(word);
    let (orig, head_name, switch_to) = match &branch_arg {
        Some(b) => {
            let rev = switch::resolve_rev(ctx, ws, b)
                .await?
                .ok_or_else(|| fatal(format!("no such branch/commit '{b}'")))?;
            match rev.kind {
                RevKind::Branch(name) => (
                    rev.commit,
                    format!("refs/heads/{name}"),
                    Some((
                        Dest::Existing {
                            branch: name,
                            tip: Some(rev.commit),
                        },
                        b.clone(),
                    )),
                ),
                _ => (
                    rev.commit,
                    "detached HEAD".to_owned(),
                    Some((Dest::Detach { commit: rev.commit }, b.clone())),
                ),
            }
        }
        None => (
            head,
            cur_branch
                .as_ref()
                .map_or_else(|| "detached HEAD".to_owned(), |b| format!("refs/heads/{b}")),
            None,
        ),
    };
    let head_root = root_of(ws, Some(head)).await?;
    let mut stash = None;
    let (unstaged, staged) = dirty_state(ctx, ws, head_root).await?;
    if unstaged || staged {
        let autostash = if a.no_autostash {
            false
        } else {
            a.autostash || ctx.config.bool("rebase.autoStash") == Some(true)
        };
        if !autostash {
            return Err(refuse_dirty(unstaged, staged));
        }
        stash = Some(create_autostash(ctx, ws, head, head_root).await?);
    }

    // The options this run takes from the command line and the configuration.
    let config_true = |key: &str| ctx.config.bool(key) == Some(true);
    let autosquash = if a.no_autosquash {
        false
    } else {
        a.autosquash || (explicit && config_true("rebase.autoSquash"))
    };
    let update_refs = if a.no_update_refs {
        false
    } else {
        a.update_refs || (explicit && config_true("rebase.updateRefs"))
    };
    let rebase_merges = a.rebase_merges.is_some() && !a.no_rebase_merges;
    let keep_empty = !a.no_keep_empty;
    let empty = empty_arg.unwrap_or(if explicit {
        Empty::Stop
    } else if a.exec.is_empty() {
        Empty::Drop
    } else {
        Empty::Keep
    });

    let hist = &ws.engine;
    let base = HistoryApi::merge_base(hist, upstream, &[orig], false)
        .await
        .map_err(fatal)?;
    let base = base.first().copied();
    let onto_base = HistoryApi::merge_base(hist, onto, &[orig], false)
        .await
        .map_err(fatal)?;
    let up_to_date = base == Some(onto) && onto_base.first() == Some(&onto);
    // git starts every rebase without the previous merge's AUTO_MERGE.
    seqstate::remove(gitdir, "AUTO_MERGE");
    let branch_label = match head_name.strip_prefix("refs/heads/") {
        Some(b) => format!("Current branch {b}"),
        None => "HEAD".to_owned(),
    };
    let forces = explicit || !a.exec.is_empty() || a.autosquash || rebase_merges || update_refs;
    if up_to_date && !forces {
        if a.force {
            println!("{branch_label} is up to date, rebase forced.");
        } else {
            if let Some((dest, typed)) = switch_to {
                quiet_move(ctx, dest, typed).await?;
            }
            println!("{branch_label} is up to date.");
            if let Some(s) = stash {
                apply_autostash(ctx, ws, s).await?;
            }
            return Ok(ExitCode::SUCCESS);
        }
    }

    if !a.no_verify {
        let runner = Runner::for_ctx(ctx)?;
        let env = HookEnv::of(ctx, "rebase");
        let hook = Hook::PreRebase {
            upstream: upstream_text.clone(),
            branch: branch_arg.clone(),
        };
        if let Gate::Reject(_) = runner.gate(hook, &env).await? {
            if let Some(s) = stash {
                apply_autostash(ctx, ws, s).await?;
            }
            return Err(fatal("The pre-rebase hook refused to rebase."));
        }
    }
    if let Some((dest, typed)) = switch_to {
        quiet_move(ctx, dest, typed).await?;
    }

    let abbrev = switch::abbrev_len(ctx);
    let short = |o: &GitOid| o.to_string()[..abbrev].to_owned();
    let onto_git = conflict::git_id(ws, onto).await?;
    let orig_git = conflict::git_id(ws, orig).await?;
    let upstream_git = conflict::git_id(ws, upstream).await?;

    // The generated list.
    let range = collect_range(ws, orig, upstream).await?;
    let mut list = if rebase_merges {
        merges_todo(
            ctx,
            ws,
            &range,
            base.unwrap_or(upstream),
            orig,
            keep_empty,
            comment,
        )
        .await?
    } else {
        plain_todo(ws, &range, keep_empty, comment).await?
    };
    if todo::is_empty(&list) {
        list = vec![Item::new(Op::Noop, None, "")];
    }
    if autosquash {
        list = rearrange(ctx, ws, list).await?;
    }
    list = todo::add_exec(list, &a.exec);
    let mut tracked: Vec<(String, GitOid)> = Vec::new();
    if update_refs {
        let (with_refs, t) = add_update_refs(ws, list, &head_name).await?;
        list = with_refs;
        tracked = t;
    }
    for e in &a.exec {
        if e.len() > MAX_LINE {
            return Err(fatal("asc: the --exec command is too long"));
        }
    }
    if todo::count(&list) > MAX_TODO {
        return Err(fatal("asc: too many commits to rebase"));
    }

    let dir = state_dir(gitdir);
    std::fs::create_dir(&dir)
        .map_err(|e| fatal(format!("cannot create '{}': {e}", dir.display())))?;
    let mut run = Run {
        dir,
        gitdir: gitdir.to_owned(),
        head_name,
        onto: onto_git,
        orig_head: orig_git,
        todo: Vec::new(),
        done: Vec::new(),
        end: 0,
        quiet: a.quiet,
        verbose: a.verbose,
        empty,
        allow_ff: !a.force,
        comment,
        abbrev,
    };
    run.write("head-name", &format!("{}\n", run.head_name))?;
    run.write("onto", &format!("{}\n", run.onto))?;
    run.write("orig-head", &format!("{}\n", run.orig_head))?;
    run.write("interactive", "")?;
    run.write("no-reschedule-failed-exec", "")?;
    match run.empty {
        Empty::Keep => run.write("keep_redundant_commits", "")?,
        Empty::Drop => run.write("drop_redundant_commits", "")?,
        Empty::Stop => {}
    }
    if run.quiet {
        run.write("quiet", "")?;
    }
    if run.verbose {
        run.write("verbose", "")?;
    }
    if rebase_merges {
        run.write("refs-to-delete", "")?;
    }
    let header = todo::rebase_header(
        &short(&upstream_git),
        &short(&run.orig_head),
        &short(&run.onto),
        todo::count(&list),
        comment,
    );
    run.write(
        "git-rebase-todo.backup",
        &todo::backup_text(&list, &header, comment),
    )?;
    if explicit {
        // The editor sees short ids and the help block.
        run.write(
            "git-rebase-todo",
            &todo::editor_text(&list, &short, &header, comment),
        )?;
        let path = run.dir.join("git-rebase-todo");
        if !todo::edit_todo(ctx, &path).await? {
            let _ = std::fs::remove_dir_all(&run.dir);
            return Ok(ExitCode::from(1));
        }
        let edited = need_text(&run.dir, "git-rebase-todo")?;
        let stripped = todo::strip_comments(&edited, comment);
        if stripped.trim().is_empty() {
            let _ = std::fs::remove_dir_all(&run.dir);
            return Err(CliError::Raw("error: nothing to do".to_owned(), 1));
        }
        match parse_text(ctx, ws, &run.dir, &stripped).await {
            Ok(l) => list = l,
            Err(Ok(errors)) => {
                // The head is on the new base by the time the list is found wanting.
                detach_to(ctx, ws, onto, &onto_text).await?;
                return Err(parse_failure(ctx, &errors, true));
            }
            Err(Err(e)) => return Err(e),
        }
    }
    run.end = todo::count(&list);

    // Leading picks that already sit on the base are kept without being replayed.
    let mut current = onto;
    let mut skipped = 0;
    if !a.force {
        for (i, item) in list.iter().enumerate() {
            if !item.counts() || item.op == Op::Noop || item.op == Op::Drop {
                skipped = i + 1;
                continue;
            }
            if item.op != Op::Pick {
                break;
            }
            let Some(oid) = item.oid else { break };
            let id = conflict::changeset_of(ws, oid).await?;
            let cs = ws.engine.get_changeset(id).await?;
            if cs.parents.as_slice() != [current] {
                break;
            }
            current = id;
            skipped = i + 1;
        }
    }
    // Only a prefix that ends on a command moves: trailing comments stay with the todo list.
    let mut rest = list.split_off(skipped);
    run.done = list;
    while run.done.last().is_some_and(|i| !i.counts()) {
        rest.insert(0, run.done.pop().unwrap_or_else(Item::blank));
    }
    run.todo = rest;
    run.save_progress()?;
    run.write("end", &format!("{}\n", run.end))?;
    seqstate::write_orig_head(gitdir, run.orig_head)?;
    if let Some(s) = stash {
        run.write("autostash", &format!("{s}\n"))?;
    }
    if !tracked.is_empty() {
        let mut text = String::new();
        for (name, oid) in &tracked {
            text.push_str(&format!("{name}\n{oid}\n{}\n", "0".repeat(40)));
        }
        run.write("update-refs", &text)?;
    }
    detach_to(ctx, ws, current, &onto_text).await?;
    drive(ctx, ws, &mut run).await
}

fn already(gitdir: &Path) -> CliError {
    let dir = state_dir(gitdir);
    CliError::Raw(
        format!(
            "fatal: It seems that there is already a rebase-merge directory, and\nI wonder if you are in the middle of another rebase.  If that is the\ncase, please try\n\tgit rebase (--continue | --abort | --skip)\nIf that is not the case, please\n\trm -fr \"{}\"\nand run me again.  I am stopping in case you still have something valuable there.\n",
            dir.display()
        ),
        128,
    )
}

fn first_line(message: &[u8]) -> String {
    String::from_utf8_lossy(message)
        .lines()
        .next()
        .unwrap_or_default()
        .to_owned()
}

/// Switches to a branch or detaches at a commit without printing anything.
async fn quiet_move(ctx: &Ctx, dest: Dest, typed: String) -> Result<()> {
    let opts = switch::Opts {
        quiet: true,
        ..switch::Opts::default()
    };
    switch::perform(
        ctx,
        Move {
            dest,
            opts,
            typed,
            keep_tree: false,
        },
    )
    .await
    .map(|_| ())
}

async fn detach_to(ctx: &Ctx, ws: &WsCtx, target: ChangesetId, typed: &str) -> Result<()> {
    let _ = ws;
    quiet_move(ctx, Dest::Detach { commit: target }, typed.to_owned()).await
}

// ---------------------------------------------------------------- generating the list

/// One commit of the range with what the todo list needs of it.
struct Ranged {
    id: ChangesetId,
    parents: Vec<ChangesetId>,
    generation: u32,
}

/// The commits of `upstream..orig`, oldest first.
async fn collect_range(
    ws: &WsCtx,
    orig: ChangesetId,
    upstream: ChangesetId,
) -> Result<Vec<Ranged>> {
    let q = LogQuery {
        include: vec![orig],
        exclude: vec![upstream],
        limit: Some(MAX_TODO + 1),
        ..LogQuery::default()
    };
    let entries = HistoryApi::log(&ws.engine, &q, None)
        .await
        .map_err(fatal)?
        .entries;
    if entries.len() > MAX_TODO {
        return Err(fatal("asc: too many commits to rebase"));
    }
    let mut out: Vec<Ranged> = entries
        .into_iter()
        .map(|e| Ranged {
            id: e.id,
            parents: e.parents,
            generation: e.generation,
        })
        .collect();
    out.reverse();
    Ok(out)
}

async fn is_empty_commit(ws: &WsCtx, cs: &Changeset) -> Result<bool> {
    Ok(root_of(ws, cs.parents.first().copied()).await? == cs.root)
}

/// The list without `--rebase-merges`: every non-merge commit becomes a `pick`.
async fn plain_todo(
    ws: &WsCtx,
    range: &[Ranged],
    keep_empty: bool,
    comment: u8,
) -> Result<Vec<Item>> {
    let mut list = Vec::new();
    for r in range {
        if r.parents.len() > 1 {
            continue;
        }
        let cs = ws.engine.get_changeset(r.id).await?;
        if cs.parents.is_empty() {
            return Err(fatal("asc: rebasing a root commit is not supported yet"));
        }
        let empty = is_empty_commit(ws, &cs).await?;
        if empty && !keep_empty {
            continue;
        }
        let oid = conflict::git_id(ws, r.id).await?;
        list.push(todo::pick_item(
            &Commit {
                oid,
                subject: first_line(&cs.message),
                empty,
            },
            comment,
        ));
    }
    Ok(list)
}

/// A label git can use as a ref name: letters, digits and `-_.` stay, everything else turns into
/// a dash.
fn sanitize_label(text: &str) -> String {
    let mut out: String = text
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '.' || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect();
    while out.starts_with(['-', '.']) {
        out.remove(0);
    }
    if out.is_empty() || out.ends_with(".lock") {
        out.push_str("branch");
    }
    out
}

/// The label a merge's second parent gets from the merge subject: `Merge branch 'x'` is `x`,
/// `Merge pull request #1 from o/x` is `o/x`, anything else the subject.
fn label_from_subject(subject: &str) -> String {
    let named = subject.strip_prefix("Merge ").and_then(|r| {
        let start = r.find('\'')? + 1;
        let len = r[start..].find('\'')?;
        Some(r[start..start + len].to_owned())
    });
    if let Some(n) = named {
        return n;
    }
    if let Some(r) = subject.strip_prefix("Merge pull request ")
        && let Some(i) = r.find(" from ")
    {
        return r[i + 6..].to_owned();
    }
    subject.to_owned()
}

/// Labels of a `--rebase-merges` list: unique, in the order they are handed out.
struct Labels2 {
    by_commit: HashMap<ChangesetId, String>,
    used: HashSet<String>,
}

impl Labels2 {
    fn give(&mut self, id: ChangesetId, wanted: &str) -> String {
        if let Some(l) = self.by_commit.get(&id) {
            return l.clone();
        }
        let base = sanitize_label(wanted);
        let mut label = base.clone();
        let mut n = 1;
        while self.used.contains(&label.to_lowercase()) {
            n += 1;
            label = format!("{base}-{n}");
        }
        self.used.insert(label.to_lowercase());
        self.by_commit.insert(id, label.clone());
        label
    }
}

/// The list with `--rebase-merges` (git's `make_script_with_merges`): `label`, `reset` and
/// `merge` commands that rebuild the topology, branch by branch.
async fn merges_todo(
    ctx: &Ctx,
    ws: &WsCtx,
    range: &[Ranged],
    bottom: ChangesetId,
    orig: ChangesetId,
    keep_empty: bool,
    comment: u8,
) -> Result<Vec<Item>> {
    let cc = comment as char;
    // Parents first: generation orders them, the log order breaks ties.
    let mut ordered: Vec<&Ranged> = range.iter().collect();
    ordered.sort_by_key(|r| r.generation);
    let interesting: HashSet<ChangesetId> = ordered.iter().map(|r| r.id).collect();
    let mut labels = Labels2 {
        by_commit: HashMap::new(),
        used: HashSet::new(),
    };
    labels.give(bottom, "onto");
    // Branch names decorate the tips they point at.
    let mut branch_at: HashMap<ChangesetId, String> = HashMap::new();
    for e in ws.db.refs().list(ws.repo.key, "refs/heads/").await? {
        if let RefTarget::Changeset(c) = e.target {
            let name = String::from_utf8_lossy(e.name.as_bytes()).into_owned();
            branch_at
                .entry(c)
                .or_insert_with(|| name.strip_prefix("refs/heads/").unwrap_or(&name).to_owned());
        }
    }
    let abbrev = switch::abbrev_len(ctx);
    // Phase one: the todo line of every commit, the tips of merged branches and their labels.
    let mut lines: HashMap<ChangesetId, Option<Item>> = HashMap::new();
    let mut onelines: HashMap<ChangesetId, String> = HashMap::new();
    let mut tips: Vec<ChangesetId> = Vec::new();
    for r in &ordered {
        let cs = ws.engine.get_changeset(r.id).await?;
        let oneline = String::from_utf8_lossy(&cs.message)
            .lines()
            .take_while(|l| !l.trim().is_empty())
            .collect::<Vec<_>>()
            .join(" ");
        onelines.insert(r.id, oneline.clone());
        let oid = conflict::git_id(ws, r.id).await?;
        if cs.parents.len() <= 1 {
            if cs.parents.is_empty() {
                return Err(fatal("asc: rebasing a root commit is not supported yet"));
            }
            let empty = is_empty_commit(ws, &cs).await?;
            if empty && !keep_empty {
                lines.insert(r.id, None);
            } else {
                lines.insert(
                    r.id,
                    Some(todo::pick_item(
                        &Commit {
                            oid,
                            subject: oneline,
                            empty,
                        },
                        comment,
                    )),
                );
            }
            continue;
        }
        let mut names = Vec::new();
        for &p in &cs.parents[1..] {
            if interesting.contains(&p) {
                if !tips.contains(&p) {
                    tips.push(p);
                }
                let wanted = branch_at
                    .get(&p)
                    .cloned()
                    .unwrap_or_else(|| label_from_subject(&oneline));
                names.push(labels.give(p, &wanted));
            } else {
                let pg = conflict::git_id(ws, p).await?;
                names.push(labels.give(p, &pg.to_string()[..abbrev]));
            }
        }
        let mut item = Item::new(
            Op::Merge,
            Some(oid),
            format!("{} {cc} {oneline}", names.join(" ")),
        );
        item.keep = Keep::Message;
        lines.insert(r.id, Some(item));
    }
    // Phase two: a commit with a second child is a branch point.
    let mut seen_parent: HashSet<ChangesetId> = HashSet::new();
    for r in &ordered {
        for &p in &r.parents {
            if interesting.contains(&p) && !seen_parent.insert(p) {
                labels.give(p, "branch-point");
            }
        }
    }
    tips.push(orig);
    // Phase three: branch by branch, each walked back along first parents.
    let mut out = vec![Item::new(Op::Label, None, "onto"), Item::blank()];
    let mut shown: HashSet<ChangesetId> = HashSet::new();
    let by_id: HashMap<ChangesetId, &Ranged> = ordered.iter().map(|r| (r.id, *r)).collect();
    for (n, tip) in tips.iter().enumerate() {
        let mut chain: Vec<ChangesetId> = Vec::new();
        let mut cur = Some(*tip);
        while let Some(c) = cur {
            if !interesting.contains(&c) || shown.contains(&c) {
                break;
            }
            chain.push(c);
            shown.insert(c);
            cur = by_id.get(&c).and_then(|r| r.parents.first().copied());
        }
        if chain.is_empty() {
            continue;
        }
        chain.reverse();
        let first = by_id[&chain[0]];
        let is_last = n + 1 == tips.len();
        out.push(Item::blank());
        if !is_last && let Some(l) = labels.by_commit.get(tip) {
            out.push(Item::comment(format!("{cc} Branch {l}")));
        }
        let base = first.parents.first().copied().unwrap_or(bottom);
        let base_label = match labels.by_commit.get(&base) {
            Some(l) => l.clone(),
            None => {
                let g = conflict::git_id(ws, base).await?;
                labels.give(base, &g.to_string()[..abbrev])
            }
        };
        let reset = if interesting.contains(&base) {
            format!(
                "{base_label} {cc} {}",
                onelines.get(&base).cloned().unwrap_or_default()
            )
        } else {
            base_label
        };
        out.push(Item::new(Op::Reset, None, reset));
        for c in &chain {
            if let Some(Some(item)) = lines.get(c) {
                out.push(item.clone());
            }
            if let Some(l) = labels.by_commit.get(c)
                && (!is_last || *c != *tip)
            {
                out.push(Item::new(Op::Label, None, l.clone()));
            }
        }
    }
    Ok(out)
}

/// `--autosquash`: moves the `fixup!`, `squash!` and `amend!` commits behind their targets.
async fn rearrange(ctx: &Ctx, ws: &WsCtx, list: Vec<Item>) -> Result<Vec<Item>> {
    let mut subjects: Vec<Option<String>> = Vec::with_capacity(list.len());
    for item in &list {
        match item.oid {
            Some(oid) if item.op == Op::Pick => {
                let id = conflict::changeset_of(ws, oid).await?;
                let cs = ws.engine.get_changeset(id).await?;
                subjects.push(Some(message::subject(&cs.message)));
            }
            _ => subjects.push(None),
        }
    }
    let wanted: Vec<String> =
        todo::autosquash_names(&subjects.iter().flatten().cloned().collect::<Vec<_>>());
    let mut names = HashMap::new();
    for w in wanted {
        if let Ok(Some(id)) = commit_named(ctx, &w).await {
            names.insert(w, conflict::git_id(ws, id).await?);
        }
    }
    Ok(todo::rearrange_squash(list, &subjects, &names))
}

/// `--update-refs`: the branches pointing at commits of the list, but not the one being rebased.
async fn add_update_refs(
    ws: &WsCtx,
    list: Vec<Item>,
    head_name: &str,
) -> Result<(Vec<Item>, Vec<(String, GitOid)>)> {
    let mut refs_at: HashMap<GitOid, Vec<String>> = HashMap::new();
    for e in ws.db.refs().list(ws.repo.key, "refs/heads/").await? {
        let name = String::from_utf8_lossy(e.name.as_bytes()).into_owned();
        if name == head_name {
            continue;
        }
        if let RefTarget::Changeset(c) = e.target {
            let git = conflict::git_id(ws, c).await?;
            refs_at.entry(git).or_default().push(name);
        }
    }
    for v in refs_at.values_mut() {
        v.sort();
    }
    Ok(todo::add_update_refs(list, &refs_at))
}

// ---------------------------------------------------------------- the loop

enum Step {
    Done,
    /// Stopped for the user, with the exit code git uses for that stop.
    Stopped(u8),
}

/// Runs the remaining commands, then finishes.
async fn drive(ctx: &Ctx, ws: &WsCtx, run: &mut Run) -> Result<ExitCode> {
    while !run.todo.is_empty() {
        let item = run.todo.remove(0);
        run.done.push(item.clone());
        run.save_progress()?;
        if !item.counts() {
            continue;
        }
        if !run.quiet {
            let end = if run.verbose { "\n" } else { "\r" };
            eprint!("Rebasing ({}/{}){end}", todo::count(&run.done), run.end);
        }
        let step = run_item(ctx, ws, run, &item).await?;
        if let Step::Stopped(code) = step {
            return Ok(ExitCode::from(code));
        }
    }
    finish(ctx, ws, run).await
}

async fn run_item(ctx: &Ctx, ws: &WsCtx, run: &mut Run, item: &Item) -> Result<Step> {
    match item.op {
        Op::Pick | Op::Reword | Op::Edit | Op::Squash | Op::Fixup => pick(ctx, ws, run, item).await,
        Op::Drop | Op::Noop | Op::Comment => Ok(Step::Done),
        Op::Exec => exec(ctx, ws, run, &item.arg).await,
        Op::Break => stop_at_head(ctx, ws).await,
        Op::Label => label(ctx, ws, run, &item.arg).await,
        Op::Reset => reset(ctx, ws, &item.arg).await,
        Op::Merge => merge(ctx, ws, run, item).await,
        Op::UpdateRef => update_ref(ws, run, &item.arg).await,
    }
}

fn reflog_word(op: Op) -> &'static str {
    match op {
        Op::Reword => "reword",
        Op::Edit => "edit",
        Op::Squash => "squash",
        Op::Fixup => "fixup",
        _ => "pick",
    }
}

async fn pick(ctx: &Ctx, ws: &WsCtx, run: &mut Run, item: &Item) -> Result<Step> {
    let oid = item
        .oid
        .ok_or_else(|| fatal("asc: a todo command lacks its commit"))?;
    let id = conflict::changeset_of(ws, oid).await?;
    let cs = ws.engine.get_changeset(id).await?;
    let Some(head) = ws.head_commit().await? else {
        return Err(fatal("asc: HEAD is unborn"));
    };
    if cs.parents.len() != 1 {
        return Err(fatal(format!(
            "asc: cannot replay {oid}: it is not a single-parent commit"
        )));
    }
    let head_root = root_of(ws, Some(head)).await?;
    // A pick that goes on removes what the last stop left.
    seqstate::remove(&run.gitdir, "REBASE_HEAD");
    let subject = first_line(&cs.message);
    let reflog = format!("rebase ({}): {subject}", reflog_word(item.op));
    let folds = item.op.is_fixup();
    // Fast-forward: the commit already sits on HEAD.
    if run.allow_ff && !folds && cs.parents[0] == head {
        let paths = changed_paths(ws, head_root, cs.root).await?;
        sync_worktree(ctx, cs.root, &paths, true).await?;
        conflict::move_head(
            ctx,
            ws,
            Some(head),
            id,
            &reflog,
            StagingAfterMove::Set(Vec::new()),
            None,
        )
        .await?;
        return after_pick(ctx, ws, run, item, oid, &cs).await;
    }
    let head_cs = ws.engine.get_changeset(head).await?;
    if folds {
        fold_prepare(run, item, oid, &cs, &head_cs)?;
    }
    let parent_root = root_of(ws, Some(cs.parents[0])).await?;
    let abbrev = run.short(&oid);
    let named = format!("{abbrev} ({subject})");
    let sides = Sides {
        base: parent_root,
        ours: head_root,
        theirs: cs.root,
    };
    let outcome = match ws.engine.rebase_metadata(&cs, head).await.map_err(fatal)? {
        Some(done) => MergeOutcome {
            root: done.root,
            conflicts: Vec::new(),
        },
        None => {
            let favor = conflict::favor_of(&[])?;
            let labels = Labels {
                base: format!("parent of {named}"),
                ours: "HEAD".into(),
                theirs: named.clone(),
            };
            let (merge_opts, differ) =
                conflict::merge_options(ctx, ws, &sides, favor, labels).await?;
            let outcome = ws
                .engine
                .merge_trees(sides.base, sides.ours, sides.theirs, &merge_opts)
                .await
                .map_err(fatal)?;
            conflict::refuse_unsupported(&outcome)?;
            conflict::precheck(ctx, ws, ReplayOp::Pick, head_root, &outcome, false).await?;
            conflict::print_messages(&conflict::messages(&outcome, &differ, "HEAD", &named));
            outcome
        }
    };
    let became_empty = outcome.is_clean()
        && outcome.conflicts.is_empty()
        && outcome.root == head_root
        && parent_root != cs.root;
    if became_empty && !folds {
        match run.empty {
            Empty::Drop => {
                eprintln!("dropping {oid} {subject} -- patch contents already upstream");
                return Ok(Step::Done);
            }
            Empty::Keep => {}
            Empty::Stop => {
                conflict::materialise(ctx, ws, head_root, head_root, &outcome).await?;
                seqstate::write_auto_merge(
                    &run.gitdir,
                    conflict::git_tree_id(ws, outcome.root).await?,
                )?;
                write_stop(ws, run, oid, &cs, &cs.message).await?;
                seqstate::write(&run.gitdir, "COMMIT_EDITMSG", &cs.message)?;
                seqstate::write(&run.gitdir, "MERGE_MSG", &cs.message)?;
                seqstate::write(
                    &run.gitdir,
                    "CHERRY_PICK_HEAD",
                    format!("{oid}\n").as_bytes(),
                )?;
                let t = tool(ctx);
                eprint!(
                    "The previous cherry-pick is now empty, possibly due to conflict resolution.\nIf you wish to commit it anyway, use:\n\n    {t} commit --allow-empty\n\nOtherwise, please use '{t} rebase --skip'\n"
                );
                eprint!("{}", empty_status(run, ws).await?);
                eprintln!("Could not apply {abbrev}... {}", item.arg);
                return Ok(Step::Stopped(1));
            }
        }
    }
    conflict::materialise(ctx, ws, head_root, head_root, &outcome).await?;
    // git's merge machinery leaves the merged tree in AUTO_MERGE, clean or not.
    seqstate::write_auto_merge(&run.gitdir, conflict::git_tree_id(ws, outcome.root).await?)?;
    if !outcome.is_clean() {
        let mut text = if folds {
            need_text(&run.dir, "message-squash")?.into_bytes()
        } else {
            cs.message.clone()
        };
        text.extend_from_slice(&conflict::conflicts_block(&outcome, run.comment));
        seqstate::write(&run.gitdir, "MERGE_MSG", &text)?;
        write_stop(ws, run, oid, &cs, &text).await?;
        eprintln!("error: could not apply {abbrev}... {subject}");
        if conflict::advice(ctx, "mergeConflict") {
            let t = tool(ctx);
            eprintln!(
                "hint: Resolve all conflicts manually, mark them as resolved with\n\
                 hint: \"{t} add/rm <conflicted_files>\", then run \"{t} rebase --continue\".\n\
                 hint: You can instead skip this commit: run \"{t} rebase --skip\".\n\
                 hint: To abort and get back to the state before \"{t} rebase\", run \"{t} rebase --abort\".\n\
                 hint: Disable this message with \"{t} config set advice.mergeConflict false\""
            );
        }
        eprintln!("Could not apply {abbrev}... {}", item.arg);
        return Ok(Step::Stopped(1));
    }
    if folds {
        return fold_commit(ctx, ws, run, item, oid, outcome.root, head, &reflog).await;
    }
    // git writes the message to COMMIT_EDITMSG and lets prepare-commit-msg change it.
    let msg_file = run.gitdir.join("COMMIT_EDITMSG");
    let runner = Runner::for_ctx(ctx)?;
    // Without the hook git leaves COMMIT_EDITMSG alone.
    let hooked = runner.exists(&Hook::PrepareCommitMsg {
        file: msg_file.clone(),
        source: Some(MsgSource::Message),
    });
    if hooked {
        std::fs::write(&msg_file, &cs.message)?;
    }
    let env = HookEnv::of(ctx, "rebase")
        .with_index(run.gitdir.join("index"))
        .without_editor();
    let prepare = Hook::PrepareCommitMsg {
        file: msg_file.clone(),
        source: Some(MsgSource::Message),
    };
    if let Gate::Reject(_) = runner.gate(prepare, &env).await? {
        write_stop(ws, run, oid, &cs, &cs.message).await?;
        eprintln!("error: 'prepare-commit-msg' hook failed");
        eprintln!("Could not apply {abbrev}... {}", item.arg);
        return Ok(Step::Stopped(1));
    }
    // git leaves the author of the last pick in author-script.
    run.write("author-script", &author_script(&cs.author))?;
    let picked_message = if hooked {
        std::fs::read(&msg_file)?
    } else {
        cs.message.clone()
    };
    let idents = conflict::identities(ctx)?;
    let (new, _new_git) = conflict::write_commit(
        ws,
        NewCommit {
            parents: vec![head],
            author: cs.author.clone(),
            committer: idents.committer.clone(),
            message: picked_message,
            root: outcome.root,
        },
    )
    .await?;
    let op = conflict::move_head(
        ctx,
        ws,
        Some(head),
        new,
        &reflog,
        StagingAfterMove::Set(Vec::new()),
        Some(&idents.committer),
    )
    .await?;
    ext::rewrite_sink().record(new, &[id], RewriteOp::Rebase, op);
    ext::index_sink()
        .on_commit(ws.workspace.ws.id, new)
        .await
        .map_err(fatal)?;
    ws_ops::clear(&ws.db, ws.workspace.ws.id).await?;
    let _ = runner.run(Hook::PostCommit, &env).await?;
    after_pick(ctx, ws, run, item, oid, &cs).await
}

/// What the stopped command leaves for the user: `message`, `author-script`, `patch`,
/// `stopped-sha` and `REBASE_HEAD`.
async fn write_stop(ws: &WsCtx, run: &Run, oid: GitOid, cs: &Changeset, text: &[u8]) -> Result<()> {
    run.write("message", &String::from_utf8_lossy(text))?;
    run.write("author-script", &author_script(&cs.author))?;
    run.write(
        "patch",
        &String::from_utf8_lossy(&patch_text(ws, cs).await?),
    )?;
    run.write("stopped-sha", &format!("{oid}\n"))?;
    seqstate::write(&run.gitdir, "REBASE_HEAD", format!("{oid}\n").as_bytes())?;
    Ok(())
}

/// The text `git commit` prints when there is nothing to commit during a rebase.
async fn empty_status(run: &Run, ws: &WsCtx) -> Result<String> {
    let _ = ws;
    let Some(info) = todo::read_info(&run.gitdir, run.abbrev) else {
        return Ok(String::new());
    };
    let mut out = format!("{}\n", info.head_line());
    out.push_str(&info.block(false, true, "git"));
    out.push_str("\nnothing to commit, working tree clean\n");
    Ok(out)
}

/// The work after a pick wrote its commit: `edit` stops, `reword` asks for the message, and the
/// commit joins the rewritten list.
async fn after_pick(
    ctx: &Ctx,
    ws: &WsCtx,
    run: &mut Run,
    item: &Item,
    oid: GitOid,
    cs: &Changeset,
) -> Result<Step> {
    let head = ws
        .head_commit()
        .await?
        .ok_or_else(|| fatal("asc: HEAD is unborn"))?;
    let head_git = conflict::git_id(ws, head).await?;
    match item.op {
        Op::Edit => {
            run.write("message", &String::from_utf8_lossy(&cs.message))?;
            run.write("author-script", &author_script(&cs.author))?;
            run.write(
                "patch",
                &String::from_utf8_lossy(&patch_text(ws, cs).await?),
            )?;
            run.write("stopped-sha", &format!("{oid}\n"))?;
            run.write("amend", &format!("{head_git}\n"))?;
            seqstate::write(&run.gitdir, "REBASE_HEAD", format!("{oid}\n").as_bytes())?;
            if !run.quiet {
                let t = tool(ctx);
                eprint!(
                    "Stopped at {}...  {}\nYou can amend the commit now, with\n\n  {t} commit --amend \n\nOnce you are satisfied with your changes, run\n\n  {t} rebase --continue\n",
                    run.short(&oid),
                    item.arg
                );
            }
            return Ok(Step::Stopped(0));
        }
        Op::Reword if !reword(ctx, ws, run, item, oid, cs).await? => {
            return Ok(Step::Stopped(1));
        }
        _ => {}
    }
    let head = ws
        .head_commit()
        .await?
        .ok_or_else(|| fatal("asc: HEAD is unborn"))?;
    run.record_rewritten(oid, conflict::git_id(ws, head).await?)?;
    Ok(Step::Done)
}

/// `reword`: opens the editor on the message of the commit just made and amends it. `false`
/// means the commit was not amended and the rebase stopped.
async fn reword(
    ctx: &Ctx,
    ws: &WsCtx,
    run: &Run,
    item: &Item,
    oid: GitOid,
    cs: &Changeset,
) -> Result<bool> {
    let head = ws
        .head_commit()
        .await?
        .ok_or_else(|| fatal("asc: HEAD is unborn"))?;
    let head_cs = ws.engine.get_changeset(head).await?;
    let head_git = conflict::git_id(ws, head).await?;
    let plan = Amend {
        head,
        head_cs: &head_cs,
        root: head_cs.root,
        text: head_cs.message.clone(),
        source: MsgSource::Commit(head_git),
        edit: true,
        verify: true,
        cleanup: message::Cleanup::Default,
        summary: true,
        reflog: "rebase (reword)".to_owned(),
        preds: vec![conflict::changeset_of(ws, oid).await?],
        kind: RewriteOp::Rebase,
    };
    match amend_head(ctx, ws, run, plan).await? {
        Some(_) => Ok(true),
        None => {
            write_stop(ws, run, oid, cs, &head_cs.message).await?;
            eprintln!("Could not apply {}... {}", run.short(&oid), item.arg);
            Ok(false)
        }
    }
}

// ---------------------------------------------------------------- amending HEAD

/// An amend of HEAD in the way `git commit --amend` makes one for the sequencer.
struct Amend<'a> {
    head: ChangesetId,
    head_cs: &'a Changeset,
    /// The tree of the new commit.
    root: TreeId,
    /// The message the commit starts from.
    text: Vec<u8>,
    source: MsgSource,
    /// Whether the commit editor opens.
    edit: bool,
    /// Whether `commit-msg` runs (git passes `--no-verify` except for `reword`).
    verify: bool,
    cleanup: message::Cleanup,
    /// Whether git's one-line summary prints.
    summary: bool,
    reflog: String,
    preds: Vec<ChangesetId>,
    kind: RewriteOp,
}

/// The lines under the Author/Date block of the editor template: where the rebase is, then what
/// the commit changes against its parent.
async fn template_status(run: &Run, ws: &WsCtx, from: TreeId, to: TreeId) -> Result<String> {
    let mut out = String::new();
    if let Some(info) = todo::read_info(&run.gitdir, run.abbrev) {
        out.push_str(&format!("{}\n", info.head_line()));
        out.push_str(&info.block(false, false, "git"));
    }
    out.push('\n');
    let changes = ws.engine.leafdiff(from, to, None).await?;
    if !changes.is_empty() {
        out.push_str("Changes to be committed:\n");
        for (path, change) in &changes {
            let label = match change {
                Change::Delete => "deleted:",
                _ => {
                    if ws.engine.lookup(from, path).await?.is_some() {
                        "modified:"
                    } else {
                        "new file:"
                    }
                }
            };
            out.push_str(&format!(
                "\t{label:<12}{}\n",
                String::from_utf8_lossy(path.as_bytes())
            ));
        }
        out.push('\n');
    }
    Ok(out)
}

fn comment_lines(text: &str, c: char) -> Vec<u8> {
    let mut out = String::new();
    for line in text.split_inclusive('\n') {
        out.push(c);
        if !matches!(line.as_bytes().first(), Some(b'\n' | b'\t')) {
            out.push(' ');
        }
        out.push_str(line);
    }
    out.into_bytes()
}

/// Amends HEAD: writes `COMMIT_EDITMSG`, runs `prepare-commit-msg`, the editor and `commit-msg`,
/// writes the commit and moves HEAD. `None` when a hook, the editor or an empty message stopped
/// it (the reason is printed).
async fn amend_head(
    ctx: &Ctx,
    ws: &WsCtx,
    run: &Run,
    a: Amend<'_>,
) -> Result<Option<(ChangesetId, GitOid)>> {
    let comment = run.comment;
    let idents = conflict::identities(ctx)?;
    let author = a.head_cs.author.clone();
    let edit_file = run.gitdir.join("COMMIT_EDITMSG");
    let parent_root = root_of(ws, a.head_cs.parents.first().copied()).await?;
    let mut buf = a.text.clone();
    if a.edit {
        if !buf.is_empty() && !buf.ends_with(b"\n") {
            buf.push(b'\n');
        }
        buf.push(b'\n');
        let c = comment as char;
        let mut shown = String::new();
        if ident::display(&author) != ident::display(&idents.committer) {
            shown.push_str(&format!("{c} Author:    {}\n", ident::display(&author)));
        }
        shown.push_str(&format!(
            "{c} Date:      {}\n",
            super::format::date::Mode::default().render(
                author.time(),
                author.tz_min(),
                author.time()
            )
        ));
        shown.push_str(&format!("{c}\n"));
        let status = comment_lines(&template_status(run, ws, parent_root, a.root).await?, c);
        buf.extend_from_slice(&message::template(comment, shown.as_bytes(), &status, None));
    }
    std::fs::write(&edit_file, &buf)?;
    let runner = Runner::for_ctx(ctx)?;
    let mut env = HookEnv::of(ctx, "rebase").with_index(run.gitdir.join("index"));
    if !a.edit {
        env = env.without_editor();
    }
    if a.verify
        && let Gate::Reject(_) = runner.gate(Hook::PreCommit, &env).await?
    {
        return Ok(None);
    }
    let prepare = Hook::PrepareCommitMsg {
        file: edit_file.clone(),
        source: Some(a.source.clone()),
    };
    if let Gate::Reject(_) = runner.gate(prepare, &env).await? {
        return Ok(None);
    }
    if a.edit && !todo::run_editor(ctx, &message::editor(ctx), &edit_file).await? {
        eprintln!("Please supply the message using either -m or -F option.");
        return Ok(None);
    }
    if a.verify
        && let Gate::Reject(_) = runner
            .gate(
                Hook::CommitMsg {
                    file: edit_file.clone(),
                },
                &env,
            )
            .await?
    {
        return Ok(None);
    }
    let raw = std::fs::read(&edit_file)?;
    let text = message::cleanup(&raw, a.cleanup, a.edit, comment);
    if text.is_empty() {
        eprintln!("Aborting commit due to empty commit message.");
        return Ok(None);
    }
    let (new, new_git) = conflict::write_commit(
        ws,
        NewCommit {
            parents: a.head_cs.parents.clone(),
            author: author.clone(),
            committer: idents.committer.clone(),
            message: text.clone(),
            root: a.root,
        },
    )
    .await?;
    let op = conflict::move_head(
        ctx,
        ws,
        Some(a.head),
        new,
        &a.reflog,
        StagingAfterMove::Set(Vec::new()),
        Some(&idents.committer),
    )
    .await?;
    ext::rewrite_sink().record(new, &a.preds, a.kind, op);
    ext::index_sink()
        .on_commit(ws.workspace.ws.id, new)
        .await
        .map_err(fatal)?;
    ws_ops::clear(&ws.db, ws.workspace.ws.id).await?;
    // `git commit` removes the merge state files; REBASE_HEAD stays.
    for name in [
        "MERGE_MSG",
        "SQUASH_MSG",
        "AUTO_MERGE",
        "CHERRY_PICK_HEAD",
        "REVERT_HEAD",
    ] {
        seqstate::remove(&run.gitdir, name);
    }
    if a.summary {
        print_summary(
            ws,
            &new_git.to_string(),
            &message::subject(&text),
            &author,
            &idents.committer,
            parent_root,
            a.root,
            true,
        )
        .await?;
    }
    let post_env = HookEnv::of(ctx, "rebase")
        .with_index(run.gitdir.join("index"))
        .without_editor();
    let _ = runner.run(Hook::PostCommit, &post_env).await?;
    // `git commit --amend` tells post-rewrite about the commit it replaced.
    let old_git = conflict::git_id(ws, a.head).await?;
    let _ = runner
        .run(
            Hook::PostRewrite {
                kind: RewriteKind::Amend,
                pairs: vec![(old_git, new_git)],
            },
            &post_env,
        )
        .await?;
    Ok(Some((new, new_git)))
}

// ---------------------------------------------------------------- squash and fixup

fn lossy(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// The message files for folding `item` into HEAD (git's `update_squash_messages`): the combined
/// message, the `current-fixups` list and, while only fixups were seen, `message-fixup`.
fn fold_prepare(
    run: &Run,
    item: &Item,
    oid: GitOid,
    cs: &Changeset,
    head_cs: &Changeset,
) -> Result<()> {
    let c = run.comment as char;
    let fixups = read_text(&run.dir, "current-fixups")?.unwrap_or_default();
    let count = fixups.lines().filter(|l| !l.is_empty()).count();
    let head_msg = lossy(&head_cs.message);
    let picked = lossy(&cs.message);
    let body = if item.keep != Keep::No {
        todo::amend_body(&picked)
    } else if item.op == Op::Squash {
        todo::without_squash_subject(&picked)
    } else {
        picked.as_str()
    };
    let fold = if item.op == Op::Squash {
        Fold::Squash
    } else {
        Fold::Fixup
    };
    let combined = if count == 0 {
        todo::start_squash(&head_msg, body, fold, c)
    } else {
        let mut b = todo::renumber(&need_text(&run.dir, "message-squash")?, count + 2, c);
        todo::append_squash(&mut b, count + 2, body, fold, c);
        b
    };
    run.write("message-squash", &combined)?;
    let line = todo::fixup_line(item.op, item.keep, &oid);
    let all = if fixups.is_empty() {
        line
    } else {
        format!("{fixups}\n{line}")
    };
    run.write("current-fixups", &all)?;
    if item.op == Op::Squash {
        seqstate::remove(&run.dir, "message-fixup");
    } else if item.keep != Keep::No {
        run.write("message-fixup", body)?;
    } else if count == 0 && !todo::seen_squash(&all) {
        run.write("message-fixup", &head_msg)?;
    }
    Ok(())
}

/// The commit of a `squash`/`fixup` once its tree is known: HEAD is amended. Only the last step of
/// a chain opens the editor (unless the chain only has fixups).
#[allow(clippy::too_many_arguments)]
async fn fold_commit(
    ctx: &Ctx,
    ws: &WsCtx,
    run: &mut Run,
    item: &Item,
    oid: GitOid,
    root: TreeId,
    head: ChangesetId,
    reflog: &str,
) -> Result<Step> {
    let head_cs = ws.engine.get_changeset(head).await?;
    let is_final = !run.next_op_folds();
    let head_git = conflict::git_id(ws, head).await?;
    let (text, edit, cleanup) = if !is_final {
        (
            need_text(&run.dir, "message-squash")?,
            false,
            message::Cleanup::Verbatim,
        )
    } else if seqstate::exists(&run.dir, "message-fixup") {
        let edit = item.keep == Keep::Edit;
        (
            need_text(&run.dir, "message-fixup")?,
            edit,
            if edit {
                message::Cleanup::Default
            } else {
                message::Cleanup::Verbatim
            },
        )
    } else {
        let squash = need_text(&run.dir, "message-squash")?;
        seqstate::write(&run.gitdir, "SQUASH_MSG", squash.as_bytes())?;
        (squash, true, message::Cleanup::Default)
    };
    run.write("amend", &format!("{head_git}\n"))?;
    let mut preds = Vec::new();
    for line in read_text(&run.dir, "rewritten-pending")?
        .unwrap_or_default()
        .lines()
        .filter(|l| !l.is_empty())
        .take(MAX_TODO)
    {
        if let Ok(o) = line.trim().parse::<GitOid>() {
            preds.push(conflict::changeset_of(ws, o).await?);
        }
    }
    preds.push(conflict::changeset_of(ws, oid).await?);
    let stop_text = text.clone();
    let plan = Amend {
        head,
        head_cs: &head_cs,
        root,
        text: text.into_bytes(),
        source: MsgSource::Message,
        edit,
        verify: false,
        cleanup,
        // `git commit` is quiet unless the editor ran.
        summary: edit,
        reflog: reflog.to_owned(),
        preds,
        kind: RewriteOp::Squash,
    };
    let cs = ws
        .engine
        .get_changeset(conflict::changeset_of(ws, oid).await?)
        .await?;
    let Some((_, new_git)) = amend_head(ctx, ws, run, plan).await? else {
        write_stop(ws, run, oid, &cs, stop_text.as_bytes()).await?;
        seqstate::write(&run.gitdir, "MERGE_MSG", stop_text.as_bytes())?;
        eprintln!("Could not apply {}... {}", run.short(&oid), item.arg);
        return Ok(Step::Stopped(1));
    };
    seqstate::remove(&run.dir, "amend");
    if is_final {
        for name in ["message-squash", "message-fixup", "current-fixups"] {
            seqstate::remove(&run.dir, name);
        }
    }
    run.record_rewritten(oid, new_git)?;
    Ok(Step::Done)
}

// ---------------------------------------------------------------- other commands

/// The paths whose entries differ between two trees.
async fn changed_paths(ws: &WsCtx, from: TreeId, to: TreeId) -> Result<Vec<RepoPath>> {
    Ok(ws
        .engine
        .leafdiff(from, to, None)
        .await?
        .into_iter()
        .map(|(p, _)| p)
        .collect())
}

/// `git diff-tree -p` of `cs` against its parent, as `rebase-merge/patch` holds it.
async fn patch_text(ws: &WsCtx, cs: &Changeset) -> Result<Vec<u8>> {
    let from = root_of(ws, cs.parents.first().copied()).await?;
    let opts = DiffOptions::default();
    let changes = ws
        .engine
        .diff_trees(from, cs.root, None, &opts)
        .await
        .map_err(fatal)?;
    let dctx = DiffCtx {
        opts: &opts,
        content: &ws.engine,
        attrs: &NoAttributes,
    };
    let mut out = Vec::new();
    write_patch(&mut out, &dctx, &changes)
        .await
        .map_err(fatal)?;
    Ok(out)
}

async fn exec(ctx: &Ctx, ws: &WsCtx, run: &Run, text: &str) -> Result<Step> {
    eprintln!("Executing: {text}");
    let mut cmd = ctx.child(ctx.shell())?;
    cmd.arg("-c")
        .arg(text)
        .current_dir(ws.projection.toplevel())
        .stdin(Stdio::null());
    let status = tokio::task::spawn_blocking(move || cmd.status())
        .await
        .map_err(|e| fatal(format!("asc: exec: {e}")))?
        .map_err(|e| fatal(format!("asc: cannot run '{text}': {e}")))?;
    if status.success() {
        return Ok(Step::Done);
    }
    let _ = run;
    let t = tool(ctx);
    eprint!(
        "warning: execution failed: {text}\nYou can fix the problem, and then run\n\n  {t} rebase --continue\n\n\n"
    );
    Ok(Step::Stopped(1))
}

/// `break`: stops with HEAD as it is.
async fn stop_at_head(ctx: &Ctx, ws: &WsCtx) -> Result<Step> {
    let head = ws
        .head_commit()
        .await?
        .ok_or_else(|| fatal("asc: HEAD is unborn"))?;
    eprintln!(
        "Stopped at {}",
        switch::commit_line(ctx, ws, head)
            .await?
            .replacen(' ', " (", 1)
            + ")"
    );
    Ok(Step::Stopped(0))
}

fn rewritten_ref(label: &str) -> Result<String> {
    let full = format!("refs/rewritten/{label}");
    match RefName::new(full.clone().into_bytes()) {
        Ok(_) if !label.is_empty() && !label.contains("..") => Ok(full),
        _ => Err(fatal(format!("'{label}' is not a valid label"))),
    }
}

/// Points `full` at `new` (or removes it) through the ref chokepoint.
async fn set_ref(
    ctx: &Ctx,
    ws: &WsCtx,
    full: &str,
    new: Option<ChangesetId>,
    reason: &str,
) -> Result<()> {
    let old = switch::ref_target(ws, full).await?;
    if old.is_none() && new.is_none() {
        return Ok(());
    }
    let txn = RefTxn {
        updates: vec![RefUpdate {
            name: switch::ref_name(full)?,
            expect: old.map_or(Expect::Absent, Expect::Value),
            new: new.map(RefTarget::Changeset),
            expires_at: None,
        }],
        actor: actor(),
        reason: String::new(),
        op_id: OpId::new().0,
    };
    ctx.update_local_refs(txn, reason, StagingAfterMove::Keep)
        .await
}

/// `label <name>`: `refs/rewritten/<name>` names HEAD. The label is listed in `refs-to-delete`.
async fn label(ctx: &Ctx, ws: &WsCtx, run: &Run, name: &str) -> Result<Step> {
    let full = rewritten_ref(name.trim())?;
    let head = ws
        .head_commit()
        .await?
        .ok_or_else(|| fatal("asc: HEAD is unborn"))?;
    set_ref(
        ctx,
        ws,
        &full,
        Some(head),
        &format!("rebase (label): {}", name.trim()),
    )
    .await?;
    let mut listed = read_text(&run.dir, "refs-to-delete")?.unwrap_or_default();
    listed.push_str(&format!("{full}\n"));
    run.write("refs-to-delete", &listed)?;
    Ok(Step::Done)
}

/// The commit a `reset` or `merge` operand names: a label, else any revision.
async fn labelled(ctx: &Ctx, ws: &WsCtx, name: &str) -> Result<ChangesetId> {
    if let Ok(full) = rewritten_ref(name)
        && let Some(target) = switch::ref_target(ws, &full).await?
    {
        return switch::peel(ws, target).await;
    }
    commit_named(ctx, name)
        .await?
        .ok_or_else(|| fatal(format!("could not resolve '{name}'")))
}

/// The label before the ` # oneline` of a `reset` or `merge` argument.
fn operand(arg: &str, comment: char) -> &str {
    let marker = format!(" {comment} ");
    arg.split(&marker).next().unwrap_or(arg).trim()
}

/// `reset <label>`: HEAD (detached) and the work tree go to the labelled commit.
async fn reset(ctx: &Ctx, ws: &WsCtx, arg: &str) -> Result<Step> {
    let target = labelled(ctx, ws, operand(arg, message::comment_char(ctx) as char)).await?;
    let head = ws.head_commit().await?;
    if head != Some(target) {
        detach_to(ctx, ws, target, operand(arg, '#')).await?;
    }
    Ok(Step::Done)
}

/// `merge -C <commit> <label> # oneline`: a merge commit of HEAD and the labelled commit, with the
/// original merge's message and author. A conflicting merge stops the rebase.
async fn merge(ctx: &Ctx, ws: &WsCtx, run: &mut Run, item: &Item) -> Result<Step> {
    let comment = run.comment as char;
    let names: Vec<&str> = operand(&item.arg, comment).split_whitespace().collect();
    if names.len() != 1 {
        return Err(fatal(
            "asc: only merges of two parents are supported in a rebase",
        ));
    }
    let theirs = labelled(ctx, ws, names[0]).await?;
    let head = ws
        .head_commit()
        .await?
        .ok_or_else(|| fatal("asc: HEAD is unborn"))?;
    let original = match item.oid {
        Some(o) => Some(
            ws.engine
                .get_changeset(conflict::changeset_of(ws, o).await?)
                .await?,
        ),
        None => None,
    };
    let head_root = root_of(ws, Some(head)).await?;
    let their_root = root_of(ws, Some(theirs)).await?;
    let bases = HistoryApi::merge_base(&ws.engine, head, &[theirs], false)
        .await
        .map_err(fatal)?;
    let base_root = match bases.first() {
        Some(b) => root_of(ws, Some(*b)).await?,
        None => CliEngine::empty_tree_id(),
    };
    let sides = Sides {
        base: base_root,
        ours: head_root,
        theirs: their_root,
    };
    let labels = Labels {
        base: "merge base".into(),
        ours: "HEAD".into(),
        theirs: names[0].to_owned(),
    };
    let (merge_opts, differ) =
        conflict::merge_options(ctx, ws, &sides, conflict::favor_of(&[])?, labels).await?;
    let outcome = ws
        .engine
        .merge_trees(sides.base, sides.ours, sides.theirs, &merge_opts)
        .await
        .map_err(fatal)?;
    conflict::refuse_unsupported(&outcome)?;
    if !outcome.is_clean() {
        conflict::print_messages(&conflict::messages(&outcome, &differ, "HEAD", names[0]));
        let t = tool(ctx);
        eprintln!(
            "error: could not merge {}\nCannot recreate this merge: resolve the conflicts by hand, then run \"{t} rebase --abort\".",
            names[0]
        );
        return Ok(Step::Stopped(1));
    }
    conflict::precheck(ctx, ws, ReplayOp::Pick, head_root, &outcome, false).await?;
    conflict::materialise(ctx, ws, head_root, head_root, &outcome).await?;
    let idents = conflict::identities(ctx)?;
    let (text, author) = match &original {
        Some(o) => (o.message.clone(), o.author.clone()),
        None => (
            format!("Merge branch '{}'\n", names[0]).into_bytes(),
            idents.author.clone(),
        ),
    };
    seqstate::write(&run.gitdir, "COMMIT_EDITMSG", &text)?;
    let (new, new_git) = conflict::write_commit(
        ws,
        NewCommit {
            parents: vec![head, theirs],
            author,
            committer: idents.committer.clone(),
            message: text,
            root: outcome.root,
        },
    )
    .await?;
    let reflog = format!("rebase (merge): {}", names[0]);
    let op = conflict::move_head(
        ctx,
        ws,
        Some(head),
        new,
        &reflog,
        StagingAfterMove::Set(Vec::new()),
        Some(&idents.committer),
    )
    .await?;
    if let Some(o) = item.oid {
        let old = conflict::changeset_of(ws, o).await?;
        ext::rewrite_sink().record(new, &[old], RewriteOp::Rebase, op);
    } else {
        ext::rewrite_sink().record(new, &[], RewriteOp::New, op);
    }
    ext::index_sink()
        .on_commit(ws.workspace.ws.id, new)
        .await
        .map_err(fatal)?;
    ws_ops::clear(&ws.db, ws.workspace.ws.id).await?;
    if let Some(o) = item.oid {
        run.record_rewritten(o, new_git)?;
    }
    Ok(Step::Done)
}

/// `update-ref <ref>`: HEAD is the new value of the ref when the rebase ends.
async fn update_ref(ws: &WsCtx, run: &Run, name: &str) -> Result<Step> {
    let head = ws
        .head_commit()
        .await?
        .ok_or_else(|| fatal("asc: HEAD is unborn"))?;
    let head_git = conflict::git_id(ws, head).await?;
    let mut entries = read_update_refs(&run.dir)?;
    if let Some(e) = entries.iter_mut().find(|e| e.0 == name.trim()) {
        e.2 = Some(head_git);
    }
    write_update_refs(run, &entries)?;
    Ok(Step::Done)
}

type RefEntry = (String, GitOid, Option<GitOid>);

fn read_update_refs(dir: &Path) -> Result<Vec<RefEntry>> {
    let text = read_text(dir, "update-refs")?.unwrap_or_default();
    let lines: Vec<&str> = text.lines().collect();
    let bad = || fatal("asc: rebase-merge/update-refs is malformed");
    let mut out = Vec::new();
    for chunk in lines.chunks(3).take(MAX_TODO) {
        let [name, before, after] = chunk else {
            return Err(bad());
        };
        let before: GitOid = before.trim().parse().map_err(|_| bad())?;
        let after = if after.trim().bytes().all(|b| b == b'0') {
            None
        } else {
            Some(after.trim().parse::<GitOid>().map_err(|_| bad())?)
        };
        out.push(((*name).to_owned(), before, after));
    }
    Ok(out)
}

fn write_update_refs(run: &Run, entries: &[RefEntry]) -> Result<()> {
    let mut text = String::new();
    for (name, before, after) in entries {
        let after = after.map_or_else(|| "0".repeat(40), |o| o.to_string());
        text.push_str(&format!("{name}\n{before}\n{after}\n"));
    }
    run.write("update-refs", &text)
}

// ---------------------------------------------------------------- finishing

async fn finish(ctx: &Ctx, ws: &WsCtx, run: &mut Run) -> Result<ExitCode> {
    let Some(head) = ws.head_commit().await? else {
        return Err(fatal("asc: HEAD is unborn"));
    };
    if let Some(branch) = run.branch().map(str::to_owned) {
        let full = RefName::new(run.head_name.clone().into_bytes()).map_err(fatal)?;
        let old = switch::ref_target(ws, &run.head_name).await?;
        let saved = switch::Saved::capture(ws).await?;
        switch::point_head(
            ws,
            &Head::Branch(branch.clone()),
            &run.head_name,
            saved.base,
        )
        .await?;
        let txn = RefTxn {
            updates: vec![RefUpdate {
                name: full,
                expect: old.map_or(Expect::Absent, Expect::Value),
                new: Some(RefTarget::Changeset(head)),
                expires_at: None,
            }],
            actor: actor(),
            reason: String::new(),
            op_id: OpId::new().0,
        };
        let reflog = format!("rebase (finish): {} onto {}", run.head_name, run.onto);
        if let Err(e) = ctx
            .update_local_refs(txn, &reflog, StagingAfterMove::Keep)
            .await
        {
            switch::restore_head(ws, &saved).await;
            return Err(e);
        }
    }
    let moved = apply_update_refs(ctx, ws, run).await?;
    let pairs = rewritten_pairs(run)?;
    if !pairs.is_empty() {
        let runner = Runner::for_ctx(ctx)?;
        let env = HookEnv::of(ctx, "rebase");
        let _ = runner
            .run(
                Hook::PostRewrite {
                    kind: RewriteKind::Rebase,
                    pairs,
                },
                &env,
            )
            .await?;
    }
    let stash = autostash_of(run)?;
    let head_name = run.head_name.clone();
    delete_rewritten_refs(ctx, ws).await?;
    // git leaves its state directory behind when a ref could not be moved.
    if moved.failed.is_empty() {
        let _ = std::fs::remove_dir_all(&run.dir);
    }
    if let Some(s) = stash {
        apply_autostash(ctx, ws, s).await?;
    }
    if !run.quiet {
        let name = if head_name == "detached HEAD" {
            "detached HEAD"
        } else {
            head_name.as_str()
        };
        eprintln!("Successfully rebased and updated {name}.");
    }
    for e in &moved.errors {
        eprintln!("{e}");
    }
    if !run.quiet && !moved.updated.is_empty() {
        eprintln!("Updated the following refs with --update-refs:");
        for r in &moved.updated {
            eprintln!("\t{r}");
        }
    }
    if !moved.failed.is_empty() {
        eprintln!("Failed to update the following refs with --update-refs:");
        for r in &moved.failed {
            eprintln!("\t{r}");
        }
        return Ok(ExitCode::from(1));
    }
    Ok(ExitCode::SUCCESS)
}

/// What moving the refs of `update-ref` commands did.
#[derive(Default)]
struct MovedRefs {
    updated: Vec<String>,
    failed: Vec<String>,
    errors: Vec<String>,
}

/// Moves the refs `update-ref` commands recorded; a ref that moved since the rebase began is
/// left alone with git's error.
async fn apply_update_refs(ctx: &Ctx, ws: &WsCtx, run: &Run) -> Result<MovedRefs> {
    let mut out = MovedRefs::default();
    for (name, before, after) in read_update_refs(&run.dir)? {
        let Some(after) = after else { continue };
        let now = match switch::ref_target(ws, &name).await? {
            Some(RefTarget::Changeset(c)) => Some(conflict::git_id(ws, c).await?),
            _ => None,
        };
        if now != Some(before) {
            let at = now.map_or_else(|| "0".repeat(40), |n| n.to_string());
            out.errors.push(format!(
                "error: update_ref failed for ref '{name}': cannot lock ref '{name}': is at {at} but expected {before}"
            ));
            out.failed.push(name);
            continue;
        }
        let new = conflict::changeset_of(ws, after).await?;
        set_ref(ctx, ws, &name, Some(new), "rebase (update-ref)").await?;
        out.updated.push(name);
    }
    Ok(out)
}

/// Removes every `refs/rewritten/*` label.
async fn delete_rewritten_refs(ctx: &Ctx, ws: &WsCtx) -> Result<()> {
    let names: Vec<String> = ws
        .db
        .refs()
        .list(ws.repo.key, "refs/rewritten/")
        .await?
        .into_iter()
        .take(MAX_TODO)
        .map(|e| String::from_utf8_lossy(e.name.as_bytes()).into_owned())
        .collect();
    for n in names {
        set_ref(ctx, ws, &n, None, "rebase (cleanup)").await?;
    }
    Ok(())
}

fn rewritten_pairs(run: &Run) -> Result<Vec<(GitOid, GitOid)>> {
    let mut out = Vec::new();
    for line in read_text(&run.dir, "rewritten-list")?
        .unwrap_or_default()
        .lines()
        .take(MAX_TODO)
    {
        let (a, b) = line
            .split_once(' ')
            .ok_or_else(|| fatal("asc: rebase-merge/rewritten-list is malformed"))?;
        let parse = |s: &str| {
            s.trim()
                .parse::<GitOid>()
                .map_err(|_| fatal("asc: rebase-merge/rewritten-list is malformed"))
        };
        out.push((parse(a)?, parse(b)?));
    }
    Ok(out)
}

fn autostash_of(run: &Run) -> Result<Option<GitOid>> {
    match read_text(&run.dir, "autostash")? {
        Some(t) => Ok(Some(t.trim().parse().map_err(|_| {
            fatal("asc: rebase-merge/autostash holds a bad object id")
        })?)),
        None => Ok(None),
    }
}

// ---------------------------------------------------------------- continue, skip, abort, quit

/// Resets the staged paths and the paths a stopped commit touched back to `target_root`.
async fn reset_touched(ctx: &Ctx, ws: &WsCtx, target_root: TreeId) -> Result<()> {
    let rows = stage::overlay(&ws.db, ws.workspace.ws.id, StageSet::Default).await?;
    let head_root = root_of(ws, ws.head_commit().await?).await?;
    let mut paths: Vec<RepoPath> = rows
        .iter()
        .filter_map(|r| RepoPath::new(r.path.clone()).ok())
        .collect();
    paths.extend(changed_paths(ws, head_root, target_root).await?);
    paths.sort();
    paths.dedup();
    sync_worktree(ctx, target_root, &paths, true).await?;
    stage::clear(&ws.db, ws.workspace.ws.id, StageSet::Default).await?;
    ext::index_sink()
        .on_stage_changed(ws.workspace.ws.id, &[])
        .await
        .map_err(fatal)
}

/// git's advice when `--continue` finds staged changes with nothing stopped on a conflict.
fn staged_advice(ctx: &Ctx) -> CliError {
    let t = tool(ctx);
    CliError::Raw(
        format!(
            "error: you have staged changes in your working tree\nIf these changes are meant to be squashed into the previous commit, run:\n\n  {t} commit --amend \n\nIf they are meant to go into a new commit, run:\n\n  {t} commit \n\nIn both cases, once you're done, continue with:\n\n  {t} rebase --continue\n"
        ),
        1,
    )
}

async fn resume(ctx: &Ctx, ws: &WsCtx, run: &mut Run) -> Result<ExitCode> {
    let head = ws
        .head_commit()
        .await?
        .ok_or_else(|| fatal("asc: HEAD is unborn"))?;
    let head_root = root_of(ws, Some(head)).await?;
    let rows = stage::overlay(&ws.db, ws.workspace.ws.id, StageSet::Default).await?;
    let fresh = workspaces::get(&ws.db, ws.workspace.ws.id)
        .await?
        .ok_or_else(|| fatal("asc: this workspace is not registered in state.db"))?;
    let parts = WcParts::new(ctx, ws, ws.projection.git_dir().join("index"))?;
    let st = status::compute(&parts.wc(&ws.engine), &ws.db, &fresh)
        .await
        .map_err(fatal)?;
    let unmerged: Vec<String> = rows
        .iter()
        .filter(|r| r.stage > 0)
        .map(|r| String::from_utf8_lossy(&r.path).into_owned())
        .collect();
    if !unmerged.is_empty() || !st.unstaged.is_empty() {
        let mut shown = std::collections::BTreeSet::new();
        for p in &unmerged {
            shown.insert(format!("{p}: needs merge"));
        }
        for c in &st.unstaged {
            shown.insert(format!(
                "{}: needs update",
                String::from_utf8_lossy(c.path.as_bytes())
            ));
        }
        let mut out = String::new();
        for l in shown {
            out.push_str(&l);
            out.push('\n');
        }
        out.push_str(
            "You must edit all merge conflicts and then\nmark them as resolved using git add\n",
        );
        std::io::stdout().write_all(out.as_bytes())?;
        return Ok(ExitCode::from(1));
    }
    let changes = stagebuild::rows_to_changes(ws, head_root, &rows).await?;
    let root = ws.engine.apply(head_root, &changes).await?;
    let staged = root != head_root;
    let current = run.current().cloned();
    let amend = seqstate::read_oid(&run.dir, "amend")?;
    let stopped = seqstate::read_oid(&run.dir, "stopped-sha")?;
    if amend.is_none()
        && let Some(picked) = stopped
    {
        // A pick stopped on a conflict, an empty result or a message that could not be written.
        let old = conflict::changeset_of(ws, picked).await?;
        let folding = current.as_ref().filter(|i| i.op.is_fixup());
        if !staged {
            // Nothing left to commit (the resolution equals HEAD, or the result is empty): the
            // commit is given up and the rebase goes on.
            run.record_rewritten(picked, conflict::git_id(ws, head).await?)?;
            run.clear_stop_uncommitted();
        } else if let Some(item) = folding {
            let item = item.clone();
            let step =
                fold_commit(ctx, ws, run, &item, picked, root, head, "rebase (continue)").await?;
            if let Step::Stopped(code) = step {
                return Ok(ExitCode::from(code));
            }
            run.clear_stop();
        } else {
            let author = parse_author_script(&need_text(&run.dir, "author-script")?)?;
            let text = need_text(&run.dir, "message")?;
            seqstate::write(&run.gitdir, "COMMIT_EDITMSG", text.as_bytes())?;
            let final_msg =
                message::cleanup(text.as_bytes(), message::Cleanup::Strip, true, run.comment);
            let idents = conflict::identities(ctx)?;
            let (new, new_git) = conflict::write_commit(
                ws,
                NewCommit {
                    parents: vec![head],
                    author: author.clone(),
                    committer: idents.committer.clone(),
                    message: final_msg.clone(),
                    root,
                },
            )
            .await?;
            let subject = message::subject(&final_msg);
            let op = conflict::move_head(
                ctx,
                ws,
                Some(head),
                new,
                &format!("rebase (continue): {subject}"),
                StagingAfterMove::Set(Vec::new()),
                Some(&idents.committer),
            )
            .await?;
            ext::rewrite_sink().record(new, &[old], RewriteOp::Rebase, op);
            ext::index_sink()
                .on_commit(ws.workspace.ws.id, new)
                .await
                .map_err(fatal)?;
            ws_ops::clear(&ws.db, ws.workspace.ws.id).await?;
            run.record_rewritten(picked, new_git)?;
            run.clear_stop();
            print_summary(
                ws,
                &new_git.to_string(),
                &subject,
                &author,
                &idents.committer,
                head_root,
                root,
                false,
            )
            .await?;
            let runner = Runner::for_ctx(ctx)?;
            let env = HookEnv::of(ctx, "rebase")
                .with_index(run.gitdir.join("index"))
                .without_editor();
            let _ = runner.run(Hook::PostCommit, &env).await?;
        }
    } else if let Some(to_amend) = amend {
        let author_kept = read_text(&run.dir, "author-script")?;
        // `edit` (or a `reword` whose editor failed) stopped with HEAD at `amend`.
        if staged {
            if head != conflict::changeset_of(ws, to_amend).await? {
                return Err(CliError::Raw(
                    "error: \nYou have uncommitted changes in your working tree. Please, commit them\nfirst and then run 'git rebase --continue' again.".to_owned(),
                    1,
                ));
            }
            let head_cs = ws.engine.get_changeset(head).await?;
            let text = need_text(&run.dir, "message")
                .map(String::into_bytes)
                .unwrap_or_else(|_| head_cs.message.clone());
            // The commit made for staged changes opens the message editor; a reword also runs
            // the commit-msg hook.
            let reword = current.as_ref().is_some_and(|i| i.op == Op::Reword);
            let preds = match stopped {
                Some(s) => vec![conflict::changeset_of(ws, s).await?],
                None => Vec::new(),
            };
            let plan = Amend {
                head,
                head_cs: &head_cs,
                root,
                text,
                source: MsgSource::Message,
                edit: true,
                verify: reword,
                cleanup: message::Cleanup::Default,
                summary: true,
                reflog: "rebase (continue)".to_owned(),
                preds,
                kind: RewriteOp::Rebase,
            };
            if amend_head(ctx, ws, run, plan).await?.is_none() {
                return Ok(ExitCode::from(1));
            }
        }
        if let Some(s) = stopped {
            let now = ws
                .head_commit()
                .await?
                .ok_or_else(|| fatal("asc: HEAD is unborn"))?;
            run.record_rewritten(s, conflict::git_id(ws, now).await?)?;
        }
        if staged {
            run.clear_stop();
        } else {
            run.clear_stop_uncommitted();
            // git leaves the author of an `edit` stop behind when nothing was amended.
            if let Some(a) = author_kept {
                run.write("author-script", &a)?;
            }
        }
    } else if staged {
        return Err(staged_advice(ctx));
    }
    drive(ctx, ws, run).await
}

async fn skip(ctx: &Ctx, ws: &WsCtx, run: &mut Run) -> Result<ExitCode> {
    let head = ws.head_commit().await?;
    let head_root = root_of(ws, head).await?;
    reset_touched(ctx, ws, head_root).await?;
    if seqstate::exists(&run.dir, "amend") || !seqstate::exists(&run.dir, "stopped-sha") {
        // Not stopped on a failed commit (an `edit`, a `break` or an `exec`): skipping is
        // continuing with a clean tree.
        return resume(ctx, ws, run).await;
    }
    if run.current().is_some_and(|i| i.op.is_fixup()) {
        unfold_last(run)?;
    }
    run.clear_stop();
    drive(ctx, ws, run).await
}

/// Takes the last `squash`/`fixup` out of the collected message when it is skipped.
fn unfold_last(run: &Run) -> Result<()> {
    let fixups = read_text(&run.dir, "current-fixups")?.unwrap_or_default();
    let mut lines: Vec<&str> = fixups.lines().filter(|l| !l.is_empty()).collect();
    lines.pop();
    if lines.is_empty() {
        for name in ["message-squash", "message-fixup", "current-fixups"] {
            seqstate::remove(&run.dir, name);
        }
        return Ok(());
    }
    let c = run.comment as char;
    let squash = need_text(&run.dir, "message-squash")?;
    let n = lines.len() + 2;
    let cut = squash
        .rfind(&format!("\n{c} This is the commit message #{n}:"))
        .or_else(|| squash.rfind(&format!("\n{c} The commit message #{n} will be skipped:")));
    if let Some(i) = cut {
        run.write(
            "message-squash",
            &todo::renumber(&squash[..i], lines.len() + 1, c),
        )?;
    }
    run.write("current-fixups", &lines.join("\n"))
}

async fn abort(ctx: &Ctx, ws: &WsCtx, run: Run) -> Result<ExitCode> {
    ext::op_sink().snapshot_wc();
    let head = ws.head_commit().await?;
    let orig = conflict::changeset_of(ws, run.orig_head).await?;
    let target_root = root_of(ws, Some(orig)).await?;
    reset_touched(ctx, ws, target_root).await?;
    // ORIG_HEAD keeps the value the rebase started with.
    if head != Some(orig) {
        conflict::move_head(
            ctx,
            ws,
            head,
            orig,
            "rebase (abort): returning to original",
            StagingAfterMove::Set(Vec::new()),
            None,
        )
        .await?;
    }
    if let Some(branch) = run.branch() {
        let saved = switch::Saved::capture(ws).await?;
        switch::point_head(
            ws,
            &Head::Branch(branch.to_owned()),
            &run.head_name,
            saved.base,
        )
        .await?;
    }
    let stash = autostash_of(&run)?;
    run.clear_stop();
    for name in ["REBASE_HEAD", "SQUASH_MSG"] {
        seqstate::remove(&run.gitdir, name);
    }
    delete_rewritten_refs(ctx, ws).await?;
    let _ = std::fs::remove_dir_all(&run.dir);
    if let Some(s) = stash {
        apply_autostash(ctx, ws, s).await?;
    }
    Ok(ExitCode::SUCCESS)
}

async fn quit(ctx: &Ctx, ws: &WsCtx, run: Run) -> Result<ExitCode> {
    if let Some(s) = autostash_of(&run)? {
        store_autostash(ctx, ws, s).await?;
        let t = tool(ctx);
        eprintln!(
            "Autostash exists; creating a new stash entry.\nYour changes are safe in the stash.\nYou can run \"{t} stash pop\" or \"{t} stash drop\" at any time."
        );
    }
    delete_rewritten_refs(ctx, ws).await?;
    let _ = std::fs::remove_dir_all(&run.dir);
    Ok(ExitCode::SUCCESS)
}

/// `--edit-todo`: the commands left, in the editor. A list that does not parse is shown as it is.
async fn edit_todo(ctx: &Ctx, ws: &WsCtx, run: &mut Run) -> Result<ExitCode> {
    let abbrev = run.abbrev;
    let short = |o: &GitOid| o.to_string()[..abbrev].to_owned();
    let path = run.dir.join("git-rebase-todo");
    let raw = need_text(&run.dir, "git-rebase-todo")?;
    if let Ok(done) = parse_state(
        ctx,
        ws,
        &run.dir,
        &read_text(&run.dir, "done")?.unwrap_or_default(),
    )
    .await
    {
        run.done = done;
    }
    let body = match parse_state(ctx, ws, &run.dir, &raw).await {
        Ok(list) => todo::render_short(&list, &short),
        Err(Ok(errors)) => {
            for e in errors {
                eprintln!("error: {e}");
            }
            raw.clone()
        }
        Err(Err(e)) => return Err(e),
    };
    run.write(
        "git-rebase-todo",
        &todo::edit_todo_text(&body, tool(ctx), run.comment),
    )?;
    if !todo::edit_todo(ctx, &path).await? {
        run.write("git-rebase-todo", &raw)?;
        return Err(CliError::Raw(
            "error: could not execute the todo command".to_owned(),
            1,
        ));
    }
    let edited = need_text(&run.dir, "git-rebase-todo")?;
    let stripped = todo::strip_comments(&edited, run.comment);
    match parse_state(ctx, ws, &run.dir, &stripped).await {
        Ok(list) => {
            run.todo = list;
            run.write("git-rebase-todo", &todo::render_full(&run.todo))?;
            let total = todo::count(&run.done) + todo::count(&run.todo);
            run.write("end", &format!("{total}\n"))?;
            Ok(ExitCode::SUCCESS)
        }
        Err(Ok(errors)) => {
            run.write("git-rebase-todo", &edited)?;
            Err(parse_failure(ctx, &errors, true))
        }
        Err(Err(e)) => Err(e),
    }
}

#[allow(clippy::too_many_arguments)]
async fn print_summary(
    ws: &WsCtx,
    git: &str,
    subject: &str,
    author: &Signature,
    committer: &Signature,
    from: TreeId,
    to: TreeId,
    show_date: bool,
) -> Result<()> {
    let mut out = Vec::new();
    writeln!(out, "[detached HEAD {}] {subject}", &git[..7])?;
    if author.name() != committer.name() || author.email() != committer.email() {
        writeln!(
            out,
            " Author: {} <{}>",
            String::from_utf8_lossy(author.name()),
            String::from_utf8_lossy(author.email())
        )?;
    }
    if show_date {
        let date = super::format::date::Mode::default().render(
            author.time(),
            author.tz_min(),
            author.time(),
        );
        writeln!(out, " Date: {date}")?;
    }
    let opts = DiffOptions {
        renames: Some(RenameOptions {
            limit: 100,
            ..RenameOptions::default()
        }),
        ..DiffOptions::default()
    };
    let changes = ws
        .engine
        .diff_trees(from, to, None, &opts)
        .await
        .map_err(fatal)?;
    let dctx = DiffCtx {
        opts: &opts,
        content: &ws.engine,
        attrs: &NoAttributes,
    };
    write_shortstat(&mut out, &dctx, &changes)
        .await
        .map_err(fatal)?;
    write_summary(&mut out, &changes, &opts).map_err(fatal)?;
    std::io::stdout().write_all(&out)?;
    Ok(())
}

// ---------------------------------------------------------------- autostash

/// `git stash create autostash`, then a hard reset to HEAD. Prints git's "Created autostash".
async fn create_autostash(
    ctx: &Ctx,
    ws: &WsCtx,
    head: ChangesetId,
    head_root: TreeId,
) -> Result<GitOid> {
    let (index_root, unmerged) = index_tree(ws, head_root).await?;
    if !unmerged.is_empty() {
        return Err(fatal("cannot rebase: you have unmerged files"));
    }
    let dirty = stagebuild::stat_dirty_rows(ws, &ctx.config).await?;
    let changes = stagebuild::rows_to_changes(ws, index_root, &dirty).await?;
    let work_root = ws.engine.apply(index_root, &changes).await?;
    stagebuild::index_spine(ws, work_root, &changes).await?;
    let idents = conflict::identities(ctx)?;
    let (who, writer) = (idents.committer.clone(), idents.author.clone());
    let branch = match ws.projection.read_head()? {
        Head::Branch(b) => b,
        Head::Detached(_) => "(no branch)".to_owned(),
    };
    let head_cs = ws.engine.get_changeset(head).await?;
    let head_git = conflict::git_id(ws, head).await?;
    let abbrev = &head_git.to_string()[..switch::abbrev_len(ctx)];
    let (index_id, _) = conflict::write_commit(
        ws,
        NewCommit {
            parents: vec![head],
            author: writer.clone(),
            committer: who.clone(),
            message: format!(
                "index on {branch}: {abbrev} {}\n",
                first_line(&head_cs.message)
            )
            .into_bytes(),
            root: index_root,
        },
    )
    .await?;
    let (_, stash_git) = conflict::write_commit(
        ws,
        NewCommit {
            parents: vec![head, index_id],
            author: writer,
            committer: who,
            message: format!("On {branch}: autostash").into_bytes(),
            root: work_root,
        },
    )
    .await?;
    println!(
        "Created autostash: {}",
        &stash_git.to_string()[..switch::abbrev_len(ctx)]
    );
    // `reset --hard`: the staged and the modified paths go back to HEAD.
    let mut paths = changed_paths(ws, head_root, index_root).await?;
    paths.extend(
        dirty
            .iter()
            .filter_map(|r| RepoPath::new(r.path.clone()).ok()),
    );
    paths.sort();
    paths.dedup();
    sync_worktree(ctx, head_root, &paths, true).await?;
    stage::clear(&ws.db, ws.workspace.ws.id, StageSet::Default).await?;
    ext::index_sink()
        .on_stage_changed(ws.workspace.ws.id, &[])
        .await
        .map_err(fatal)?;
    Ok(stash_git)
}

/// `git stash apply <stash>`: merges the stashed work tree onto HEAD and leaves the result in the
/// work tree (new files staged), printing git's lines.
async fn apply_autostash(ctx: &Ctx, ws: &WsCtx, stash: GitOid) -> Result<()> {
    let id = conflict::changeset_of(ws, stash).await?;
    let cs = ws.engine.get_changeset(id).await?;
    let Some(&base_commit) = cs.parents.first() else {
        return Err(fatal("asc: the autostash has no parent"));
    };
    let head = ws
        .head_commit()
        .await?
        .ok_or_else(|| fatal("asc: HEAD is unborn"))?;
    let head_root = root_of(ws, Some(head)).await?;
    let base_root = root_of(ws, Some(base_commit)).await?;
    let sides = Sides {
        base: base_root,
        ours: head_root,
        theirs: cs.root,
    };
    let labels = Labels {
        base: "Stash base".into(),
        ours: "Updated upstream".into(),
        theirs: "Stashed changes".into(),
    };
    let (merge_opts, differ) =
        conflict::merge_options(ctx, ws, &sides, conflict::favor_of(&[])?, labels).await?;
    let outcome = ws
        .engine
        .merge_trees(sides.base, sides.ours, sides.theirs, &merge_opts)
        .await
        .map_err(fatal)?;
    conflict::refuse_unsupported(&outcome)?;
    conflict::precheck(ctx, ws, ReplayOp::Pick, head_root, &outcome, false).await?;
    conflict::print_messages(&conflict::messages(
        &outcome,
        &differ,
        "Updated upstream",
        "Stashed changes",
    ));
    conflict::materialise(ctx, ws, head_root, head_root, &outcome).await?;
    seqstate::write_auto_merge(
        ws.projection.git_dir(),
        conflict::git_tree_id(ws, outcome.root).await?,
    )?;
    // Only new files stay staged; edits are left for `status` to show as unstaged.
    let rows = stage::overlay(&ws.db, ws.workspace.ws.id, StageSet::Default).await?;
    let mut keep = Vec::new();
    for r in rows {
        let new_file = match RepoPath::new(r.path.clone()) {
            Ok(p) => ws.engine.lookup(head_root, &p).await?.is_none() && r.file.is_some(),
            Err(_) => false,
        };
        if r.stage > 0 || new_file {
            keep.push(r);
        }
    }
    stage::replace(&ws.db, ws.workspace.ws.id, StageSet::Default, &keep).await?;
    ext::index_sink()
        .on_stage_changed(ws.workspace.ws.id, &[])
        .await
        .map_err(fatal)?;
    if outcome.is_clean() {
        eprintln!("Applied autostash.");
    } else {
        store_autostash(ctx, ws, stash).await?;
        let t = tool(ctx);
        eprintln!(
            "Applying autostash resulted in conflicts.\nYour changes are safe in the stash.\nYou can run \"{t} stash pop\" or \"{t} stash drop\" at any time."
        );
    }
    Ok(())
}

/// Keeps the autostash commit reachable: `refs/stash` when free, else `refs/asc/autostash/<id>`.
async fn store_autostash(ctx: &Ctx, ws: &WsCtx, stash: GitOid) -> Result<()> {
    let id = conflict::changeset_of(ws, stash).await?;
    let name = if switch::ref_target(ws, "refs/stash").await?.is_none() {
        "refs/stash".to_owned()
    } else {
        format!("refs/asc/autostash/{stash}")
    };
    let txn = RefTxn {
        updates: vec![RefUpdate {
            name: switch::ref_name(&name)?,
            expect: Expect::Absent,
            new: Some(RefTarget::Changeset(id)),
            expires_at: None,
        }],
        actor: actor(),
        reason: String::new(),
        op_id: OpId::new().0,
    };
    ctx.update_local_refs(txn, "autostash", StagingAfterMove::Keep)
        .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_author_script_quotes_like_git_and_parses_back() {
        let sig =
            Signature::new(b"A 'Q' Thor".to_vec(), b"a@b".to_vec(), 1_700_000_120, -90).unwrap();
        let text = author_script(&sig);
        assert!(text.contains("GIT_AUTHOR_NAME='A '\\''Q'\\'' Thor'\n"));
        assert!(text.contains("GIT_AUTHOR_DATE='@1700000120 -0130'\n"));
        let back = parse_author_script(&text).unwrap();
        assert_eq!(back.name(), sig.name());
        assert_eq!(back.time(), sig.time());
        assert_eq!(back.tz_min(), -90);
    }

    #[test]
    fn a_malformed_author_script_is_refused() {
        assert!(parse_author_script("GIT_AUTHOR_NAME=A\n").is_err());
        assert!(parse_author_script("EVIL='x'\n").is_err());
        assert!(
            parse_author_script(
                "GIT_AUTHOR_NAME='a'\nGIT_AUTHOR_EMAIL='b'\nGIT_AUTHOR_DATE='@1 +99'\n"
            )
            .is_err()
        );
    }

    #[test]
    fn labels_come_from_the_merge_subject() {
        assert_eq!(
            label_from_subject("Merge branch 'feature x' into master"),
            "feature x"
        );
        assert_eq!(
            label_from_subject("Merge pull request #7 from org/topic"),
            "org/topic"
        );
        assert_eq!(label_from_subject("join the two"), "join the two");
        assert_eq!(sanitize_label("feature x/y"), "feature-x-y");
        assert_eq!(sanitize_label("..."), "branch");
    }

    #[test]
    fn labels_stay_unique_whatever_the_case() {
        let mut l = Labels2 {
            by_commit: HashMap::new(),
            used: HashSet::new(),
        };
        let a = ChangesetId::from_bytes([1; 32]);
        let b = ChangesetId::from_bytes([2; 32]);
        assert_eq!(l.give(a, "Topic"), "Topic");
        assert_eq!(l.give(a, "other"), "Topic");
        assert_eq!(l.give(b, "topic"), "topic-2");
    }

    #[test]
    fn an_operand_stops_at_the_oneline() {
        assert_eq!(operand("branch-point # m1 # x", '#'), "branch-point");
        assert_eq!(operand("onto", '#'), "onto");
    }

    #[test]
    fn the_update_refs_file_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let a: GitOid = "a".repeat(40).parse().unwrap();
        let b: GitOid = "b".repeat(40).parse().unwrap();
        std::fs::write(
            dir.path().join("update-refs"),
            format!(
                "refs/heads/x\n{a}\n{}\nrefs/heads/y\n{a}\n{b}\n",
                "0".repeat(40)
            ),
        )
        .unwrap();
        let got = read_update_refs(dir.path()).unwrap();
        assert_eq!(
            got,
            vec![
                ("refs/heads/x".to_owned(), a, None),
                ("refs/heads/y".to_owned(), a, Some(b))
            ]
        );
        std::fs::write(
            dir.path().join("update-refs"),
            "refs/heads/x\nnot-an-id\n0\n",
        )
        .unwrap();
        assert!(read_update_refs(dir.path()).is_err());
    }
}
