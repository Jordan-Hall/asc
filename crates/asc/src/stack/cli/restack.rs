//! `asc restack [<change>] [--downstack|--upstack|--only]` (SPEC §20.7, §20.1 D7): rebases each
//! change of a stack whose parent is obsolete or landed onto the parent's live successor.
//!
//! The rewrite runs in memory (`Engine::execute_restack`): a change whose paths are disjoint
//! from what its new base changed is rebuilt from metadata alone, and the executor moves no
//! ref. This file applies the result: the rewrites are registered (generation, git ids, the
//! change index with `RewriteOp::Restack`), and every branch that pointed at a rewritten change
//! moves in ONE `Ctx::update_local_refs` transaction, so a stop or a failure leaves every ref
//! where it was.
//!
//! A conflict stops the run. The conflicted change is materialised in git's own `rebase-merge/`
//! by a child `asc rebase --onto`, the rest of the plan is saved in `<gitdir>/asc-stack-op.json`
//! ([`crate::stack::opstate`]), and the exit code is 1. `asc continue` and `asc abort` finish or
//! undo it. A signing failure stops with exit 1 and saves nothing. With `commit.gpgSign=true`
//! every rewritten commit is signed, the replayed one included.

use std::collections::{HashMap, HashSet, VecDeque};
use std::io::Write;
use std::process::{ExitCode, Stdio};

use async_trait::async_trait;
use asc_core::change::RewriteOp;
use asc_core::git::{EXTRA_GIT_GPGSIG, export_commit, tree_oid};
use asc_core::id::{ChangesetId, GitOid, RefTarget};
use asc_core::path::RefName;
use asc_engine::change::{self, ChangeRows, Recorder, RewriteSink as _};
use asc_engine::commit::{NewChangeset, PathPolicy};
use asc_engine::derive::Gen;
use asc_engine::merge::MergeOptions;
use asc_engine::stack::restack::{CommitSigner, Outcome, Pause, SignError, TxnMeta};
use asc_git::projection::Head;
use asc_state::state::tables::cs_gen;
use asc_state::state::workspaces;
use asc_store::refs::{Expect, RefTxn, RefUpdate, Refs as _};
use asc_wc::status;
use serde::Serialize;

use super::info::resolve_change;
use super::sl::{LStacks, Local, clean, fatal, stack_err};
use crate::cli::common::{CliError, Ctx, Result, StagingAfterMove, WsCtx};
use crate::cli::conflict;
use crate::cli::output::Output;
use crate::cli::pager::PagerWant;
use crate::cli::restore::{WcParts, actor, index_tree, root_of, sync_worktree};
use crate::cli::switch::{self, Dest, Move};
use crate::ext::{self, OpId};
use crate::hooks_runner::{Hook, HookEnv, RewriteKind, Runner};
use crate::native::NativeSpec;
use crate::stack::json::{VERSION, line};
use crate::stack::opstate::{self, RefFollow, StackOp};

/// Most refs read when looking for the branches that follow a rewrite.
const MAX_REFS: usize = 100_000;
/// Longest signature the signing program may return.
const MAX_SIGNATURE: usize = 64 * 1024;

#[derive(clap::Parser, Debug)]
#[command(name = "restack", disable_help_flag = true, disable_version_flag = true)]
pub struct Args {
    /// The change to restack from; default the current HEAD.
    pub change: Option<String>,
    /// The stack from its bottom up to the change.
    #[arg(long, conflicts_with_all = ["upstack", "only"])]
    pub downstack: bool,
    /// The change and everything above it.
    #[arg(long, conflicts_with_all = ["downstack", "only"])]
    pub upstack: bool,
    /// Only the change.
    #[arg(long, conflicts_with_all = ["downstack", "upstack"])]
    pub only: bool,
    #[arg(long)]
    pub json: bool,
}

pub const NATIVE: &[NativeSpec] = &[NativeSpec {
    cmd: "restack",
    flags: &["--downstack", "--upstack", "--only", "--json"],
    placeholders: &[],
}];

super::verb!(RESTACK, "restack");
/// `asc stack restack` is the same command as `asc restack` (SPEC 20.7 lists the stack verbs under both spellings).
super::info::sub!(STACK_RESTACK, "restack", Args, run);

// ---------------------------------------------------------------- report

#[derive(Serialize)]
pub(crate) struct Row {
    pub(crate) change: String,
    pub(crate) subject: String,
    pub(crate) old: String,
    pub(crate) new: String,
}

#[derive(Serialize)]
struct Report<'a> {
    version: u32,
    /// `complete`, `nothing` or `paused`.
    status: &'a str,
    rewrites: &'a [Row],
    paused_at: Option<&'a Row>,
}

/// What a drive ended with.
pub(crate) enum Flow {
    Done(Vec<Row>),
    Paused(Row),
}

fn render(rows: &[Row]) -> String {
    let mut out = String::new();
    for r in rows {
        let short: String = r.change.chars().take(12).collect();
        out.push_str(&format!("Restacked {short} {}\n", clean(&r.subject)));
    }
    out
}

pub(crate) fn emit(ctx: &Ctx, json: bool, flow: &Flow) -> Result<ExitCode> {
    let mut out = Output::open(ctx, "stack", PagerWant::Never)?;
    let (status, rows, at): (&str, &[Row], Option<&Row>) = match flow {
        Flow::Done(r) if r.is_empty() => ("nothing", &[], None),
        Flow::Done(r) => ("complete", r, None),
        Flow::Paused(r) => ("paused", &[], Some(r)),
    };
    if json {
        let text = line(&Report { version: VERSION, status, rewrites: rows, paused_at: at }).map_err(fatal)?;
        out.write_all(text.as_bytes())?;
    } else {
        out.write_all(render(rows).as_bytes())?;
    }
    out.finish()?;
    Ok(if at.is_some() { ExitCode::from(1) } else { ExitCode::SUCCESS })
}

// ---------------------------------------------------------------- children

/// Runs `asc <args>` with the terminal inherited, and returns its exit code.
pub(crate) fn run_asc(ctx: &Ctx, args: &[&str]) -> Result<i32> {
    let exe = std::env::current_exe().map_err(|e| fatal(format!("cannot find asc: {e}")))?;
    let status = ctx
        .child(exe)?
        .args(["-c", "advice.mergeConflict=false"])
        .args(args)
        .stdin(Stdio::null())
        .status()
        .map_err(|e| fatal(format!("cannot run asc: {e}")))?;
    Ok(status.code().unwrap_or(1))
}

/// Whether git's `rebase-merge/` state exists.
pub(crate) fn rebase_paused(ws: &WsCtx) -> bool {
    ws.projection.git_dir().join("rebase-merge").exists()
}

// ---------------------------------------------------------------- signing

/// Signs with git's signing program (`gpg.program`, default `gpg`), as `commit -S` does.
struct Gpg<'a> {
    ctx: &'a Ctx,
    key: String,
}

#[async_trait]
impl CommitSigner for Gpg<'_> {
    async fn sign(&self, payload: &[u8]) -> std::result::Result<Vec<u8>, SignError> {
        let program = self.ctx.config.get("gpg.program").unwrap_or_else(|| "gpg".to_owned());
        let mut cmd = self.ctx.child(self.ctx.shell()).map_err(|e| SignError(e.to_string()))?;
        cmd.args(["-c", "exec \"$@\"", "sh"])
            .arg(&program)
            .args(["--status-fd=2", "-bsau"])
            .arg(&self.key)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped());
        let payload = payload.to_vec();
        let out = tokio::task::spawn_blocking(move || -> std::io::Result<std::process::Output> {
            let mut child = cmd.spawn()?;
            if let Some(mut pipe) = child.stdin.take() {
                let _ = pipe.write_all(&payload);
            }
            child.wait_with_output()
        })
        .await
        .map_err(|e| SignError(e.to_string()))?
        .map_err(|e| SignError(e.to_string()))?;
        if !out.status.success() || out.stdout.is_empty() || out.stdout.len() > MAX_SIGNATURE {
            return Err(SignError("gpg failed to sign the data".into()));
        }
        Ok(out.stdout)
    }
}

fn signer(ctx: &Ctx, on: bool) -> Result<Option<Gpg<'_>>> {
    if !on {
        return Ok(None);
    }
    let key = match ctx.config.get("user.signingkey") {
        Some(k) => k,
        None => crate::cli::commit::ident::display(&conflict::identities(ctx)?.committer),
    };
    Ok(Some(Gpg { ctx, key }))
}

/// Signs the replayed commit `r` when it carries no signature yet. Returns its replacement.
pub(crate) async fn resign(ctx: &Ctx, ws: &WsCtx, r: ChangesetId) -> Result<ChangesetId> {
    let cs = ws.engine.get_changeset(r).await?;
    if cs.extra.get(&EXTRA_GIT_GPGSIG.to_vec()).is_some() {
        return Ok(r);
    }
    let Some(gpg) = signer(ctx, true)? else { return Ok(r) };
    let mut parent_gits: Vec<GitOid> = Vec::new();
    for &p in &cs.parents {
        parent_gits.push(conflict::git_id(ws, p).await?);
    }
    let root_git = tree_oid(ws.engine.get_tree(cs.root).await?.as_ref());
    let payload = export_commit(&cs, root_git, &parent_gits).map_err(fatal)?;
    let mut sig = gpg
        .sign(&payload)
        .await
        .map_err(|e| CliError::Raw(format!("error: {e}\nfatal: failed to write commit object"), 128))?;
    while matches!(sig.last(), Some(b'\n' | b'\r')) {
        sig.pop();
    }
    let mut extra: Vec<(Vec<u8>, Vec<u8>)> = cs.extra.as_slice().to_vec();
    extra.retain(|(k, _)| k.as_slice() != EXTRA_GIT_GPGSIG);
    extra.push((EXTRA_GIT_GPGSIG.to_vec(), sig));
    let policy = PathPolicy::Native { windows_safe: ws.repo.repo.windows_safe };
    let id = ws
        .engine
        .create_changeset(
            NewChangeset {
                parents: cs.parents.clone(),
                author: cs.author.clone(),
                committer: cs.committer.clone(),
                message: cs.message.clone(),
                extra,
                root: cs.root,
                copies: Vec::new(),
            },
            policy,
        )
        .await?;
    register(ws, id).await?;
    Ok(id)
}

// ---------------------------------------------------------------- registering

/// Makes a stored rewrite known to the rest of asc: generation, git ids of its trees.
pub(crate) async fn register(ws: &WsCtx, new: ChangesetId) -> Result<()> {
    let cs = ws.engine.get_changeset(new).await?;
    let gen_no = ws.engine.get_derived::<Gen>(new).await.map_err(fatal)?;
    ws.engine.warm(new).await.map_err(fatal)?;
    cs_gen::insert_many(
        &ws.db,
        ws.repo.key,
        &[cs_gen::CsGen { cs: new, gen_no: u64::from(gen_no.generation), parents: cs.parents.clone() }],
    )
    .await?;
    let first = root_of(ws, cs.parents.first().copied()).await?;
    conflict::index_result(ws, first, cs.root).await
}

// ---------------------------------------------------------------- planning

/// The live successor of the obsolete `parent`: a visible, non-obsolete changeset of the same
/// change in its successor closure.
async fn successor(l: &Local<'_>, st: &LStacks<'_, '_>, parent: ChangesetId) -> Result<Option<ChangesetId>> {
    let Some(row) = l.rows.get(parent).await.map_err(fatal)? else { return Ok(None) };
    let mut seen = HashSet::from([parent]);
    let mut queue = VecDeque::from([parent]);
    let mut live: Vec<ChangesetId> = Vec::new();
    while let Some(x) = queue.pop_front() {
        for s in l.rows.successors(x, change::MAX_ROWS).await.map_err(fatal)? {
            if !seen.insert(s) {
                continue;
            }
            if seen.len() > asc_engine::stack::MAX_SET {
                return Err(fatal("successor closure is too large"));
            }
            if let Some(sr) = l.rows.get(s).await.map_err(fatal)?
                && sr.change == row.change
                && st.is_visible(s).await.map_err(stack_err)?
                && !st.is_obsolete(s).await.map_err(stack_err)?
            {
                live.push(s);
            }
            queue.push_back(s);
        }
    }
    match live.as_slice() {
        [] => Ok(None),
        [one] => Ok(Some(*one)),
        _ => Err(CliError::Raw("asc: restack: the parent change is divergent; resolve that first".into(), 1)),
    }
}

/// The stack to restack and the position of the target in it.
async fn pick_stack(
    ctx: &Ctx,
    l: &Local<'_>,
    st: &LStacks<'_, '_>,
    word: Option<&str>,
) -> Result<Option<(asc_engine::stack::Stack, usize)>> {
    let Some(current) = st.current_head() else {
        return Err(CliError::Raw("asc: HEAD is not on a change".into(), 1));
    };
    if word.is_none() {
        let stack = st.stack_of(current).await.map_err(stack_err)?;
        let last = stack.changes.len();
        return Ok((last > 0).then(|| (stack, last - 1)));
    }
    let (cs, _) = resolve_change(ctx, l, st, word).await?;
    let mut heads = vec![current];
    heads.extend(st.heads().iter().copied());
    let mut tried = HashSet::new();
    for h in heads {
        if !tried.insert(h) {
            continue;
        }
        let stack = st.stack_of(h).await.map_err(stack_err)?;
        if let Some(i) = stack.changes.iter().position(|&c| c == cs) {
            return Ok(Some((stack, i)));
        }
    }
    Err(CliError::Raw("asc: restack: the change is not on the stack of any branch".into(), 1))
}

/// The branches (and a detached HEAD) that follow a rewrite of one of `steps`.
pub(crate) async fn followers(ws: &WsCtx, steps: &[ChangesetId]) -> Result<Vec<RefFollow>> {
    let set: HashSet<ChangesetId> = steps.iter().copied().collect();
    let entries = ws.db.refs().list(ws.repo.key, "refs/heads/").await?;
    if entries.len() > MAX_REFS {
        return Err(fatal(format!("more than {MAX_REFS} branches")));
    }
    let mut out = Vec::new();
    for e in entries {
        if let RefTarget::Changeset(cs) = e.target
            && set.contains(&cs)
        {
            out.push(RefFollow { name: e.name, from: cs });
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------- the run

pub async fn run(ctx: &Ctx, a: Args) -> Result<ExitCode> {
    let l = Local::open(ctx).await?;
    let ws = l.ws;
    refuse_in_progress(ws)?;
    let st = l.stacks().await?;
    let Some((stack, idx)) = pick_stack(ctx, &l, &st, a.change.as_deref()).await? else {
        return emit(ctx, a.json, &Flow::Done(Vec::new()));
    };
    let n = stack.changes.len();
    let (lo, hi) = if a.only {
        (idx, idx)
    } else if a.downstack {
        (0, idx)
    } else if a.upstack {
        (idx, n - 1)
    } else {
        (0, n - 1)
    };
    let mut first = None;
    for k in lo..=hi {
        if st.needs_restack(stack.changes[k]).await.map_err(stack_err)? {
            first = Some(k);
            break;
        }
    }
    let Some(k) = first else { return emit(ctx, a.json, &Flow::Done(Vec::new())) };
    let steps: Vec<ChangesetId> = stack.changes[k..=hi].to_vec();
    let bottom = ws.engine.get_changeset(steps[0]).await?;
    let parent = bottom.parents[0];
    let onto = match successor(&l, &st, parent).await? {
        Some(s) => s,
        // A landed parent: the change moves onto the trunk it landed in.
        None => stack
            .trunk
            .ok_or_else(|| CliError::Raw("asc: restack: the stack has no trunk to restack onto".into(), 1))?,
    };
    if onto == parent {
        return emit(ctx, a.json, &Flow::Done(Vec::new()));
    }
    if st.graph().is_ancestor(steps[0], onto).await.map_err(fatal)? {
        return Err(CliError::Raw("asc: restack: the new base is a descendant of the change".into(), 1));
    }
    let head = ws
        .head_commit()
        .await?
        .ok_or_else(|| CliError::Raw("asc: HEAD is not on a change".into(), 1))?;
    let head_branch = match ws.projection.read_head()? {
        Head::Branch(b) => Some(b),
        Head::Detached(_) => None,
    };
    let sign = ctx.config.bool("commit.gpgsign") == Some(true);
    let op = StackOp {
        op_id: uuid::Uuid::now_v7(),
        head_branch,
        head,
        follow: followers(ws, &steps).await?,
        done: Vec::new(),
        at: steps[0],
        at_onto: onto,
        rest: Vec::new(),
        sign,
    };
    drop(st);
    let flow = drive(ctx, ws, op, Vec::new(), onto, steps).await?;
    emit(ctx, a.json, &flow)
}

/// Refuses to start while another operation is in progress.
fn refuse_in_progress(ws: &WsCtx) -> Result<()> {
    let git = ws.projection.git_dir();
    if opstate::exists(git) {
        return Err(CliError::Fatal(
            "asc: a restack is already in progress (fix the conflicts, then run 'asc continue', or run 'asc abort')".into(),
        ));
    }
    for f in ["rebase-merge", "rebase-apply", "MERGE_HEAD", "CHERRY_PICK_HEAD", "REVERT_HEAD", "sequencer"] {
        if git.join(f).exists() {
            return Err(CliError::Fatal(format!(
                "asc: cannot restack: an operation is in progress ({f}); finish it with 'asc continue' or 'asc abort'"
            )));
        }
    }
    Ok(())
}

/// A frozen change is never rewritten locally (SPEC §20.3): nothing has moved yet when this
/// refuses, because the refs move in `apply` only.
pub(crate) async fn refuse_frozen(ws: &WsCtx, rewriting: impl Iterator<Item = ChangesetId>) -> Result<()> {
    let rows = asc_state::changes::StateChanges::new(ws.db.clone(), ws.repo.key);
    let frozen = rows.frozen_set().await.map_err(fatal)?;
    if frozen.is_empty() {
        return Ok(());
    }
    for cs in rewriting {
        if let Some(row) = rows.get(cs).await.map_err(fatal)?
            && frozen.contains(&row.change)
        {
            return Err(CliError::Raw(
                format!("asc: change {} is frozen; run 'asc stack unfreeze' to rewrite it", row.change.short()),
                1,
            ));
        }
    }
    Ok(())
}

/// Rewrites `steps` onto `onto` until the stack is done or a conflict stops it. `done` holds
/// the rewrites already made; `op` is the operation record saved when a conflict stops it.
pub(crate) async fn drive(
    ctx: &Ctx,
    ws: &WsCtx,
    mut op: StackOp,
    mut done: Vec<(ChangesetId, ChangesetId)>,
    mut onto: ChangesetId,
    mut steps: Vec<ChangesetId>,
) -> Result<Flow> {
    refuse_frozen(ws, done.iter().map(|d| d.0).chain(steps.iter().copied())).await?;
    loop {
        if steps.is_empty() {
            return apply(ctx, ws, &op, &done).await.map(Flow::Done);
        }
        let plan = ws.engine.plan_restack(&steps, onto).await.map_err(fatal)?;
        let gpg = signer(ctx, op.sign)?;
        let meta = TxnMeta { actor: actor(), reason: "restack".into(), op_id: op.op_id };
        let outcome = ws
            .engine
            .execute_restack(
                &plan,
                &[],
                gpg.as_ref().map(|g| g as &dyn CommitSigner),
                &MergeOptions::default(),
                &meta,
            )
            .await
            .map_err(fatal)?;
        match outcome {
            Outcome::Complete { rewrites, .. } => {
                for r in rewrites.iter().filter(|r| r.old != r.new) {
                    register(ws, r.new).await?;
                }
                done.extend(rewrites.iter().map(|r| (r.old, r.new)));
                return apply(ctx, ws, &op, &done).await.map(Flow::Done);
            }
            Outcome::Paused { cause: Pause::Sign(msg), .. } => {
                return Err(CliError::Raw(format!("error: restack: signing failed: {}\nNo ref was moved.", clean(&msg)), 1));
            }
            Outcome::Paused { rewrites, at, .. } => {
                for r in rewrites.iter().filter(|r| r.old != r.new) {
                    register(ws, r.new).await?;
                }
                done.extend(rewrites.iter().map(|r| (r.old, r.new)));
                let new_onto = rewrites.last().map_or(onto, |r| r.new);
                let pos = steps.iter().position(|&s| s == at).ok_or_else(|| fatal("the conflicted change is not in the plan"))?;
                let rest = steps[pos + 1..].to_vec();
                op.done = done.clone();
                match replay(ctx, ws, &mut op, at, new_onto, &rest).await? {
                    Some(r) => {
                        done.push((at, r));
                        onto = r;
                        steps = rest;
                    }
                    None => return Ok(Flow::Paused(row_of(ws, at, at).await?)),
                }
            }
        }
    }
}

/// Whether the work tree or the stage differs from HEAD.
pub(crate) async fn dirty(ctx: &Ctx, ws: &WsCtx, head_root: asc_core::id::TreeId) -> Result<bool> {
    let fresh = workspaces::get(&ws.db, ws.workspace.ws.id)
        .await?
        .ok_or_else(|| fatal("this workspace is not registered in state.db"))?;
    let parts = WcParts::new(ctx, ws, ws.projection.git_dir().join("index"))?;
    let st = status::compute(&parts.wc(&ws.engine), &ws.db, &fresh).await.map_err(fatal)?;
    let (index, unmerged) = index_tree(ws, head_root).await?;
    Ok(!st.unstaged.is_empty() || !unmerged.is_empty() || !st.unmerged.is_empty() || index != head_root)
}

pub(crate) fn refuse_dirty() -> CliError {
    CliError::Raw(
        "error: cannot restack: You have uncommitted changes.\nerror: Please commit or stash them.".into(),
        1,
    )
}

/// Replays `at` onto `onto` through `asc rebase`, which leaves a conflict in git's own state.
/// `Some(commit)` when the replay was clean; `None` when it stopped (the operation is saved).
async fn replay(
    ctx: &Ctx,
    ws: &WsCtx,
    op: &mut StackOp,
    at: ChangesetId,
    onto: ChangesetId,
    rest: &[ChangesetId],
) -> Result<Option<ChangesetId>> {
    let head_root = root_of(ws, ws.head_commit().await?).await?;
    if dirty(ctx, ws, head_root).await? {
        return Err(refuse_dirty());
    }
    let cs = ws.engine.get_changeset(at).await?;
    let parent = cs.parents[0];
    op.at = at;
    op.at_onto = onto;
    op.rest = rest.to_vec();
    let git = ws.projection.git_dir().to_owned();
    opstate::save(&git, op)?;
    // Changeset ids: the engine resolver answers them without the git map.
    let (onto_arg, parent_arg, at_arg) = (onto.to_string(), parent.to_string(), at.to_string());
    let code = run_asc(ctx, &["rebase", "-q", "--no-verify", "--onto", &onto_arg, &parent_arg, &at_arg])?;
    if code == 0 {
        return settle(ctx, ws, op).await.map(Some);
    }
    if rebase_paused(ws) {
        let row = row_of(ws, at, at).await?;
        eprintln!(
            "asc: restack stopped: could not apply {} {}\nResolve the conflicts and mark them with 'asc add', then run 'asc continue'.\nTo go back to where you were, run 'asc abort'.",
            row.change.chars().take(12).collect::<String>(),
            clean(&row.subject)
        );
        return Ok(None);
    }
    opstate::clear(&git);
    Err(CliError::Fatal(format!("asc: restack: replaying the change failed (exit {code}); nothing was moved")))
}

/// After the conflicted change was replayed: checks HEAD is its replay and signs it.
pub(crate) async fn settle(ctx: &Ctx, ws: &WsCtx, op: &StackOp) -> Result<ChangesetId> {
    let head = ws
        .head_commit()
        .await?
        .ok_or_else(|| fatal("HEAD is unborn"))?;
    if head == op.at_onto {
        // The change became empty on its new base and was dropped.
        return Ok(head);
    }
    let cs = ws.engine.get_changeset(head).await?;
    if cs.parents.as_slice() != [op.at_onto] {
        return Err(fatal("HEAD is not the replay of the paused change; run 'asc abort'"));
    }
    if op.sign { resign(ctx, ws, head).await } else { Ok(head) }
}

async fn row_of(ws: &WsCtx, old: ChangesetId, new: ChangesetId) -> Result<Row> {
    let cs = ws.engine.get_changeset(new).await?;
    let first = cs.message.split(|&b| b == b'\n').next().unwrap_or_default();
    let change = change_of(ws, old).await?.unwrap_or_default();
    Ok(Row {
        change,
        subject: String::from_utf8_lossy(first).chars().take(200).collect(),
        old: conflict::git_id(ws, old).await?.to_string(),
        new: conflict::git_id(ws, new).await?.to_string(),
    })
}

async fn change_of(ws: &WsCtx, cs: ChangesetId) -> Result<Option<String>> {
    let rows = asc_state::changes::StateChanges::new(ws.db.clone(), ws.repo.key);
    Ok(rows.get(cs).await.map_err(fatal)?.map(|r| r.change.to_string()))
}

// ---------------------------------------------------------------- applying

fn head_name() -> Result<RefName> {
    RefName::new_onelevel(b"HEAD".to_vec()).map_err(fatal)
}

/// Moves every following ref to the rewrite of the change it pointed at, in one transaction,
/// then records the rewrites. HEAD ends where it began: on its branch, or detached at the
/// rewrite of its commit.
pub(crate) async fn apply(
    ctx: &Ctx,
    ws: &WsCtx,
    op: &StackOp,
    pairs: &[(ChangesetId, ChangesetId)],
) -> Result<Vec<Row>> {
    let git = ws.projection.git_dir().to_owned();
    let map: HashMap<ChangesetId, ChangesetId> = pairs.iter().filter(|(o, n)| o != n).copied().collect();
    if map.is_empty() {
        opstate::clear(&git);
        return Ok(Vec::new());
    }
    let cur = ws.head_commit().await?;
    let at_origin = match ws.projection.read_head()? {
        Head::Branch(b) => op.head_branch.as_deref() == Some(b.as_str()) && cur == Some(op.head),
        Head::Detached(_) => op.head_branch.is_none() && cur == Some(op.head),
    };
    let new_head = map.get(&op.head).copied().unwrap_or(op.head);

    // Git ids first: a ref move that lands HEAD on a rewrite needs the commit's git id.
    let mut out = Vec::new();
    let mut hook_pairs: Vec<(GitOid, GitOid)> = Vec::new();
    for &(old, new) in pairs.iter().filter(|(o, n)| o != n) {
        let row = row_of(ws, old, new).await?;
        if let (Ok(o), Ok(n)) = (row.old.parse(), row.new.parse()) {
            hook_pairs.push((o, n));
        }
        out.push(row);
    }

    let mut updates: Vec<RefUpdate> = Vec::new();
    let mut seen = HashSet::new();
    for f in &op.follow {
        if let Some(&new) = map.get(&f.from)
            && seen.insert(f.name.clone())
        {
            updates.push(RefUpdate {
                name: f.name.clone(),
                expect: Expect::Value(RefTarget::Changeset(f.from)),
                new: Some(RefTarget::Changeset(new)),
                expires_at: None,
            });
        }
    }
    if at_origin && op.head_branch.is_none() && new_head != op.head {
        updates.push(RefUpdate {
            name: head_name()?,
            expect: Expect::Value(RefTarget::Changeset(op.head)),
            new: Some(RefTarget::Changeset(new_head)),
            expires_at: None,
        });
    }

    // The work tree follows HEAD before any ref moves, and only when the stage is clean.
    let mut synced: Option<(asc_core::id::TreeId, Vec<asc_core::path::RepoPath>)> = None;
    let mut staging = StagingAfterMove::Keep;
    if at_origin && new_head != op.head {
        let old_root = root_of(ws, Some(op.head)).await?;
        let new_root = root_of(ws, Some(new_head)).await?;
        if old_root != new_root {
            if dirty(ctx, ws, old_root).await? {
                return Err(refuse_dirty());
            }
            let paths: Vec<_> = ws.engine.leafdiff(old_root, new_root, None).await?.into_iter().map(|(p, _)| p).collect();
            sync_worktree(ctx, new_root, &paths, true).await?;
            synced = Some((old_root, paths));
            staging = StagingAfterMove::Set(Vec::new());
        }
    }
    if !updates.is_empty() {
        let txn = RefTxn { updates, actor: actor(), reason: String::new(), op_id: op.op_id };
        let reason = format!("restack: {} change{}", map.len(), if map.len() == 1 { "" } else { "s" });
        if let Err(e) = ctx.update_local_refs(txn, &reason, staging).await {
            if let Some((old_root, paths)) = synced {
                let _ = sync_worktree(ctx, old_root, &paths, true).await;
            }
            return Err(e);
        }
    }
    opstate::clear(&git);

    let opid = OpId(op.op_id);
    let rows_db = asc_state::changes::StateChanges::new(ws.db.clone(), ws.repo.key);
    let recorder = Recorder::new(rows_db);
    for &(old, new) in pairs.iter().filter(|(o, n)| o != n) {
        recorder.record(new, &[old], RewriteOp::Restack, asc_engine::change::OpId(op.op_id)).await.map_err(fatal)?;
        ext::rewrite_sink().record(new, &[old], RewriteOp::Restack, opid);
        ext::index_sink().on_commit(ws.workspace.ws.id, new).await.map_err(fatal)?;
    }

    if !at_origin {
        // The replay left HEAD detached on the replayed change; put it back.
        let dest = match &op.head_branch {
            Some(b) => match switch::branch_tip(ws, b).await? {
                Some(tip) => Dest::Existing { branch: b.clone(), tip: Some(tip) },
                None => Dest::Detach { commit: new_head },
            },
            None => Dest::Detach { commit: new_head },
        };
        reattach(ctx, dest).await?;
    }

    let runner = Runner::for_ctx(ctx)?;
    let env = HookEnv::of(ctx, "restack");
    let _ = runner.run(Hook::PostRewrite { kind: RewriteKind::Rebase, pairs: hook_pairs }, &env).await?;
    Ok(out)
}

/// Moves HEAD to `dest` without printing anything.
pub(crate) async fn reattach(ctx: &Ctx, dest: Dest) -> Result<()> {
    let opts = switch::Opts { quiet: true, ..switch::Opts::default() };
    let typed = match &dest {
        Dest::Existing { branch, .. } => branch.clone(),
        _ => "HEAD".to_owned(),
    };
    switch::perform(ctx, Move { dest, opts, typed, keep_tree: false })
        .await
        .map(|_| ())
        .map_err(|e| CliError::Fatal(format!("asc: HEAD could not be restored ({e}); run 'asc switch' to the branch you were on")))
}
