//! `push --land`, `push -o land` and AGit `refs/for/<b>` against an asc server (SPEC §8.5, §13.5,
//! §20.5).
//!
//! `push.rs` decides and uploads; the server's `UpdateRefs` answer is read here. A LAND answer
//! names the new tip of each ref and the changesets the server rewrote (`old → new`). The client
//! then brings the landed changesets in with the ordinary fetch (remote-tracking refs move with
//! it) and moves its own branches that pointed at a rewritten changeset, through
//! `Ctx::update_local_refs`. The checked-out branch moves only when the work tree has no local
//! change in the paths the land touched, as for a fast-forward `merge`; otherwise it stays and
//! a hint says so. Everything the server sends is bounded and parsed before it is used.

use std::collections::HashMap;

use asc_core::change::RewriteOp;
use asc_core::id::{ChangesetId, FileId, RefTarget};
use asc_core::object::Change;
use asc_core::path::RepoPath;
use asc_engine::merge::MergeOutcome;
use asc_state::state::stage::{self, StageSet};
use asc_store::refs::{Expect, RefTxn, RefUpdate, Refs};

use super::common::{CliError, Ctx, Result, StagingAfterMove, WsCtx};
use super::conflict::{self, Op};
use super::fetch::{Opts as FetchOpts, fetch_one};
use super::restore::{actor, root_of, sync_worktree};
use super::switch::ref_name;
use crate::ext;
use crate::remote::server::{PushIntent, UpdateOutcome};
use crate::remote::{self, Opened};

/// The push options other than `land`, and whether `land` was among them.
pub fn split_options(options: &[String]) -> (bool, Vec<String>) {
    let land = options.iter().any(|o| o == "land");
    let rest = options.iter().filter(|o| *o != "land").cloned().collect();
    (land, rest)
}

/// What a push armed on the server remote: `-o land` or `--land`, `-o <option>` pass-through.
pub fn intent(land: bool, force: bool, options: Vec<String>) -> PushIntent {
    PushIntent {
        force,
        land,
        no_content_merge: false,
        options,
    }
}

/// Whether `dst` is an AGit pull request ref (never stored, SPEC §2.7).
pub fn is_agit(dst: &str) -> bool {
    dst.starts_with("refs/for/")
}

/// What a LAND answered, by destination ref.
#[derive(Debug, Default)]
pub struct Landing {
    pub tips: HashMap<String, (Option<RefTarget>, Option<RefTarget>)>,
    pub rewritten: Vec<(ChangesetId, ChangesetId)>,
}

impl Landing {
    pub fn of(outcome: &UpdateOutcome) -> Self {
        Self {
            tips: outcome
                .results
                .iter()
                .map(|(name, old, new)| (name.clone(), (*old, *new)))
                .collect(),
            rewritten: outcome.rewritten.clone(),
        }
    }

    /// The tip the server left `dst` at.
    pub fn new_tip(&self, dst: &str) -> Option<RefTarget> {
        self.tips.get(dst).and_then(|(_, new)| *new)
    }

    /// The tip `dst` had before the land (the `T` of "rebased onto T").
    pub fn old_tip(&self, dst: &str) -> Option<RefTarget> {
        self.tips.get(dst).and_then(|(old, _)| *old)
    }

    pub fn rewrote(&self) -> bool {
        !self.rewritten.is_empty()
    }
}

/// Brings the landed changesets in and moves the local branches that pointed at a rewritten
/// changeset. Returns hints for the user.
pub async fn follow(
    ctx: &Ctx,
    ws: &WsCtx,
    opened: &Opened,
    landing: &Landing,
) -> Result<Vec<String>> {
    let mut notes = Vec::new();
    if landing.tips.is_empty() && landing.rewritten.is_empty() {
        return Ok(notes);
    }
    if opened.configured {
        // The ordinary fetch: changeset bodies, and the remote-tracking refs at the landed tips.
        let quiet = FetchOpts {
            quiet: true,
            ..FetchOpts::default()
        };
        drop(fetch_one(ctx, ws, &opened.name, &quiet, &[]).await?);
    }
    if landing.rewritten.is_empty() {
        return Ok(notes);
    }
    let moved: HashMap<ChangesetId, ChangesetId> = landing.rewritten.iter().copied().collect();
    let op = ext::OpId::new();
    for (old, new) in &landing.rewritten {
        ext::rewrite_sink().record(*new, &[*old], RewriteOp::Land, op);
    }
    let head_ref = match ws.projection.read_head()? {
        asc_git::projection::Head::Branch(b) => Some(format!("refs/heads/{b}")),
        _ => None,
    };
    let mut updates: Vec<RefUpdate> = Vec::new();
    let mut head_move: Option<(ChangesetId, ChangesetId)> = None;
    for entry in ws.db.refs().list(ws.repo.key, "refs/heads/").await? {
        let RefTarget::Changeset(tip) = entry.target else {
            continue;
        };
        let Some(&new) = moved.get(&tip) else {
            continue;
        };
        let name = String::from_utf8_lossy(entry.name.as_bytes()).into_owned();
        if head_ref.as_deref() == Some(name.as_str()) {
            head_move = Some((tip, new));
            continue;
        }
        updates.push(RefUpdate {
            name: ref_name(&name)?,
            expect: Expect::Value(RefTarget::Changeset(tip)),
            new: Some(RefTarget::Changeset(new)),
            expires_at: None,
        });
    }
    let mut head_plan: Option<(ChangesetId, ChangesetId, Vec<RepoPath>)> = None;
    if let (Some((old, new)), Some(head)) = (head_move, head_ref.as_deref()) {
        match head_is_movable(ctx, ws, old, new).await {
            Ok(touched) => head_plan = Some((old, new, touched)),
            Err(CliError::Raw(text, _)) => {
                let branch = head.strip_prefix("refs/heads/").unwrap_or(head);
                notes.push(format!(
                    "hint: the server landed {branch} but this work tree has local changes in files the land touched, so the branch stays where it was.
hint: commit or stash, then run 'asc reset --hard {}/{branch}' or 'asc pull --rebase'.
{}",
                    opened.name,
                    text.lines().next().unwrap_or("")
                ));
            }
            Err(e) => return Err(e),
        }
    }
    if !updates.is_empty() {
        let txn = RefTxn {
            updates,
            actor: actor(),
            reason: String::new(),
            op_id: op.0,
        };
        ctx.update_local_refs(txn, "push: land", StagingAfterMove::Keep)
            .await?;
    }
    if let Some((old, new, touched)) = head_plan {
        // As a fast-forward `merge` does: index, work tree, then the branch with the staged
        // changes at other paths kept (the precheck refused any overlap).
        let (from, to) = (root_of(ws, Some(old)).await?, root_of(ws, Some(new)).await?);
        // The work tree is written from the local tiers: fetch what the land brought in.
        let files: Vec<(FileId, u64)> = ws
            .engine
            .leafdiff(from, to, None)
            .await?
            .into_iter()
            .filter_map(|(_, c)| match c {
                Change::Write { file, size, .. } => Some((file, size)),
                _ => None,
            })
            .collect();
        remote::hydrate_files(ctx, ws, &*opened.remote, &files).await?;
        conflict::index_result(ws, from, to).await?;
        sync_worktree(ctx, to, &touched, true).await?;
        let kept = stage::overlay(&ws.db, ws.workspace.ws.id, StageSet::Default).await?;
        conflict::move_head(
            ctx,
            ws,
            Some(old),
            new,
            "push: land",
            StagingAfterMove::Set(kept),
            None,
        )
        .await?;
    }
    Ok(notes)
}

/// The paths a move of HEAD from `old` to `new` touches, when no local change overlaps them.
async fn head_is_movable(
    ctx: &Ctx,
    ws: &WsCtx,
    old: ChangesetId,
    new: ChangesetId,
) -> Result<Vec<RepoPath>> {
    let (from, to) = (root_of(ws, Some(old)).await?, root_of(ws, Some(new)).await?);
    let outcome = MergeOutcome {
        root: to,
        conflicts: Vec::new(),
    };
    conflict::precheck(ctx, ws, Op::FastForward, from, &outcome, false).await?;
    Ok(ws
        .engine
        .leafdiff(from, to, None)
        .await?
        .into_iter()
        .map(|(p, _)| p)
        .collect())
}

/// The text after a pushed ref on a LAND summary line.
pub fn annotation(landing: &Landing, dst: &str, onto: &str) -> String {
    if landing.rewrote() {
        let _ = dst;
        format!("landed: rebased onto {onto}")
    } else {
        "landed".to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn land_is_taken_out_of_the_options() {
        let (land, rest) = split_options(&["topic=x".into(), "land".into(), "title=y".into()]);
        assert!(land);
        assert_eq!(rest, ["topic=x", "title=y"]);
        let (land, rest) = split_options(&[]);
        assert!(!land && rest.is_empty());
    }

    #[test]
    fn agit_refs_are_recognised() {
        assert!(is_agit("refs/for/master/asc-abc"));
        assert!(!is_agit("refs/heads/for/master"));
    }

    #[test]
    fn annotation_names_the_rebase() {
        let mut l = Landing::default();
        assert_eq!(annotation(&l, "refs/heads/master", "abc"), "landed");
        l.rewritten.push((
            ChangesetId::from_bytes([1; 32]),
            ChangesetId::from_bytes([2; 32]),
        ));
        assert_eq!(
            annotation(&l, "refs/heads/master", "abc"),
            "landed: rebased onto abc"
        );
    }
}
