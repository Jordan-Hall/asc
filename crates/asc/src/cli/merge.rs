//! `asc merge [<options>] <commit>` (SPEC §4.4): fast-forward, three-way merge through the engine
//! (`merge_trees`, `ort` semantics), `--no-ff`, `--ff-only`, `--squash`, `--no-commit`,
//! `-X ours|theirs`, `-m`, `--abort`, `--continue`, `--quit`, with git's state files
//! (`seqstate`), messages and hooks (`pre-merge-commit`, `prepare-commit-msg`, `commit-msg`,
//! `post-merge`).
//!
//! Not served, and refused with a message rather than approximated: octopus merges (more than
//! one commit), merging into an unborn branch, file/directory and case conflicts, the editor
//! (`-e`), signing and strategies other than `ort`.

use std::ffi::OsString;
use std::io::Write as _;
use std::process::ExitCode;

use asc_core::id::{ChangesetId, GitOid, TreeId};
use asc_core::path::RefName;
use asc_engine::diff::{DiffCtx, DiffOptions, NoAttributes, RenameOptions, write_stat};
use asc_engine::diff::format::write_summary;
use asc_engine::graph::LogQuery;
use asc_engine::history::HistoryApi;
use asc_engine::merge::{Labels, MergeOutcome};
use asc_state::state::stage::{self, StageSet};
use asc_store::refs::Refs;
use clap::Parser as _;

use super::commit::message::{self, Cleanup};
use super::common::{CliEngine, CliError, Ctx, Result, StagingAfterMove, WsCtx};
use super::conflict::{self, NewCommit, Op, Sides, fatal};
use super::format::date::Mode as DateMode;
use super::restore::{resolve_commit, root_of, sync_worktree};
use super::seqstate::{self, FfMode, MergeState};
use crate::hooks_runner::{Gate, Hook, HookEnv, MsgSource, Runner};
use crate::native::NativeSpec;

#[derive(clap::Parser, Debug)]
#[command(name = "merge", disable_help_flag = true, disable_version_flag = true)]
pub struct Args {
    /// The merge commit message.
    #[arg(short = 'm', long = "message")]
    pub message: Vec<String>,
    #[arg(long = "no-commit", overrides_with = "commit")]
    pub no_commit: bool,
    #[arg(long = "commit", overrides_with = "no_commit")]
    pub commit: bool,
    #[arg(long = "ff", overrides_with_all = ["no_ff", "ff_only"])]
    pub ff: bool,
    #[arg(long = "no-ff", overrides_with_all = ["ff", "ff_only"])]
    pub no_ff: bool,
    #[arg(long = "ff-only", overrides_with_all = ["ff", "no_ff"])]
    pub ff_only: bool,
    #[arg(long = "squash", overrides_with = "no_squash")]
    pub squash: bool,
    #[arg(long = "no-squash", overrides_with = "squash")]
    pub no_squash: bool,
    #[arg(short = 'X', long = "strategy-option")]
    pub strategy_option: Vec<String>,
    #[arg(short = 's', long = "strategy")]
    pub strategy: Vec<String>,
    #[arg(long = "abort")]
    pub abort: bool,
    #[arg(long = "continue")]
    pub cont: bool,
    #[arg(long = "quit")]
    pub quit: bool,
    #[arg(short = 'q', long = "quiet")]
    pub quiet: bool,
    /// `-n` is `--no-stat`.
    #[arg(short = 'n', long = "no-stat")]
    pub no_stat: bool,
    #[arg(long = "stat")]
    pub stat: bool,
    #[arg(long = "no-edit")]
    pub no_edit: bool,
    #[arg(long = "no-verify")]
    pub no_verify: bool,
    #[arg(long = "verify")]
    pub verify: bool,
    #[arg(long = "allow-unrelated-histories")]
    pub allow_unrelated: bool,
    /// The commits to merge.
    pub commits: Vec<OsString>,
}

pub const NATIVE: &[NativeSpec] = &[NativeSpec {
    cmd: "merge",
    flags: &[
        "-m",
        "--message",
        "--no-commit",
        "--commit",
        "--ff",
        "--no-ff",
        "--ff-only",
        "--squash",
        "--no-squash",
        "-X",
        "--strategy-option",
        "-s",
        "--strategy",
        "--abort",
        "--continue",
        "--quit",
        "-q",
        "--quiet",
        "-n",
        "--no-stat",
        "--stat",
        "--no-edit",
        "--no-verify",
        "--verify",
        "--allow-unrelated-histories",
    ],
    placeholders: &[],
}];

impl Args {
    fn ff_mode(&self) -> FfMode {
        if self.ff_only {
            FfMode::Only
        } else if self.no_ff {
            FfMode::Never
        } else {
            FfMode::Allow
        }
    }
}

pub async fn run(ctx: &Ctx, a: Args) -> Result<ExitCode> {
    let ws = ctx.ws()?;
    let gitdir = ws.projection.git_dir().to_owned();
    let modes = [a.abort, a.cont, a.quit].iter().filter(|&&m| m).count();
    if modes > 1 {
        return Err(CliError::Usage("error: options '--abort', '--continue' and '--quit' cannot be used together".into()));
    }
    if a.abort {
        return abort(ctx, ws, &gitdir).await;
    }
    if a.cont {
        return resume(ctx, &gitdir).await;
    }
    if a.quit {
        if !seqstate::exists(&gitdir, "MERGE_HEAD") {
            return Err(CliError::Fatal("There is no merge in progress (MERGE_HEAD missing).".into()));
        }
        seqstate::clear_merge(&gitdir);
        return Ok(ExitCode::SUCCESS);
    }
    if let Some(bad) = a.strategy.iter().find(|s| !matches!(s.as_str(), "ort" | "recursive")) {
        return Err(fatal(format!("asc: merge strategy '{bad}' is not supported (only ort)")));
    }
    start(ctx, ws, &gitdir, &a).await
}

// ---------------------------------------------------------------- abort, continue

async fn abort(ctx: &Ctx, ws: &WsCtx, gitdir: &std::path::Path) -> Result<ExitCode> {
    if !seqstate::exists(gitdir, "MERGE_HEAD") {
        return Err(CliError::Fatal("There is no merge to abort (MERGE_HEAD missing).".into()));
    }
    crate::ext::op_sink().snapshot_wc();
    let head = ws.head_commit().await?;
    let head_root = root_of(ws, head).await?;
    let rows = stage::overlay(&ws.db, ws.workspace.ws.id, StageSet::Default).await?;
    let mut paths: Vec<asc_core::path::RepoPath> = rows
        .iter()
        .filter_map(|r| asc_core::path::RepoPath::new(r.path.clone()).ok())
        .collect();
    paths.sort();
    paths.dedup();
    sync_worktree(ctx, head_root, &paths, true).await?;
    stage::clear(&ws.db, ws.workspace.ws.id, StageSet::Default).await?;
    crate::ext::index_sink()
        .on_stage_changed(ws.workspace.ws.id, &[])
        .await
        .map_err(fatal)?;
    seqstate::clear_merge(gitdir);
    Ok(ExitCode::SUCCESS)
}

async fn resume(ctx: &Ctx, gitdir: &std::path::Path) -> Result<ExitCode> {
    if !seqstate::exists(gitdir, "MERGE_HEAD") {
        return Err(CliError::Fatal("There is no merge in progress (MERGE_HEAD missing).".into()));
    }
    let args = super::commit::Args::try_parse_from(["commit"]).map_err(|e| fatal(e.to_string()))?;
    super::commit::run(ctx, args).await
}

// ---------------------------------------------------------------- starting a merge

/// What the user named, for the message and the markers.
struct Target {
    id: ChangesetId,
    /// The argument as typed: the marker label.
    name: String,
    /// `branch 'x'`, `tag 'x'`, `remote-tracking branch 'x'` or `commit 'x'`.
    described: String,
}

async fn describe(ws: &WsCtx, name: &str) -> String {
    for (prefix, what) in [
        ("refs/heads/", "branch"),
        ("refs/tags/", "tag"),
        ("refs/remotes/", "remote-tracking branch"),
    ] {
        let Ok(full) = RefName::new(format!("{prefix}{name}").into_bytes()) else { continue };
        if let Ok(Some(_)) = ws.db.refs().get(ws.repo.key, &full).await {
            return format!("{what} '{name}'");
        }
    }
    format!("commit '{name}'")
}

fn unmerged_error() -> CliError {
    CliError::Raw(
        "error: Merging is not possible because you have unmerged files.\n\
         hint: Fix them up in the work tree, and then use 'git add/rm <file>'\n\
         hint: as appropriate to mark resolution and make a commit.\n\
         fatal: Exiting because of an unresolved conflict."
            .to_owned(),
        128,
    )
}

/// `Merge branch 'x'` with ` into <branch>` unless the current branch is a suppressed one.
fn default_message(ctx: &Ctx, ws: &WsCtx, t: &Target) -> Result<String> {
    let mut text = format!("Merge {}", t.described);
    if let asc_git::projection::Head::Branch(b) = ws.projection.read_head()? {
        let configured = ctx.config.get_all("merge.suppressDest");
        let suppressed = if configured.is_empty() {
            b == "master" || b == "main"
        } else {
            configured.iter().any(|g| g == &b || g == "*")
        };
        if !suppressed {
            text.push_str(&format!(" into {b}"));
        }
    }
    Ok(text)
}

async fn start(ctx: &Ctx, ws: &WsCtx, gitdir: &std::path::Path, a: &Args) -> Result<ExitCode> {
    let rows = stage::overlay(&ws.db, ws.workspace.ws.id, StageSet::Default).await?;
    if rows.iter().any(|r| r.stage > 0) {
        return Err(unmerged_error());
    }
    if seqstate::exists(gitdir, "MERGE_HEAD") {
        return Err(CliError::Raw(
            "fatal: You have not concluded your merge (MERGE_HEAD exists).\nPlease, commit your changes before you merge."
                .to_owned(),
            128,
        ));
    }
    if a.commits.is_empty() {
        return Err(CliError::Fatal("No remote for the current branch.".into()));
    }
    if a.commits.len() > 1 {
        return Err(fatal("asc: octopus merges (more than one commit) are not supported yet"));
    }
    let favor = conflict::favor_of(&a.strategy_option)?;
    let Some(head) = ws.head_commit().await? else {
        return Err(fatal("asc: merging into an unborn branch is not supported yet"));
    };
    let name = a.commits[0].to_string_lossy().into_owned();
    let Some(id) = resolve_commit(ctx, &name).await? else {
        return Err(CliError::Raw(format!("merge: {name} - not something we can merge"), 1));
    };
    let target = Target { id, described: describe(ws, &name).await, name };
    let head_git = conflict::git_id(ws, head).await?;
    conflict::git_id(ws, target.id).await?;
    seqstate::write_orig_head(gitdir, head_git)?;

    let up_to_date = target.id == head || ws.engine.is_ancestor(target.id, head).await.map_err(fatal)?;
    if up_to_date {
        if !a.quiet {
            println!("Already up to date.{}", if a.squash { " (nothing to squash)" } else { "" });
        }
        return Ok(ExitCode::SUCCESS);
    }
    let bases = ws.engine.merge_base(head, &[target.id], true).await.map_err(fatal)?;
    if bases.is_empty() && !a.allow_unrelated {
        return Err(CliError::Fatal("refusing to merge unrelated histories".into()));
    }
    let can_ff = ws.engine.is_ancestor(head, target.id).await.map_err(fatal)?;
    let mode = a.ff_mode();
    if mode == FfMode::Only && !can_ff {
        let mut text = String::new();
        if conflict::advice(ctx, "diverging") {
            text.push_str(
                "hint: Diverging branches can't be fast-forwarded, you need to either:\n\
                 hint:\n\
                 hint: \tgit merge --no-ff\n\
                 hint:\n\
                 hint: or:\n\
                 hint:\n\
                 hint: \tgit rebase\n\
                 hint:\n\
                 hint: Disable this message with \"git config set advice.diverging false\"\n",
            );
        }
        text.push_str("fatal: Not possible to fast-forward, aborting.");
        return Err(CliError::Raw(text, 128));
    }
    if can_ff && mode != FfMode::Never {
        return fast_forward(ctx, ws, gitdir, a, head, &target).await;
    }
    real_merge(ctx, ws, gitdir, a, head, &bases, &target, favor).await
}

// ---------------------------------------------------------------- output helpers

/// The diffstat and the create/delete/mode lines between two trees.
async fn print_stat(ws: &WsCtx, from: TreeId, to: TreeId) -> Result<()> {
    let opts = DiffOptions {
        renames: Some(RenameOptions { limit: 100, ..RenameOptions::default() }),
        ..DiffOptions::default()
    };
    let changes = ws.engine.diff_trees(from, to, None, &opts).await.map_err(fatal)?;
    let dctx = DiffCtx { opts: &opts, content: &ws.engine, attrs: &NoAttributes };
    let mut out = Vec::new();
    write_stat(&mut out, &dctx, &changes).await.map_err(fatal)?;
    write_summary(&mut out, &changes, &opts).map_err(fatal)?;
    std::io::stdout().write_all(&out)?;
    Ok(())
}

fn short(oid: GitOid) -> String {
    oid.to_string()[..7].to_owned()
}

/// `Squashed commit of the following:` and the commits `head..target`, newest first.
async fn squash_message(ws: &WsCtx, head: ChangesetId, target: ChangesetId) -> Result<Vec<u8>> {
    let q = LogQuery { include: vec![target], exclude: vec![head], limit: Some(10_000), ..LogQuery::default() };
    let page = HistoryApi::log(&ws.engine, &q, None).await.map_err(fatal)?;
    let mut out = b"Squashed commit of the following:\n".to_vec();
    for entry in &page.entries {
        let cs = ws.engine.get_changeset(entry.id).await?;
        let git = conflict::git_id(ws, entry.id).await?;
        let date = DateMode::default().render(cs.author.time(), cs.author.tz_min(), cs.author.time());
        out.extend_from_slice(
            format!(
                "\ncommit {git}\nAuthor: {} <{}>\nDate:   {date}\n\n",
                String::from_utf8_lossy(cs.author.name()),
                String::from_utf8_lossy(cs.author.email())
            )
            .as_bytes(),
        );
        let text = String::from_utf8_lossy(&cs.message);
        for line in text.trim_end_matches('\n').split('\n') {
            out.extend_from_slice(format!("    {line}\n").as_bytes());
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------- fast-forward

async fn fast_forward(
    ctx: &Ctx,
    ws: &WsCtx,
    gitdir: &std::path::Path,
    a: &Args,
    head: ChangesetId,
    target: &Target,
) -> Result<ExitCode> {
    let head_root = root_of(ws, Some(head)).await?;
    let target_root = root_of(ws, Some(target.id)).await?;
    let (old, new) = (conflict::git_id(ws, head).await?, conflict::git_id(ws, target.id).await?);
    if !a.quiet {
        println!("Updating {}..{}", short(old), short(new));
        std::io::stdout().flush()?;
    }
    let outcome = MergeOutcome { root: target_root, conflicts: Vec::new() };
    conflict::precheck(ctx, ws, Op::FastForward, head_root, &outcome, true).await?;
    if !a.quiet {
        println!("Fast-forward");
    }
    let squash = a.squash;
    if squash {
        if !a.quiet {
            println!("Squash commit -- not updating HEAD");
        }
        seqstate::write(gitdir, "SQUASH_MSG", &squash_message(ws, head, target.id).await?)?;
    }
    let touched: Vec<_> = ws
        .engine
        .leafdiff(head_root, target_root, None)
        .await?
        .into_iter()
        .map(|(p, _)| p)
        .collect();
    conflict::index_result(ws, head_root, target_root).await?;
    sync_worktree(ctx, target_root, &touched, true).await?;
    // Staged changes at other paths stay staged: the precheck refused any overlap.
    let kept = stage::overlay(&ws.db, ws.workspace.ws.id, StageSet::Default).await?;
    if squash {
        // HEAD stays; the index takes the target's changes.
        let mut rows = stage_rows_against_head(ws, head_root, target_root).await?;
        rows.extend(kept);
        stage::replace(&ws.db, ws.workspace.ws.id, StageSet::Default, &rows).await?;
        crate::ext::index_sink().on_stage_changed(ws.workspace.ws.id, &[]).await.map_err(fatal)?;
    } else {
        conflict::move_head(
            ctx,
            ws,
            Some(head),
            target.id,
            &format!("merge {}: Fast-forward", target.name),
            StagingAfterMove::Set(kept),
            None,
        )
        .await?;
    }
    if !a.quiet && !a.no_stat {
        print_stat(ws, head_root, target_root).await?;
    }
    post_merge(ctx, ws, squash).await?;
    Ok(ExitCode::SUCCESS)
}

/// Stage-0 rows for every path where `root` differs from `head_root`.
async fn stage_rows_against_head(ws: &WsCtx, head_root: TreeId, root: TreeId) -> Result<Vec<stage::StageRow>> {
    let mut rows = Vec::new();
    for (path, _) in ws.engine.leafdiff(head_root, root, None).await? {
        match ws.engine.lookup(root, &path).await? {
            Some(e) => rows.push(conflict::leaf_row(&path, 0, &e)),
            None => rows.push(stage::StageRow::from_leaf(path.as_bytes().to_vec(), None)),
        }
    }
    Ok(rows)
}

async fn post_merge(ctx: &Ctx, ws: &WsCtx, squash: bool) -> Result<()> {
    let runner = Runner::for_ctx(ctx)?;
    let env = HookEnv::of(ctx, "merge").with_index(ws.projection.git_dir().join("index")).without_editor();
    let _ = runner.run(Hook::PostMerge { squash }, &env).await?;
    Ok(())
}

// ---------------------------------------------------------------- three-way merge

/// The tree to merge over: the one base's tree, the empty tree when unrelated, or the virtual
/// base of several (their merge, markers kept, as git does).
async fn base_tree(ws: &WsCtx, bases: &[ChangesetId], opts: &asc_engine::merge::MergeOptions) -> Result<TreeId> {
    match bases {
        [] => Ok(CliEngine::empty_tree_id()),
        [one] => root_of(ws, Some(*one)).await,
        [first, rest @ ..] => {
            let mut acc = root_of(ws, Some(*first)).await?;
            let mut acc_commit = *first;
            for &next in rest {
                let common = ws.engine.merge_base(acc_commit, &[next], false).await.map_err(fatal)?;
                let under = match common.first() {
                    Some(&c) => root_of(ws, Some(c)).await?,
                    None => CliEngine::empty_tree_id(),
                };
                let theirs = root_of(ws, Some(next)).await?;
                acc = ws.engine.merge_trees(under, acc, theirs, opts).await.map_err(fatal)?.root;
                acc_commit = next;
            }
            Ok(acc)
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn real_merge(
    ctx: &Ctx,
    ws: &WsCtx,
    gitdir: &std::path::Path,
    a: &Args,
    head: ChangesetId,
    bases: &[ChangesetId],
    target: &Target,
    favor: asc_engine::merge::Favor,
) -> Result<ExitCode> {
    let head_root = root_of(ws, Some(head)).await?;
    let theirs_root = root_of(ws, Some(target.id)).await?;
    let base_label = match bases {
        [one] => short(conflict::git_id(ws, *one).await?),
        _ => "merged common ancestors".to_owned(),
    };
    let labels = Labels { base: base_label, ours: "HEAD".into(), theirs: target.name.clone() };
    let plain = conflict::merge_options(
        ctx,
        ws,
        &Sides { base: head_root, ours: head_root, theirs: head_root },
        favor,
        labels.clone(),
    )
    .await?
    .0;
    let base_root = base_tree(ws, bases, &plain).await?;
    let sides = Sides { base: base_root, ours: head_root, theirs: theirs_root };
    let (opts, differ) = conflict::merge_options(ctx, ws, &sides, favor, labels).await?;
    let outcome = ws.engine.merge_trees(base_root, head_root, theirs_root, &opts).await.map_err(fatal)?;
    conflict::refuse_unsupported(&outcome)?;
    conflict::precheck(ctx, ws, Op::Merge, head_root, &outcome, false).await?;

    conflict::print_messages(&conflict::messages(&outcome, &differ, "HEAD", &target.name));
    conflict::materialise(ctx, ws, head_root, head_root, &outcome).await?;
    seqstate::write_auto_merge(gitdir, conflict::git_tree_id(ws, outcome.root).await?)?;

    let comment = message::comment_char(ctx);
    let mut text = if a.message.is_empty() {
        default_message(ctx, ws, target)?
    } else {
        a.message.join("\n\n")
    }
    .into_bytes();
    text.push(b'\n');
    let target_git = conflict::git_id(ws, target.id).await?;
    let conflicted = !outcome.is_clean();

    if a.squash {
        seqstate::write(gitdir, "SQUASH_MSG", &squash_message(ws, head, target.id).await?)?;
        if conflicted {
            let mut msg = Vec::new();
            msg.extend_from_slice(&conflict::conflicts_block(&outcome, comment));
            seqstate::write(gitdir, "MERGE_MSG", &msg)?;
        }
        println!("Squash commit -- not updating HEAD");
        return Ok(finish_stopped(conflicted));
    }
    if conflicted {
        text.extend_from_slice(&conflict::conflicts_block(&outcome, comment));
    }
    seqstate::write_merge(gitdir, &MergeState { heads: &[target_git], message: &text, mode: a.ff_mode() })?;
    if conflicted || a.no_commit {
        return Ok(finish_stopped(conflicted));
    }

    // The automatic commit: hooks, the message as the hooks left it, the commit, the ref move.
    let runner = Runner::for_ctx(ctx)?;
    let env = HookEnv::of(ctx, "merge").with_index(gitdir.join("index")).without_editor();
    let verify = !a.no_verify;
    let not_committing = || eprintln!("Not committing merge; use 'git commit' to complete the merge.");
    if verify && let Gate::Reject(_) = runner.gate(Hook::PreMergeCommit, &env).await? {
        not_committing();
        return Ok(ExitCode::from(1));
    }
    let msg_file = gitdir.join("MERGE_MSG");
    let prepare = Hook::PrepareCommitMsg { file: msg_file.clone(), source: Some(MsgSource::Merge) };
    if let Gate::Reject(_) = runner.gate(prepare, &env).await? {
        not_committing();
        return Ok(ExitCode::from(1));
    }
    if verify && let Gate::Reject(_) = runner.gate(Hook::CommitMsg { file: msg_file.clone() }, &env).await? {
        not_committing();
        return Ok(ExitCode::from(1));
    }
    let raw = seqstate::read(gitdir, "MERGE_MSG")?.unwrap_or_default();
    let final_msg = message::cleanup(&raw, Cleanup::Whitespace, false, comment);
    let idents = conflict::identities(ctx)?;
    let (id, _) = conflict::write_commit(
        ws,
        NewCommit {
            parents: vec![head, target.id],
            author: idents.author,
            committer: idents.committer.clone(),
            message: final_msg,
            root: outcome.root,
        },
    )
    .await?;
    let op = conflict::move_head(
        ctx,
        ws,
        Some(head),
        id,
        &format!("merge {}: Merge made by the 'ort' strategy.", target.name),
        StagingAfterMove::Set(Vec::new()),
        Some(&idents.committer),
    )
    .await?;
    conflict::announce(ws, id, op).await?;
    seqstate::clear_merge(gitdir);
    crate::ext::index_sink().on_commit(ws.workspace.ws.id, id).await.map_err(fatal)?;
    if !a.quiet {
        println!("Merge made by the 'ort' strategy.");
        if !a.no_stat {
            print_stat(ws, head_root, outcome.root).await?;
        }
    }
    post_merge(ctx, ws, false).await?;
    Ok(ExitCode::SUCCESS)
}

/// Prints how a stopped merge ended and returns its exit code.
fn finish_stopped(conflicted: bool) -> ExitCode {
    if conflicted {
        println!("Automatic merge failed; fix conflicts and then commit the result.");
        ExitCode::from(1)
    } else {
        eprintln!("Automatic merge went well; stopped before committing as requested");
        ExitCode::SUCCESS
    }
}
