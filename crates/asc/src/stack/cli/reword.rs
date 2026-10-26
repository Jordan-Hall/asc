//! `asc reword [<change>] [-m <msg>|-e]` (SPEC §20.7, §20.1): edits the message of any draft
//! change without touching its tree, then restacks what sits above it.
//!
//! The message goes through `commit-msg` (not `pre-commit`: the tree is not committed again),
//! unless `--no-verify`. The rewrite is stored as an amend of the change (same change id,
//! the old changeset as predecessor). Every branch that pointed at the old changeset moves to
//! the new one in ONE `Ctx::update_local_refs` transaction; a rejecting hook or a failed
//! signature moves no ref. `post-rewrite amend` runs once with the pair, and the restack of the
//! descendants runs its own `post-rewrite rebase` with all of its pairs.
//!
//! The helpers [`rewrite`], [`move_refs`] and [`reparent_above`] are shared with `asc modify`.

use std::io::Write;
use std::path::Path;
use std::process::{ExitCode, Stdio};

use async_trait::async_trait;
use asc_core::change::RewriteOp;
use asc_core::git::{EXTRA_GIT_GPGSIG, export_commit, tree_oid};
use asc_core::id::{ChangesetId, GitOid, RefTarget, TreeId};
use asc_engine::change::{self, Recorder, RewriteSink as _};
use asc_engine::commit::{NewChangeset, PathPolicy};
use asc_engine::derive::Gen;
use asc_engine::stack::restack::{CommitSigner, SignError};
use asc_git::projection::Head;
use asc_state::state::tables::cs_gen;
use asc_store::refs::{Expect, RefTxn, RefUpdate, Refs as _};
use uuid::Uuid;

use super::info::resolve_change;
use super::restack::{self, Flow};
use super::sl::{Local, clean, fatal, stack_err};
use crate::cli::common::{CliError, Ctx, Result, StagingAfterMove, WsCtx};
use crate::cli::conflict;
use crate::cli::restore::{actor, root_of};
use crate::hooks_runner::{Gate, Hook, HookEnv, RewriteKind, Runner};
use crate::native::NativeSpec;
use crate::stack::opstate::{self, RefFollow, StackOp};

/// Most branches read when looking for the refs that follow a rewrite.
const MAX_REFS: usize = 100_000;
/// Most rounds of [`reparent_above`] (one per chain).
const MAX_ROUNDS: usize = 64;
/// Longest signature the signing program may return.
const MAX_SIGNATURE: usize = 64 * 1024;
/// Longest message accepted (a bound on untrusted input).
const MAX_MESSAGE: usize = 1024 * 1024;

#[derive(clap::Parser, Debug)]
#[command(name = "reword", disable_help_flag = true, disable_version_flag = true)]
pub struct Args {
    /// The change to reword; default the current HEAD.
    pub change: Option<String>,
    #[arg(short = 'm', long = "message", conflicts_with = "edit")]
    pub message: Vec<String>,
    /// Edit the message in the editor (the default without `-m`).
    #[arg(short = 'e', long)]
    pub edit: bool,
    #[arg(short = 'n', long = "no-verify")]
    pub no_verify: bool,
}

pub const NATIVE: &[NativeSpec] = &[NativeSpec {
    cmd: "reword",
    flags: &["-m", "--message", "-e", "--edit", "-n", "--no-verify", "--json"],
    placeholders: &[],
}];

super::verb!(REWORD, "reword", json: super::json_op::Done::IfLogged);

// ---------------------------------------------------------------- shared

/// Refuses to start while a restack or a git operation is in progress.
pub(crate) fn refuse_busy(ws: &WsCtx, verb: &str) -> Result<()> {
    let git = ws.projection.git_dir();
    if opstate::exists(git) {
        return Err(CliError::Fatal(format!(
            "asc: {verb}: a restack is in progress (fix the conflicts, then run 'asc continue', or run 'asc abort')"
        )));
    }
    for f in ["rebase-merge", "rebase-apply", "MERGE_HEAD", "CHERRY_PICK_HEAD", "REVERT_HEAD", "sequencer"] {
        if git.join(f).exists() {
            return Err(CliError::Fatal(format!(
                "asc: cannot {verb}: an operation is in progress ({f}); finish it with 'asc continue' or 'asc abort'"
            )));
        }
    }
    Ok(())
}

/// [`resolve_change`] with git's exit code (128) when the change is not a draft: there is
/// nothing to rewrite, as `git commit --amend` has nothing to amend on an unborn HEAD.
pub(crate) async fn draft(
    ctx: &Ctx,
    l: &Local<'_>,
    st: &super::sl::LStacks<'_, '_>,
    word: Option<&str>,
    verb: &str,
) -> Result<ChangesetId> {
    match resolve_change(ctx, l, st, word).await {
        Ok((cs, _)) => Ok(cs),
        Err(CliError::Raw(msg, 1)) => Err(CliError::Fatal(format!("{msg} ({verb} rewrites draft changes only)"))),
        Err(e) => Err(e),
    }
}

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

/// Makes a stored changeset known to the rest of asc: generation and git ids of its trees.
async fn register(ws: &WsCtx, new: ChangesetId) -> Result<()> {
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

/// Stores the rewrite of `old` with `message` and `root` (the old values when `None`), signs
/// it when `commit.gpgSign=true`, and records it as an amend of `old` (and of every `also`
/// predecessor, which become obsolete with it). Moves no ref.
pub(crate) async fn rewrite(
    ctx: &Ctx,
    ws: &WsCtx,
    old: ChangesetId,
    message: Option<Vec<u8>>,
    root: Option<TreeId>,
    also: &[ChangesetId],
    op_id: Uuid,
) -> Result<ChangesetId> {
    let cs = ws.engine.get_changeset(old).await?;
    let mut extra: Vec<(Vec<u8>, Vec<u8>)> = cs.extra.as_slice().to_vec();
    extra.retain(|(k, _)| k.as_slice() != EXTRA_GIT_GPGSIG);
    let idents = conflict::identities(ctx)?;
    let root = root.unwrap_or(cs.root);
    let new = NewChangeset {
        parents: cs.parents.clone(),
        author: cs.author.clone(),
        committer: idents.committer.clone(),
        message: message.unwrap_or_else(|| cs.message.to_vec()),
        extra,
        root,
        copies: Vec::new(),
    };
    let policy = PathPolicy::Native { windows_safe: ws.repo.repo.windows_safe };
    let mut id = ws.engine.create_changeset(new.clone(), policy).await?;
    if ctx.config.bool("commit.gpgsign") == Some(true) {
        let key = match ctx.config.get("user.signingkey") {
            Some(k) => k,
            None => crate::cli::commit::ident::display(&idents.committer),
        };
        let gpg = Gpg { ctx, key };
        let unsigned = ws.engine.get_changeset(id).await?;
        let mut parent_gits: Vec<GitOid> = Vec::new();
        for &p in &unsigned.parents {
            parent_gits.push(conflict::git_id(ws, p).await?);
        }
        let root_git = tree_oid(ws.engine.get_tree(root).await?.as_ref());
        let payload = export_commit(&unsigned, root_git, &parent_gits).map_err(fatal)?;
        let mut sig = gpg
            .sign(&payload)
            .await
            .map_err(|e| CliError::Raw(format!("error: {e}\nfatal: failed to write commit object"), 128))?;
        while matches!(sig.last(), Some(b'\n' | b'\r')) {
            sig.pop();
        }
        let mut signed = new;
        signed.extra.push((EXTRA_GIT_GPGSIG.to_vec(), sig));
        id = ws.engine.create_changeset(signed, policy).await?;
    }
    register(ws, id).await?;
    let mut preds = vec![old];
    preds.extend(also.iter().copied());
    let rows = asc_state::changes::StateChanges::new(ws.db.clone(), ws.repo.key);
    Recorder::new(rows).record(id, &preds, RewriteOp::Amend, change::OpId(op_id)).await.map_err(fatal)?;
    Ok(id)
}

/// Moves every branch (and a detached HEAD) that points at `old` to `new`, in one transaction.
pub(crate) async fn move_refs(ctx: &Ctx, ws: &WsCtx, old: ChangesetId, new: ChangesetId, op_id: Uuid, reason: &str) -> Result<()> {
    let entries = ws.db.refs().list(ws.repo.key, "refs/heads/").await?;
    if entries.len() > MAX_REFS {
        return Err(fatal(format!("more than {MAX_REFS} branches")));
    }
    let mut updates: Vec<RefUpdate> = Vec::new();
    for e in entries {
        if e.target == RefTarget::Changeset(old) {
            updates.push(RefUpdate {
                name: e.name,
                expect: Expect::Value(RefTarget::Changeset(old)),
                new: Some(RefTarget::Changeset(new)),
                expires_at: None,
            });
        }
    }
    if matches!(ws.projection.read_head()?, Head::Detached(_)) && ws.head_commit().await? == Some(old) {
        updates.push(RefUpdate {
            name: asc_core::path::RefName::new_onelevel(b"HEAD".to_vec()).map_err(fatal)?,
            expect: Expect::Value(RefTarget::Changeset(old)),
            new: Some(RefTarget::Changeset(new)),
            expires_at: None,
        });
    }
    if updates.is_empty() {
        return Ok(());
    }
    let txn = RefTxn { updates, actor: actor(), reason: String::new(), op_id };
    ctx.update_local_refs(txn, reason, StagingAfterMove::Keep).await
}

/// Moves every draft chain that starts on `old` onto `new` (the rewrite of `old`, or a change
/// inserted after it), through the restack executor. A rewrite with no ref of its own is not
/// visible to the stack model, so the chains are driven directly instead of through the
/// obsolescence rules of `asc restack`. Branches and HEAD follow the rewritten changes in one
/// transaction. Exit 1 when a conflict stopped the run (`asc continue` / `asc abort` finish it).
pub(crate) async fn reparent_above(ctx: &Ctx, old: ChangesetId, new: ChangesetId) -> Result<ExitCode> {
    let ws = ctx.ws()?;
    for _ in 0..MAX_ROUNDS {
        let l = Local::open(ctx).await?;
        let st = l.stacks().await?;
        let mut found: Option<Vec<ChangesetId>> = None;
        for &h in st.heads() {
            let stack = st.stack_of(h).await.map_err(stack_err)?;
            let start = stack.changes.iter().position(|&c| c == old).map_or(0, |i| i + 1);
            let Some(&first) = stack.changes.get(start) else { continue };
            if first == new {
                continue;
            }
            if ws.engine.get_changeset(first).await?.parents.first() == Some(&old) {
                found = Some(stack.changes[start..].to_vec());
                break;
            }
        }
        drop(st);
        drop(l);
        let Some(steps) = found else { return Ok(ExitCode::SUCCESS) };

        let entries = ws.db.refs().list(ws.repo.key, "refs/heads/").await?;
        if entries.len() > MAX_REFS {
            return Err(fatal(format!("more than {MAX_REFS} branches")));
        }
        let follow: Vec<RefFollow> = entries
            .into_iter()
            .filter_map(|e| match e.target {
                RefTarget::Changeset(cs) if steps.contains(&cs) => Some(RefFollow { name: e.name, from: cs }),
                _ => None,
            })
            .collect();
        let head = ws.head_commit().await?.ok_or_else(|| CliError::Raw("asc: HEAD is not on a change".into(), 1))?;
        let head_branch = match ws.projection.read_head()? {
            Head::Branch(b) => Some(b),
            Head::Detached(_) => None,
        };
        let op = StackOp {
            op_id: Uuid::now_v7(),
            head_branch,
            head,
            follow,
            done: Vec::new(),
            at: steps[0],
            at_onto: new,
            rest: Vec::new(),
            sign: ctx.config.bool("commit.gpgsign") == Some(true),
        };
        let flow = restack::drive(ctx, ws, op, Vec::new(), new, steps).await?;
        let code = restack::emit(ctx, false, &flow)?;
        if matches!(flow, Flow::Paused(_)) {
            return Ok(code);
        }
    }
    Err(CliError::Fatal("asc: too many chains above the rewritten change".into()))
}

/// The commit-message text of `-m` values: paragraphs joined by a blank line, as git does.
pub(crate) fn join_messages(parts: &[String]) -> Vec<u8> {
    let mut out = parts.join("\n\n").into_bytes();
    out.push(b'\n');
    out
}

/// Git's default cleanup for a message without a template: trailing blanks off each line,
/// leading and trailing blank lines dropped, runs of blank lines collapsed.
fn cleanup(raw: &[u8]) -> Vec<u8> {
    let text = String::from_utf8_lossy(raw);
    let mut out = String::new();
    let mut blank = false;
    for line in text.lines() {
        let line = line.trim_end();
        if line.is_empty() {
            blank = !out.is_empty();
            continue;
        }
        if blank {
            out.push('\n');
            blank = false;
        }
        out.push_str(line);
        out.push('\n');
    }
    out.into_bytes()
}

/// Runs the editor on `file`: `GIT_EDITOR`, `core.editor`, `VISUAL`, `EDITOR`, then `vi`.
async fn edit(ctx: &Ctx, file: &Path) -> Result<bool> {
    let from_env = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
    let editor = ctx
        .env
        .editor
        .as_ref()
        .map(|e| e.to_string_lossy().into_owned())
        .filter(|e| !e.is_empty())
        .or_else(|| ctx.config.get("core.editor"))
        .or_else(|| from_env("VISUAL"))
        .or_else(|| from_env("EDITOR"))
        .unwrap_or_else(|| "vi".to_owned());
    if editor == ":" {
        return Ok(true);
    }
    let mut cmd = ctx.child(ctx.shell())?;
    cmd.arg("-c").arg(format!("{editor} \"$@\"")).arg(&editor).arg(file);
    let status = tokio::task::spawn_blocking(move || cmd.status()).await.map_err(fatal)??;
    if !status.success() {
        eprintln!("error: There was a problem with the editor '{editor}'.");
    }
    Ok(status.success())
}

// ---------------------------------------------------------------- the verb

pub async fn run(ctx: &Ctx, a: Args) -> Result<ExitCode> {
    let l = Local::open(ctx).await?;
    let ws = l.ws;
    refuse_busy(ws, "reword")?;
    let st = l.stacks().await?;
    let cs = draft(ctx, &l, &st, a.change.as_deref(), "reword").await?;
    drop(st);
    drop(l);

    let old = ws.engine.get_changeset(cs).await?;
    let runner = Runner::for_ctx(ctx)?;
    let file = ws.projection.git_dir().join("COMMIT_EDITMSG");
    let use_editor = a.message.is_empty();
    let initial = if use_editor { old.message.to_vec() } else { join_messages(&a.message) };
    if initial.len() > MAX_MESSAGE {
        return Err(CliError::Fatal("asc: reword: the message is too long".into()));
    }
    std::fs::write(&file, &initial)?;
    let mut env = HookEnv::of(ctx, "reword");
    if !use_editor {
        env = env.without_editor();
    }
    if use_editor && !edit(ctx, &file).await? {
        return Err(CliError::Fatal("asc: reword: the editor failed; nothing was changed".into()));
    }
    if !a.no_verify
        && let Gate::Reject(_) = runner.gate(Hook::CommitMsg { file: file.clone() }, &env).await?
    {
        return Ok(ExitCode::from(1));
    }
    let raw = std::fs::read(&file)?;
    if raw.len() > MAX_MESSAGE {
        return Err(CliError::Fatal("asc: reword: the message is too long".into()));
    }
    let text = cleanup(&raw);
    if text.is_empty() {
        eprintln!("Aborting reword due to empty commit message.");
        return Ok(ExitCode::from(1));
    }
    if text == old.message.as_slice() {
        eprintln!("asc: reword: the message is unchanged");
        return Ok(ExitCode::SUCCESS);
    }

    let op_id = Uuid::now_v7();
    let new = rewrite(ctx, ws, cs, Some(text), None, &[], op_id).await?;
    let subject: String = String::from_utf8_lossy(
        ws.engine.get_changeset(new).await?.message.split(|&b| b == b'\n').next().unwrap_or_default(),
    )
    .chars()
    .take(200)
    .collect();
    move_refs(ctx, ws, cs, new, op_id, &format!("reword: {}", clean(&subject))).await?;

    let pair = (conflict::git_id(ws, cs).await?, conflict::git_id(ws, new).await?);
    let _ = runner.run(Hook::PostRewrite { kind: RewriteKind::Amend, pairs: vec![pair] }, &env).await?;
    reparent_above(ctx, cs, new).await
}
