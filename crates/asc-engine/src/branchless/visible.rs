//! The visible set, and the plans for `asc hide` and `asc unhide` (SPEC §21.2.1, §21.2.2).
//!
//! A visible draft is a draft on the first-parent walk from a visible head, stopping at the
//! first changeset that is immutable or has no `ChangeMeta`. The walk never goes below the
//! trunk, never looks at tags, and is bounded by [`stack::MAX_STACK`] per head and
//! [`stack::MAX_SET`] in all.

use std::collections::BTreeSet;

use asc_core::change::ChangeId;
use asc_core::id::ChangesetId;

use super::{DraftHead, DraftRows, Error, Result};
use crate::change::ChangeRows;
use crate::graph::{GraphSource, JumpIndex};
use crate::stack::{self, StackEnv, Stacks};

/// Visible draft heads for [`StackEnv::draft_heads`], read from the draft table.
pub async fn visible_draft_heads<D: DraftRows>(rows: &D) -> Result<Vec<ChangesetId>> {
    let heads = rows.visible_heads(stack::MAX_SET + 1).await?;
    if heads.len() > stack::MAX_SET {
        return Err(Error::TooLarge("draft heads", stack::MAX_SET));
    }
    Ok(heads.into_iter().map(|h| h.cs).collect())
}

/// The drafts visible from the stack environment's heads.
pub async fn visible_set<S: GraphSource, J: JumpIndex, C: ChangeRows, E: StackEnv>(
    stacks: &Stacks<'_, S, J, C, E>,
) -> Result<BTreeSet<ChangesetId>> {
    visible_set_from(stacks, stacks.heads()).await
}

/// The drafts visible from `heads`.
pub async fn visible_set_from<S: GraphSource, J: JumpIndex, C: ChangeRows, E: StackEnv>(
    stacks: &Stacks<'_, S, J, C, E>,
    heads: &[ChangesetId],
) -> Result<BTreeSet<ChangesetId>> {
    let mut out = BTreeSet::new();
    for &head in heads {
        let mut cur = head;
        let mut steps = 0usize;
        // A walk that joins an earlier one stops there: everything below is already in.
        while !out.contains(&cur) && stacks.is_draft(cur).await? {
            steps += 1;
            if steps > stack::MAX_STACK {
                return Err(Error::TooLarge("stack length", stack::MAX_STACK));
            }
            out.insert(cur);
            if out.len() > stack::MAX_SET {
                return Err(Error::TooLarge("visible set", stack::MAX_SET));
            }
            match stacks.graph().node(cur).await?.parents.first() {
                Some(&p) => cur = p,
                None => break,
            }
        }
    }
    Ok(out)
}

/// Draft heads that a visible successor with the same change has made obsolete.
pub async fn obsolete_heads<S: GraphSource, J: JumpIndex, C: ChangeRows, E: StackEnv>(
    stacks: &Stacks<'_, S, J, C, E>,
    heads: &[DraftHead],
) -> Result<Vec<ChangesetId>> {
    if heads.len() > stack::MAX_SET {
        return Err(Error::TooLarge("draft heads", stack::MAX_SET));
    }
    let mut out = Vec::new();
    for h in heads.iter().filter(|h| h.is_visible()) {
        if stacks.is_obsolete(h.cs).await? {
            out.push(h.cs);
        }
    }
    Ok(out)
}

/// What `asc hide <rev>` changes, for `Drafts::hide`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HidePlan {
    /// Visible heads that have the hidden revision as an ancestor; hiding them hides the
    /// revision and its descendants.
    pub heads: Vec<ChangesetId>,
    /// The revision's parent, when it is a draft that no other visible head reaches: it
    /// becomes a head so it is not hidden with the revision.
    pub expose: Option<(ChangesetId, ChangeId)>,
}

/// Plans hiding `target`. `draft_heads` are the local draft head rows.
pub async fn hide_plan<S: GraphSource, J: JumpIndex, C: ChangeRows, E: StackEnv>(
    stacks: &Stacks<'_, S, J, C, E>,
    draft_heads: &[DraftHead],
    target: ChangesetId,
) -> Result<HidePlan> {
    if !stacks.is_draft(target).await? {
        return Err(Error::NotDraft(target));
    }
    if draft_heads.len() > stack::MAX_SET {
        return Err(Error::TooLarge("draft heads", stack::MAX_SET));
    }
    let mut heads = Vec::new();
    for h in draft_heads.iter().filter(|h| h.is_visible()) {
        if stacks.graph().is_ancestor(target, h.cs).await? {
            heads.push(h.cs);
        }
    }
    if heads.is_empty() {
        return Err(Error::NotVisible(target));
    }
    let node = stacks.graph().node(target).await?;
    let mut expose = None;
    if let Some(&parent) = node.parents.first()
        && let Some(row) = stacks.draft_row(parent).await?
    {
        let mut reached = false;
        for &other in stacks.heads().iter().filter(|o| !heads.contains(o)) {
            if stacks.graph().is_ancestor(parent, other).await? {
                reached = true;
                break;
            }
        }
        if !reached {
            expose = Some((parent, row.change));
        }
    }
    Ok(HidePlan { heads, expose })
}

/// Resolves `asc unhide <id>` to the head row to clear or insert.
pub async fn unhide_plan<S: GraphSource, J: JumpIndex, C: ChangeRows, E: StackEnv>(
    stacks: &Stacks<'_, S, J, C, E>,
    target: ChangesetId,
) -> Result<(ChangesetId, ChangeId)> {
    match stacks.draft_row(target).await? {
        Some(row) => Ok((target, row.change)),
        None => Err(Error::NotDraft(target)),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use proptest::prelude::*;

    use super::*;
    use crate::stack::testutil::{Fx, change_of};

    fn head(fx: &Fx, i: usize, hidden: bool) -> DraftHead {
        DraftHead {
            cs: fx.ids[i],
            change: change_of(i),
            added: i as i64,
            hidden_at: hidden.then_some(1),
        }
    }

    /// Everything on or below a trunk tip, by explicit closure.
    fn immutable(parents: &[Vec<usize>], trunk: &[usize]) -> HashSet<usize> {
        let mut seen = HashSet::new();
        let mut stack: Vec<usize> = trunk.to_vec();
        while let Some(x) = stack.pop() {
            if seen.insert(x) {
                stack.extend(&parents[x]);
            }
        }
        seen
    }

    /// The naive oracle: first-parent walk, stopping at the first immutable node.
    fn oracle(parents: &[Vec<usize>], imm: &HashSet<usize>, heads: &[usize]) -> Vec<usize> {
        let mut out = HashSet::new();
        for &h in heads {
            let mut cur = Some(h);
            while let Some(c) = cur {
                if imm.contains(&c) {
                    break;
                }
                out.insert(c);
                cur = parents[c].first().copied();
            }
        }
        let mut v: Vec<usize> = out.into_iter().collect();
        v.sort_unstable();
        v
    }

    proptest! {
        #[test]
        fn visible_set_matches_the_naive_oracle(
            shape in proptest::collection::vec((0usize..100, 0usize..100, any::<bool>()), 1..24),
            head_picks in proptest::collection::vec(0usize..100, 0..5),
            trunk_pick in 0usize..100,
        ) {
            let n = shape.len();
            let mut parents: Vec<Vec<usize>> = vec![Vec::new()];
            for (i, (a, b, merge)) in shape.iter().enumerate().skip(1) {
                let mut ps = vec![a % i];
                if *merge && b % i != ps[0] {
                    ps.push(b % i);
                }
                parents.push(ps);
            }
            let trunk = trunk_pick % n;
            let imm = immutable(&parents, &[trunk]);
            let drafts: Vec<usize> = (0..n).filter(|i| !imm.contains(i)).collect();
            let heads: Vec<usize> = head_picks.iter().map(|h| h % n).collect();
            let refs: Vec<&[usize]> = parents.iter().map(Vec::as_slice).collect();
            let fx = Fx::new(&refs, &drafts, &[trunk], &[]);
            let got = futures::executor::block_on(async {
                let st = fx.stacks().await;
                let hs: Vec<ChangesetId> = heads.iter().map(|&h| fx.ids[h]).collect();
                visible_set_from(&st, &hs).await.unwrap()
            });
            prop_assert_eq!(fx.indexes(&got), oracle(&parents, &imm, &heads));
        }
    }

    #[tokio::test]
    async fn hiding_a_mid_stack_change_hides_descendants_and_exposes_the_parent() {
        // 0 trunk; stack 1 <- 2 <- 3 <- 4.
        let mut fx = Fx::new(&[&[], &[0], &[1], &[2], &[3]], &[1, 2, 3, 4], &[0], &[]);
        fx.env.draft_heads = vec![fx.ids[4]];
        let rows = vec![head(&fx, 4, false)];
        let st = fx.stacks().await;
        assert_eq!(
            fx.indexes(&visible_set(&st).await.unwrap()),
            vec![1, 2, 3, 4]
        );
        let plan = hide_plan(&st, &rows, fx.ids[3]).await.unwrap();
        assert_eq!(plan.heads, vec![fx.ids[4]]);
        assert_eq!(plan.expose, Some((fx.ids[2], change_of(2))));
        drop(st);

        // After applying the plan, the head set is {2}.
        fx.env.draft_heads = vec![fx.ids[2]];
        let st = fx.stacks().await;
        assert_eq!(fx.indexes(&visible_set(&st).await.unwrap()), vec![1, 2]);
    }

    #[tokio::test]
    async fn hide_does_not_expose_a_parent_another_head_reaches() {
        // 1 <- 2 and 1 <- 3: hiding 2 leaves 1 visible through 3.
        let mut fx = Fx::new(&[&[], &[0], &[1], &[1]], &[1, 2, 3], &[0], &[]);
        fx.env.draft_heads = vec![fx.ids[2], fx.ids[3]];
        let rows = vec![head(&fx, 2, false), head(&fx, 3, false)];
        let st = fx.stacks().await;
        let plan = hide_plan(&st, &rows, fx.ids[2]).await.unwrap();
        assert_eq!(plan.heads, vec![fx.ids[2]]);
        assert_eq!(plan.expose, None);
    }

    #[tokio::test]
    async fn hide_rejects_immutable_and_unreached_targets() {
        let mut fx = Fx::new(&[&[], &[0], &[1]], &[1, 2], &[0], &[]);
        fx.env.draft_heads = vec![fx.ids[1]];
        let rows = vec![head(&fx, 1, false)];
        let st = fx.stacks().await;
        assert!(matches!(
            hide_plan(&st, &rows, fx.ids[0]).await,
            Err(Error::NotDraft(_))
        ));
        assert!(matches!(
            hide_plan(&st, &rows, fx.ids[2]).await,
            Err(Error::NotVisible(_))
        ));
        assert!(matches!(
            unhide_plan(&st, fx.ids[0]).await,
            Err(Error::NotDraft(_))
        ));
        assert_eq!(
            unhide_plan(&st, fx.ids[2]).await.unwrap(),
            (fx.ids[2], change_of(2))
        );
    }

    #[tokio::test]
    async fn rewritten_head_is_obsolete_when_the_successor_is_visible() {
        // 1 (change 1) rewritten as 2 (change 1, pred 1); head 2 is visible.
        let mut fx = Fx::new(&[&[], &[0], &[0]], &[1], &[0], &[]);
        fx.add_row(2, 1, &[1]);
        fx.env.draft_heads = vec![fx.ids[1], fx.ids[2]];
        let rows = vec![head(&fx, 1, false), head(&fx, 2, false)];
        let st = fx.stacks().await;
        assert_eq!(obsolete_heads(&st, &rows).await.unwrap(), vec![fx.ids[1]]);
    }

    #[tokio::test]
    async fn visibility_looks_at_no_tags_and_does_not_walk_below_trunk() {
        // A 300-deep trunk with a 3-draft stack on top.
        let n = 300;
        let owned: Vec<Vec<usize>> = (0..n + 3)
            .map(|i| if i == 0 { Vec::new() } else { vec![i - 1] })
            .collect();
        let refs: Vec<&[usize]> = owned.iter().map(Vec::as_slice).collect();
        let drafts: Vec<usize> = (n..n + 3).collect();
        let mut fx = Fx::new(&refs, &drafts, &[n - 1], &[]);
        fx.env.draft_heads = vec![fx.ids[n + 2]];
        let st = fx.stacks().await;
        let before = fx.graph.reads();
        let set = visible_set(&st).await.unwrap();
        assert_eq!(fx.indexes(&set), vec![n, n + 1, n + 2]);
        assert!(fx.graph.reads() - before < 20, "walked below the trunk");
        let calls = fx.env.calls.lock().unwrap();
        assert!(
            calls.iter().all(|c| [
                "trunk_tips",
                "immutable_tips",
                "heads",
                "draft_heads",
                "current_head"
            ]
            .contains(c)),
            "unexpected environment calls: {calls:?}"
        );
    }
}
