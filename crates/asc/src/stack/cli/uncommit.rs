//! `asc uncommit [<change>] [-a|-p|<pathspec>…]` (SPEC §20.2, §20.7): moves the change's tree, or
//! the chosen paths, back into the work tree and the stage.
//!
//! The change is rewritten (same change id, the old changeset as predecessor) with the parent's
//! content for the chosen paths. The work tree and the stage are not touched: the stage is
//! rebased across the move (as `asc reset --soft` does), so the content that left the change is
//! now staged. Only the checked-out change can be uncommitted, because the work tree is the one
//! place the content can go, and only a change with nothing above it, because the changes above
//! would be rebuilt without the content they sit on. `-p` is refused: asc never opens a prompt;
//! name the paths instead.

use std::ffi::OsString;
use std::process::ExitCode;

use asc_core::id::ChangesetId;
use asc_engine::change::ChangeRows as _;
use uuid::Uuid;

use super::info::resolve_change;
use super::reword::{move_refs, refuse_busy, rewrite};
use super::rewrite::{Made, emit, emit_nothing, locate};
use super::restack::Flow;
use super::sl::{Local, fatal};
use crate::cli::common::{CliError, Ctx, Result, WsCtx};
use crate::cli::conflict;
use crate::cli::diff::pathspec;
use crate::cli::restore::root_of;
use crate::ext::{self, OpId};
use crate::hooks_runner::{Hook, HookEnv, RewriteKind, Runner};
use crate::native::NativeSpec;

/// Most pathspecs one run takes.
const MAX_PATHSPECS: usize = 4096;

#[derive(clap::Parser, Debug)]
#[command(name = "uncommit", disable_help_flag = true, disable_version_flag = true)]
pub struct Args {
    /// Uncommit everything (the default without a pathspec).
    #[arg(short = 'a', long = "all", conflicts_with = "patch")]
    pub all: bool,
    /// Choose hunks (refused: asc never opens a prompt).
    #[arg(short = 'p', long = "patch")]
    pub patch: bool,
    /// Report what would move; change nothing.
    #[arg(short = 'n', long = "dry-run")]
    pub dry_run: bool,
    #[arg(long)]
    pub json: bool,
    /// `[<change>] [<pathspec>…]`: the first word is the change when it names a draft change.
    #[arg(num_args = 0.., value_name = "CHANGE-OR-PATHSPEC")]
    pub words: Vec<OsString>,
    /// Pathspecs after `--`.
    #[arg(last = true)]
    pub after: Vec<OsString>,
}

pub const NATIVE: &[NativeSpec] = &[NativeSpec {
    cmd: "uncommit",
    flags: &["-a", "--all", "-p", "--patch", "-n", "--dry-run", "--json"],
    placeholders: &[],
}];

super::verb!(UNCOMMIT, "uncommit");

/// Runs `post-rewrite amend` for one rewritten pair.
pub(crate) async fn post_rewrite(ctx: &Ctx, ws: &WsCtx, verb: &str, old: ChangesetId, new: ChangesetId) -> Result<()> {
    let runner = Runner::for_ctx(ctx)?;
    let env = HookEnv::of(ctx, verb);
    let pair = (conflict::git_id(ws, old).await?, conflict::git_id(ws, new).await?);
    let _ = runner.run(Hook::PostRewrite { kind: RewriteKind::Amend, pairs: vec![pair] }, &env).await?;
    Ok(())
}

/// The checked-out change, which must have nothing above it.
pub(crate) async fn checked_out(ctx: &Ctx, l: &Local<'_>, word: Option<&str>, verb: &str) -> Result<ChangesetId> {
    let st = l.stacks().await?;
    let (x, _) = resolve_change(ctx, l, &st, word).await?;
    if st.current_head() != Some(x) {
        return Err(CliError::Fatal(format!(
            "asc: {verb}: only the checked-out change can be rewritten this way; switch to it first"
        )));
    }
    let loc = locate(&st, x, verb).await?;
    if !loc.upper.is_empty() {
        return Err(CliError::Raw(format!("asc: {verb}: the change has changes above it; {verb} the top change"), 1));
    }
    Ok(x)
}

pub(crate) fn subject_of(message: &[u8]) -> String {
    let first = message.split(|&b| b == b'\n').next().unwrap_or_default();
    String::from_utf8_lossy(first).chars().take(200).collect()
}

pub async fn run(ctx: &Ctx, a: Args) -> Result<ExitCode> {
    if a.patch {
        return Err(CliError::Raw(
            "asc: uncommit: -p needs a prompt, which asc never opens; name the paths instead".into(),
            1,
        ));
    }
    if a.words.len() + a.after.len() > MAX_PATHSPECS {
        return Err(CliError::Raw("asc: uncommit: too many pathspecs".into(), 1));
    }
    let l = Local::open(ctx).await?;
    let ws = l.ws;
    refuse_busy(ws, "uncommit")?;

    // The first word is the change when it names one; otherwise every word is a pathspec.
    let mut words = a.words.clone();
    let mut word: Option<String> = None;
    if let Some(first) = words.first().and_then(|w| w.to_str()).map(str::to_owned) {
        let st = l.stacks().await?;
        if resolve_change(ctx, &l, &st, Some(&first)).await.is_ok() {
            word = Some(first);
            words.remove(0);
        }
    }
    words.extend(a.after.iter().cloned());
    if a.all && !words.is_empty() {
        return Err(CliError::Usage("usage: asc uncommit [<change>] [-a|<pathspec>...]".into()));
    }

    let x = checked_out(ctx, &l, word.as_deref(), "uncommit").await?;
    let cx = ws.engine.get_changeset(x).await?;
    if cx.parents.len() != 1 {
        return Err(CliError::Raw("asc: uncommit: the change is a merge or a root".into(), 1));
    }
    let base_root = root_of(ws, Some(cx.parents[0])).await?;
    let root = if words.is_empty() {
        base_root
    } else {
        let mut changes = Vec::new();
        for w in &words {
            let spec = pathspec(ctx, std::slice::from_ref(w))?
                .ok_or_else(|| CliError::Raw("asc: uncommit: an empty pathspec".into(), 1))?;
            let found = ws.engine.leafdiff(cx.root, base_root, Some(&spec)).await?;
            if found.is_empty() {
                return Err(CliError::Raw(
                    format!("asc: uncommit: '{}' matches no file this change touches", super::sl::clean(&w.to_string_lossy())),
                    1,
                ));
            }
            changes.extend(found);
        }
        changes.sort_by(|p, q| p.0.as_bytes().cmp(q.0.as_bytes()));
        changes.dedup_by(|p, q| p.0 == q.0);
        ws.engine.apply(cx.root, &changes).await?
    };
    if root == cx.root {
        return emit_nothing(ctx, a.json, "uncommit");
    }
    let subject = subject_of(&cx.message);
    let change = l.rows.get(x).await.map_err(fatal)?.map(|r| r.change.to_string()).unwrap_or_default();
    if a.dry_run {
        let made = Made { cs: x, change, subject };
        return emit(ctx, a.json, "uncommit", "Would uncommit", &[made], 0, &Flow::Done(Vec::new()));
    }
    drop(l);
    let op = Uuid::now_v7();
    let new = rewrite(ctx, ws, x, None, Some(root), &[], op).await?;
    ext::rewrite_sink().record(new, &[x], asc_core::change::RewriteOp::Amend, OpId(op));
    ext::index_sink().on_commit(ws.workspace.ws.id, new).await.map_err(fatal)?;
    move_refs(ctx, ws, x, new, op, "uncommit").await?;
    post_rewrite(ctx, ws, "uncommit", x, new).await?;
    let made = Made { cs: new, change, subject };
    emit(ctx, a.json, "uncommit", "Uncommitted", &[made], 0, &Flow::Done(Vec::new()))
}
