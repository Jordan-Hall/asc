//! `DraftHeadSink` (SPEC §21.2.2, B03): keeps `draft_heads` and `cloud_queue` current for every
//! changeset a command writes. C04's sink calls it right after `RewriteSink::record`. It runs in
//! both modes (`asc.branchless` on or off), prints nothing and never fails a command.
//!
//! The maintenance rows change in one transaction of their own, opened after the change-index
//! row is written: `Recorder` commits its own transaction, so the two cannot share one without
//! changing the engine's `ChangeRows` contract.

use asc_core::change::{ChangeId, RewriteOp};
use asc_core::id::ChangesetId;
use asc_state::drafts::{self, Drafts};
use asc_state::state::StateDb;
use asc_store::refs::RepoKey;

use crate::cli::common::Ctx;

/// Key of the queue bound (`asc.cloud.queueMax`).
const QUEUE_MAX_KEY: &str = "asc.cloud.queueMax";

/// Unix seconds.
fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

/// The queue bound: `asc.cloud.queueMax`, clamped to `1..=QUEUE_MAX`.
fn queue_max(ctx: &Ctx) -> usize {
    ctx.config
        .get(QUEUE_MAX_KEY)
        .and_then(|v| v.trim().parse::<usize>().ok())
        .map_or(drafts::QUEUE_MAX, |n| n.clamp(1, drafts::QUEUE_MAX))
}

/// Records the draft-head effect of one `RewriteSink::record` call.
///
/// `change` is the change id the recorder settled on for `new`. Errors are swallowed: the
/// command has already succeeded and the sink is invisible.
pub async fn record(
    ctx: &Ctx,
    new: ChangesetId,
    change: ChangeId,
    preds: &[ChangesetId],
    kind: RewriteOp,
) {
    let Ok(ws) = ctx.ws() else { return };
    let Ok(cs) = ws.engine.get_changeset(new).await else {
        return;
    };
    let _ = apply(
        &ws.db,
        ws.repo.key,
        (new, change),
        &cs.parents,
        preds,
        kind,
        queue_max(ctx),
    )
    .await;
}

/// Applies the §21.2.2 maintenance table for one new or rewritten changeset.
pub async fn apply(
    db: &StateDb,
    repo: RepoKey,
    (new, change): (ChangesetId, ChangeId),
    parents: &[ChangesetId],
    preds: &[ChangesetId],
    kind: RewriteOp,
    queue_max: usize,
) -> drafts::Result<()> {
    let d = Drafts::new(repo);
    let at = now();
    let mut tx = db.begin().await?;
    match kind {
        // A fetched or imported commit lives on a trunk or a remote ref: not a draft.
        RewriteOp::Import => {}
        // A landed commit is immutable at once, and so are the drafts it replaces.
        RewriteOp::Land => {
            let mut all = preds.to_vec();
            all.push(new);
            d.immutable(&mut tx, &all).await?;
        }
        RewriteOp::New => {
            d.new_changeset(&mut tx, new, change, parents, at, queue_max)
                .await?;
        }
        RewriteOp::Amend
        | RewriteOp::Rebase
        | RewriteOp::Absorb
        | RewriteOp::Restack
        | RewriteOp::Fold
        | RewriteOp::Squash
        | RewriteOp::Split => match preds.split_first() {
            Some((first, rest)) => {
                // Folded predecessors leave the tables with the first one.
                d.immutable(&mut tx, rest).await?;
                d.rewrite(&mut tx, *first, (new, change), parents, at, queue_max)
                    .await?;
            }
            None => {
                d.new_changeset(&mut tx, new, change, parents, at, queue_max)
                    .await?;
            }
        },
    }
    tx.commit().await.map_err(drafts::DraftsError::from)?;
    Ok(())
}
