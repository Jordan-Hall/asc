//! `asc rebase` without the editor (SPEC §4.4): `rebase [--onto <newbase>] [<upstream> [<branch>]]`
//! with `--continue`, `--skip`, `--abort`, `--quit`, `--autostash`, `--exec`, `--no-verify`,
//! `--force-rebase`, `-q` and `-v`.
//!
//! The state is git's `rebase-merge/` directory, written as git 2.55 writes it for a rebase on the
//! sequencer (`head-name`, `onto`, `orig-head`, `end`, `msgnum`, `done`, `git-rebase-todo` and its
//! `.backup`, `message`, `author-script`, `patch`, `stopped-sha`, `rewritten-list`, `interactive`,
//! `autostash`), plus `REBASE_HEAD`, `ORIG_HEAD`, `MERGE_MSG` and `AUTO_MERGE`. Every file is
//! untrusted input when a command resumes: size-capped, parsed strictly, ids looked up in the
//! repository before use.
//!
//! Each commit of `upstream..branch` (merges are flattened away) is replayed onto the new base. A
//! commit whose parent is already HEAD is kept as it is (git's fast-forward pick); a commit
//! whose paths are disjoint from what HEAD changed since its parent is rebuilt from metadata alone
//! (`rebase_metadata`, SPEC §4.4); every other commit goes through the engine's three-way merge.
//! Refs move through `Ctx::update_local_refs`; HEAD stays detached until the last commit.
//!
//! Not served: the editor and `-i` (U27d), `--root`, `--rebase-merges`, `--keep-base`, merge
//! strategies other than `ort`, and git's patch-id detection of commits already upstream (such a
//! commit becomes empty on replay and is dropped with git's "dropping" line instead of being left
//! out of the todo list up front). `pre-rebase` and `post-rewrite` run; `post-checkout` runs on the
//! first and last head move. `--autostash` stores a conflicted stash under `refs/stash` when that
//! ref is free and under `refs/asc/autostash/<id>` otherwise.

use std::ffi::OsString;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{ExitCode, Stdio};

use asc_core::change::RewriteOp;
use asc_core::id::{ChangesetId, GitOid, RefTarget, TreeId};
use asc_core::object::Changeset;
use asc_core::path::{RefName, RepoPath};
use asc_core::sig::Signature;
use asc_engine::diff::{DiffCtx, DiffOptions, NoAttributes, RenameOptions, write_patch, write_shortstat};
use asc_engine::diff::format::write_summary;
use asc_engine::graph::LogQuery;
use asc_engine::history::HistoryApi;
use asc_engine::merge::{Labels, MergeOutcome};
use asc_git::projection::Head;
use asc_state::state::stage::{self, StageSet};
use asc_state::state::tables::ws_ops;
use asc_state::state::workspaces;
use asc_store::refs::{Expect, RefTxn, RefUpdate};
use asc_wc::status;

use super::commit::message::{self, Cleanup};
use super::commit::stagebuild;
use super::common::{CliError, Ctx, Result, StagingAfterMove, WsCtx};
use super::conflict::{self, NewCommit, Op, Sides, fatal};
use super::restore::{WcParts, actor, index_tree, root_of, sync_worktree, tool};
use super::seqstate;
use super::switch::{self, Dest, Move, RevKind};
use crate::ext::{self, OpId};
use crate::hooks_runner::{Gate, Hook, HookEnv, RewriteKind, Runner};
use crate::native::NativeSpec;

/// Most commands a todo list may hold; a longer one is refused instead of truncated.
const MAX_TODO: usize = 100_000;
/// Longest `--exec` command or todo line accepted.
const MAX_LINE: usize = 64 * 1024;

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

/// git's help block under the todo list in `git-rebase-todo.backup`.
const TODO_HELP: &str = "# Commands:
# p, pick <commit> = use commit
# r, reword <commit> = use commit, but edit the commit message
# e, edit <commit> = use commit, but stop for amending
# s, squash <commit> = use commit, but meld into previous commit
# f, fixup [-C | -c] <commit> = like \"squash\" but keep only the previous
#                    commit's log message, unless -C is used, in which case
#                    keep only this commit's message; -c is same as -C but
#                    opens the editor
# x, exec <command> = run command (the rest of the line) using shell
# b, break = stop here (continue rebase later with 'git rebase --continue')
# d, drop <commit> = remove commit
# l, label <label> = label current HEAD with a name
# t, reset <label> = reset HEAD to a label
# m, merge [-C <commit> | -c <commit>] <label> [# <oneline>]
#         create a merge commit using the original merge commit's
#         message (or the oneline, if no original merge commit was
#         specified); use -c <commit> to reword the commit message
# u, update-ref <ref> = track a placeholder for the <ref> to be updated
#                       to this position in the new commits. The <ref> is
#                       updated at the end of the rebase
#
# These lines can be re-ordered; they are executed from top to bottom.
#
# If you remove a line here THAT COMMIT WILL BE LOST.
#
# However, if you remove everything, the rebase will be aborted.
#
";

// ---------------------------------------------------------------- the todo list

/// One todo command.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Cmd {
    Pick { oid: GitOid, subject: String },
    Exec(String),
}

impl Cmd {
    fn render(&self) -> String {
        match self {
            Self::Pick { oid, subject } => format!("pick {oid} # {subject}"),
            Self::Exec(cmd) => format!("exec {cmd}"),
        }
    }
}

/// Parses a todo file. Blank and comment lines are skipped; anything else must be `pick <oid>` or
/// `exec <command>`.
fn parse_todo(text: &str, what: &str) -> Result<Vec<Cmd>> {
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.trim_end_matches('\r');
        if line.trim().is_empty() || line.starts_with('#') {
            continue;
        }
        if line.len() > MAX_LINE || out.len() >= MAX_TODO {
            return Err(fatal(format!("asc: {what} is too large")));
        }
        let (word, rest) = line.split_once(' ').unwrap_or((line, ""));
        match word {
            "pick" | "p" => {
                let (sha, subject) = match rest.split_once(" # ") {
                    Some((s, t)) => (s, t),
                    None => (rest, ""),
                };
                let oid: GitOid = sha
                    .trim()
                    .parse()
                    .map_err(|_| fatal(format!("asc: {what}: bad object id in '{line}'")))?;
                out.push(Cmd::Pick { oid, subject: subject.to_owned() });
            }
            "exec" | "x" if !rest.trim().is_empty() => out.push(Cmd::Exec(rest.to_owned())),
            _ => return Err(fatal(format!("asc: {what}: unsupported command '{line}'"))),
        }
    }
    Ok(out)
}

fn render_todo(list: &[Cmd]) -> String {
    list.iter().map(|c| format!("{}\n", c.render())).collect()
}

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
    let (name, email, date) = (name.ok_or_else(bad)?, email.ok_or_else(bad)?, date.ok_or_else(bad)?);
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
    Signature::new(name.into_bytes(), email.into_bytes(), secs, sign * (hours * 60 + mins)).map_err(|_| bad())
}

// ---------------------------------------------------------------- the state directory

/// A rebase in progress: what `rebase-merge/` holds.
struct Run {
    dir: PathBuf,
    gitdir: PathBuf,
    head_name: String,
    onto: GitOid,
    orig_head: GitOid,
    todo: Vec<Cmd>,
    done: Vec<Cmd>,
    end: usize,
    quiet: bool,
    verbose: bool,
    keep_redundant: bool,
    /// Fast-forward picks whose parent is HEAD (off with `--force-rebase`, not persisted: git
    /// does not persist it either).
    allow_ff: bool,
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

impl Run {
    fn load(gitdir: &Path) -> Result<Self> {
        let dir = state_dir(gitdir);
        let head_name = need_text(&dir, "head-name")?.trim_end().to_owned();
        if head_name != "detached HEAD" && RefName::new(head_name.clone().into_bytes()).is_err() {
            return Err(fatal("asc: rebase-merge/head-name holds a bad ref name"));
        }
        let todo = parse_todo(&read_text(&dir, "git-rebase-todo")?.unwrap_or_default(), "rebase-merge/git-rebase-todo")?;
        let done = parse_todo(&read_text(&dir, "done")?.unwrap_or_default(), "rebase-merge/done")?;
        let end: usize = need_text(&dir, "end")?
            .trim()
            .parse()
            .map_err(|_| fatal("asc: rebase-merge/end is not a number"))?;
        Ok(Self {
            head_name,
            onto: need_oid(&dir, "onto")?,
            orig_head: need_oid(&dir, "orig-head")?,
            todo,
            done,
            end,
            quiet: seqstate::exists(&dir, "quiet"),
            verbose: seqstate::exists(&dir, "verbose"),
            keep_redundant: seqstate::exists(&dir, "keep_redundant_commits"),
            allow_ff: true,
            gitdir: gitdir.to_owned(),
            dir,
        })
    }

    fn write(&self, name: &str, text: &str) -> Result<()> {
        seqstate::write(&self.dir, name, text.as_bytes())
    }

    /// The files that change with every command.
    fn save_progress(&self) -> Result<()> {
        self.write("git-rebase-todo", &render_todo(&self.todo))?;
        self.write("done", &render_todo(&self.done))?;
        self.write("msgnum", &format!("{}\n", self.done.len()))
    }

    fn branch(&self) -> Option<&str> {
        self.head_name.strip_prefix("refs/heads/")
    }

    fn clear_stop(&self) {
        for name in ["message", "author-script", "patch", "stopped-sha"] {
            seqstate::remove(&self.dir, name);
        }
        for name in ["REBASE_HEAD", "MERGE_MSG", "AUTO_MERGE"] {
            seqstate::remove(&self.gitdir, name);
        }
    }

    /// Appends `old new` to `rewritten-list`.
    fn record(&self, old: GitOid, new: GitOid) -> Result<()> {
        let mut text = read_text(&self.dir, "rewritten-list")?.unwrap_or_default();
        text.push_str(&format!("{old} {new}\n"));
        self.write("rewritten-list", &text)
    }
}

// ---------------------------------------------------------------- entry

pub async fn run(ctx: &Ctx, a: Args) -> Result<ExitCode> {
    let ws = ctx.ws()?;
    let gitdir = ws.projection.git_dir().to_owned();
    let modes = [a.cont, a.skip, a.abort, a.quit].iter().filter(|&&m| m).count();
    if modes > 1 {
        return Err(CliError::Usage(format!(
            "error: options '--continue', '--skip', '--abort' and '--quit' cannot be used together\n{USAGE}"
        )));
    }
    if modes == 1 {
        if !a.words.is_empty() || a.onto.is_some() {
            return Err(CliError::Usage(USAGE.to_owned()));
        }
        if !state_dir(&gitdir).is_dir() {
            return Err(fatal("no rebase in progress"));
        }
        let mut run = Run::load(&gitdir)?;
        run.quiet |= a.quiet;
        return if a.cont {
            resume(ctx, ws, &mut run).await
        } else if a.skip {
            skip(ctx, ws, &mut run).await
        } else if a.abort {
            abort(ctx, ws, run).await
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
    let st = status::compute(&parts.wc(&ws.engine), &ws.db, &fresh).await.map_err(fatal)?;
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
    let second = if unstaged && staged { "error: Additionally, your index contains uncommitted changes.\n" } else { "" };
    CliError::Raw(format!("{first}\n{second}error: Please commit or stash them."), 1)
}

async fn start(ctx: &Ctx, ws: &WsCtx, gitdir: &Path, a: Args) -> Result<ExitCode> {
    if state_dir(gitdir).exists() {
        return Err(already(gitdir));
    }
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
            let tracked = cur_branch.as_deref().and_then(|b| switch::upstream_ref(ctx, b));
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
                RevKind::Branch(name) => (rev.commit, format!("refs/heads/{name}"), Some((Dest::Existing { branch: name, tip: Some(rev.commit) }, b.clone()))),
                _ => (rev.commit, "detached HEAD".to_owned(), Some((Dest::Detach { commit: rev.commit }, b.clone()))),
            }
        }
        None => (head, cur_branch.as_ref().map_or_else(|| "detached HEAD".to_owned(), |b| format!("refs/heads/{b}")), None),
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

    let hist = &ws.engine;
    let base = HistoryApi::merge_base(hist, upstream, &[orig], false).await.map_err(fatal)?;
    let base = base.first().copied();
    let onto_base = HistoryApi::merge_base(hist, onto, &[orig], false).await.map_err(fatal)?;
    let up_to_date = base == Some(onto) && onto_base.first() == Some(&onto);
    let branch_label = match head_name.strip_prefix("refs/heads/") {
        Some(b) => format!("Current branch {b}"),
        None => "HEAD".to_owned(),
    };
    let exec_forces = !a.exec.is_empty();
    if up_to_date && !exec_forces {
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
        let hook = Hook::PreRebase { upstream: upstream_text.clone(), branch: branch_arg.clone() };
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

    // The commits to replay, oldest first, merges flattened away.
    let q = LogQuery { include: vec![orig], exclude: vec![upstream], limit: Some(MAX_TODO + 1), ..LogQuery::default() };
    let mut ids: Vec<ChangesetId> = Vec::new();
    for e in HistoryApi::log(hist, &q, None).await.map_err(fatal)?.entries {
        if e.parents.len() <= 1 {
            ids.push(e.id);
        }
    }
    if ids.len() > MAX_TODO {
        return Err(fatal("asc: too many commits to rebase"));
    }
    ids.reverse();
    let mut todo = Vec::new();
    for id in ids {
        let cs = ws.engine.get_changeset(id).await?;
        if cs.parents.is_empty() {
            return Err(fatal("asc: rebasing a root commit is not supported yet"));
        }
        let oid = conflict::git_id(ws, id).await?;
        todo.push(Cmd::Pick { oid, subject: first_line(&cs.message) });
        todo.extend(a.exec.iter().map(|c| Cmd::Exec(c.clone())));
    }
    let end = todo.len();
    let onto_git = conflict::git_id(ws, onto).await?;
    let orig_git = conflict::git_id(ws, orig).await?;

    // Leading picks that already sit on the base are kept without being replayed.
    let mut done = Vec::new();
    let mut current = onto;
    if !a.force {
        while let Some(Cmd::Pick { oid, .. }) = todo.first() {
            let id = conflict::changeset_of(ws, *oid).await?;
            let cs = ws.engine.get_changeset(id).await?;
            if cs.parents.as_slice() != [current] {
                break;
            }
            current = id;
            done.push(todo.remove(0));
        }
    }

    let dir = state_dir(gitdir);
    std::fs::create_dir(&dir).map_err(|e| fatal(format!("cannot create '{}': {e}", dir.display())))?;
    let run = Run {
        dir,
        gitdir: gitdir.to_owned(),
        head_name,
        onto: onto_git,
        orig_head: orig_git,
        todo,
        done,
        end,
        quiet: a.quiet,
        verbose: a.verbose,
        keep_redundant: !a.exec.is_empty(),
        allow_ff: !a.force,
    };
    run.write("head-name", &format!("{}\n", run.head_name))?;
    run.write("onto", &format!("{}\n", run.onto))?;
    run.write("orig-head", &format!("{}\n", run.orig_head))?;
    run.write("interactive", "")?;
    run.write("no-reschedule-failed-exec", "")?;
    run.write(if run.keep_redundant { "keep_redundant_commits" } else { "drop_redundant_commits" }, "")?;
    if run.quiet {
        run.write("quiet", "")?;
    }
    if run.verbose {
        run.write("verbose", "")?;
    }
    let abbrev = switch::abbrev_len(ctx);
    let short = |o: &GitOid| o.to_string()[..abbrev].to_owned();
    let upstream_git = conflict::git_id(ws, upstream).await?;
    let all: Vec<Cmd> = run.done.iter().chain(&run.todo).cloned().collect();
    let count = all.len();
    run.write(
        "git-rebase-todo.backup",
        &format!(
            "{}\n# Rebase {}..{} onto {} ({} command{})\n#\n{TODO_HELP}",
            render_todo(&all),
            short(&upstream_git),
            short(&run.orig_head),
            short(&run.onto),
            count,
            if count == 1 { "" } else { "s" },
        ),
    )?;
    run.save_progress()?;
    run.write("end", &format!("{}\n", run.end))?;
    seqstate::write_orig_head(gitdir, run.orig_head)?;
    if let Some(s) = stash {
        run.write("autostash", &format!("{s}\n"))?;
    }
    detach_to(ctx, ws, current, &onto_text).await?;
    let mut run = run;
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
    String::from_utf8_lossy(message).lines().next().unwrap_or_default().to_owned()
}

/// Switches to a branch or detaches at a commit without printing anything.
async fn quiet_move(ctx: &Ctx, dest: Dest, typed: String) -> Result<()> {
    let opts = switch::Opts { quiet: true, ..switch::Opts::default() };
    switch::perform(ctx, Move { dest, opts, typed, keep_tree: false }).await.map(|_| ())
}

async fn detach_to(ctx: &Ctx, ws: &WsCtx, target: ChangesetId, typed: &str) -> Result<()> {
    let _ = ws;
    quiet_move(ctx, Dest::Detach { commit: target }, typed.to_owned()).await
}

// ---------------------------------------------------------------- the loop

enum Step {
    Done,
    Stopped,
}

/// Runs the remaining commands, then finishes.
async fn drive(ctx: &Ctx, ws: &WsCtx, run: &mut Run) -> Result<ExitCode> {
    while !run.todo.is_empty() {
        let cmd = run.todo.remove(0);
        run.done.push(cmd.clone());
        run.save_progress()?;
        if !run.quiet {
            let end = if run.verbose { "\n" } else { "\r" };
            eprint!("Rebasing ({}/{}){end}", run.done.len(), run.end);
        }
        let step = match &cmd {
            Cmd::Pick { oid, subject } => pick(ctx, ws, run, *oid, subject).await?,
            Cmd::Exec(text) => exec(ctx, ws, run, text).await?,
        };
        if matches!(step, Step::Stopped) {
            return Ok(ExitCode::from(1));
        }
    }
    finish(ctx, ws, run).await
}

async fn pick(ctx: &Ctx, ws: &WsCtx, run: &Run, oid: GitOid, subject: &str) -> Result<Step> {
    let id = conflict::changeset_of(ws, oid).await?;
    let cs = ws.engine.get_changeset(id).await?;
    let Some(head) = ws.head_commit().await? else {
        return Err(fatal("asc: HEAD is unborn"));
    };
    if cs.parents.len() != 1 {
        return Err(fatal(format!("asc: cannot replay {oid}: it is not a single-parent commit")));
    }
    let head_root = root_of(ws, Some(head)).await?;
    let reflog = format!("rebase (pick): {subject}");
    // Fast-forward: the commit already sits on HEAD.
    if run.allow_ff && cs.parents[0] == head {
        let paths = changed_paths(ws, head_root, cs.root).await?;
        sync_worktree(ctx, cs.root, &paths, true).await?;
        conflict::move_head(ctx, ws, Some(head), id, &reflog, StagingAfterMove::Set(Vec::new()), None).await?;
        run.record(oid, oid)?;
        return Ok(Step::Done);
    }
    let parent_root = root_of(ws, Some(cs.parents[0])).await?;
    let abbrev = oid.to_string()[..switch::abbrev_len(ctx)].to_owned();
    let named = format!("{abbrev} ({})", first_line(&cs.message));
    let sides = Sides { base: parent_root, ours: head_root, theirs: cs.root };
    let outcome = match ws.engine.rebase_metadata(&cs, head).await.map_err(fatal)? {
        Some(done) => MergeOutcome { root: done.root, conflicts: Vec::new() },
        None => {
            let favor = conflict::favor_of(&[])?;
            let labels = Labels { base: format!("parent of {named}"), ours: "HEAD".into(), theirs: named.clone() };
            let (merge_opts, differ) = conflict::merge_options(ctx, ws, &sides, favor, labels).await?;
            let outcome = ws.engine.merge_trees(sides.base, sides.ours, sides.theirs, &merge_opts).await.map_err(fatal)?;
            conflict::refuse_unsupported(&outcome)?;
            conflict::precheck(ctx, ws, Op::Pick, head_root, &outcome, false).await?;
            conflict::print_messages(&conflict::messages(&outcome, &differ, "HEAD", &named));
            outcome
        }
    };
    if outcome.is_clean() && outcome.conflicts.is_empty() && outcome.root == head_root && parent_root != cs.root && !run.keep_redundant {
        eprintln!("dropping {oid} {subject} -- patch contents already upstream");
        return Ok(Step::Done);
    }
    conflict::materialise(ctx, ws, head_root, head_root, &outcome).await?;
    let mut text = cs.message.clone();
    if !outcome.is_clean() {
        seqstate::write_auto_merge(&run.gitdir, conflict::git_tree_id(ws, outcome.root).await?)?;
        text.extend_from_slice(&conflict::conflicts_block(&outcome, message::comment_char(ctx)));
        seqstate::write(&run.gitdir, "MERGE_MSG", &text)?;
        run.write("message", &String::from_utf8_lossy(&text))?;
        seqstate::write(&run.gitdir, "REBASE_HEAD", format!("{oid}\n").as_bytes())?;
        run.write("stopped-sha", &format!("{oid}\n"))?;
        run.write("author-script", &author_script(&cs.author))?;
        run.write("patch", &String::from_utf8_lossy(&patch_text(ws, &cs).await?))?;
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
        eprintln!("Could not apply {abbrev}... # {subject}");
        return Ok(Step::Stopped);
    }
    let idents = conflict::identities(ctx)?;
    let (new, new_git) = conflict::write_commit(
        ws,
        NewCommit {
            parents: vec![head],
            author: cs.author.clone(),
            committer: idents.committer.clone(),
            message: cs.message.clone(),
            root: outcome.root,
        },
    )
    .await?;
    let op = conflict::move_head(ctx, ws, Some(head), new, &reflog, StagingAfterMove::Set(Vec::new()), Some(&idents.committer)).await?;
    ext::rewrite_sink().record(new, &[id], RewriteOp::Rebase, op);
    ext::index_sink().on_commit(ws.workspace.ws.id, new).await.map_err(fatal)?;
    ws_ops::clear(&ws.db, ws.workspace.ws.id).await?;
    run.record(oid, new_git)?;
    let runner = Runner::for_ctx(ctx)?;
    let env = HookEnv::of(ctx, "rebase").with_index(run.gitdir.join("index")).without_editor();
    let _ = runner.run(Hook::PostCommit, &env).await?;
    Ok(Step::Done)
}

/// The paths whose entries differ between two trees.
async fn changed_paths(ws: &WsCtx, from: TreeId, to: TreeId) -> Result<Vec<RepoPath>> {
    Ok(ws.engine.leafdiff(from, to, None).await?.into_iter().map(|(p, _)| p).collect())
}

/// `git diff-tree -p` of `cs` against its parent, as `rebase-merge/patch` holds it.
async fn patch_text(ws: &WsCtx, cs: &Changeset) -> Result<Vec<u8>> {
    let from = root_of(ws, cs.parents.first().copied()).await?;
    let opts = DiffOptions::default();
    let changes = ws.engine.diff_trees(from, cs.root, None, &opts).await.map_err(fatal)?;
    let dctx = DiffCtx { opts: &opts, content: &ws.engine, attrs: &NoAttributes };
    let mut out = Vec::new();
    write_patch(&mut out, &dctx, &changes).await.map_err(fatal)?;
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
    eprint!("warning: execution failed: {text}\nYou can fix the problem, and then run\n\n  {t} rebase --continue\n\n\n");
    Ok(Step::Stopped)
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
        switch::point_head(ws, &Head::Branch(branch.clone()), &run.head_name, saved.base).await?;
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
        if let Err(e) = ctx.update_local_refs(txn, &reflog, StagingAfterMove::Keep).await {
            switch::restore_head(ws, &saved).await;
            return Err(e);
        }
    }
    let pairs = rewritten_pairs(run)?;
    if !pairs.is_empty() {
        let runner = Runner::for_ctx(ctx)?;
        let env = HookEnv::of(ctx, "rebase");
        let _ = runner.run(Hook::PostRewrite { kind: RewriteKind::Rebase, pairs }, &env).await?;
    }
    let stash = autostash_of(run)?;
    let head_name = run.head_name.clone();
    let _ = std::fs::remove_dir_all(&run.dir);
    if let Some(s) = stash {
        apply_autostash(ctx, ws, s).await?;
    }
    if !run.quiet {
        let name = if head_name == "detached HEAD" { "detached HEAD" } else { head_name.as_str() };
        eprintln!("Successfully rebased and updated {name}.");
    }
    Ok(ExitCode::SUCCESS)
}

fn rewritten_pairs(run: &Run) -> Result<Vec<(GitOid, GitOid)>> {
    let mut out = Vec::new();
    for line in read_text(&run.dir, "rewritten-list")?.unwrap_or_default().lines().take(MAX_TODO) {
        let (a, b) = line.split_once(' ').ok_or_else(|| fatal("asc: rebase-merge/rewritten-list is malformed"))?;
        let parse = |s: &str| s.trim().parse::<GitOid>().map_err(|_| fatal("asc: rebase-merge/rewritten-list is malformed"));
        out.push((parse(a)?, parse(b)?));
    }
    Ok(out)
}

fn autostash_of(run: &Run) -> Result<Option<GitOid>> {
    match read_text(&run.dir, "autostash")? {
        Some(t) => Ok(Some(t.trim().parse().map_err(|_| fatal("asc: rebase-merge/autostash holds a bad object id"))?)),
        None => Ok(None),
    }
}

// ---------------------------------------------------------------- continue, skip, abort, quit

/// Resets the staged paths and the paths a stopped commit touched back to `target_root`.
async fn reset_touched(ctx: &Ctx, ws: &WsCtx, target_root: TreeId) -> Result<()> {
    let rows = stage::overlay(&ws.db, ws.workspace.ws.id, StageSet::Default).await?;
    let head_root = root_of(ws, ws.head_commit().await?).await?;
    let mut paths: Vec<RepoPath> = rows.iter().filter_map(|r| RepoPath::new(r.path.clone()).ok()).collect();
    paths.extend(changed_paths(ws, head_root, target_root).await?);
    paths.sort();
    paths.dedup();
    sync_worktree(ctx, target_root, &paths, true).await?;
    stage::clear(&ws.db, ws.workspace.ws.id, StageSet::Default).await?;
    ext::index_sink().on_stage_changed(ws.workspace.ws.id, &[]).await.map_err(fatal)
}

async fn resume(ctx: &Ctx, ws: &WsCtx, run: &mut Run) -> Result<ExitCode> {
    let head = ws.head_commit().await?.ok_or_else(|| fatal("asc: HEAD is unborn"))?;
    let head_root = root_of(ws, Some(head)).await?;
    let rows = stage::overlay(&ws.db, ws.workspace.ws.id, StageSet::Default).await?;
    let fresh = workspaces::get(&ws.db, ws.workspace.ws.id)
        .await?
        .ok_or_else(|| fatal("asc: this workspace is not registered in state.db"))?;
    let parts = WcParts::new(ctx, ws, ws.projection.git_dir().join("index"))?;
    let st = status::compute(&parts.wc(&ws.engine), &ws.db, &fresh).await.map_err(fatal)?;
    let unmerged: Vec<String> = rows.iter().filter(|r| r.stage > 0).map(|r| String::from_utf8_lossy(&r.path).into_owned()).collect();
    if !unmerged.is_empty() || !st.unstaged.is_empty() {
        let mut shown = std::collections::BTreeSet::new();
        for p in &unmerged {
            shown.insert(format!("{p}: needs merge"));
        }
        for c in &st.unstaged {
            shown.insert(format!("{}: needs update", String::from_utf8_lossy(c.path.as_bytes())));
        }
        let mut out = String::new();
        for l in shown {
            out.push_str(&l);
            out.push('\n');
        }
        out.push_str("You must edit all merge conflicts and then\nmark them as resolved using git add\n");
        std::io::stdout().write_all(out.as_bytes())?;
        return Ok(ExitCode::from(1));
    }
    if seqstate::exists(&run.gitdir, "REBASE_HEAD") {
        let picked = seqstate::read_oid(&run.gitdir, "REBASE_HEAD")?.ok_or_else(|| fatal("asc: REBASE_HEAD is empty"))?;
        let changes = stagebuild::rows_to_changes(ws, head_root, &rows).await?;
        let root = ws.engine.apply(head_root, &changes).await?;
        if root == head_root {
            let t = "No changes - did you forget to use 'git add'?\nIf there is nothing left to stage, chances are that something else\nalready introduced the same changes; you might want to skip this patch.\n";
            std::io::stdout().write_all(t.as_bytes())?;
            return Ok(ExitCode::from(1));
        }
        let author = parse_author_script(&need_text(&run.dir, "author-script")?)?;
        let text = need_text(&run.dir, "message")?;
        seqstate::write(&run.gitdir, "COMMIT_EDITMSG", text.as_bytes())?;
        let final_msg = message::cleanup(text.as_bytes(), Cleanup::Strip, true, message::comment_char(ctx));
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
        let old = conflict::changeset_of(ws, picked).await?;
        let op = conflict::move_head(ctx, ws, Some(head), new, &format!("rebase (continue): {subject}"), StagingAfterMove::Set(Vec::new()), Some(&idents.committer)).await?;
        ext::rewrite_sink().record(new, &[old], RewriteOp::Rebase, op);
        ext::index_sink().on_commit(ws.workspace.ws.id, new).await.map_err(fatal)?;
        ws_ops::clear(&ws.db, ws.workspace.ws.id).await?;
        run.record(picked, new_git)?;
        run.clear_stop();
        print_summary(ws, &new_git.to_string(), &subject, &author, &idents.committer, head_root, root).await?;
        let runner = Runner::for_ctx(ctx)?;
        let env = HookEnv::of(ctx, "rebase").with_index(run.gitdir.join("index")).without_editor();
        let _ = runner.run(Hook::PostCommit, &env).await?;
    }
    drive(ctx, ws, run).await
}

async fn skip(ctx: &Ctx, ws: &WsCtx, run: &mut Run) -> Result<ExitCode> {
    let head = ws.head_commit().await?;
    let head_root = root_of(ws, head).await?;
    reset_touched(ctx, ws, head_root).await?;
    run.clear_stop();
    drive(ctx, ws, run).await
}

async fn abort(ctx: &Ctx, ws: &WsCtx, run: Run) -> Result<ExitCode> {
    ext::op_sink().snapshot_wc();
    let head = ws.head_commit().await?;
    let orig = conflict::changeset_of(ws, run.orig_head).await?;
    let target_root = root_of(ws, Some(orig)).await?;
    reset_touched(ctx, ws, target_root).await?;
    if let Some(h) = head {
        seqstate::write_orig_head(&run.gitdir, conflict::git_id(ws, h).await?)?;
    }
    if head != Some(orig) {
        conflict::move_head(ctx, ws, head, orig, "rebase (abort): returning to original", StagingAfterMove::Set(Vec::new()), None).await?;
    }
    if let Some(branch) = run.branch() {
        let saved = switch::Saved::capture(ws).await?;
        switch::point_head(ws, &Head::Branch(branch.to_owned()), &run.head_name, saved.base).await?;
    }
    let stash = autostash_of(&run)?;
    run.clear_stop();
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
    let _ = std::fs::remove_dir_all(&run.dir);
    Ok(ExitCode::SUCCESS)
}

async fn print_summary(
    ws: &WsCtx,
    git: &str,
    subject: &str,
    author: &Signature,
    committer: &Signature,
    from: TreeId,
    to: TreeId,
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
    let opts = DiffOptions {
        renames: Some(RenameOptions { limit: 100, ..RenameOptions::default() }),
        ..DiffOptions::default()
    };
    let changes = ws.engine.diff_trees(from, to, None, &opts).await.map_err(fatal)?;
    let dctx = DiffCtx { opts: &opts, content: &ws.engine, attrs: &NoAttributes };
    write_shortstat(&mut out, &dctx, &changes).await.map_err(fatal)?;
    write_summary(&mut out, &changes, &opts).map_err(fatal)?;
    std::io::stdout().write_all(&out)?;
    Ok(())
}

// ---------------------------------------------------------------- autostash

/// `git stash create autostash`, then a hard reset to HEAD. Prints git's "Created autostash".
async fn create_autostash(ctx: &Ctx, ws: &WsCtx, head: ChangesetId, head_root: TreeId) -> Result<GitOid> {
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
            message: format!("index on {branch}: {abbrev} {}\n", first_line(&head_cs.message)).into_bytes(),
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
    println!("Created autostash: {}", &stash_git.to_string()[..switch::abbrev_len(ctx)]);
    // `reset --hard`: the staged and the modified paths go back to HEAD.
    let mut paths = changed_paths(ws, head_root, index_root).await?;
    paths.extend(dirty.iter().filter_map(|r| RepoPath::new(r.path.clone()).ok()));
    paths.sort();
    paths.dedup();
    sync_worktree(ctx, head_root, &paths, true).await?;
    stage::clear(&ws.db, ws.workspace.ws.id, StageSet::Default).await?;
    ext::index_sink().on_stage_changed(ws.workspace.ws.id, &[]).await.map_err(fatal)?;
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
    let head = ws.head_commit().await?.ok_or_else(|| fatal("asc: HEAD is unborn"))?;
    let head_root = root_of(ws, Some(head)).await?;
    let base_root = root_of(ws, Some(base_commit)).await?;
    let sides = Sides { base: base_root, ours: head_root, theirs: cs.root };
    let labels = Labels { base: "Stash base".into(), ours: "Updated upstream".into(), theirs: "Stashed changes".into() };
    let (merge_opts, differ) = conflict::merge_options(ctx, ws, &sides, conflict::favor_of(&[])?, labels).await?;
    let outcome = ws.engine.merge_trees(sides.base, sides.ours, sides.theirs, &merge_opts).await.map_err(fatal)?;
    conflict::refuse_unsupported(&outcome)?;
    conflict::precheck(ctx, ws, Op::Pick, head_root, &outcome, false).await?;
    conflict::print_messages(&conflict::messages(&outcome, &differ, "Updated upstream", "Stashed changes"));
    conflict::materialise(ctx, ws, head_root, head_root, &outcome).await?;
    seqstate::write_auto_merge(ws.projection.git_dir(), conflict::git_tree_id(ws, outcome.root).await?)?;
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
    ext::index_sink().on_stage_changed(ws.workspace.ws.id, &[]).await.map_err(fatal)?;
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
    ctx.update_local_refs(txn, "autostash", StagingAfterMove::Keep).await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn oid(c: char) -> GitOid {
        c.to_string().repeat(40).parse().unwrap()
    }

    #[test]
    fn todo_lines_round_trip_and_comments_are_skipped() {
        let list = vec![
            Cmd::Pick { oid: oid('a'), subject: "first # not a comment".into() },
            Cmd::Exec("make test".into()),
        ];
        let text = render_todo(&list);
        let with_help = format!("{text}\n# Rebase x..y onto z (2 commands)\n#\n{TODO_HELP}");
        assert_eq!(parse_todo(&with_help, "t").unwrap(), list);
    }

    #[test]
    fn a_todo_with_an_unknown_command_or_a_bad_id_is_refused() {
        assert!(parse_todo("drop aaaa\n", "t").is_err());
        assert!(parse_todo("pick nothex # x\n", "t").is_err());
        assert!(parse_todo("exec \n", "t").is_err());
    }

    #[test]
    fn the_author_script_quotes_like_git_and_parses_back() {
        let sig = Signature::new(b"A 'Q' Thor".to_vec(), b"a@b".to_vec(), 1_700_000_120, -90).unwrap();
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
        assert!(parse_author_script("GIT_AUTHOR_NAME='a'\nGIT_AUTHOR_EMAIL='b'\nGIT_AUTHOR_DATE='@1 +99'\n").is_err());
    }
}
