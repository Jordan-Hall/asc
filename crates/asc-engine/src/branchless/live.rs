//! The live restack and update planner (SPEC §21.3.8).
//!
//! [`plan_live`] takes plain inputs (the trunk tip, the user's visible own drafts, the worktree
//! state and the `asc.live.*` config) and returns a [`LivePlan`]: fetch only, restack, restack and
//! move, or a [`Notice`]. Restacks run in memory through a [`Restacker`] (C06) and are returned
//! only when every one of them is conflict-free; a conflicting, over-cap or paused run
//! restacks nothing and returns a notice. The working copy is moved only when no safety gate
//! blocks it.
//!
//! The planner moves no ref and runs no hook. The caller applies [`LiveApply::txn`] through
//! `Ctx::update_local_refs` as one op-log entry of kind [`OP_KIND`] (C03/C04), under the
//! worktree's asc index lock, so `asc undo` reverts it (C15). Other users' drafts are ignored.
//! Every bound is explicit: stacks, steps and changed paths are capped by `asc.live.*`.

use asc_core::id::ChangesetId;
use asc_store::objects::Objects;
use asc_store::refs::{Actor, RefTxn, Refs};

use crate::Engine;
use crate::merge::MergeOptions;
use crate::stack::restack::{
    self, CommitSigner, Outcome, Pause, RefMove, RestackPlan, Rewrite, TxnMeta,
};

/// The op-log kind of an automatic live change.
pub const OP_KIND: &str = "live";
/// Default `asc.live.maxStack`.
pub const DEFAULT_MAX_STACK: usize = 50;
/// Default `asc.live.maxFiles`.
pub const DEFAULT_MAX_FILES: usize = 5000;
/// Hard bound on the stacks one plan accepts, whatever the config says.
pub const MAX_STACKS: usize = 1024;

/// Failures that are not a notice.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Restack(#[from] restack::Error),
    #[error("{0} exceeds the limit of {1}")]
    TooLarge(&'static str, usize),
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

/// The `asc.live.*` settings. Each key implies the ones above it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LiveConfig {
    pub fetch: bool,
    pub restack: bool,
    pub update: bool,
    pub max_stack: usize,
    pub max_files: usize,
}

impl Default for LiveConfig {
    fn default() -> Self {
        Self {
            fetch: true,
            restack: false,
            update: false,
            max_stack: DEFAULT_MAX_STACK,
            max_files: DEFAULT_MAX_FILES,
        }
    }
}

impl LiveConfig {
    /// `update` implies `restack`, which implies `fetch`.
    pub fn effective(mut self) -> Self {
        self.restack |= self.update;
        self.fetch |= self.restack;
        self
    }
}

/// An operation that blocks the move while it is in progress.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InProgress {
    Rebase,
    Merge,
    CherryPick,
    Bisect,
}

/// What the caller read from the worktree, under the asc index lock when it is held by us.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct WorktreeState {
    /// A tracked file differs from the checked-out commit.
    pub dirty: bool,
    /// An untracked file would be overwritten by the move.
    pub colliding_untracked: bool,
    pub operation: Option<InProgress>,
    pub unresolved_conflicts: bool,
    /// Another process holds the asc index lock.
    pub index_lock_held: bool,
    /// `asc live pause` or an earlier live conflict paused this worktree.
    pub paused: bool,
}

/// Why the working copy was not moved.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MoveBlock {
    Dirty,
    CollidingUntracked,
    Operation(InProgress),
    UnresolvedConflicts,
    IndexLocked,
}

impl WorktreeState {
    /// The first gate that blocks a move, or `None` when the move is safe.
    pub fn move_block(&self) -> Option<MoveBlock> {
        if self.index_lock_held {
            Some(MoveBlock::IndexLocked)
        } else if let Some(op) = self.operation {
            Some(MoveBlock::Operation(op))
        } else if self.unresolved_conflicts {
            Some(MoveBlock::UnresolvedConflicts)
        } else if self.dirty {
            Some(MoveBlock::Dirty)
        } else if self.colliding_untracked {
            Some(MoveBlock::CollidingUntracked)
        } else {
            None
        }
    }
}

/// One draft stack to consider.
#[derive(Clone, Debug)]
pub struct DraftStack {
    /// Authored by the configured user. Other users' drafts are ignored.
    pub own: bool,
    /// The commit the stack sits on now.
    pub base: ChangesetId,
    /// First-parent chain, oldest first.
    pub steps: Vec<ChangesetId>,
    /// Paths changed by the stack and by the trunk move under it.
    pub changed_paths: usize,
}

/// Plain inputs of [`plan_live`].
#[derive(Clone, Debug)]
pub struct LiveInputs {
    pub trunk_tip: ChangesetId,
    /// The change checked out in this worktree.
    pub checked_out: Option<ChangesetId>,
    /// The checkout is on trunk itself, not on a draft.
    pub on_trunk: bool,
    pub stacks: Vec<DraftStack>,
    pub worktree: WorktreeState,
    pub config: LiveConfig,
}

/// Why nothing was applied; shown by `asc status`, `asc sl` and the IDE plugins.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Notice {
    /// Live mode is paused for this worktree.
    Paused,
    /// A restack conflicts at `at`; nothing was restacked.
    Conflict {
        at: ChangesetId,
    },
    OverStack {
        steps: usize,
        max: usize,
    },
    OverFiles {
        paths: usize,
        max: usize,
    },
    /// The signer failed; nothing was restacked.
    SignFailed(String),
}

/// What one stack's restack produced.
#[derive(Debug)]
pub struct StackDone {
    pub rewrites: Vec<Rewrite>,
    pub txn: Option<RefTxn>,
}

/// Restacks one stack in memory. Moves no ref.
///
/// The futures carry no `Send` bound here, so the engine adapter compiles; the bound is
/// inferred at each concrete use.
#[allow(async_fn_in_trait)]
pub trait Restacker {
    async fn restack(
        &self,
        stack: &DraftStack,
        onto: ChangesetId,
    ) -> Result<std::result::Result<StackDone, Notice>>;
}

/// The refs and rewrites of an applied plan.
#[derive(Debug)]
pub struct LiveApply {
    pub rewrites: Vec<Rewrite>,
    /// One transaction for every stack; apply it as a single op.
    pub txn: Option<RefTxn>,
}

impl LiveApply {
    /// The op-log kind to record.
    pub fn op_kind(&self) -> &'static str {
        OP_KIND
    }

    /// `(new, old)` pairs that undo the rewrites.
    pub fn inverse(&self) -> Vec<(ChangesetId, ChangesetId)> {
        self.rewrites
            .iter()
            .filter(|r| r.old != r.new)
            .map(|r| (r.new, r.old))
            .collect()
    }
}

/// The plan for one wake-up of one worktree.
#[derive(Debug)]
pub enum LivePlan {
    /// Fetch only: nothing to restack or move, or restack is off.
    FetchOnly,
    /// Restack; the working copy stays (`move_blocked` says why, `None` when update is off).
    Restack {
        apply: LiveApply,
        move_blocked: Option<MoveBlock>,
    },
    /// Restack (possibly nothing) and move the working copy to `to`.
    RestackAndMove { apply: LiveApply, to: ChangesetId },
    /// Nothing applied.
    Notice(Notice),
}

fn merge_txns(txns: Vec<RefTxn>) -> Option<RefTxn> {
    let mut it = txns.into_iter();
    let mut first = it.next()?;
    for t in it {
        first.updates.extend(t.updates);
    }
    Some(first)
}

/// Plans one live update.
pub async fn plan_live<R: Restacker>(inputs: &LiveInputs, restacker: &R) -> Result<LivePlan> {
    let cfg = inputs.config.effective();
    if !cfg.restack {
        return Ok(LivePlan::FetchOnly);
    }
    if inputs.stacks.len() > MAX_STACKS {
        return Err(Error::TooLarge("stacks", MAX_STACKS));
    }
    let todo: Vec<&DraftStack> = inputs
        .stacks
        .iter()
        .filter(|s| s.own && !s.steps.is_empty() && s.base != inputs.trunk_tip)
        .collect();
    let move_trunk = cfg.update && inputs.on_trunk && inputs.checked_out != Some(inputs.trunk_tip);
    if todo.is_empty() && !move_trunk {
        return Ok(LivePlan::FetchOnly);
    }
    if inputs.worktree.paused {
        return Ok(LivePlan::Notice(Notice::Paused));
    }
    let steps = todo
        .iter()
        .try_fold(0usize, |a, s| a.checked_add(s.steps.len()))
        .unwrap_or(usize::MAX);
    if steps > cfg.max_stack {
        return Ok(LivePlan::Notice(Notice::OverStack {
            steps,
            max: cfg.max_stack,
        }));
    }
    let paths = todo
        .iter()
        .try_fold(0usize, |a, s| a.checked_add(s.changed_paths))
        .unwrap_or(usize::MAX);
    if paths > cfg.max_files {
        return Ok(LivePlan::Notice(Notice::OverFiles {
            paths,
            max: cfg.max_files,
        }));
    }
    let mut rewrites = Vec::new();
    let mut txns = Vec::new();
    for s in todo {
        match restacker.restack(s, inputs.trunk_tip).await? {
            Ok(done) => {
                rewrites.extend(done.rewrites);
                txns.extend(done.txn);
            }
            // One conflict applies nothing: rewrites already stored stay unreferenced.
            Err(notice) => return Ok(LivePlan::Notice(notice)),
        }
    }
    let target = if !cfg.update {
        None
    } else if inputs.on_trunk {
        Some(inputs.trunk_tip)
    } else {
        inputs.checked_out.and_then(|old| {
            rewrites
                .iter()
                .find(|r| r.old == old && r.new != old)
                .map(|r| r.new)
        })
    };
    let apply = LiveApply {
        rewrites,
        txn: merge_txns(txns),
    };
    Ok(match (target, inputs.worktree.move_block()) {
        (Some(to), None) => LivePlan::RestackAndMove { apply, to },
        (Some(_), Some(block)) => LivePlan::Restack {
            apply,
            move_blocked: Some(block),
        },
        (None, _) => LivePlan::Restack {
            apply,
            move_blocked: None,
        },
    })
}

/// [`Restacker`] over the engine's in-memory restack. Runs no hook.
pub struct EngineRestacker<'a, O: Objects, R: Refs> {
    pub engine: &'a Engine<O, R>,
    pub opts: MergeOptions,
    pub actor: Actor,
    pub op_id: uuid::Uuid,
    pub signer: Option<Box<dyn CommitSigner>>,
    /// Refs that follow their change when it is rewritten.
    pub moves: Vec<RefMove>,
}

impl<O: Objects, R: Refs> Restacker for EngineRestacker<'_, O, R> {
    async fn restack(
        &self,
        stack: &DraftStack,
        onto: ChangesetId,
    ) -> Result<std::result::Result<StackDone, Notice>> {
        let plan: RestackPlan = self.engine.plan_restack(&stack.steps, onto).await?;
        let meta = TxnMeta {
            actor: self.actor.clone(),
            reason: OP_KIND.into(),
            op_id: self.op_id,
        };
        let signer = self.signer.as_deref();
        let out = self
            .engine
            .execute_restack(&plan, &self.moves, signer, &self.opts, &meta)
            .await?;
        Ok(match out {
            Outcome::Complete { rewrites, txn } => Ok(StackDone { rewrites, txn }),
            Outcome::Paused { at, cause, .. } => Err(match cause {
                Pause::Conflicts(_) => Notice::Conflict { at },
                Pause::Sign(m) => Notice::SignFailed(m),
            }),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::merge::testutil::{child, commit, engine, files};

    struct Fix {
        e: crate::merge::testutil::TestEngine,
        stack: DraftStack,
        trunk: ChangesetId,
    }

    /// Base with a, b; trunk edits `trunk_file`; a one-change stack edits `stack_file`.
    async fn fix(trunk_file: &str, stack_file: &str) -> Fix {
        let e = engine();
        let root = files(&e, &[("a", "1\n"), ("b", "1\n")]).await;
        let base = commit(&e, &[], root, "base").await;
        let trunk = child(&e, base, &[(trunk_file, "trunk\n")], vec![]).await;
        let d = child(&e, base, &[(stack_file, "mine\n")], vec![]).await;
        Fix {
            e,
            stack: DraftStack {
                own: true,
                base,
                steps: vec![d],
                changed_paths: 1,
            },
            trunk,
        }
    }

    fn inputs(f: &Fix, update: bool) -> LiveInputs {
        LiveInputs {
            trunk_tip: f.trunk,
            checked_out: f.stack.steps.last().copied(),
            on_trunk: false,
            stacks: vec![f.stack.clone()],
            worktree: WorktreeState::default(),
            config: LiveConfig {
                restack: true,
                update,
                ..LiveConfig::default()
            },
        }
    }

    async fn run(f: &Fix, i: &LiveInputs) -> LivePlan {
        let r = EngineRestacker {
            engine: &f.e,
            opts: MergeOptions::default(),
            actor: Actor {
                id: "u".into(),
                via: "daemon".into(),
            },
            op_id: uuid::Uuid::nil(),
            signer: None,
            moves: Vec::new(),
        };
        plan_live(i, &r).await.unwrap()
    }

    /// Counts calls; restacks to fixed results.
    struct Counting(std::sync::atomic::AtomicUsize);
    impl Restacker for Counting {
        async fn restack(
            &self,
            _: &DraftStack,
            _: ChangesetId,
        ) -> Result<std::result::Result<StackDone, Notice>> {
            self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(Ok(StackDone {
                rewrites: Vec::new(),
                txn: None,
            }))
        }
    }

    #[tokio::test]
    async fn clean_restack_and_move_is_one_live_entry_that_undo_inverts() {
        let f = fix("a", "b").await;
        let LivePlan::RestackAndMove { apply, to } = run(&f, &inputs(&f, true)).await else {
            panic!("expected restack and move");
        };
        assert_eq!(apply.op_kind(), "live");
        let old = f.stack.steps[0];
        assert_eq!(apply.rewrites.len(), 1);
        assert_eq!(apply.rewrites[0].new, to);
        assert_eq!(apply.inverse(), vec![(to, old)]);
    }

    #[tokio::test]
    async fn restack_only_never_moves() {
        let f = fix("a", "b").await;
        let plan = run(&f, &inputs(&f, false)).await;
        assert!(matches!(
            plan,
            LivePlan::Restack {
                move_blocked: None,
                ..
            }
        ));
    }

    #[tokio::test]
    async fn fetch_only_by_default() {
        let f = fix("a", "b").await;
        let mut i = inputs(&f, false);
        i.config = LiveConfig::default();
        assert!(matches!(run(&f, &i).await, LivePlan::FetchOnly));
    }

    #[tokio::test]
    async fn a_dirty_tree_is_never_moved() {
        let f = fix("a", "b").await;
        for (dirty, untracked, block) in [
            (true, false, MoveBlock::Dirty),
            (false, true, MoveBlock::CollidingUntracked),
        ] {
            let mut i = inputs(&f, true);
            i.worktree.dirty = dirty;
            i.worktree.colliding_untracked = untracked;
            match run(&f, &i).await {
                LivePlan::Restack { move_blocked, .. } => assert_eq!(move_blocked, Some(block)),
                other => panic!("moved or noticed: {other:?}"),
            }
        }
    }

    #[tokio::test]
    async fn each_operation_and_gate_blocks_the_move() {
        let f = fix("a", "b").await;
        for op in [
            InProgress::Rebase,
            InProgress::Merge,
            InProgress::CherryPick,
            InProgress::Bisect,
        ] {
            let mut i = inputs(&f, true);
            i.worktree.operation = Some(op);
            assert!(matches!(
                run(&f, &i).await,
                LivePlan::Restack {
                    move_blocked: Some(MoveBlock::Operation(o)),
                    ..
                } if o == op
            ));
        }
        let mut i = inputs(&f, true);
        i.worktree.unresolved_conflicts = true;
        assert!(matches!(
            run(&f, &i).await,
            LivePlan::Restack {
                move_blocked: Some(MoveBlock::UnresolvedConflicts),
                ..
            }
        ));
        let mut i = inputs(&f, true);
        i.worktree.index_lock_held = true;
        assert!(matches!(
            run(&f, &i).await,
            LivePlan::Restack {
                move_blocked: Some(MoveBlock::IndexLocked),
                ..
            }
        ));
    }

    #[tokio::test]
    async fn on_trunk_moves_to_the_tip_unless_blocked() {
        let f = fix("a", "b").await;
        let mut i = inputs(&f, true);
        i.stacks.clear();
        i.on_trunk = true;
        i.checked_out = Some(f.stack.base);
        match run(&f, &i).await {
            LivePlan::RestackAndMove { to, apply } => {
                assert_eq!(to, f.trunk);
                assert!(apply.rewrites.is_empty());
            }
            other => panic!("{other:?}"),
        }
        i.worktree.dirty = true;
        assert!(matches!(
            run(&f, &i).await,
            LivePlan::Restack {
                move_blocked: Some(MoveBlock::Dirty),
                ..
            }
        ));
    }

    #[tokio::test]
    async fn other_users_drafts_are_ignored() {
        let f = fix("a", "b").await;
        let mut i = inputs(&f, true);
        i.stacks[0].own = false;
        i.checked_out = None;
        let c = Counting(Default::default());
        assert!(matches!(
            plan_live(&i, &c).await.unwrap(),
            LivePlan::FetchOnly
        ));
        assert_eq!(c.0.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn a_conflicting_plan_restacks_nothing_and_returns_a_notice() {
        let f = fix("a", "a").await;
        let i = inputs(&f, true);
        match run(&f, &i).await {
            LivePlan::Notice(Notice::Conflict { at }) => assert_eq!(at, f.stack.steps[0]),
            other => panic!("{other:?}"),
        }
    }

    #[tokio::test]
    async fn over_cap_input_returns_a_notice_without_restacking() {
        let f = fix("a", "b").await;
        let c = Counting(Default::default());
        let mut i = inputs(&f, true);
        i.config.max_stack = 0;
        assert!(matches!(
            plan_live(&i, &c).await.unwrap(),
            LivePlan::Notice(Notice::OverStack { steps: 1, max: 0 })
        ));
        let mut i = inputs(&f, true);
        i.config.max_files = 0;
        assert!(matches!(
            plan_live(&i, &c).await.unwrap(),
            LivePlan::Notice(Notice::OverFiles { paths: 1, max: 0 })
        ));
        assert_eq!(c.0.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn a_paused_worktree_gets_a_notice() {
        let f = fix("a", "b").await;
        let mut i = inputs(&f, true);
        i.worktree.paused = true;
        assert!(matches!(
            run(&f, &i).await,
            LivePlan::Notice(Notice::Paused)
        ));
    }
}
