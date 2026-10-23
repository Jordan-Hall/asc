//! `asc cherry-pick` (SPEC §4.4), and the replay machinery `revert` shares: the engine merges the
//! picked change onto HEAD, and the result is committed (author kept for a pick, the current
//! identity for a revert) or left staged with `-n`. A conflict leaves git's state:
//! `CHERRY_PICK_HEAD` / `REVERT_HEAD`, `MERGE_MSG` with the `# Conflicts:` block, `AUTO_MERGE`,
//! the staged stages 1/2/3 and, for several commits, `sequencer/`. `--continue`, `--skip`,
//! `--abort` and `--quit` work from those files.
//!
//! Not served: the editor (`-e`), signing, `--signoff`, `--ff`, `--empty=` and file/directory
//! conflicts. Hooks run: `post-commit` after each commit; the commit hooks of `--continue`'s
//! `git commit` (pre-commit, commit-msg) are not run.

use std::ffi::OsString;
use std::io::Write as _;
use std::path::Path;
use std::process::ExitCode;

use asc_core::id::{ChangesetId, TreeId};
use asc_core::object::Changeset;
use asc_core::path::RepoPath;
use asc_engine::diff::{DiffCtx, DiffOptions, NoAttributes, RenameOptions, write_shortstat};
use asc_engine::diff::format::write_summary;
use asc_engine::graph::LogQuery;
use asc_engine::history::HistoryApi;
use asc_engine::merge::Labels;
use asc_state::state::stage::{self, StageSet};
use asc_state::state::tables::ws_ops;

use super::commit::message::{self, Cleanup};
use super::commit::stagebuild;
use super::common::{CliEngine, CliError, Ctx, Result, StagingAfterMove, WsCtx};
use super::conflict::{self, NewCommit, Op, Sides, fatal};
use super::restore::{index_tree, resolve_commit, root_of, sync_worktree, tool};
use super::seqstate::{self, Opts, Replay, Sequence, Todo};
use crate::hooks_runner::{Hook, HookEnv, Runner};
use crate::native::NativeSpec;

#[derive(clap::Parser, Debug)]
#[command(name = "cherry-pick", disable_help_flag = true, disable_version_flag = true)]
pub struct Args {
    #[arg(short = 'n', long = "no-commit")]
    pub no_commit: bool,
    /// Append "(cherry picked from commit ...)" to the message.
    #[arg(short = 'x')]
    pub record_origin: bool,
    #[arg(short = 'm', long = "mainline")]
    pub mainline: Option<usize>,
    #[arg(long = "continue")]
    pub cont: bool,
    #[arg(long = "skip")]
    pub skip: bool,
    #[arg(long = "abort")]
    pub abort: bool,
    #[arg(long = "quit")]
    pub quit: bool,
    #[arg(short = 'X', long = "strategy-option")]
    pub strategy_option: Vec<String>,
    #[arg(short = 's', long = "strategy")]
    pub strategy: Vec<String>,
    #[arg(long = "allow-empty")]
    pub allow_empty: bool,
    #[arg(long = "keep-redundant-commits")]
    pub keep_redundant: bool,
    #[arg(long = "no-edit")]
    pub no_edit: bool,
    pub commits: Vec<OsString>,
}

pub const NATIVE: &[NativeSpec] = &[NativeSpec {
    cmd: "cherry-pick",
    flags: &[
        "-n",
        "--no-commit",
        "-x",
        "-m",
        "--mainline",
        "--continue",
        "--skip",
        "--abort",
        "--quit",
        "-X",
        "--strategy-option",
        "-s",
        "--strategy",
        "--allow-empty",
        "--keep-redundant-commits",
        "--no-edit",
    ],
    placeholders: &[],
}];

/// The flags of either command, as the replay needs them.
pub struct Flags {
    pub opts: Opts,
    pub cont: bool,
    pub skip: bool,
    pub abort: bool,
    pub quit: bool,
    pub strategy: Vec<String>,
    pub commits: Vec<OsString>,
}

pub async fn run(ctx: &Ctx, a: Args) -> Result<ExitCode> {
    let flags = Flags {
        opts: Opts {
            no_commit: a.no_commit,
            allow_empty: a.allow_empty,
            keep_redundant: a.keep_redundant,
            record_origin: a.record_origin,
            mainline: a.mainline,
            strategy_options: a.strategy_option,
            ..Opts::default()
        },
        cont: a.cont,
        skip: a.skip,
        abort: a.abort,
        quit: a.quit,
        strategy: a.strategy,
        commits: a.commits,
    };
    replay(ctx, Replay::Pick, flags).await
}

// ---------------------------------------------------------------- entry

fn failed(replay: Replay) -> String {
    format!("fatal: {} failed", replay.command())
}

fn unmerged_files(replay: Replay) -> CliError {
    let what = match replay {
        Replay::Pick => "Cherry-picking",
        Replay::Revert => "Reverting",
    };
    CliError::Raw(
        format!(
            "error: {what} is not possible because you have unmerged files.\n\
             hint: Fix them up in the work tree, and then use 'git add/rm <file>'\n\
             hint: as appropriate to mark resolution and make a commit.\n{}",
            failed(replay)
        ),
        128,
    )
}

const COMMON_OPTIONS: &str = "
    --quit                end revert or cherry-pick sequence
    --continue            resume revert or cherry-pick sequence
    --abort               cancel revert or cherry-pick sequence
    --skip                skip current commit and continue
    --[no-]cleanup <mode> how to strip spaces and #comments from message
    -n, --no-commit       don't automatically commit
    --commit              opposite of --no-commit
    -e, --[no-]edit       edit the commit message
    -s, --[no-]signoff    add a Signed-off-by trailer
    -m, --[no-]mainline <parent-number>
                          select mainline parent
    --[no-]rerere-autoupdate
                          update the index with reused conflict resolution if possible
    --[no-]strategy <strategy>
                          merge strategy
    -X, --[no-]strategy-option <option>
                          option for merge strategy
    -S, --[no-]gpg-sign[=<key-id>]
                          GPG sign commit
";

/// git's usage text for a missing commit argument.
fn usage(replay: Replay) -> CliError {
    let text = match replay {
        Replay::Pick => format!(
            "usage: git cherry-pick [--edit] [-n] [-m <parent-number>] [-s] [-x] [--ff]
                       [-S[<keyid>]] <commit>...
   or: git cherry-pick (--continue | --skip | --abort | --quit)
{COMMON_OPTIONS}    -x                    append commit name
    --[no-]ff             allow fast-forward
    --[no-]allow-empty    preserve initially empty commits
    --[no-]allow-empty-message
                          allow commits with empty messages
    --[no-]keep-redundant-commits
                          deprecated: use --empty=keep instead
    --empty (stop|drop|keep)
                          how to handle commits that become empty
"
        ),
        Replay::Revert => format!(
            "usage: git revert [--[no-]edit] [-n] [-m <parent-number>] [-s] [-S[<keyid>]] <commit>...
   or: git revert (--continue | --skip | --abort | --quit)
{COMMON_OPTIONS}    --[no-]reference      use the 'reference' format to refer to commits
"
        ),
    };
    CliError::Usage(text)
}

/// The entry both commands use.
pub async fn replay(ctx: &Ctx, replay: Replay, f: Flags) -> Result<ExitCode> {
    let ws = ctx.ws()?;
    let gitdir = ws.projection.git_dir().to_owned();
    if [f.cont, f.skip, f.abort, f.quit].iter().filter(|&&m| m).count() > 1 {
        return Err(CliError::Usage(format!(
            "error: options '--continue', '--skip', '--abort' and '--quit' cannot be used together\n{}",
            usage_text(replay)
        )));
    }
    if let Some(bad) = f.strategy.iter().find(|s| !matches!(s.as_str(), "ort" | "recursive")) {
        return Err(fatal(format!("asc: merge strategy '{bad}' is not supported (only ort)")));
    }
    conflict::favor_of(&f.opts.strategy_options)?;
    if f.cont || f.skip || f.abort || f.quit {
        if !f.commits.is_empty() {
            return Err(usage(replay));
        }
        let in_progress = Sequence::exists(&gitdir) || seqstate::replay_in_progress(&gitdir).is_some();
        if !in_progress {
            let what = if f.skip { replay.command() } else { "cherry-pick or revert" };
            return Err(CliError::Raw(format!("error: no {what} in progress\n{}", failed(replay)), 128));
        }
        return if f.cont {
            resume(ctx, ws, &gitdir, replay).await
        } else if f.skip {
            skip(ctx, ws, &gitdir, replay).await
        } else if f.abort {
            abort(ctx, ws, &gitdir, replay).await
        } else {
            Sequence::remove(&gitdir);
            seqstate::remove(&gitdir, "CHERRY_PICK_HEAD");
            seqstate::remove(&gitdir, "REVERT_HEAD");
            Ok(ExitCode::SUCCESS)
        };
    }
    if f.commits.is_empty() {
        return Err(usage(replay));
    }
    start(ctx, ws, &gitdir, replay, f).await
}

fn usage_text(replay: Replay) -> String {
    let cmd = replay.command();
    format!("usage: git {cmd} (--continue | --skip | --abort | --quit)")
}

/// The commits to replay, in the order they are replayed.
async fn targets(ctx: &Ctx, ws: &WsCtx, replay: Replay, specs: &[OsString]) -> Result<Vec<ChangesetId>> {
    let mut out = Vec::new();
    for spec in specs {
        let text = spec.to_string_lossy().into_owned();
        let bad = || CliError::Fatal(format!("bad revision '{text}'"));
        if let Some((from, to)) = text.split_once("..").filter(|(_, t)| !t.starts_with('.')) {
            let from = if from.is_empty() { "HEAD" } else { from };
            let to = if to.is_empty() { "HEAD" } else { to };
            let (Some(a), Some(b)) = (resolve_commit(ctx, from).await?, resolve_commit(ctx, to).await?) else {
                return Err(bad());
            };
            let q = LogQuery { include: vec![b], exclude: vec![a], limit: Some(100_000), ..LogQuery::default() };
            let mut ids: Vec<ChangesetId> = HistoryApi::log(&ws.engine, &q, None)
                .await
                .map_err(fatal)?
                .entries
                .into_iter()
                .map(|e| e.id)
                .collect();
            if replay == Replay::Pick {
                ids.reverse(); // oldest first
            }
            out.extend(ids);
        } else {
            out.push(resolve_commit(ctx, &text).await?.ok_or_else(bad)?);
        }
    }
    Ok(out)
}

fn first_line(message: &[u8]) -> String {
    let text = String::from_utf8_lossy(message);
    text.lines().next().unwrap_or_default().to_owned()
}

async fn start(ctx: &Ctx, ws: &WsCtx, gitdir: &Path, replay: Replay, f: Flags) -> Result<ExitCode> {
    let rows = stage::overlay(&ws.db, ws.workspace.ws.id, StageSet::Default).await?;
    if rows.iter().any(|r| r.stage > 0) {
        return Err(unmerged_files(replay));
    }
    let Some(head) = ws.head_commit().await? else {
        return Err(fatal(format!("asc: {} on an unborn branch is not supported yet", replay.command())));
    };
    let ids = targets(ctx, ws, replay, &f.commits).await?;
    if ids.is_empty() {
        return Ok(ExitCode::SUCCESS);
    }
    let multi = ids.len() > 1;
    // git checks for an operation in progress only when it creates a sequencer (several commits).
    if multi && (Sequence::exists(gitdir) || seqstate::replay_in_progress(gitdir).is_some()) {
        let cmd = replay.command();
        return Err(CliError::Raw(
            format!(
                "error: a cherry-pick or revert is already in progress\nhint: try \"git {cmd} (--continue | --skip | --abort | --quit)\"\n{}",
                failed(replay)
            ),
            128,
        ));
    }
    let head_git = conflict::git_id(ws, head).await?;
    let mut todo = Vec::new();
    for &id in &ids {
        let cs = ws.engine.get_changeset(id).await?;
        let git = conflict::git_id(ws, id).await?;
        todo.push(Todo { replay, abbrev: git.to_string()[..7].to_owned(), subject: first_line(&cs.message), oid: Some(git) });
    }
    let seq = Sequence { head: head_git, abort_safety: head_git, todo, opts: f.opts };
    if multi {
        seq.save(gitdir)?;
    }
    drive(ctx, ws, gitdir, replay, seq, multi).await
}

/// Replays the todo list from its first entry.
async fn drive(
    ctx: &Ctx,
    ws: &WsCtx,
    gitdir: &Path,
    replay: Replay,
    mut seq: Sequence,
    multi: bool,
) -> Result<ExitCode> {
    let mut first = true;
    while let Some(item) = seq.todo.first().cloned() {
        let id = match item.oid {
            Some(oid) => conflict::changeset_of(ws, oid).await?,
            None => resolve_commit(ctx, &item.abbrev)
                .await?
                .ok_or_else(|| fatal(format!("sequencer/todo: cannot resolve '{}'", item.abbrev)))?,
        };
        // Staged changes from an earlier no-commit step are expected; the first step starts clean.
        let staged_ok = seq.opts.no_commit && !first;
        first = false;
        match step(ctx, ws, gitdir, item.replay, id, &seq.opts, staged_ok).await? {
            Step::Stopped => {
                if multi {
                    seq.save(gitdir)?;
                }
                return Ok(ExitCode::from(1));
            }
            Step::Done => {
                seq.todo.remove(0);
                if let Some(h) = ws.head_commit().await? {
                    seq.abort_safety = conflict::git_id(ws, h).await?;
                }
                if multi && !seq.todo.is_empty() {
                    seq.save(gitdir)?;
                }
            }
        }
    }
    let _ = replay;
    if multi || Sequence::exists(gitdir) {
        Sequence::remove(gitdir);
    }
    Ok(ExitCode::SUCCESS)
}

// ---------------------------------------------------------------- one commit

enum Step {
    Done,
    Stopped,
}

/// The parent a replay is measured against, with git's errors for merges.
async fn parent_root(ws: &WsCtx, replay: Replay, id: ChangesetId, cs: &Changeset, mainline: Option<usize>) -> Result<TreeId> {
    let git = conflict::git_id(ws, id).await?;
    let refuse = |text: String| CliError::Raw(format!("error: {text}\n{}", failed(replay)), 128);
    let parent = match (cs.parents.len(), mainline) {
        (0, _) => return Ok(CliEngine::empty_tree_id()),
        (1, None | Some(1)) => cs.parents[0],
        (_, None) => return Err(refuse(format!("commit {git} is a merge but no -m option was given."))),
        (n, Some(m)) if m == 0 || m > n => return Err(refuse(format!("commit {git} does not have parent {m}"))),
        (_, Some(m)) => cs.parents[m - 1],
    };
    root_of(ws, Some(parent)).await
}

/// `Revert "x"` / `Reapply "x"` and the "This reverts" line.
fn revert_message(subject: &str, git: &str, mainline_parent: Option<&str>) -> String {
    let title = match subject.strip_prefix("Revert \"").and_then(|s| s.strip_suffix('"')) {
        Some(inner) => format!("Reapply \"{inner}\""),
        None => format!("Revert \"{subject}\""),
    };
    match mainline_parent {
        Some(p) => format!("{title}\n\nThis reverts commit {git}, reversing\nchanges made to {p}.\n"),
        None => format!("{title}\n\nThis reverts commit {git}.\n"),
    }
}

/// Appends `(cherry picked from commit <id>)`, after a blank line unless the message already ends
/// in a trailer block.
fn with_origin(message: &[u8], git: &str) -> Vec<u8> {
    let mut out = message.to_vec();
    if !out.ends_with(b"\n") {
        out.push(b'\n');
    }
    let text = String::from_utf8_lossy(&out).into_owned();
    let paragraphs: Vec<&str> = text.trim_end_matches('\n').split("\n\n").collect();
    let is_trailer = |l: &str| {
        l.starts_with("(cherry picked from commit ")
            || l.split_once(": ").is_some_and(|(k, _)| !k.is_empty() && k.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-'))
    };
    let footer = paragraphs.len() > 1 && paragraphs.last().is_some_and(|p| p.lines().all(is_trailer));
    if !footer {
        out.push(b'\n');
    }
    out.extend_from_slice(format!("(cherry picked from commit {git})\n").as_bytes());
    out
}

async fn step(
    ctx: &Ctx,
    ws: &WsCtx,
    gitdir: &Path,
    replay: Replay,
    id: ChangesetId,
    opts: &Opts,
    staged_ok: bool,
) -> Result<Step> {
    let cs = ws.engine.get_changeset(id).await?;
    let git = conflict::git_id(ws, id).await?;
    let Some(head) = ws.head_commit().await? else {
        return Err(fatal("asc: HEAD is unborn"));
    };
    let head_root = root_of(ws, Some(head)).await?;
    let current = if staged_ok { index_tree(ws, head_root).await?.0 } else { head_root };
    let parent = parent_root(ws, replay, id, &cs, opts.mainline).await?;
    let abbrev = git.to_string()[..7].to_owned();
    let subject = first_line(&cs.message);
    let named = format!("{abbrev} ({subject})");
    let parent_named = format!("parent of {named}");
    let (sides, theirs_label, base_label, op) = match replay {
        Replay::Pick => (Sides { base: parent, ours: current, theirs: cs.root }, named.clone(), parent_named, Op::Pick),
        Replay::Revert => (Sides { base: cs.root, ours: current, theirs: parent }, parent_named, named.clone(), Op::Revert),
    };
    let favor = conflict::favor_of(&opts.strategy_options)?;
    let labels = Labels { base: base_label, ours: "HEAD".into(), theirs: theirs_label.clone() };
    let (merge_opts, differ) = conflict::merge_options(ctx, ws, &sides, favor, labels).await?;
    let outcome = ws.engine.merge_trees(sides.base, sides.ours, sides.theirs, &merge_opts).await.map_err(fatal)?;
    conflict::refuse_unsupported(&outcome)?;
    conflict::precheck(ctx, ws, op, current, &outcome, staged_ok).await?;
    conflict::print_messages(&conflict::messages(&outcome, &differ, "HEAD", &theirs_label));
    conflict::materialise(ctx, ws, head_root, current, &outcome).await?;
    seqstate::write_auto_merge(gitdir, conflict::git_tree_id(ws, outcome.root).await?)?;

    let mut text = match replay {
        Replay::Pick if opts.record_origin => with_origin(&cs.message, &git.to_string()),
        Replay::Pick => cs.message.clone(),
        Replay::Revert => {
            let mainline_parent = match opts.mainline {
                Some(m) => Some(conflict::git_id(ws, cs.parents[m - 1]).await?.to_string()),
                None => None,
            };
            revert_message(&subject, &git.to_string(), mainline_parent.as_deref()).into_bytes()
        }
    };
    let comment = message::comment_char(ctx);
    if !outcome.is_clean() {
        text.extend_from_slice(&conflict::conflicts_block(&outcome, comment));
        seqstate::write(gitdir, "MERGE_MSG", &text)?;
        // `cherry-pick -n` leaves no CHERRY_PICK_HEAD; `revert -n` leaves REVERT_HEAD.
        if !(opts.no_commit && replay == Replay::Pick) {
            seqstate::write(gitdir, replay.head_file(), format!("{git}\n").as_bytes())?;
        }
        let verb = if replay == Replay::Pick { "apply" } else { "revert" };
        eprintln!("error: could not {verb} {abbrev}... {subject}");
        if conflict::advice(ctx, "mergeConflict") && opts.no_commit {
            let t = tool(ctx);
            eprintln!(
                "hint: after resolving the conflicts, mark the corrected paths\n\
                 hint: with '{t} add <paths>' or '{t} rm <paths>'\n\
                 hint: Disable this message with \"{t} config set advice.mergeConflict false\""
            );
        } else if conflict::advice(ctx, "mergeConflict") {
            let (t, cmd) = (tool(ctx), replay.command());
            eprintln!(
                "hint: After resolving the conflicts, mark them with\n\
                 hint: \"{t} add/rm <pathspec>\", then run\n\
                 hint: \"{t} {cmd} --continue\".\n\
                 hint: You can instead skip this commit with \"{t} {cmd} --skip\".\n\
                 hint: To abort and get back to the state before \"{t} {cmd}\",\n\
                 hint: run \"{t} {cmd} --abort\".\n\
                 hint: Disable this message with \"{t} config set advice.mergeConflict false\""
            );
        }
        return Ok(Step::Stopped);
    }
    if opts.no_commit {
        seqstate::write(gitdir, "MERGE_MSG", &text)?;
        if replay == Replay::Revert {
            seqstate::write(gitdir, replay.head_file(), format!("{git}\n").as_bytes())?;
        }
        return Ok(Step::Done);
    }
    let originally_empty = !cs.parents.is_empty() && parent == cs.root;
    let allowed = opts.keep_redundant || (originally_empty && opts.allow_empty);
    if outcome.root == head_root && !allowed {
        seqstate::write(gitdir, "MERGE_MSG", &text)?;
        if replay == Replay::Pick {
            seqstate::write(gitdir, replay.head_file(), format!("{git}\n").as_bytes())?;
            stopped_empty(ctx, ws, replay).await?;
        } else {
            // A revert runs `git commit`, which fails with its "nothing to commit" status and
            // leaves no REVERT_HEAD.
            seqstate::write(gitdir, "COMMIT_EDITMSG", &text)?;
            let status = super::status::long_text(ctx, ws, None).await?;
            std::io::stdout().write_all(&status)?;
        }
        return Ok(Step::Stopped);
    }
    let author = match replay {
        Replay::Pick => Some(cs.author.clone()),
        Replay::Revert => None,
    };
    commit_result(ctx, ws, gitdir, replay, head, head_root, outcome.root, author, &text, false).await?;
    Ok(Step::Done)
}

/// git's text when a replayed commit has become empty.
async fn stopped_empty(ctx: &Ctx, ws: &WsCtx, replay: Replay) -> Result<()> {
    let (t, cmd) = (tool(ctx), replay.command());
    eprintln!(
        "The previous {cmd} is now empty, possibly due to conflict resolution.\n\
         If you wish to commit it anyway, use:\n\n    {t} commit --allow-empty\n\n\
         Otherwise, please use '{t} {cmd} --skip'"
    );
    let text = super::status::long_text(ctx, ws, None).await?;
    std::io::stdout().write_all(&text)?;
    Ok(())
}

/// Writes the commit for a clean replay (or a resolved one), moves the branch and prints git's
/// summary.
#[allow(clippy::too_many_arguments)]
async fn commit_result(
    ctx: &Ctx,
    ws: &WsCtx,
    gitdir: &Path,
    replay: Replay,
    head: ChangesetId,
    head_root: TreeId,
    root: TreeId,
    author: Option<asc_core::sig::Signature>,
    message_text: &[u8],
    strip: bool,
) -> Result<()> {
    let idents = conflict::identities(ctx)?;
    let comment = message::comment_char(ctx);
    // git's `commit` (after a conflict) leaves COMMIT_EDITMSG; a clean replay does not.
    if strip {
        seqstate::write(gitdir, "COMMIT_EDITMSG", message_text)?;
    }
    // A replay keeps the message bytes; a resolved one drops the `# Conflicts:` comment block.
    let final_msg = if strip {
        message::cleanup(message_text, Cleanup::Strip, true, comment)
    } else {
        message_text.to_vec()
    };
    let author = author.unwrap_or_else(|| idents.author.clone());
    let (id, git) = conflict::write_commit(
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
    let label = conflict::branch_label(ws)?;
    let op = conflict::move_head(
        ctx,
        ws,
        Some(head),
        id,
        &format!("{}: {subject}", replay.command()),
        StagingAfterMove::Set(Vec::new()),
        Some(&idents.committer),
    )
    .await?;
    conflict::announce(ws, id, op).await?;
    if strip {
        seqstate::clear_replay(gitdir); // `git commit` also removes AUTO_MERGE
    } else {
        seqstate::remove(gitdir, replay.head_file());
        seqstate::remove(gitdir, "MERGE_MSG");
    }
    ws_ops::clear(&ws.db, ws.workspace.ws.id).await?;
    // A resolved revert goes through `git commit`, which shows no author date.
    let show_date = !(strip && replay == Replay::Revert);
    print_summary(ws, &label, &git.to_string(), &subject, &author, &idents.committer, head_root, root, show_date).await?;
    let runner = Runner::for_ctx(ctx)?;
    let env = HookEnv::of(ctx, replay.command()).with_index(gitdir.join("index")).without_editor();
    let _ = runner.run(Hook::PostCommit, &env).await?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn print_summary(
    ws: &WsCtx,
    label: &str,
    git: &str,
    subject: &str,
    author: &asc_core::sig::Signature,
    committer: &asc_core::sig::Signature,
    from: TreeId,
    to: TreeId,
    show_date: bool,
) -> Result<()> {
    let mut out = Vec::new();
    writeln!(out, "[{label} {}] {subject}", &git[..7])?;
    if author.name() != committer.name() || author.email() != committer.email() {
        writeln!(
            out,
            " Author: {} <{}>",
            String::from_utf8_lossy(author.name()),
            String::from_utf8_lossy(author.email())
        )?;
    }
    if show_date {
        let date = super::format::date::Mode::default().render(author.time(), author.tz_min(), author.time());
        writeln!(out, " Date: {date}")?;
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

// ---------------------------------------------------------------- continue, skip, abort

/// Resets the paths a stopped replay touched back to HEAD (git's `reset --merge`): the staged
/// paths only, so unrelated local work stays.
async fn reset_touched(ctx: &Ctx, ws: &WsCtx, target_root: TreeId) -> Result<()> {
    let rows = stage::overlay(&ws.db, ws.workspace.ws.id, StageSet::Default).await?;
    let head_root = root_of(ws, ws.head_commit().await?).await?;
    let mut paths: Vec<RepoPath> = rows.iter().filter_map(|r| RepoPath::new(r.path.clone()).ok()).collect();
    paths.extend(ws.engine.leafdiff(head_root, target_root, None).await?.into_iter().map(|(p, _)| p));
    paths.sort();
    paths.dedup();
    sync_worktree(ctx, target_root, &paths, true).await?;
    stage::clear(&ws.db, ws.workspace.ws.id, StageSet::Default).await?;
    crate::ext::index_sink().on_stage_changed(ws.workspace.ws.id, &[]).await.map_err(fatal)
}

async fn write_orig_head(ws: &WsCtx, gitdir: &Path) -> Result<()> {
    if let Some(h) = ws.head_commit().await? {
        seqstate::write_orig_head(gitdir, conflict::git_id(ws, h).await?)?;
    }
    Ok(())
}

async fn skip(ctx: &Ctx, ws: &WsCtx, gitdir: &Path, replay: Replay) -> Result<ExitCode> {
    crate::ext::op_sink().snapshot_wc();
    let head = ws.head_commit().await?;
    let head_root = root_of(ws, head).await?;
    reset_touched(ctx, ws, head_root).await?;
    write_orig_head(ws, gitdir).await?;
    seqstate::clear_replay(gitdir);
    let Some(mut seq) = Sequence::load(gitdir)? else {
        return Ok(ExitCode::SUCCESS);
    };
    if !seq.todo.is_empty() {
        seq.todo.remove(0);
    }
    seq.save(gitdir)?;
    drive(ctx, ws, gitdir, replay, seq, true).await
}

async fn abort(ctx: &Ctx, ws: &WsCtx, gitdir: &Path, _replay: Replay) -> Result<ExitCode> {
    crate::ext::op_sink().snapshot_wc();
    let head = ws.head_commit().await?;
    if let Some(seq) = Sequence::load(gitdir)? {
        let current = match head {
            Some(h) => Some(conflict::git_id(ws, h).await?),
            None => None,
        };
        if current != Some(seq.abort_safety) {
            eprintln!("warning: You seem to have moved HEAD. Not rewinding, check your HEAD!");
            Sequence::remove(gitdir);
            seqstate::clear_replay(gitdir);
            return Ok(ExitCode::SUCCESS);
        }
        let original = conflict::changeset_of(ws, seq.head).await?;
        let target_root = root_of(ws, Some(original)).await?;
        reset_touched(ctx, ws, target_root).await?;
        write_orig_head(ws, gitdir).await?;
        if head != Some(original) {
            conflict::move_head(ctx, ws, head, original, "reset: moving to HEAD", StagingAfterMove::Set(Vec::new()), None).await?;
        }
        Sequence::remove(gitdir);
    } else {
        let head_root = root_of(ws, head).await?;
        reset_touched(ctx, ws, head_root).await?;
        write_orig_head(ws, gitdir).await?;
    }
    seqstate::clear_replay(gitdir);
    Ok(ExitCode::SUCCESS)
}

async fn resume(ctx: &Ctx, ws: &WsCtx, gitdir: &Path, replay: Replay) -> Result<ExitCode> {
    let rows = stage::overlay(&ws.db, ws.workspace.ws.id, StageSet::Default).await?;
    if rows.iter().any(|r| r.stage > 0) {
        return Err(unmerged_files(replay));
    }
    let seq = Sequence::load(gitdir)?;
    let active = seqstate::replay_in_progress(gitdir);
    let opts = seq.as_ref().map(|s| s.opts.clone()).unwrap_or_default();
    if let Some(kind) = active {
        let Some(head) = ws.head_commit().await? else {
            return Err(fatal("asc: HEAD is unborn"));
        };
        let head_root = root_of(ws, Some(head)).await?;
        let changes = stagebuild::rows_to_changes(ws, head_root, &rows).await?;
        let root = ws.engine.apply(head_root, &changes).await?;
        let picked = seqstate::read_oid(gitdir, kind.head_file())?.ok_or_else(|| fatal("asc: the replay head file is empty"))?;
        let picked = conflict::changeset_of(ws, picked).await?;
        let cs = ws.engine.get_changeset(picked).await?;
        if root == head_root && !opts.keep_redundant && !opts.allow_empty {
            stopped_empty(ctx, ws, kind).await?;
            return Ok(ExitCode::from(1));
        }
        let text = seqstate::read(gitdir, "MERGE_MSG")?.unwrap_or_else(|| cs.message.clone());
        let author = (kind == Replay::Pick).then(|| cs.author.clone());
        commit_result(ctx, ws, gitdir, kind, head, head_root, root, author, &text, true).await?;
    }
    let Some(mut seq) = seq else {
        return Ok(ExitCode::SUCCESS);
    };
    if !seq.todo.is_empty() {
        seq.todo.remove(0);
    }
    if let Some(h) = ws.head_commit().await? {
        seq.abort_safety = conflict::git_id(ws, h).await?;
    }
    seq.save(gitdir)?;
    drive(ctx, ws, gitdir, replay, seq, true).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_revert_message_names_the_commit_and_a_revert_of_a_revert_reapplies() {
        let git = "a".repeat(40);
        assert_eq!(
            revert_message("side change", &git, None),
            format!("Revert \"side change\"\n\nThis reverts commit {git}.\n")
        );
        assert!(revert_message("Revert \"x\"", &git, None).starts_with("Reapply \"x\"\n\n"));
        assert!(revert_message("m", &git, Some("bbb")).contains("reversing\nchanges made to bbb.\n"));
    }

    #[test]
    fn the_origin_line_follows_a_blank_line_unless_a_trailer_block_ends_the_message() {
        let git = "c".repeat(40);
        let plain = with_origin(b"subject\n\nbody\n", &git);
        assert_eq!(plain, format!("subject\n\nbody\n\n(cherry picked from commit {git})\n").into_bytes());
        let one_line = with_origin(b"subject\n", &git);
        assert_eq!(one_line, format!("subject\n\n(cherry picked from commit {git})\n").into_bytes());
        let trailers = with_origin(b"subject\n\nSigned-off-by: A <a@b>\n", &git);
        assert_eq!(
            trailers,
            format!("subject\n\nSigned-off-by: A <a@b>\n(cherry picked from commit {git})\n").into_bytes()
        );
    }

    #[test]
    fn the_subject_is_the_first_line() {
        assert_eq!(first_line(b"one\ntwo\n"), "one");
        assert_eq!(first_line(b""), "");
    }
}
