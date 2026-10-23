//! `asc absorb [-n] [-f] [-p] [--into <revset>]` (SPEC §20.7): moves each staged hunk (or each
//! unstaged tracked hunk when nothing is staged) into the mutable change that introduced the
//! lines it touches, then restacks the changes above.
//!
//! The assignment is the engine's ([`asc_engine::stack::absorb`]): `blame.v1` line origins on
//! HEAD's stack. Ambiguous hunks, hunks owned by an immutable change, new files, deletions,
//! type changes and binary files stay. Without `-f` the command only previews; `-n` says so
//! explicitly and `-p` adds the lines of every hunk.
//!
//! Applying stores the rewrites, then moves every branch that pointed at a rewritten change in
//! ONE `Ctx::update_local_refs` transaction. The work tree is never written: it already holds
//! the final content of every file, so what stayed shows as the same modification afterwards.
//! Staged rows of fully absorbed files are dropped; rows of files that kept a hunk, and every
//! row for a path that was not absorbable, are kept. A conflict between a group of hunks and the
//! changes above leaves that group where it was; a signing failure moves no ref.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fs;
use std::io::Write;
use std::process::{ExitCode, Stdio};

use async_trait::async_trait;
use bytes::Bytes;
use asc_core::change::RewriteOp;
use asc_core::content::Body;
use asc_core::id::{ChangesetId, GitOid, RefTarget, TreeId};
use asc_core::object::{Change, Kind};
use asc_core::path::{RefName, RepoPath};
use asc_engine::change::{self, ChangeRows, Recorder, RewriteSink as _};
use asc_engine::commit::PathPolicy;
use asc_engine::merge::MergeOptions;
use asc_engine::stack::absorb::{self, AbsorbPlan, Decision, FileEdit, MAX_FILE_BYTES};
use asc_engine::stack::restack::{CommitSigner, SignError, TxnMeta};
use asc_git::projection::Head;
use asc_state::state::stage::{self, StageSet};
use asc_state::state::workspaces;
use asc_store::refs::{Expect, RefTxn, RefUpdate};
use asc_wc::{SafeRoot, status};
use serde::Serialize;

use super::restack::{followers, register};
use super::sl::{LStacks, Local, clean, fatal, stack_err};
use crate::cli::add::{self, convert};
use crate::cli::common::{CliError, Ctx, Result, StagingAfterMove, WsCtx};
use crate::cli::conflict;
use crate::cli::output::Output;
use crate::cli::pager::PagerWant;
use crate::cli::restore::{WcParts, actor, index_tree, root_of};
use crate::ext::{self, OpId};
use crate::hooks_runner::{Hook, HookEnv, RewriteKind, Runner};
use crate::native::NativeSpec;
use crate::stack::json::{VERSION, line};
use crate::stack::opstate;

/// Longest signature the signing program may return.
const MAX_SIGNATURE: usize = 64 * 1024;
/// Longest subject printed.
const SUBJECT_MAX: usize = 200;
/// Most lines of one hunk `-p` prints.
const PRINT_LINES: usize = 200;

#[derive(clap::Parser, Debug)]
#[command(name = "absorb", disable_help_flag = true, disable_version_flag = true)]
pub struct Args {
    /// Show where every hunk would go and change nothing (the default).
    #[arg(short = 'n', long = "dry-run", conflicts_with = "force")]
    pub dry_run: bool,
    /// Apply.
    #[arg(short = 'f', long = "force")]
    pub force: bool,
    /// Print the lines of every hunk.
    #[arg(short = 'p', long = "print-changes")]
    pub print: bool,
    /// Only these changes may take hunks (SPEC §20.3 revset subset).
    #[arg(long = "into")]
    pub into: Option<String>,
    #[arg(long)]
    pub json: bool,
}

pub const NATIVE: &[NativeSpec] = &[NativeSpec {
    cmd: "absorb",
    flags: &[
        "-n",
        "--dry-run",
        "-f",
        "--force",
        "-p",
        "--print-changes",
        "--into",
        "--json",
    ],
    placeholders: &[],
}];

super::verb!(ABSORB, "absorb");

// ---------------------------------------------------------------- report

#[derive(Serialize)]
struct HunkOut {
    path: String,
    /// One-based first line in HEAD's version; the line before an insertion.
    line: u32,
    removed: u32,
    added: u32,
    /// The change that takes (or would take) the hunk.
    change: Option<String>,
    commit: Option<String>,
    /// Why the hunk stays.
    reason: Option<&'static str>,
}

#[derive(Serialize)]
struct SkippedOut {
    path: String,
    reason: String,
}

#[derive(Serialize)]
struct RewriteOut {
    change: String,
    subject: String,
    old: String,
    new: String,
}

#[derive(Serialize)]
struct Report<'a> {
    version: u32,
    /// `preview`, `applied` or `nothing`.
    status: &'a str,
    hunks: &'a [HunkOut],
    skipped: &'a [SkippedOut],
    rewrites: &'a [RewriteOut],
}

fn subject_of(message: &[u8]) -> String {
    let first = message.split(|&b| b == b'\n').next().unwrap_or_default();
    String::from_utf8_lossy(first)
        .chars()
        .take(SUBJECT_MAX)
        .collect()
}

fn short(s: &str) -> String {
    s.chars().take(12).collect()
}

fn path_text(p: &RepoPath) -> String {
    clean(&String::from_utf8_lossy(p.as_bytes()))
}

/// Who a target is, as the report names it.
struct Who {
    change: String,
    commit: String,
    subject: String,
}

async fn who(l: &Local<'_>, cs: ChangesetId) -> Result<Who> {
    let change = l
        .rows
        .get(cs)
        .await
        .map_err(fatal)?
        .map(|r| r.change.to_string())
        .unwrap_or_default();
    let c = l.ws.engine.get_changeset(cs).await?;
    Ok(Who {
        change,
        commit: l.git_id(cs).await?,
        subject: subject_of(&c.message),
    })
}

async fn hunk_rows(
    l: &Local<'_>,
    plan: &AbsorbPlan,
) -> Result<(Vec<HunkOut>, BTreeMap<ChangesetId, Who>)> {
    let mut names: BTreeMap<ChangesetId, Who> = BTreeMap::new();
    let mut out = Vec::with_capacity(plan.decisions.len());
    for d in &plan.decisions {
        let (change, commit, reason) = match d.target {
            Ok(cs) => {
                if let std::collections::btree_map::Entry::Vacant(v) = names.entry(cs) {
                    v.insert(who(l, cs).await?);
                }
                let w = &names[&cs];
                (Some(w.change.clone()), Some(w.commit.clone()), None)
            }
            Err(why) => (None, None, Some(why.as_str())),
        };
        out.push(HunkOut {
            path: path_text(&d.hunk.path),
            line: d.hunk.old_start + u32::from(d.hunk.old_len > 0),
            removed: d.hunk.old_len,
            added: d.hunk.new_len,
            change,
            commit,
            reason,
        });
    }
    Ok((out, names))
}

fn render_preview(
    plan: &AbsorbPlan,
    names: &BTreeMap<ChangesetId, Who>,
    chain: &[ChangesetId],
    edits: &HashMap<&RepoPath, &FileEdit>,
    print: bool,
    applied: bool,
) -> String {
    let mut out = String::new();
    let taken = plan.decisions.iter().filter(|d| d.target.is_ok()).count();
    let verb = if applied { "Absorbed" } else { "Would absorb" };
    out.push_str(&format!(
        "{verb} {taken} of {} hunk{}\n",
        plan.decisions.len(),
        if plan.decisions.len() == 1 { "" } else { "s" }
    ));
    for &cs in chain.iter().rev() {
        let Some(w) = names.get(&cs) else { continue };
        out.push_str(&format!("{} {}\n", short(&w.change), clean(&w.subject)));
        for d in plan.decisions.iter().filter(|d| d.target == Ok(cs)) {
            hunk_line(&mut out, d, edits, print);
        }
    }
    let stays: Vec<&Decision> = plan
        .decisions
        .iter()
        .filter(|d| d.target.is_err())
        .collect();
    if !stays.is_empty() {
        out.push_str("Staying where they are:\n");
        for d in stays {
            hunk_line(&mut out, d, edits, print);
        }
    }
    for (p, why) in &plan.skipped {
        out.push_str(&format!(
            "  {} (file skipped: {})\n",
            path_text(p),
            why.as_str()
        ));
    }
    out
}

fn hunk_line(out: &mut String, d: &Decision, edits: &HashMap<&RepoPath, &FileEdit>, print: bool) {
    let h = &d.hunk;
    let why = match d.target {
        Err(w) => format!(" ({})", w.as_str()),
        Ok(_) => String::new(),
    };
    out.push_str(&format!(
        "  {}:{} -{} +{}{why}\n",
        path_text(&h.path),
        h.old_start + u32::from(h.old_len > 0),
        h.old_len,
        h.new_len
    ));
    if !print {
        return;
    }
    let Some(e) = edits.get(&h.path) else { return };
    let old = asc_engine::diff::line::split_lines(&e.old);
    let (s, n) = (h.old_start as usize, h.old_len as usize);
    let mut shown = 0usize;
    let mut put = |sign: char, l: &[u8]| {
        if shown < PRINT_LINES {
            let t = String::from_utf8_lossy(l);
            out.push_str(&format!(
                "    {sign}{}\n",
                clean(t.trim_end_matches(['\n', '\r']))
            ));
        }
        shown += 1;
    };
    for l in old.iter().skip(s).take(n) {
        put('-', l);
    }
    for l in asc_engine::diff::line::split_lines(&h.replacement) {
        put('+', l);
    }
    if shown > PRINT_LINES {
        out.push_str(&format!("    ... {} more lines\n", shown - PRINT_LINES));
    }
}

fn emit(
    ctx: &Ctx,
    json: bool,
    status: &str,
    hunks: &[HunkOut],
    skipped: &[SkippedOut],
    rewrites: &[RewriteOut],
    text: &str,
) -> Result<ExitCode> {
    let mut out = Output::open(ctx, "stack", PagerWant::Never)?;
    if json {
        let r = Report {
            version: VERSION,
            status,
            hunks,
            skipped,
            rewrites,
        };
        out.write_all(line(&r).map_err(fatal)?.as_bytes())?;
    } else {
        out.write_all(text.as_bytes())?;
    }
    out.finish()?;
    Ok(ExitCode::SUCCESS)
}

// ---------------------------------------------------------------- reading the changes

/// What the work tree offers to absorb.
struct Offer {
    edits: Vec<FileEdit>,
    /// Paths that cannot be absorbed (new, deleted, retyped, unreadable).
    skipped: Vec<(RepoPath, String)>,
}

async fn read(ws: &WsCtx, e: &asc_core::object::Entry) -> Result<Bytes> {
    ws.engine
        .read_file(asc_core::id::FileId::from_bytes(e.id), e.size)
        .await
        .map_err(fatal)
}

fn is_text_kind(k: Kind) -> bool {
    matches!(k, Kind::File | Kind::Exec)
}

/// The staged changes against HEAD, or, with none, the unstaged changes of tracked files.
async fn offer(ctx: &Ctx, ws: &WsCtx, head_root: TreeId) -> Result<Offer> {
    let fresh = workspaces::get(&ws.db, ws.workspace.ws.id)
        .await?
        .ok_or_else(|| fatal("this workspace is not registered in state.db"))?;
    let parts = WcParts::new(ctx, ws, ws.projection.git_dir().join("index"))?;
    let st = status::compute(&parts.wc(&ws.engine), &ws.db, &fresh)
        .await
        .map_err(fatal)?;
    let (index, unmerged) = index_tree(ws, head_root).await?;
    if !unmerged.is_empty() || !st.unmerged.is_empty() {
        return Err(CliError::Raw(
            "error: cannot absorb: you have unmerged paths.\nfatal: resolve them first".into(),
            128,
        ));
    }
    let mut o = Offer {
        edits: Vec::new(),
        skipped: Vec::new(),
    };
    // Intent-to-add rows are not content; only real staged rows put absorb in staged mode.
    let staged = stage::overlay(&ws.db, ws.workspace.ws.id, StageSet::Default)
        .await?
        .iter()
        .any(|r| r.stage == 0 && !r.intent_to_add);
    if staged && index != head_root {
        for (path, change) in ws.engine.leafdiff(head_root, index, None).await? {
            let Change::Write { kind, .. } = change else {
                let why = if matches!(change, Change::Delete) {
                    "deleted"
                } else {
                    "submodule"
                };
                o.skipped.push((path, why.into()));
                continue;
            };
            let old = ws.engine.lookup(head_root, &path).await?;
            let new = ws.engine.lookup(index, &path).await?;
            match (old, new) {
                (Some(a), Some(b))
                    if is_text_kind(a.kind) && is_text_kind(b.kind) && is_text_kind(kind) =>
                {
                    if a.size > MAX_FILE_BYTES as u64 || b.size > MAX_FILE_BYTES as u64 {
                        o.skipped.push((path, "too-large".into()));
                        continue;
                    }
                    let (x, y) = (read(ws, &a).await?, read(ws, &b).await?);
                    o.edits.push(FileEdit {
                        path,
                        old: x,
                        new: y,
                    });
                }
                (None, _) => o.skipped.push((path, "new-file".into())),
                _ => o.skipped.push((path, "type-changed".into())),
            }
        }
        return Ok(o);
    }
    let gate = SafeRoot::new(ws.projection.toplevel());
    let mut attrs = add::workspace_attrs(ctx, ws)?;
    for pc in &st.unstaged {
        let path = pc.path.clone();
        if pc.kind != status::ChangeKind::Modified {
            let why = match pc.kind {
                status::ChangeKind::Deleted => "deleted",
                status::ChangeKind::Added => "new-file",
                _ => "type-changed",
            };
            o.skipped.push((path, why.into()));
            continue;
        }
        let Some(entry) = ws.engine.lookup(head_root, &path).await? else {
            o.skipped.push((path, "new-file".into()));
            continue;
        };
        if !is_text_kind(entry.kind) {
            o.skipped.push((path, "type-changed".into()));
            continue;
        }
        let Some(abs) = gate.resolve(&path) else {
            o.skipped.push((path, "unreadable".into()));
            continue;
        };
        let meta = match fs::symlink_metadata(&abs) {
            Ok(m) if m.is_file() => m,
            _ => {
                o.skipped.push((path, "type-changed".into()));
                continue;
            }
        };
        if meta.len() > MAX_FILE_BYTES as u64 || entry.size > MAX_FILE_BYTES as u64 {
            o.skipped.push((path, "too-large".into()));
            continue;
        }
        let eol = attrs
            .lookup(&path)
            .map_err(|e| CliError::Fatal(e.to_string()))?
            .eol;
        let got = match convert::ingest_file(&abs, eol, &ws.home.tmp()) {
            Ok(g) => g,
            Err(e) => {
                o.skipped
                    .push((path, format!("unreadable: {}", clean(&e.to_string()))));
                continue;
            }
        };
        let Body::Inline(new) = got.body else {
            o.skipped.push((path, "too-large".into()));
            continue;
        };
        let old = read(ws, &entry).await?;
        o.edits.push(FileEdit {
            path,
            old,
            new: Bytes::from(new.to_vec()),
        });
    }
    Ok(o)
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
        let program = self
            .ctx
            .config
            .get("gpg.program")
            .unwrap_or_else(|| "gpg".to_owned());
        let mut cmd = self
            .ctx
            .child(self.ctx.shell())
            .map_err(|e| SignError(e.to_string()))?;
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

fn signer(ctx: &Ctx) -> Result<Option<Gpg<'_>>> {
    if ctx.config.bool("commit.gpgsign") != Some(true) {
        return Ok(None);
    }
    let key = match ctx.config.get("user.signingkey") {
        Some(k) => k,
        None => crate::cli::commit::ident::display(&conflict::identities(ctx)?.committer),
    };
    Ok(Some(Gpg { ctx, key }))
}

// ---------------------------------------------------------------- the run

fn refuse_in_progress(ws: &WsCtx) -> Result<()> {
    let git = ws.projection.git_dir();
    if opstate::exists(git) {
        return Err(CliError::Fatal(
            "asc: cannot absorb: a restack is in progress (run 'asc continue' or 'asc abort')".into(),
        ));
    }
    for f in [
        "rebase-merge",
        "rebase-apply",
        "MERGE_HEAD",
        "CHERRY_PICK_HEAD",
        "REVERT_HEAD",
        "sequencer",
    ] {
        if git.join(f).exists() {
            return Err(CliError::Fatal(format!(
                "asc: cannot absorb: an operation is in progress ({f}); finish it with 'asc continue' or 'asc abort'"
            )));
        }
    }
    Ok(())
}

/// HEAD's stack (bottom first; empty unless it ends at HEAD) and the changes that may take
/// hunks: all of it, or what `--into` selects.
async fn candidates(
    st: &LStacks<'_, '_>,
    head: ChangesetId,
    into: Option<&str>,
) -> Result<(Vec<ChangesetId>, BTreeSet<ChangesetId>)> {
    let stack = st.stack_of(head).await.map_err(stack_err)?;
    // The rewrite restacks everything above the lowest target, so the chain must end at HEAD.
    if stack.changes.last() != Some(&head) {
        return Ok((Vec::new(), BTreeSet::new()));
    }
    let allowed = match into {
        Some(r) => asc_engine::stack::revset::eval_str(st, r)
            .await
            .map_err(stack_err)?
            .into_iter()
            .filter(|c| stack.changes.contains(c))
            .collect(),
        None => stack.changes.iter().copied().collect(),
    };
    Ok((stack.changes, allowed))
}

pub async fn run(ctx: &Ctx, a: Args) -> Result<ExitCode> {
    let l = Local::open(ctx).await?;
    let ws = l.ws;
    refuse_in_progress(ws)?;
    let Some(head) = ws.head_commit().await? else {
        eprintln!("asc: absorb: HEAD is not on a change; nothing can take a hunk");
        return emit(ctx, a.json, "nothing", &[], &[], &[], "");
    };
    let (chain, allowed_set) = {
        let st = l.stacks().await?;
        candidates(&st, head, a.into.as_deref()).await?
    };
    if chain.is_empty() {
        eprintln!("asc: absorb: HEAD is not a mutable change; nothing can take a hunk");
        return emit(ctx, a.json, "nothing", &[], &[], &[], "");
    }
    let head_root = root_of(ws, Some(head)).await?;
    let found = offer(ctx, ws, head_root).await?;
    let mut plan = ws
        .engine
        .plan_absorb(head, &allowed_set, &found.edits)
        .await
        .map_err(absorb_err)?;
    let mut skipped: Vec<SkippedOut> = found
        .skipped
        .iter()
        .map(|(p, w)| SkippedOut {
            path: path_text(p),
            reason: w.clone(),
        })
        .collect();
    skipped.extend(plan.skipped.iter().map(|(p, w)| SkippedOut {
        path: path_text(p),
        reason: w.as_str().into(),
    }));
    let edits: HashMap<&RepoPath, &FileEdit> = found.edits.iter().map(|e| (&e.path, e)).collect();

    if plan.decisions.is_empty() {
        let mut text = String::from("Nothing to absorb\n");
        for s in &skipped {
            text.push_str(&format!("  {} (file skipped: {})\n", s.path, s.reason));
        }
        return emit(ctx, a.json, "nothing", &[], &skipped, &[], &text);
    }
    if !a.force || plan.decisions.iter().all(|d| d.target.is_err()) {
        let (rows, names) = hunk_rows(&l, &plan).await?;
        let text = render_preview(&plan, &names, &chain, &edits, a.print, false);
        let status = if a.force { "nothing" } else { "preview" };
        return emit(ctx, a.json, status, &rows, &skipped, &[], &text);
    }

    // ---- apply
    let policy = PathPolicy::Native {
        windows_safe: ws.repo.repo.windows_safe,
    };
    let gpg = signer(ctx)?;
    let op_id = uuid::Uuid::now_v7();
    let meta = TxnMeta {
        actor: actor(),
        reason: "absorb".into(),
        op_id,
    };
    let done = ws
        .engine
        .execute_absorb(
            &mut plan,
            &chain,
            &found.edits,
            gpg.as_ref().map(|g| g as &dyn CommitSigner),
            &MergeOptions::default(),
            policy,
            &meta,
        )
        .await
        .map_err(absorb_err)?;
    let (rows, names) = hunk_rows(&l, &plan).await?;
    if done.rewrites.is_empty() {
        let text = render_preview(&plan, &names, &chain, &edits, a.print, false);
        return emit(ctx, a.json, "nothing", &rows, &skipped, &[], &text);
    }
    for r in &done.rewrites {
        register(ws, r.new).await?;
    }
    let map: HashMap<ChangesetId, ChangesetId> =
        done.rewrites.iter().map(|r| (r.old, r.new)).collect();
    let new_head = map.get(&head).copied().unwrap_or(head);

    // Git ids and report rows first: a ref move that lands HEAD on a rewrite needs them.
    let mut report: Vec<RewriteOut> = Vec::new();
    let mut hook_pairs: Vec<(GitOid, GitOid)> = Vec::new();
    for r in &done.rewrites {
        let (old_git, new_git) = (
            conflict::git_id(ws, r.old).await?,
            conflict::git_id(ws, r.new).await?,
        );
        hook_pairs.push((old_git, new_git));
        let w = who(&l, r.old).await?;
        report.push(RewriteOut {
            change: w.change,
            subject: w.subject,
            old: old_git.to_string(),
            new: new_git.to_string(),
        });
    }

    let mut updates: Vec<RefUpdate> = Vec::new();
    let mut seen = HashSet::new();
    for f in followers(ws, &chain).await? {
        let (name, from) = (f.name, f.from);
        if let Some(&new) = map.get(&from)
            && seen.insert(name.clone())
        {
            updates.push(RefUpdate {
                name,
                expect: Expect::Value(RefTarget::Changeset(from)),
                new: Some(RefTarget::Changeset(new)),
                expires_at: None,
            });
        }
    }
    if matches!(ws.projection.read_head()?, Head::Detached(_)) && new_head != head {
        updates.push(RefUpdate {
            name: RefName::new_onelevel(b"HEAD".to_vec()).map_err(fatal)?,
            expect: Expect::Value(RefTarget::Changeset(head)),
            new: Some(RefTarget::Changeset(new_head)),
            expires_at: None,
        });
    }

    // Staged rows stay unless their file was absorbed completely.
    let mut partial: HashSet<&RepoPath> = found.skipped.iter().map(|(p, _)| p).collect();
    partial.extend(plan.skipped.iter().map(|(p, _)| p));
    partial.extend(
        plan.decisions
            .iter()
            .filter(|d| d.target.is_err())
            .map(|d| &d.hunk.path),
    );
    let absorbed: HashSet<Vec<u8>> = plan
        .decisions
        .iter()
        .filter(|d| d.target.is_ok() && !partial.contains(&d.hunk.path))
        .map(|d| d.hunk.path.as_bytes().to_vec())
        .collect();
    let mut rows_kept = stage::overlay(&ws.db, ws.workspace.ws.id, StageSet::Default).await?;
    rows_kept.retain(|r| !absorbed.contains(&r.path));

    let taken = plan.decisions.iter().filter(|d| d.target.is_ok()).count();
    let txn = RefTxn {
        updates,
        actor: actor(),
        reason: String::new(),
        op_id,
    };
    let reason = format!("absorb: {taken} hunk{}", if taken == 1 { "" } else { "s" });
    ctx.update_local_refs(txn, &reason, StagingAfterMove::Set(rows_kept))
        .await?;

    let rows_db = asc_state::changes::StateChanges::new(ws.db.clone(), ws.repo.key);
    let recorder = Recorder::new(rows_db);
    for r in &done.rewrites {
        let op = if r.target {
            RewriteOp::Absorb
        } else {
            RewriteOp::Restack
        };
        recorder
            .record(r.new, &[r.old], op, change::OpId(op_id))
            .await
            .map_err(fatal)?;
        ext::rewrite_sink().record(r.new, &[r.old], op, OpId(op_id));
        ext::index_sink()
            .on_commit(ws.workspace.ws.id, r.new)
            .await
            .map_err(fatal)?;
    }
    let runner = Runner::for_ctx(ctx)?;
    let env = HookEnv::of(ctx, "absorb");
    let _ = runner
        .run(
            Hook::PostRewrite {
                kind: RewriteKind::Rebase,
                pairs: hook_pairs,
            },
            &env,
        )
        .await?;

    let text = render_preview(&plan, &names, &chain, &edits, a.print, true);
    emit(ctx, a.json, "applied", &rows, &skipped, &report, &text)
}

fn absorb_err(e: absorb::Error) -> CliError {
    match e {
        absorb::Error::Sign(m) => CliError::Raw(
            format!(
                "error: absorb: signing failed: {}\nNo ref was moved.",
                clean(&m)
            ),
            1,
        ),
        other => fatal(format!("absorb: {other}")),
    }
}
