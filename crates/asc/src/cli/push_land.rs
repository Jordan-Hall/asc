//! `push --land`, `push -o land` and AGit `refs/for/<b>` against an asc server (SPEC §8.5, §13.5,
//! §20.5).
//!
//! `push.rs` decides and uploads; the server's `UpdateRefs` answer is read here. A LAND answer
//! names the new tip of each ref and the changesets the server rewrote (`old → new`). The client
//! then brings the landed changesets in with the ordinary fetch (remote-tracking refs move with
//! it) and moves its own branches that pointed at a rewritten changeset, through
//! `Ctx::update_local_refs`. The checked-out branch moves only when the work tree has no local
//! change in the paths the land touched, as for a fast-forward `merge`; otherwise it stays and
//! a hint says so. Everything the server sends is bounded and parsed before it is used, and the
//! rewrite list is checked before it moves anything (see [`verify`]): the server may name only
//! changesets this push uploaded, and each replacement must lie under the tip it reported.

use std::collections::HashMap;
use std::future::Future;

use asc_core::change::RewriteOp;
use asc_core::id::{ChangesetId, FileId, RefTarget};
use asc_core::object::Change;
use asc_core::path::RepoPath;
use asc_engine::history::HistoryApi;
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

/// A ref this push updated.
#[derive(Clone, Debug)]
pub struct Pushed {
    /// The local branch (`refs/heads/x`) the pushed value came from, when it was one.
    pub src: Option<String>,
    /// The remote ref that was updated.
    pub dst: String,
    /// The remote ref's tip before the push.
    pub old: Option<RefTarget>,
    /// The tip this push sent.
    pub new: Option<RefTarget>,
}

/// The rewrites that passed [`verify`], per pushed ref (same order as the `pushed` slice).
#[derive(Debug, Default)]
pub struct Verified {
    pub per_push: Vec<HashMap<ChangesetId, ChangesetId>>,
    /// One warning per dropped pair.
    pub notes: Vec<String>,
}

impl Verified {
    /// Every verified pair.
    pub fn all(&self) -> HashMap<ChangesetId, ChangesetId> {
        self.per_push
            .iter()
            .flatten()
            .map(|(o, n)| (*o, *n))
            .collect()
    }
}

/// Keeps the pairs of `landing.rewritten` a push can explain. `old` must be a changeset the
/// push uploaded for that ref (reachable from the pushed tip, not from the ref's old tip), and
/// `new` must be reachable from the tip the server reported for the ref. `ancestor(a, b)` says
/// whether `a` is `b` or an ancestor of `b`, from local bodies only. Anything else is dropped
/// with a warning and never moves a ref.
pub async fn verify<F, Fut>(landing: &Landing, pushed: &[Pushed], ancestor: F) -> Verified
where
    F: Fn(ChangesetId, ChangesetId) -> Fut,
    Fut: Future<Output = bool>,
{
    let mut out = Verified {
        per_push: vec![HashMap::new(); pushed.len()],
        notes: Vec::new(),
    };
    for (old, new) in &landing.rewritten {
        let mut unreachable = false;
        let mut kept = false;
        for (i, p) in pushed.iter().enumerate() {
            let Some(RefTarget::Changeset(sent)) = p.new else {
                continue;
            };
            // Uploaded by this push: under the pushed tip, not already under the ref's old one.
            if !ancestor(*old, sent).await {
                continue;
            }
            if let Some(RefTarget::Changeset(before)) = p.old
                && ancestor(*old, before).await
            {
                continue;
            }
            let reaches = match landing.new_tip(&p.dst) {
                Some(RefTarget::Changeset(tip)) => ancestor(*new, tip).await,
                _ => false,
            };
            if reaches {
                out.per_push[i].insert(*old, *new);
                kept = true;
            } else {
                unreachable = true;
            }
        }
        if !kept {
            let why = if unreachable {
                "the replacement is not under the landed tip"
            } else {
                "the original is not part of this push"
            };
            out.notes.push(format!(
                "warning: ignoring a rewrite the server reported ({old} -> {new}): {why}"
            ));
        }
    }
    out
}

/// The local branches to move and where: a branch moves only when it was pushed (its own
/// verified pairs apply) or is HEAD's branch (any verified pair applies), and its tip is the
/// `old` of such a pair. `branches` are `(refs/heads/x, tip)`.
pub fn select(
    branches: &[(String, ChangesetId)],
    head_ref: Option<&str>,
    pushed: &[Pushed],
    verified: &Verified,
) -> Vec<(String, ChangesetId, ChangesetId)> {
    let mut moves = Vec::new();
    for (name, tip) in branches {
        let own: Vec<usize> = pushed
            .iter()
            .enumerate()
            .filter(|(_, p)| p.src.as_deref() == Some(name.as_str()))
            .map(|(i, _)| i)
            .collect();
        let new = if !own.is_empty() {
            own.iter().find_map(|&i| verified.per_push[i].get(tip))
        } else if head_ref == Some(name.as_str()) {
            verified.per_push.iter().find_map(|m| m.get(tip))
        } else {
            None
        };
        if let Some(&new) = new {
            moves.push((name.clone(), *tip, new));
        }
    }
    moves
}

/// Brings the landed changesets in and moves the local branches that pointed at a rewritten
/// changeset. Returns hints for the user.
pub async fn follow(
    ctx: &Ctx,
    ws: &WsCtx,
    opened: &Opened,
    landing: &Landing,
    pushed: &[Pushed],
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
    // The fetch above brought in the landed tips; the engine answers from local bodies only.
    let engine = &ws.engine;
    let verified = verify(landing, pushed, |a, b| async move {
        a == b || HistoryApi::is_ancestor(engine, a, b).await.unwrap_or(false)
    })
    .await;
    notes.extend(verified.notes.iter().cloned());
    let moved = verified.all();
    if moved.is_empty() {
        return Ok(notes);
    }
    let op = ext::OpId::new();
    for (old, new) in &moved {
        ext::rewrite_sink().record(*new, &[*old], RewriteOp::Land, op);
    }
    let head_ref = match ws.projection.read_head()? {
        asc_git::projection::Head::Branch(b) => Some(format!("refs/heads/{b}")),
        _ => None,
    };
    let mut branches: Vec<(String, ChangesetId)> = Vec::new();
    for entry in ws.db.refs().list(ws.repo.key, "refs/heads/").await? {
        if let RefTarget::Changeset(tip) = entry.target {
            branches.push((
                String::from_utf8_lossy(entry.name.as_bytes()).into_owned(),
                tip,
            ));
        }
    }
    let mut updates: Vec<RefUpdate> = Vec::new();
    let mut head_move: Option<(ChangesetId, ChangesetId)> = None;
    for (name, tip, new) in select(&branches, head_ref.as_deref(), pushed, &verified) {
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

    fn id(n: u8) -> ChangesetId {
        ChangesetId::from_bytes([n; 32])
    }

    fn target(n: u8) -> Option<RefTarget> {
        Some(RefTarget::Changeset(id(n)))
    }

    /// A history by parent: 1 <- 2 <- 3 (the pushed work), 1 <- 10 <- 11 (what the server
    /// rebased onto), 11 <- 12 <- 13 (the landed copies of 2 and 3), and 50 <- 51 (unrelated).
    fn parents(n: u8) -> &'static [u8] {
        match n {
            2 => &[1],
            3 => &[2],
            10 => &[1],
            11 => &[10],
            12 => &[11],
            13 => &[12],
            51 => &[50],
            _ => &[],
        }
    }

    fn ancestor(a: ChangesetId, b: ChangesetId) -> std::future::Ready<bool> {
        let (a, b) = (a.as_bytes()[0], b.as_bytes()[0]);
        let mut stack = vec![b];
        while let Some(n) = stack.pop() {
            if n == a {
                return std::future::ready(true);
            }
            stack.extend_from_slice(parents(n));
        }
        std::future::ready(false)
    }

    fn landing(rewritten: &[(u8, u8)], tip: u8) -> Landing {
        let mut l = Landing::default();
        l.tips
            .insert("refs/heads/main".to_owned(), (target(1), target(tip)));
        l.rewritten = rewritten.iter().map(|&(o, n)| (id(o), id(n))).collect();
        l
    }

    /// `feature` was pushed to `main` (old tip 1, new tip 3).
    fn pushed() -> Vec<Pushed> {
        vec![Pushed {
            src: Some("refs/heads/feature".to_owned()),
            dst: "refs/heads/main".to_owned(),
            old: target(1),
            new: target(3),
        }]
    }

    fn branches(tips: &[(&str, u8)]) -> Vec<(String, ChangesetId)> {
        tips.iter()
            .map(|(n, t)| (format!("refs/heads/{n}"), id(*t)))
            .collect()
    }

    #[tokio::test]
    async fn the_normal_rewrite_moves_the_pushed_branch() {
        let l = landing(&[(2, 12), (3, 13)], 13);
        let v = verify(&l, &pushed(), ancestor).await;
        assert!(v.notes.is_empty(), "{:?}", v.notes);
        let moves = select(&branches(&[("feature", 3)]), None, &pushed(), &v);
        assert_eq!(moves, [("refs/heads/feature".to_owned(), id(3), id(13))]);
    }

    #[tokio::test]
    async fn a_rewrite_of_an_unrelated_changeset_moves_nothing() {
        // The server claims 51 (an unrelated branch's tip) became 13, and 1 (the old remote tip).
        let l = landing(&[(51, 13), (1, 13)], 13);
        let v = verify(&l, &pushed(), ancestor).await;
        assert!(v.all().is_empty());
        assert_eq!(v.notes.len(), 2, "{:?}", v.notes);
        let local = branches(&[("feature", 3), ("other", 51), ("main", 1)]);
        assert!(select(&local, Some("refs/heads/main"), &pushed(), &v).is_empty());
    }

    #[tokio::test]
    async fn a_replacement_outside_the_landed_tip_is_refused() {
        // 51 exists locally but is not under the tip the server reported for the ref.
        let l = landing(&[(3, 51), (2, 12)], 13);
        let v = verify(&l, &pushed(), ancestor).await;
        assert_eq!(v.all().len(), 1);
        assert_eq!(v.notes.len(), 1);
        assert!(v.notes[0].contains("landed tip"), "{:?}", v.notes);
        let moves = select(&branches(&[("feature", 3)]), None, &pushed(), &v);
        assert!(moves.is_empty(), "feature stays at 3: {moves:?}");
        // A landed tip the client does not have vouches for nothing.
        let mut none = landing(&[(2, 12)], 13);
        none.tips.clear();
        assert!(verify(&none, &pushed(), ancestor).await.all().is_empty());
    }

    #[tokio::test]
    async fn only_pushed_branches_and_heads_branch_move() {
        let l = landing(&[(3, 13)], 13);
        let v = verify(&l, &pushed(), ancestor).await;
        // `copy` and `topic` point at the rewritten tip but were not pushed.
        let local = branches(&[("feature", 3), ("copy", 3), ("topic", 3)]);
        let moves = select(&local, None, &pushed(), &v);
        assert_eq!(moves, [("refs/heads/feature".to_owned(), id(3), id(13))]);
        // HEAD's branch moves too when it is at a verified `old`.
        let moves = select(&local, Some("refs/heads/topic"), &pushed(), &v);
        assert_eq!(moves.len(), 2);
        assert!(moves.iter().any(|(n, ..)| n == "refs/heads/topic"));
        assert!(moves.iter().all(|(n, ..)| n != "refs/heads/copy"));
    }

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
