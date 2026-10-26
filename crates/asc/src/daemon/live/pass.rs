//! One live pass over one worktree (SPEC §21.3.8; B29, B30): fetch and, per `asc.live.*`, restack
//! the user's own draft stacks onto the new trunk and move the working copy.
//!
//! The pass is what the hidden verb `asc live run` runs; the daemon's production [`Runner`]
//! starts it in a child process with `core.hooksPath` pointing at an empty directory, so no hook
//! of the user's runs on a live update. Being an ordinary command, it is bracketed in the op log
//! with kind `live` and every ref it moves shares that op, so `asc undo` reverts it.
//!
//! Order of work, each step bounded:
//!
//! 1. the gates: `asc.live.fetch` (and what it implies), the pause, an asc server repository;
//! 2. the fetch of the default remote (the same engine fetch as `asc fetch`, with its sync
//!    verification), which is all `asc.live.fetch` does;
//! 3. with `asc.live.restack`: the inputs of B29's [`plan_live`] (trunk tip, the user's visible
//!    draft stacks, the worktree state) and the plan;
//! 4. the plan's result: a conflict records a pause and a notice and changes nothing; otherwise
//!    the rewrites are registered and every ref moves in one transaction, and with
//!    `asc.live.update` and no safety gate the working copy follows.
//!
//! Only the primary trunk (the first `asc.stack.trunk`, default `master`) is followed, and only a
//! linear stack: a stack that shares a change with another one is left to `asc restack`. The
//! branch HEAD is attached to, and a detached HEAD, move only together with the working copy.
//! With `commit.gpgSign` on, a restack is not attempted (a background run cannot sign) and a
//! notice says so.
//!
//! [`Runner`]: super::Runner

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;

use async_trait::async_trait;
use asc_core::change::RewriteOp;
use asc_core::id::{ChangesetId, RefTarget, TreeId};
use asc_core::path::{RefName, RepoPath};
use asc_engine::branchless::DraftRows as _;
use asc_engine::branchless::live::{
    self, DEFAULT_MAX_FILES, DEFAULT_MAX_STACK, DraftStack, EngineRestacker, InProgress, LiveApply,
    LiveConfig, LiveInputs, LivePlan, MAX_STACKS, MoveBlock, Notice, WorktreeState,
};
use asc_engine::change::{self, ChangeRows, Recorder, RewriteSink as _};
use asc_engine::derive::Gen;
use asc_engine::graph::{EngineSource, Graph, GraphSource, JumpIndex};
use asc_engine::history::HistoryApi as _;
use asc_engine::merge::MergeOptions;
use asc_engine::stack::restack::RefMove;
use asc_engine::stack::{self as engine_stack, StackEnv, Stacks};
use asc_git::projection::Head;
use asc_state::changes::StateChanges;
use asc_state::state::repos::RemoteKind;
use asc_state::state::tables::cs_gen;
use asc_state::state::workspaces::{self, Mode};
use asc_store::refs::{Expect, RefTxn, RefUpdate, Refs as _};
use asc_wc::status::{self, Untracked};

use super::super::cloud::StateRows;
use crate::cli::common::{CliError, Ctx, Result, StagingAfterMove, WsCtx};
use crate::cli::conflict;
use crate::cli::fetch;
use crate::cli::restore::{WcParts, actor, index_tree, root_of, sync_worktree};
use crate::ext::{self, OpId};

/// Most refs read for one pass.
const MAX_REFS: usize = 100_000;
/// Visits the diff of one stack may spend before it counts as over the file cap.
const DIFF_BUDGET: usize = 2_000_000;
/// Longest part of a changeset subject shown in a notice.
const MAX_SUBJECT: usize = 80;

/// What a pass did, for `asc live run`, `asc live status` and the daemon's log.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Done {
    word: &'static str,
    detail: String,
}

impl Done {
    fn new(word: &'static str, detail: impl Into<String>) -> Self {
        Self {
            word,
            detail: detail.into(),
        }
    }

    /// `off`, `paused`, `skipped`, `fetched`, `restacked`, `updated` or `notice`.
    pub fn word(&self) -> &'static str {
        self.word
    }

    pub fn detail(&self) -> &str {
        &self.detail
    }

    /// One line without control characters.
    pub fn describe(&self) -> String {
        let mut line = self.word.to_owned();
        if !self.detail.is_empty() {
            line.push_str(": ");
            line.push_str(&self.detail);
        }
        line.chars()
            .map(|c| if c.is_control() { ' ' } else { c })
            .collect()
    }
}

fn fatal(e: impl std::fmt::Display) -> CliError {
    CliError::Fatal(format!("asc: live: {e}"))
}

fn stack_err(e: engine_stack::Error) -> CliError {
    fatal(e)
}

/// The `asc.live.*` settings of this worktree, with each key implying the ones above it.
pub fn config_of(ctx: &Ctx) -> LiveConfig {
    let on = |key: &str, default: bool| ctx.config.bool(key).unwrap_or(default);
    let count = |key: &str, default: usize| {
        ctx.config
            .get(key)
            .and_then(|v| v.trim().parse::<usize>().ok())
            .filter(|n| *n > 0)
            .unwrap_or(default)
    };
    LiveConfig {
        fetch: on("asc.live.fetch", true),
        restack: on("asc.live.restack", false),
        update: on("asc.live.update", false),
        max_stack: count("asc.live.maxStack", DEFAULT_MAX_STACK),
        max_files: count("asc.live.maxFiles", DEFAULT_MAX_FILES),
    }
    .effective()
}

/// Runs one pass for the worktree of `ctx`.
pub async fn run(ctx: &Ctx) -> Result<Done> {
    let ws = ctx.ws()?;
    let id = ws.workspace.ws.id.to_string();
    let mut cfg = config_of(ctx);
    if !cfg.fetch {
        return Ok(Done::new("off", ""));
    }
    if crate::cli::live::paused(&ws.home, &id) {
        return Ok(Done::new("paused", ""));
    }
    if ws.repo.repo.remote_kind != RemoteKind::Server {
        return Ok(Done::new("skipped", "not an asc server repository"));
    }
    let remote = fetch::default_remote(ctx, ws)?;
    let opts = fetch::Opts {
        quiet: true,
        ..fetch::Opts::default()
    };
    fetch::fetch_one(ctx, ws, &remote, &opts, &[]).await?;
    if !cfg.restack {
        return Ok(Done::new("fetched", ""));
    }
    // The working copy of a virtual workspace is the mount; only a plain tree is moved here.
    if ws.workspace.ws.mode == Mode::Virtual {
        cfg.update = false;
    }
    restack(ctx, ws, cfg, &remote).await
}

// ------------------------------------------------------------------------------ stack inputs

/// The refs, trunks and heads the stack derivation reads, read once.
struct Env<'a> {
    ws: &'a WsCtx,
    refs: BTreeMap<String, ChangesetId>,
    /// Existing trunks in `asc.stack.trunk` order: name and tip.
    trunks: Vec<(String, ChangesetId)>,
    immutable: Vec<String>,
    heads: Vec<ChangesetId>,
    drafts: Vec<ChangesetId>,
    current: Option<ChangesetId>,
    email: Option<Vec<u8>>,
}

/// Whether `pat` matches `s`, where `*` matches any run of bytes and the rest is literal.
fn glob(pat: &[u8], s: &[u8]) -> bool {
    let (mut p, mut i) = (0, 0);
    let mut star: Option<(usize, usize)> = None;
    while i < s.len() {
        if p < pat.len() && pat[p] == b'*' {
            star = Some((p, i));
            p += 1;
        } else if p < pat.len() && pat[p] == s[i] {
            p += 1;
            i += 1;
        } else if let Some((sp, si)) = star {
            p = sp + 1;
            i = si + 1;
            star = Some((sp, si + 1));
        } else {
            return false;
        }
    }
    pat[p..].iter().all(|&b| b == b'*')
}

impl<'a> Env<'a> {
    async fn load(
        ctx: &Ctx,
        ws: &'a WsCtx,
        remote: &str,
        drafts: Vec<ChangesetId>,
    ) -> Result<Self> {
        let entries = ws.db.refs().list(ws.repo.key, "refs/").await?;
        if entries.len() > MAX_REFS {
            return Err(fatal(format!("more than {MAX_REFS} refs")));
        }
        let mut refs = BTreeMap::new();
        for e in entries {
            let RefTarget::Changeset(cs) = e.target else {
                continue;
            };
            if let Ok(name) = String::from_utf8(e.name.as_bytes().to_vec()) {
                refs.insert(name, cs);
            }
        }
        let mut names = ctx.config.get_all("asc.stack.trunk");
        names.retain(|t| !t.is_empty());
        if names.is_empty() {
            names.push("master".into());
        }
        let mut seen = HashSet::new();
        names.retain(|t| seen.insert(t.clone()));
        // The remote-tracking ref of the fetched remote, else the local branch.
        let trunks = names
            .into_iter()
            .filter_map(|t| {
                let tip = refs
                    .get(&format!("refs/remotes/{remote}/{t}"))
                    .or_else(|| refs.get(&format!("refs/heads/{t}")))?;
                Some((t, *tip))
            })
            .collect();
        let mut immutable = ctx.config.get_all("asc.stack.immutable");
        immutable.retain(|t| !t.is_empty());
        if immutable.is_empty() {
            immutable.push("refs/remotes/*/release/*".into());
        }
        let current = ws.head_commit().await?;
        let mut heads: Vec<ChangesetId> = refs
            .iter()
            .filter(|(k, _)| k.starts_with("refs/heads/") || k.starts_with("refs/asc/stack/"))
            .map(|(_, cs)| *cs)
            .collect();
        heads.extend(current);
        for w in workspaces::list(&ws.db, Some(ws.repo.key)).await? {
            if w.ws.id == ws.workspace.ws.id {
                continue;
            }
            let head = if w.ws.head.is_empty() {
                w.ws.base_cs
            } else {
                let name = w.ws.head.strip_prefix("refs/heads/").unwrap_or(&w.ws.head);
                refs.get(&format!("refs/heads/{name}")).copied()
            };
            heads.extend(head);
        }
        Ok(Self {
            ws,
            refs,
            trunks,
            immutable,
            heads,
            drafts,
            current,
            email: ctx.config.get("user.email").map(String::into_bytes),
        })
    }
}

fn env_err(e: impl std::fmt::Display) -> engine_stack::Error {
    engine_stack::Error::Env(e.to_string())
}

#[async_trait]
impl StackEnv for Env<'_> {
    async fn trunk_tips(&self) -> engine_stack::Result<Vec<ChangesetId>> {
        Ok(self.trunks.iter().map(|(_, tip)| *tip).collect())
    }

    async fn immutable_tips(&self) -> engine_stack::Result<Vec<ChangesetId>> {
        Ok(self
            .refs
            .iter()
            .filter(|(k, _)| {
                self.immutable
                    .iter()
                    .any(|p| glob(p.as_bytes(), k.as_bytes()))
            })
            .map(|(_, cs)| *cs)
            .collect())
    }

    async fn heads(&self) -> engine_stack::Result<Vec<ChangesetId>> {
        Ok(self.heads.clone())
    }

    async fn draft_heads(&self) -> engine_stack::Result<Vec<ChangesetId>> {
        Ok(self.drafts.clone())
    }

    async fn current_head(&self) -> engine_stack::Result<Option<ChangesetId>> {
        Ok(self.current)
    }

    fn user_email(&self) -> Option<Vec<u8>> {
        self.email.clone()
    }

    async fn author_email(&self, cs: ChangesetId) -> engine_stack::Result<Vec<u8>> {
        let c = self.ws.engine.get_changeset(cs).await.map_err(env_err)?;
        Ok(c.author.email().to_vec())
    }

    async fn landed(&self, _change: asc_core::change::ChangeId) -> engine_stack::Result<bool> {
        Ok(false)
    }

    async fn resolve_git(&self, spec: &str) -> engine_stack::Result<Vec<ChangesetId>> {
        let found = self
            .ws
            .engine
            .resolve(&[spec.to_owned()])
            .await
            .map_err(env_err)?;
        Ok(found.into_iter().flatten().collect())
    }
}

/// The paths that differ between two trees, and whether more than `cap` do (or the diff is too
/// large to walk, which counts the same).
async fn diff_of(ws: &WsCtx, a: TreeId, b: TreeId, cap: usize) -> (Vec<RepoPath>, bool) {
    match ws
        .engine
        .leafdiff_limited(a, b, None, DIFF_BUDGET, cap)
        .await
    {
        Ok((rows, _)) => {
            let cut = rows.len() > cap;
            (rows.into_iter().map(|(p, _)| p).collect(), cut)
        }
        Err(_) => (Vec::new(), true),
    }
}

/// The visible drafts as stacks on the trunk with tip `tip`: linear, not frozen and sharing no
/// change with another stack. A stack that is a prefix of a longer one is that one's business.
async fn linear_stacks<S, J, C, E>(
    stacks: &Stacks<'_, S, J, C, E>,
    drafts: &[ChangesetId],
    tip: ChangesetId,
    frozen: &HashSet<asc_core::change::ChangeId>,
    rows: &StateChanges,
) -> Result<Vec<engine_stack::Stack>>
where
    S: GraphSource,
    J: JumpIndex,
    C: ChangeRows,
    E: StackEnv,
{
    let mut found: Vec<engine_stack::Stack> = Vec::new();
    for &h in drafts {
        if found.iter().any(|s| s.head == h) {
            continue;
        }
        let st = stacks.stack_of(h).await.map_err(stack_err)?;
        if st.changes.is_empty() || st.base.is_none() || st.trunk != Some(tip) {
            continue;
        }
        found.push(st);
    }
    // A draft head below another head is part of that stack.
    let tops: Vec<engine_stack::Stack> = found
        .iter()
        .filter(|a| {
            !found
                .iter()
                .any(|b| b.head != a.head && b.changes.contains(&a.head))
        })
        .cloned()
        .collect();
    // A fork shares a change: neither side is touched.
    let mut count: HashMap<ChangesetId, usize> = HashMap::new();
    for s in &tops {
        for c in &s.changes {
            *count.entry(*c).or_default() += 1;
        }
    }
    let mut out = Vec::new();
    'stack: for s in tops {
        if s.changes
            .iter()
            .any(|c| count.get(c).is_some_and(|n| *n > 1))
        {
            continue;
        }
        if !frozen.is_empty() {
            for c in &s.changes {
                if let Some(row) = rows.get(*c).await.map_err(fatal)?
                    && frozen.contains(&row.change)
                {
                    continue 'stack;
                }
            }
        }
        out.push(s);
    }
    Ok(out)
}

/// The operation in progress in the git directory, if any.
fn operation_of(git: &Path) -> Option<InProgress> {
    let has = |f: &str| std::fs::symlink_metadata(git.join(f)).is_ok();
    if has("rebase-merge") || has("rebase-apply") || crate::stack::opstate::exists(git) {
        Some(InProgress::Rebase)
    } else if has("MERGE_HEAD") {
        Some(InProgress::Merge)
    } else if has("CHERRY_PICK_HEAD") || has("REVERT_HEAD") || has("sequencer") {
        Some(InProgress::CherryPick)
    } else if has("BISECT_LOG") {
        Some(InProgress::Bisect)
    } else {
        None
    }
}

/// Whether an untracked path is in the way of writing any of `changed`: it is one of them, or a
/// directory or file on the way to one.
fn collides(untracked: &[Untracked], changed: &[RepoPath]) -> bool {
    if untracked.is_empty() || changed.is_empty() {
        return false;
    }
    let mut wanted: HashSet<Vec<u8>> = HashSet::new();
    for p in changed {
        let bytes = p.as_bytes();
        wanted.insert(bytes.to_vec());
        for (i, b) in bytes.iter().enumerate() {
            if *b == b'/' {
                wanted.insert(bytes[..i].to_vec());
            }
        }
    }
    untracked.iter().any(|u| {
        let bytes = u.path.as_bytes();
        let bytes = bytes.strip_suffix(b"/").unwrap_or(bytes);
        wanted.contains(bytes)
    })
}

/// What the safety gates of the move read from the worktree. Writes only the stat cache of the
/// index, as `asc status` does.
async fn worktree_state(
    ctx: &Ctx,
    ws: &WsCtx,
    head: Option<ChangesetId>,
    changed: &[RepoPath],
) -> Result<WorktreeState> {
    let git = ws.projection.git_dir();
    let operation = operation_of(git);
    let index_lock_held = std::fs::symlink_metadata(git.join("index.lock")).is_ok();
    if index_lock_held {
        // Another process is writing the index; looking at it now would only wait for it.
        return Ok(WorktreeState {
            operation,
            index_lock_held,
            ..WorktreeState::default()
        });
    }
    let head_root = root_of(ws, head).await?;
    let fresh = workspaces::get(&ws.db, ws.workspace.ws.id)
        .await?
        .ok_or_else(|| fatal("this workspace is not registered in state.db"))?;
    let parts = WcParts::new(ctx, ws, git.join("index"))?;
    let st = status::compute(&parts.wc(&ws.engine), &ws.db, &fresh)
        .await
        .map_err(fatal)?;
    let (index, unmerged) = index_tree(ws, head_root).await?;
    Ok(WorktreeState {
        dirty: !st.unstaged.is_empty() || index != head_root,
        colliding_untracked: collides(&st.untracked, changed),
        operation,
        unresolved_conflicts: !unmerged.is_empty() || !st.unmerged.is_empty(),
        index_lock_held,
        paused: false,
    })
}

fn block_text(b: MoveBlock) -> &'static str {
    match b {
        MoveBlock::Dirty => "local changes",
        MoveBlock::CollidingUntracked => "untracked files in the way",
        MoveBlock::Operation(_) => "an operation is in progress",
        MoveBlock::UnresolvedConflicts => "unresolved conflicts",
        MoveBlock::IndexLocked => "the index is locked",
    }
}

// ------------------------------------------------------------------------------ the pass

async fn restack(ctx: &Ctx, ws: &WsCtx, cfg: LiveConfig, remote: &str) -> Result<Done> {
    let operation = operation_of(ws.projection.git_dir());
    // A restack under a paused rebase, merge or cherry-pick would move refs it relies on.
    if matches!(
        operation,
        Some(InProgress::Rebase | InProgress::Merge | InProgress::CherryPick)
    ) {
        return Ok(Done::new(
            "fetched",
            "an operation is in progress; nothing restacked",
        ));
    }
    let state_rows = StateRows::new(ws.db.clone(), ws.repo.key);
    let visible = state_rows
        .visible_heads(MAX_STACKS + 1)
        .await
        .map_err(fatal)?;
    let drafts: Vec<ChangesetId> = visible.iter().map(|h| h.cs).collect();
    if drafts.len() > MAX_STACKS {
        return Ok(Done::new(
            "notice",
            format!("more than {MAX_STACKS} draft heads; nothing restacked"),
        ));
    }
    let env = Env::load(ctx, ws, remote, drafts.clone()).await?;
    let Some((trunk_name, tip)) = env.trunks.first().cloned() else {
        return Ok(Done::new("fetched", "no trunk to follow"));
    };
    let rows = StateChanges::new(ws.db.clone(), ws.repo.key);
    let graph = Graph::new(EngineSource::new(&ws.engine));
    let stacks = Stacks::load(&graph, &rows, &env).await.map_err(stack_err)?;
    let frozen = rows.frozen_set().await.map_err(fatal)?;
    let found = linear_stacks(&stacks, &drafts, tip, &frozen, &rows).await?;

    let head = ws.head_commit().await?;
    let head_branch = match ws.projection.read_head()? {
        Head::Branch(b) => Some(b),
        Head::Detached(_) => None,
    };
    // The checkout is on trunk when it is an ancestor of the tip: on the trunk branch or
    // detached; a branch of the user's own is never fast-forwarded.
    let on_trunk = match head {
        Some(h) if h != tip => {
            head_branch.as_deref().is_none_or(|b| b == trunk_name)
                && graph.is_ancestor(h, tip).await.map_err(fatal)?
        }
        _ => false,
    };

    let tip_root = root_of(ws, Some(tip)).await?;
    let mut dstacks = Vec::with_capacity(found.len());
    let mut steps: HashSet<ChangesetId> = HashSet::new();
    let mut moved: Vec<RepoPath> = Vec::new();
    let mut needs = false;
    for st in &found {
        let Some(base) = st.base else { continue };
        let mut changed_paths = 0;
        if base != tip {
            needs = true;
            steps.extend(st.changes.iter().copied());
            let base_root = root_of(ws, Some(base)).await?;
            let head_root = root_of(ws, Some(st.head)).await?;
            let (trunk_moved, cut_a) = diff_of(ws, base_root, tip_root, cfg.max_files).await;
            let (own, cut_b) = diff_of(ws, base_root, head_root, cfg.max_files).await;
            changed_paths = if cut_a || cut_b {
                usize::MAX
            } else {
                trunk_moved.len().saturating_add(own.len())
            };
            moved.extend(trunk_moved);
        }
        dstacks.push(DraftStack {
            own: true,
            base,
            steps: st.changes.clone(),
            changed_paths,
        });
    }
    let move_trunk = cfg.update && on_trunk;
    if !needs && !move_trunk {
        return Ok(Done::new("fetched", ""));
    }
    if needs && ctx.config.bool("commit.gpgsign") == Some(true) {
        return notice(
            ws,
            Notice::SignFailed("commit.gpgSign is on and a background run cannot sign".into()),
        )
        .await;
    }
    if move_trunk && let Some(h) = head {
        let from = root_of(ws, Some(h)).await?;
        let (paths, _) = diff_of(ws, from, tip_root, DIFF_BUDGET).await;
        moved.extend(paths);
    }

    let worktree = if cfg.update {
        worktree_state(ctx, ws, head, &moved).await?
    } else {
        WorktreeState {
            operation,
            ..WorktreeState::default()
        }
    };
    let inputs = LiveInputs {
        trunk_tip: tip,
        checked_out: head,
        on_trunk,
        stacks: dstacks,
        worktree,
        config: cfg,
    };

    // Branches follow their change, except the one HEAD is on: that one moves with the working
    // copy or not at all.
    let head_ref = head_branch.as_ref().map(|b| format!("refs/heads/{b}"));
    let mut moves = Vec::new();
    for e in ws.db.refs().list(ws.repo.key, "refs/heads/").await? {
        if let RefTarget::Changeset(cs) = e.target
            && steps.contains(&cs)
            && head_ref.as_deref().map(str::as_bytes) != Some(e.name.as_bytes())
        {
            moves.push(RefMove {
                name: e.name,
                from: cs,
            });
        }
    }
    let op_id = OpId::new().0;
    let restacker = EngineRestacker {
        engine: &ws.engine,
        opts: MergeOptions::default(),
        actor: actor(),
        op_id,
        signer: None,
        moves,
    };
    let plan = match live::plan_live(&inputs, &restacker).await {
        Ok(plan) => plan,
        Err(live::Error::TooLarge(what, max)) => {
            return Ok(Done::new("notice", format!("more than {max} {what}")));
        }
        Err(e) => return Err(fatal(e)),
    };
    match plan {
        LivePlan::FetchOnly => Ok(Done::new("fetched", "")),
        LivePlan::Notice(n) => notice(ws, n).await,
        LivePlan::Restack {
            apply,
            move_blocked,
        } => {
            let n = apply_plan(ctx, ws, head, head_branch.as_deref(), apply, None, op_id).await?;
            let detail = match move_blocked {
                Some(b) => format!(
                    "{} change{} restacked; working copy not moved: {}",
                    n.rewrites,
                    plural(n.rewrites),
                    block_text(b)
                ),
                None => format!("{} change{} restacked", n.rewrites, plural(n.rewrites)),
            };
            Ok(Done::new("restacked", detail))
        }
        LivePlan::RestackAndMove { apply, to } => {
            let n = apply_plan(
                ctx,
                ws,
                head,
                head_branch.as_deref(),
                apply,
                Some(to),
                op_id,
            )
            .await?;
            let changes = format!("{} change{} restacked", n.rewrites, plural(n.rewrites));
            Ok(if n.moved {
                Done::new("updated", changes)
            } else {
                Done::new(
                    "restacked",
                    format!("{changes}; working copy not moved: it changed meanwhile"),
                )
            })
        }
    }
}

fn plural(n: usize) -> &'static str {
    if n == 1 { "" } else { "s" }
}

/// What a notice means: a conflict pauses the worktree until `asc live resume`, the rest are
/// only reported. Nothing was changed in either case.
async fn notice(ws: &WsCtx, n: Notice) -> Result<Done> {
    Ok(match n {
        Notice::Paused => Done::new("paused", ""),
        Notice::Conflict { at } => {
            let git = conflict::git_id(ws, at).await?.to_string();
            let cs = ws.engine.get_changeset(at).await?;
            let first = cs.message.split(|&b| b == b'\n').next().unwrap_or_default();
            let subject: String = String::from_utf8_lossy(first)
                .chars()
                .filter(|c| !c.is_control())
                .take(MAX_SUBJECT)
                .collect();
            let short: String = git.chars().take(12).collect();
            let detail = format!("restack conflicts at {short} {subject}");
            crate::cli::live::record_conflict(
                &ws.home,
                &ws.workspace.ws.id.to_string(),
                &ws.workspace.ws.path,
                ws.workspace.ws.repo.0,
                &detail,
            )?;
            Done::new("notice", format!("{detail}; live updates paused"))
        }
        Notice::OverStack { steps, max } => Done::new(
            "notice",
            format!("{steps} changes to restack, more than asc.live.maxStack ({max})"),
        ),
        Notice::OverFiles { paths, max } => Done::new(
            "notice",
            format!("{paths} changed paths, more than asc.live.maxFiles ({max})"),
        ),
        Notice::SignFailed(why) => Done::new("notice", format!("signing: {why}")),
    })
}

/// How an applied plan ended.
struct Applied {
    rewrites: usize,
    moved: bool,
}

/// Makes a stored rewrite known to the rest of asc: generation and git ids of its trees.
async fn register(ws: &WsCtx, new: ChangesetId) -> Result<()> {
    let cs = ws.engine.get_changeset(new).await?;
    let gen_no = ws.engine.get_derived::<Gen>(new).await.map_err(fatal)?;
    ws.engine.warm(new).await.map_err(fatal)?;
    cs_gen::insert_many(
        &ws.db,
        ws.repo.key,
        &[cs_gen::CsGen {
            cs: new,
            gen_no: u64::from(gen_no.generation),
            parents: cs.parents.clone(),
        }],
    )
    .await?;
    let first = root_of(ws, cs.parents.first().copied()).await?;
    conflict::index_result(ws, first, cs.root).await
}

fn ref_name(text: &str) -> Result<RefName> {
    RefName::new(text.as_bytes().to_vec()).map_err(fatal)
}

/// Applies a restack: registers the rewrites, moves every following ref in ONE transaction (so a
/// stop or a failure leaves every ref where it was), moves the working copy and HEAD when `to`
/// is given and the gates still hold, then records the rewrites. The gates are read again just
/// before the move, because the restack took time.
async fn apply_plan(
    ctx: &Ctx,
    ws: &WsCtx,
    head: Option<ChangesetId>,
    head_branch: Option<&str>,
    apply: LiveApply,
    to: Option<ChangesetId>,
    op_id: uuid::Uuid,
) -> Result<Applied> {
    let pairs: Vec<(ChangesetId, ChangesetId)> = apply
        .rewrites
        .iter()
        .filter(|r| r.old != r.new)
        .map(|r| (r.old, r.new))
        .collect();
    for &(_, new) in &pairs {
        register(ws, new).await?;
    }
    let mut updates: Vec<RefUpdate> = apply.txn.map(|t| t.updates).unwrap_or_default();
    let mut staging = StagingAfterMove::Keep;
    let mut synced: Option<(TreeId, Vec<RepoPath>)> = None;
    let mut moved = false;

    if let (Some(to), Some(old_head)) = (to, head)
        && to != old_head
        && ws.head_commit().await? == Some(old_head)
    {
        let old_root = root_of(ws, Some(old_head)).await?;
        let new_root = root_of(ws, Some(to)).await?;
        let paths: Vec<RepoPath> = ws
            .engine
            .leafdiff(old_root, new_root, None)
            .await?
            .into_iter()
            .map(|(p, _)| p)
            .collect();
        let gates = worktree_state(ctx, ws, head, &paths).await?;
        if gates.move_block().is_none() {
            if !paths.is_empty() {
                // The commits came from the server; the files they write may not be here yet.
                crate::remote::hydrate_move(ctx, ws, old_root, new_root).await;
                sync_worktree(ctx, new_root, &paths, true).await?;
                synced = Some((old_root, paths));
                staging = StagingAfterMove::Set(Vec::new());
            }
            let name = match head_branch {
                Some(b) => ref_name(&format!("refs/heads/{b}"))?,
                None => RefName::new_onelevel(b"HEAD".to_vec()).map_err(fatal)?,
            };
            updates.push(RefUpdate {
                name,
                expect: Expect::Value(RefTarget::Changeset(old_head)),
                new: Some(RefTarget::Changeset(to)),
                expires_at: None,
            });
            moved = true;
        }
    }

    if !updates.is_empty() {
        let txn = RefTxn {
            updates,
            actor: actor(),
            reason: String::new(),
            op_id,
        };
        let reason = format!(
            "live: {} change{} restacked",
            pairs.len(),
            plural(pairs.len())
        );
        if let Err(e) = ctx.update_local_refs(txn, &reason, staging).await {
            if let Some((old_root, paths)) = synced {
                let _ = sync_worktree(ctx, old_root, &paths, true).await;
            }
            return Err(e);
        }
    }

    let recorder = Recorder::new(StateChanges::new(ws.db.clone(), ws.repo.key));
    for &(old, new) in &pairs {
        recorder
            .record(new, &[old], RewriteOp::Restack, change::OpId(op_id))
            .await
            .map_err(fatal)?;
        ext::rewrite_sink().record(new, &[old], RewriteOp::Restack, OpId(op_id));
        ext::index_sink()
            .on_commit(ws.workspace.ws.id, new)
            .await
            .map_err(fatal)?;
    }
    Ok(Applied {
        rewrites: pairs.len(),
        moved,
    })
}

#[cfg(test)]
mod tests;
