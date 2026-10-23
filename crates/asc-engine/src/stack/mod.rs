//! Stack derivation (SPEC §20.3): stacks are computed from the commit graph and the change
//! index; no parent metadata is stored.
//!
//! [`Stacks`] answers the §20.3 table (immutable, draft, stack of a head, stack trunk,
//! obsolete, needs restack, divergent). [`StackEnv`] supplies the ref-derived inputs, so the
//! engine never lists refs itself and never looks at tags (§20.3: "Tags are not checked").
//! The `-r/--revset` subset lives in [`revset`].

use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};
use std::sync::Mutex;

use async_trait::async_trait;
use asc_core::change::ChangeId;
use asc_core::id::ChangesetId;

use crate::change::{self, ChangeRow, ChangeRows};
use crate::graph::{self, Graph, GraphSource, JumpIndex};

pub mod absorb;
pub mod restack;
pub mod revset;

/// Most changes one stack walk visits; a longer chain is refused, not truncated.
pub const MAX_STACK: usize = 4096;

/// Most changesets one derived set or closure holds.
pub const MAX_SET: usize = 10_000;

/// Stack derivation failures.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Graph(#[from] graph::Error),
    #[error(transparent)]
    Rows(#[from] change::Error),
    #[error("stack environment: {0}")]
    Env(String),
    #[error("revset: {0}")]
    Revset(String),
    #[error("{0} exceeds the limit of {1}")]
    TooLarge(&'static str, usize),
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Ref-derived inputs of stack derivation, resolved by the caller from `asc.stack.*` config.
///
/// There is deliberately no tag method: immutability never peels tags (SPEC §20.3).
#[async_trait]
pub trait StackEnv: Send + Sync {
    /// Trunk tips: per `asc.stack.trunk` value, the remote-tracking ref if present, else the
    /// local branch.
    async fn trunk_tips(&self) -> Result<Vec<ChangesetId>>;
    /// Tips of refs matching `asc.stack.immutable`.
    async fn immutable_tips(&self) -> Result<Vec<ChangesetId>>;
    /// Local branches and every workspace `HEAD`.
    async fn heads(&self) -> Result<Vec<ChangesetId>>;
    /// Visible draft heads (branchless mode, B04). Empty by default.
    async fn draft_heads(&self) -> Result<Vec<ChangesetId>> {
        Ok(Vec::new())
    }
    /// The current workspace's `HEAD`, if it is on a changeset.
    async fn current_head(&self) -> Result<Option<ChangesetId>>;
    /// The configured user email, for `mine()`.
    fn user_email(&self) -> Option<Vec<u8>>;
    /// The author email of a changeset, for `mine()`.
    async fn author_email(&self, cs: ChangesetId) -> Result<Vec<u8>>;
    /// Whether `change` has landed as another changeset (`landed_as` or `land_map`).
    async fn landed(&self, change: ChangeId) -> Result<bool>;
    /// Resolves a git revision spec for `git(<spec>)`, to at most [`MAX_SET`] changesets.
    async fn resolve_git(&self, spec: &str) -> Result<Vec<ChangesetId>>;
}

/// The stack of one head (SPEC §20.3).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Stack {
    pub head: ChangesetId,
    /// Draft changesets, bottom (oldest) first. Empty when `head` is not a draft.
    pub changes: Vec<ChangesetId>,
    /// What the walk stopped at: the first parent below the bottom, or a merge that ended
    /// the walk. `None` when the walk reached a root.
    pub base: Option<ChangesetId>,
    /// The tip of the trunk with the highest-generation merge base with the head.
    pub trunk: Option<ChangesetId>,
    /// The base is not the trunk tip (reported, not an error).
    pub behind: bool,
}

/// Derivation context over one graph, change index and environment.
pub struct Stacks<'a, S: GraphSource, J: JumpIndex, C: ChangeRows, E: StackEnv> {
    graph: &'a Graph<S, J>,
    rows: &'a C,
    env: &'a E,
    trunks: Vec<ChangesetId>,
    immutable_tips: Vec<ChangesetId>,
    heads: Vec<ChangesetId>,
    current: Option<ChangesetId>,
    immutable_memo: Mutex<HashMap<ChangesetId, bool>>,
}

impl<'a, S: GraphSource, J: JumpIndex, C: ChangeRows, E: StackEnv> Stacks<'a, S, J, C, E> {
    /// Reads the environment once; derivations then reuse it.
    pub async fn load(graph: &'a Graph<S, J>, rows: &'a C, env: &'a E) -> Result<Self> {
        let trunks = env.trunk_tips().await?;
        let immutable_tips = env.immutable_tips().await?;
        let mut heads = env.heads().await?;
        heads.extend(env.draft_heads().await?);
        heads.sort();
        heads.dedup();
        if heads.len() > MAX_SET {
            return Err(Error::TooLarge("heads", MAX_SET));
        }
        let current = env.current_head().await?;
        Ok(Self {
            graph,
            rows,
            env,
            trunks,
            immutable_tips,
            heads,
            current,
            immutable_memo: Mutex::new(HashMap::new()),
        })
    }

    pub fn graph(&self) -> &'a Graph<S, J> {
        self.graph
    }

    pub fn env(&self) -> &'a E {
        self.env
    }

    pub fn trunk_tips(&self) -> &[ChangesetId] {
        &self.trunks
    }

    pub fn heads(&self) -> &[ChangesetId] {
        &self.heads
    }

    pub fn current_head(&self) -> Option<ChangesetId> {
        self.current
    }

    /// Ancestor of (or equal to) a trunk tip or an immutable ref tip.
    pub async fn is_immutable(&self, cs: ChangesetId) -> Result<bool> {
        if let Some(&v) = self.memo().get(&cs) {
            return Ok(v);
        }
        let mut found = false;
        for &tip in self.trunks.iter().chain(&self.immutable_tips) {
            if self.graph.is_ancestor(cs, tip).await? {
                found = true;
                break;
            }
        }
        self.memo().insert(cs, found);
        Ok(found)
    }

    fn memo(&self) -> std::sync::MutexGuard<'_, HashMap<ChangesetId, bool>> {
        self.immutable_memo
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    /// The change row of `cs` when it is a draft: mutable and carrying a `ChangeMeta`.
    pub async fn draft_row(&self, cs: ChangesetId) -> Result<Option<ChangeRow>> {
        let Some(row) = self.rows.get(cs).await? else {
            return Ok(None);
        };
        Ok(if self.is_immutable(cs).await? {
            None
        } else {
            Some(row)
        })
    }

    pub async fn is_draft(&self, cs: ChangesetId) -> Result<bool> {
        Ok(self.draft_row(cs).await?.is_some())
    }

    /// First-parent walk from `head` while the changeset is a draft. A merge ends the walk.
    pub async fn stack_of(&self, head: ChangesetId) -> Result<Stack> {
        let mut changes = Vec::new();
        let mut cur = head;
        let base = loop {
            if !self.is_draft(cur).await? {
                break Some(cur);
            }
            let node = self.graph.node(cur).await?;
            if node.parents.len() > 1 {
                break Some(cur);
            }
            if changes.len() >= MAX_STACK {
                return Err(Error::TooLarge("stack length", MAX_STACK));
            }
            changes.push(cur);
            match node.parents.first() {
                Some(&p) => cur = p,
                None => break None,
            }
        };
        changes.reverse();
        let trunk = self.stack_trunk(head).await?;
        let behind = match (base, trunk) {
            (Some(b), Some(t)) => b != t,
            (None, Some(_)) => true,
            _ => false,
        };
        Ok(Stack {
            head,
            changes,
            base,
            trunk,
            behind,
        })
    }

    /// The trunk whose tip has the highest-generation merge base with `head`.
    pub async fn stack_trunk(&self, head: ChangesetId) -> Result<Option<ChangesetId>> {
        let mut best: Option<(u32, ChangesetId)> = None;
        for &tip in &self.trunks {
            let Some(&mb) = self.graph.merge_base(head, &[tip], false).await?.first() else {
                continue;
            };
            let generation = self.graph.node(mb).await?.generation;
            if best.is_none_or(|(g, _)| generation > g) {
                best = Some((generation, tip));
            }
        }
        Ok(best.map(|(_, t)| t))
    }

    /// The change of `parents[0]` when that is a draft; otherwise `cs` is the stack bottom.
    pub async fn parent_change(&self, cs: ChangesetId) -> Result<Option<ChangeId>> {
        let node = self.graph.node(cs).await?;
        let Some(&p) = node.parents.first() else {
            return Ok(None);
        };
        Ok(self.draft_row(p).await?.map(|r| r.change))
    }

    /// Reachable from a head (or equal to one).
    pub async fn is_visible(&self, cs: ChangesetId) -> Result<bool> {
        for &h in &self.heads {
            if self.graph.is_ancestor(cs, h).await? {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// A visible changeset with the same change lists `cs` in its predecessor closure.
    /// Anything reachable from an immutable ref is never obsolete.
    pub async fn is_obsolete(&self, cs: ChangesetId) -> Result<bool> {
        let Some(row) = self.rows.get(cs).await? else {
            return Ok(false);
        };
        if self.is_immutable(cs).await? {
            return Ok(false);
        }
        let mut seen = HashSet::from([cs]);
        let mut queue = VecDeque::from([cs]);
        while let Some(x) = queue.pop_front() {
            for s in self.rows.successors(x, change::MAX_ROWS).await? {
                if !seen.insert(s) {
                    continue;
                }
                if seen.len() > MAX_SET {
                    return Err(Error::TooLarge("successor closure", MAX_SET));
                }
                if let Some(sr) = self.rows.get(s).await?
                    && sr.change == row.change
                    && self.is_visible(s).await?
                {
                    return Ok(true);
                }
                queue.push_back(s);
            }
        }
        Ok(false)
    }

    /// The parent is obsolete, or the parent's change has landed.
    pub async fn needs_restack(&self, cs: ChangesetId) -> Result<bool> {
        let node = self.graph.node(cs).await?;
        let Some(&p) = node.parents.first() else {
            return Ok(false);
        };
        let Some(row) = self.rows.get(p).await? else {
            return Ok(false);
        };
        if self.is_obsolete(p).await? {
            return Ok(true);
        }
        Ok(self.env.landed(row.change).await? && !self.is_immutable(p).await?)
    }

    /// More than one visible, live successor head carries `change`. Changesets reachable
    /// from an immutable ref are ignored.
    pub async fn is_divergent(&self, change: ChangeId) -> Result<bool> {
        let mut live = 0usize;
        for row in self.rows.by_change(change, change::MAX_ROWS).await? {
            if self.is_immutable(row.cs).await?
                || !self.is_visible(row.cs).await?
                || self.is_obsolete(row.cs).await?
            {
                continue;
            }
            live += 1;
            if live > 1 {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Every draft on the stack of any head.
    pub async fn draft_set(&self) -> Result<BTreeSet<ChangesetId>> {
        let mut out = BTreeSet::new();
        for &h in &self.heads {
            out.extend(self.stack_of(h).await?.changes);
            if out.len() > MAX_SET {
                return Err(Error::TooLarge("draft set", MAX_SET));
            }
        }
        Ok(out)
    }

    /// Non-immutable changesets reachable from the heads (merges included).
    pub async fn mutable_set(&self) -> Result<BTreeSet<ChangesetId>> {
        let mut out = BTreeSet::new();
        let mut queue: VecDeque<ChangesetId> = self.heads.iter().copied().collect();
        while let Some(cs) = queue.pop_front() {
            if out.contains(&cs) || self.is_immutable(cs).await? {
                continue;
            }
            if out.len() >= MAX_SET {
                return Err(Error::TooLarge("mutable set", MAX_SET));
            }
            out.insert(cs);
            queue.extend(self.graph.node(cs).await?.parents.iter().copied());
        }
        Ok(out)
    }
}

/// Test fixtures shared with [`revset`] tests.
#[cfg(test)]
pub(crate) mod testutil {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};

    use asc_core::change::{MetaId, RewriteOp};
    use asc_core::codec::Enveloped;
    use asc_core::id::TreeId;
    use asc_core::object::{Changeset, SortedMap};
    use asc_core::sig::Signature;
    use futures::executor::block_on;

    use super::*;
    use crate::change::MemRows;
    use crate::graph::{NoJump, Node};

    pub struct MemSource {
        map: HashMap<ChangesetId, Arc<Changeset>>,
        gens: HashMap<ChangesetId, u32>,
        reads: AtomicU64,
    }

    #[async_trait]
    impl GraphSource for MemSource {
        async fn get(&self, id: ChangesetId) -> graph::Result<Node> {
            self.reads.fetch_add(1, Ordering::Relaxed);
            let cs = self.map.get(&id).ok_or(graph::Error::Missing(id))?;
            Ok(Node {
                generation: self.gens[&id],
                parents: cs.parents.clone(),
            })
        }
        async fn changeset(&self, id: ChangesetId) -> graph::Result<Arc<Changeset>> {
            self.map.get(&id).cloned().ok_or(graph::Error::Missing(id))
        }
        fn reads(&self) -> u64 {
            self.reads.load(Ordering::Relaxed)
        }
    }

    #[derive(Default)]
    pub struct MockEnv {
        pub trunks: Vec<ChangesetId>,
        pub imm: Vec<ChangesetId>,
        pub heads: Vec<ChangesetId>,
        pub draft_heads: Vec<ChangesetId>,
        pub current: Option<ChangesetId>,
        pub email: Option<Vec<u8>>,
        pub authors: HashMap<ChangesetId, Vec<u8>>,
        pub landed: Mutex<HashSet<ChangeId>>,
        pub git: Mutex<HashMap<String, Vec<ChangesetId>>>,
        /// Every environment method called, by name.
        pub calls: Mutex<Vec<&'static str>>,
    }

    impl MockEnv {
        fn note(&self, name: &'static str) {
            self.calls.lock().unwrap().push(name);
        }
    }

    #[async_trait]
    impl StackEnv for MockEnv {
        async fn trunk_tips(&self) -> Result<Vec<ChangesetId>> {
            self.note("trunk_tips");
            Ok(self.trunks.clone())
        }
        async fn immutable_tips(&self) -> Result<Vec<ChangesetId>> {
            self.note("immutable_tips");
            Ok(self.imm.clone())
        }
        async fn heads(&self) -> Result<Vec<ChangesetId>> {
            self.note("heads");
            Ok(self.heads.clone())
        }
        async fn draft_heads(&self) -> Result<Vec<ChangesetId>> {
            self.note("draft_heads");
            Ok(self.draft_heads.clone())
        }
        async fn current_head(&self) -> Result<Option<ChangesetId>> {
            self.note("current_head");
            Ok(self.current)
        }
        fn user_email(&self) -> Option<Vec<u8>> {
            self.email.clone()
        }
        async fn author_email(&self, cs: ChangesetId) -> Result<Vec<u8>> {
            self.authors
                .get(&cs)
                .cloned()
                .ok_or_else(|| Error::Env("unknown changeset".into()))
        }
        async fn landed(&self, change: ChangeId) -> Result<bool> {
            self.note("landed");
            Ok(self.landed.lock().unwrap().contains(&change))
        }
        async fn resolve_git(&self, spec: &str) -> Result<Vec<ChangesetId>> {
            self.note("resolve_git");
            self.git
                .lock()
                .unwrap()
                .get(spec)
                .cloned()
                .ok_or_else(|| Error::Env(format!("unknown spec {spec}")))
        }
    }

    /// A small DAG: node `i` has the parents `parents[i]` (earlier indexes).
    pub struct Fx {
        pub ids: Vec<ChangesetId>,
        pub graph: Graph<MemSource>,
        pub rows: MemRows,
        pub env: MockEnv,
    }

    pub fn change_of(i: usize) -> ChangeId {
        ChangeId::from_bytes(((i as u128) + 1).to_be_bytes())
    }

    impl Fx {
        pub fn new(
            parents: &[&[usize]],
            drafts: &[usize],
            trunk: &[usize],
            heads: &[usize],
        ) -> Self {
            Self::new_with(parents, drafts, trunk, heads, &[])
        }

        /// `authors` overrides the default author email `me@x` per node.
        pub fn new_with(
            parents: &[&[usize]],
            drafts: &[usize],
            trunk: &[usize],
            heads: &[usize],
            authors: &[(usize, &[u8])],
        ) -> Self {
            let mut ids: Vec<ChangesetId> = Vec::new();
            let mut map = HashMap::new();
            let mut gens = HashMap::new();
            let mut env = MockEnv::default();
            for (i, ps) in parents.iter().enumerate() {
                let email = authors
                    .iter()
                    .find(|(n, _)| *n == i)
                    .map_or(&b"me@x"[..], |(_, e)| *e);
                let sig = Signature::new(b"n".to_vec(), email.to_vec(), i as i64, 0).unwrap();
                let cs = Changeset {
                    parents: ps.iter().map(|&p| ids[p]).collect(),
                    root: TreeId::from_bytes([0; 32]),
                    author: sig.clone(),
                    committer: sig,
                    message: format!("n{i}").into_bytes(),
                    extra: SortedMap::default(),
                    changes: SortedMap::new(Vec::new()).unwrap(),
                };
                let id = ChangesetId::of(&cs.to_envelope());
                gens.insert(
                    id,
                    cs.parents.iter().map(|q| gens[q]).max().unwrap_or(0) + 1,
                );
                env.authors.insert(id, email.to_vec());
                ids.push(id);
                map.insert(id, Arc::new(cs));
            }
            env.trunks = trunk.iter().map(|&i| ids[i]).collect();
            env.heads = heads.iter().map(|&i| ids[i]).collect();
            let fx = Self {
                graph: Graph::new(MemSource {
                    map,
                    gens,
                    reads: AtomicU64::new(0),
                }),
                rows: MemRows::default(),
                env,
                ids,
            };
            for &d in drafts {
                fx.add_row(d, d, &[]);
            }
            fx
        }

        /// Indexes node `cs` as change number `change`, with predecessor nodes `preds`.
        pub fn add_row(&self, cs: usize, change: usize, preds: &[usize]) {
            let row = ChangeRow {
                cs: self.ids[cs],
                change: change_of(change),
                meta: MetaId::from_bytes([cs as u8; 32]),
                op: RewriteOp::New,
            };
            let preds: Vec<ChangesetId> = preds.iter().map(|&p| self.ids[p]).collect();
            assert!(block_on(self.rows.insert(&row, &preds)).unwrap());
        }

        pub fn set_current(&mut self, i: Option<usize>) {
            self.env.current = i.map(|i| self.ids[i]);
        }

        pub async fn stacks(&self) -> Stacks<'_, MemSource, NoJump, MemRows, MockEnv> {
            Stacks::load(&self.graph, &self.rows, &self.env)
                .await
                .unwrap()
        }

        /// Node indexes of `set`, sorted.
        pub fn indexes(&self, set: &BTreeSet<ChangesetId>) -> Vec<usize> {
            let mut v = self.idx(&set.iter().copied().collect::<Vec<_>>());
            v.sort_unstable();
            v
        }

        pub fn idx(&self, v: &[ChangesetId]) -> Vec<usize> {
            v.iter()
                .map(|c| self.ids.iter().position(|i| i == c).unwrap())
                .collect()
        }
    }
}

#[cfg(test)]
mod tests {
    use futures::executor::block_on;
    use proptest::prelude::*;

    use super::testutil::{Fx, change_of};
    use super::*;

    #[tokio::test]
    async fn stack_walks_first_parents_bottom_first() {
        let fx = Fx::new(&[&[], &[0], &[1], &[2]], &[1, 2, 3], &[0], &[3]);
        let st = fx.stacks().await;
        let s = st.stack_of(fx.ids[3]).await.unwrap();
        assert_eq!(fx.idx(&s.changes), vec![1, 2, 3]);
        assert_eq!(s.base, Some(fx.ids[0]));
        assert_eq!(s.trunk, Some(fx.ids[0]));
        assert!(!s.behind);
        assert_eq!(
            st.parent_change(fx.ids[2]).await.unwrap(),
            Some(change_of(1))
        );
        assert_eq!(st.parent_change(fx.ids[1]).await.unwrap(), None);
    }

    #[tokio::test]
    async fn non_draft_head_has_an_empty_stack() {
        let fx = Fx::new(&[&[], &[0]], &[1], &[0], &[1]);
        let st = fx.stacks().await;
        let s = st.stack_of(fx.ids[0]).await.unwrap();
        assert!(s.changes.is_empty());
        assert_eq!(s.base, Some(fx.ids[0]));
    }

    #[tokio::test]
    async fn merge_ends_the_walk_and_is_not_in_the_stack() {
        let fx = Fx::new(&[&[], &[0], &[0], &[1, 2], &[3]], &[1, 2, 3, 4], &[0], &[4]);
        let st = fx.stacks().await;
        let s = st.stack_of(fx.ids[4]).await.unwrap();
        assert_eq!(fx.idx(&s.changes), vec![4]);
        assert_eq!(s.base, Some(fx.ids[3]));
    }

    #[tokio::test]
    async fn root_draft_has_no_base_and_no_trunk() {
        let fx = Fx::new(&[&[], &[0]], &[0, 1], &[], &[1]);
        let st = fx.stacks().await;
        let s = st.stack_of(fx.ids[1]).await.unwrap();
        assert_eq!(fx.idx(&s.changes), vec![0, 1]);
        assert_eq!((s.base, s.trunk, s.behind), (None, None, false));
    }

    #[tokio::test]
    async fn behind_when_trunk_moved_on() {
        let fx = Fx::new(&[&[], &[0], &[0]], &[1], &[2], &[1]);
        let st = fx.stacks().await;
        let s = st.stack_of(fx.ids[1]).await.unwrap();
        assert_eq!(s.base, Some(fx.ids[0]));
        assert_eq!(s.trunk, Some(fx.ids[2]));
        assert!(s.behind);
    }

    #[tokio::test]
    async fn stack_trunk_has_the_highest_generation_merge_base() {
        for order in [[1, 2], [2, 1]] {
            let fx = Fx::new(&[&[], &[0], &[1], &[2], &[2]], &[3, 4], &order, &[4]);
            let st = fx.stacks().await;
            // Trunk tips 1 and 2: the head's merge bases are 1 and 2 (generations 2 and 3).
            assert_eq!(st.stack_trunk(fx.ids[4]).await.unwrap(), Some(fx.ids[2]));
        }
    }

    #[tokio::test]
    async fn obsolete_and_needs_restack() {
        // 1 was amended to 2 (same change); 3 still sits on the old 1.
        let fx = Fx::new(&[&[], &[0], &[0], &[1]], &[], &[0], &[2, 3]);
        fx.add_row(1, 1, &[]);
        fx.add_row(2, 1, &[1]);
        fx.add_row(3, 3, &[]);
        let st = fx.stacks().await;
        assert!(st.is_obsolete(fx.ids[1]).await.unwrap());
        assert!(!st.is_obsolete(fx.ids[2]).await.unwrap());
        assert!(st.needs_restack(fx.ids[3]).await.unwrap());
        assert!(!st.needs_restack(fx.ids[2]).await.unwrap());
        assert!(!st.is_divergent(change_of(1)).await.unwrap());
    }

    #[tokio::test]
    async fn an_invisible_successor_does_not_obsolete() {
        let fx = Fx::new(&[&[], &[0], &[0]], &[], &[0], &[1]);
        fx.add_row(1, 1, &[]);
        fx.add_row(2, 1, &[1]);
        let st = fx.stacks().await;
        assert!(!st.is_obsolete(fx.ids[1]).await.unwrap());
    }

    #[tokio::test]
    async fn divergence_needs_two_live_visible_heads() {
        let fx = Fx::new(&[&[], &[0], &[0]], &[], &[0], &[1, 2]);
        fx.add_row(1, 1, &[]);
        fx.add_row(2, 1, &[]);
        let st = fx.stacks().await;
        assert!(st.is_divergent(change_of(1)).await.unwrap());
    }

    #[tokio::test]
    async fn landed_parent_needs_restack() {
        let fx = Fx::new(&[&[], &[0], &[1]], &[1, 2], &[0], &[2]);
        let st = fx.stacks().await;
        assert!(!st.needs_restack(fx.ids[2]).await.unwrap());
        fx.env.landed.lock().unwrap().insert(change_of(1));
        assert!(st.needs_restack(fx.ids[2]).await.unwrap());
    }

    #[tokio::test]
    async fn shared_gerrit_id_on_two_immutable_commits_is_neither_obsolete_nor_divergent() {
        // 1 and 2 are release-branch and trunk cherry-picks of one change; 2 lists 1 as a pred.
        let mut fx = Fx::new(&[&[], &[0], &[0]], &[], &[2], &[1, 2]);
        fx.env.imm = vec![fx.ids[1]];
        fx.add_row(1, 7, &[]);
        fx.add_row(2, 7, &[1]);
        let st = fx.stacks().await;
        assert!(st.is_immutable(fx.ids[1]).await.unwrap());
        assert!(st.is_immutable(fx.ids[2]).await.unwrap());
        assert!(!st.is_obsolete(fx.ids[1]).await.unwrap());
        assert!(!st.is_obsolete(fx.ids[2]).await.unwrap());
        assert!(!st.is_divergent(change_of(7)).await.unwrap());
    }

    #[tokio::test]
    async fn immutable_evaluation_issues_no_tag_lookups() {
        let mut fx = Fx::new(&[&[], &[0], &[1], &[2]], &[3], &[1], &[3]);
        fx.env.imm = vec![fx.ids[2]];
        let st = fx.stacks().await;
        for &id in &fx.ids {
            st.is_immutable(id).await.unwrap();
        }
        st.draft_set().await.unwrap();
        st.mutable_set().await.unwrap();
        let calls = fx.env.calls.lock().unwrap().clone();
        assert!(!calls.is_empty());
        assert!(
            calls.iter().all(|c| !c.contains("tag")),
            "tag lookup in {calls:?}"
        );
    }

    #[tokio::test]
    async fn draft_heads_join_the_visible_set() {
        let mut fx = Fx::new(&[&[], &[0], &[1]], &[1, 2], &[0], &[]);
        fx.env.draft_heads = vec![fx.ids[2]];
        let st = fx.stacks().await;
        assert!(st.is_visible(fx.ids[1]).await.unwrap());
        assert_eq!(fx.indexes(&st.draft_set().await.unwrap()), vec![1, 2]);
    }

    #[tokio::test]
    async fn refuses_a_stack_beyond_the_bound() {
        let n = MAX_STACK + 2;
        let parents: Vec<Vec<usize>> = (0..n)
            .map(|i| if i == 0 { vec![] } else { vec![i - 1] })
            .collect();
        let refs: Vec<&[usize]> = parents.iter().map(Vec::as_slice).collect();
        let drafts: Vec<usize> = (0..n).collect();
        let fx = Fx::new(&refs, &drafts, &[], &[n - 1]);
        let st = fx.stacks().await;
        assert!(matches!(
            st.stack_of(fx.ids[n - 1]).await,
            Err(Error::TooLarge("stack length", _))
        ));
    }

    type Dag = (Vec<Vec<usize>>, Vec<bool>, usize, Vec<usize>);

    /// A random DAG (1 or 2 earlier parents), draft marks, one trunk tip and some heads.
    fn dag() -> impl Strategy<Value = Dag> {
        (2usize..12).prop_flat_map(|n| {
            let parents: Vec<BoxedStrategy<Vec<usize>>> = (0..n)
                .map(|i| {
                    if i == 0 {
                        Just(vec![]).boxed()
                    } else {
                        (0..i, proptest::option::of(0..i))
                            .prop_map(|(a, b)| match b {
                                Some(b) if b != a => vec![a, b],
                                _ => vec![a],
                            })
                            .boxed()
                    }
                })
                .collect();
            (
                parents,
                proptest::collection::vec(any::<bool>(), n),
                0..n,
                proptest::collection::vec(0..n, 1..4),
            )
        })
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(64))]

        #[test]
        fn stacks_equal_a_naive_oracle((parents, marks, trunk, heads) in dag()) {
            let n = parents.len();
            // Naive ancestor sets (including self).
            let mut anc: Vec<BTreeSet<usize>> = Vec::new();
            for ps in &parents {
                let mut s = BTreeSet::from([anc.len()]);
                for &p in ps {
                    s.extend(anc[p].iter().copied());
                }
                anc.push(s);
            }
            let imm = |i: usize| anc[trunk].contains(&i);
            let draft = |i: usize| marks[i] && !imm(i);
            let refs: Vec<&[usize]> = parents.iter().map(Vec::as_slice).collect();
            let drafts: Vec<usize> = (0..n).filter(|&i| marks[i]).collect();
            let fx = Fx::new(&refs, &drafts, &[trunk], &heads);
            let st = block_on(fx.stacks());

            let mut want_drafts = BTreeSet::new();
            let mut want_mutable = BTreeSet::new();
            for &h in &heads {
                let mut cur = h;
                let mut chain = Vec::new();
                while draft(cur) && parents[cur].len() <= 1 {
                    chain.push(cur);
                    match parents[cur].first() {
                        Some(&p) => cur = p,
                        None => break,
                    }
                }
                chain.reverse();
                let got = block_on(st.stack_of(fx.ids[h])).unwrap();
                prop_assert_eq!(fx.idx(&got.changes), chain.clone());
                want_drafts.extend(chain);
                want_mutable.extend(anc[h].iter().copied().filter(|&i| !imm(i)));
            }
            let got_drafts = fx.indexes(&block_on(st.draft_set()).unwrap());
            prop_assert_eq!(got_drafts, want_drafts.into_iter().collect::<Vec<_>>());
            let got_mutable = fx.indexes(&block_on(st.mutable_set()).unwrap());
            prop_assert_eq!(got_mutable, want_mutable.into_iter().collect::<Vec<_>>());
            for i in 0..n {
                prop_assert_eq!(block_on(st.is_immutable(fx.ids[i])).unwrap(), imm(i));
            }
        }
    }
}
