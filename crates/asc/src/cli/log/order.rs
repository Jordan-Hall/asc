//! git's revision walk (`revision.c`): newest-first by committer date, `A..B` limiting with
//! `limit_list`, and the topological sorts behind `--topo-order`, `--date-order`,
//! `--author-date-order` and `--graph`. The output order is git's, tie for tie.
//!
//! The default order streams: only the frontier of the walk is held. A range with excluded
//! commits is limited first, as git does, and the topological orders need the whole limited
//! list before the first commit can be shown.
//!
//! With pathspecs (`log --graph -- <path>`) the walk simplifies history as git's default
//! does (`try_to_simplify_commit`): a commit with no change under the paths compared with a
//! parent follows only that parent, commits with no change are not shown, and the graph draws
//! each shown commit's nearest shown ancestors (`rewrite_parents`).

use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap, VecDeque};

use asc_core::id::{ChangesetId, TreeId};
use asc_core::object::Kind;
use asc_core::path::RepoPath;

use super::revs::Selection;
use crate::cli::common::{Result, WsCtx};

/// Commits the limited walk keeps looking at after everything left is uninteresting.
const SLOP: u32 = 5;

const SEEN: u8 = 1;
const UNINTERESTING: u8 = 2;
const ADDED: u8 = 4;
const LEFT: u8 = 8;
/// No change under the pathspecs compared with the parent the walk kept.
const TREESAME: u8 = 16;
/// Named as an excluded end of the range: still relevant to simplification.
const BOTTOM: u8 = 32;

/// The order commits come out in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Sort {
    /// Newest committer date first.
    Date,
    /// `--topo-order`: children before parents, branches kept together.
    Topo,
    /// `--date-order`: children before parents, otherwise by committer date.
    TopoDate,
    /// `--author-date-order`.
    TopoAuthorDate,
}

/// One commit of the walk.
#[derive(Clone, Debug)]
pub struct Item {
    pub id: ChangesetId,
    pub time: i64,
    pub author_time: i64,
    pub parents: Vec<ChangesetId>,
    /// `<` side of a symmetric difference.
    pub left: bool,
}

/// What the walk reads of a commit.
struct Info {
    time: i64,
    author_time: i64,
    parents: Vec<ChangesetId>,
}

/// The newest-first queue: by date, equal dates in insertion order.
type Queue = BinaryHeap<(i64, Reverse<u64>, ChangesetId)>;

/// The revision walk.
pub struct Ordered<'a> {
    ws: &'a WsCtx,
    first_parent: bool,
    max_age: Option<i64>,
    flags: HashMap<ChangesetId, u8>,
    /// Parents of the commits loaded so far, for propagating "uninteresting" backwards.
    loaded: HashMap<ChangesetId, Vec<ChangesetId>>,
    queue: Queue,
    seq: u64,
    /// The limited or sorted list, when the walk cannot stream.
    ready: Option<VecDeque<Item>>,
    streaming: bool,
    /// Pathspecs history is simplified by; empty for none.
    paths: Vec<RepoPath>,
    /// Parents left after simplification, for the commits it changed.
    simplified: HashMap<ChangesetId, Vec<ChangesetId>>,
}

impl<'a> Ordered<'a> {
    /// Starts a walk over `sel`. `max_age` is `--since`: a commit older than it ends its line of
    /// history. With `paths` the history is simplified by them (only for a topological sort,
    /// which is what `--graph` uses).
    pub async fn new(
        ws: &'a WsCtx,
        sel: &Selection,
        first_parent: bool,
        sort: Sort,
        max_age: Option<i64>,
        paths: &[RepoPath],
    ) -> Result<Self> {
        let mut w = Self {
            ws,
            first_parent,
            max_age,
            flags: HashMap::new(),
            loaded: HashMap::new(),
            queue: BinaryHeap::new(),
            seq: 0,
            ready: None,
            streaming: false,
            paths: paths.to_vec(),
            simplified: HashMap::new(),
        };
        let left = sel.symmetric.map(|(a, _)| a);
        for &id in sel.exclude.iter() {
            *w.flags.entry(id).or_insert(0) |= UNINTERESTING | BOTTOM;
        }
        let mut tips: Vec<ChangesetId> = Vec::new();
        for &id in sel.include.iter().chain(sel.exclude.iter()) {
            if !tips.contains(&id) {
                tips.push(id);
            }
        }
        for &id in &tips {
            if Some(id) == left {
                *w.flags.entry(id).or_insert(0) |= LEFT;
            }
            let info = w.info(id).await?;
            if w.flags.get(&id).copied().unwrap_or(0) & UNINTERESTING != 0 {
                w.mark_parents_uninteresting(id);
            }
            *w.flags.entry(id).or_insert(0) |= SEEN;
            w.push(id, info.time);
        }
        let limited = !sel.exclude.is_empty();
        let topo = sort != Sort::Date || !w.paths.is_empty();
        if limited || topo {
            let mut list = w.limit_list(limited).await?;
            if topo {
                list = sort_topologically(list, sort);
            }
            // git sorts every commit of the walk and then leaves out those it simplified away.
            list.retain(|i| !w.ignored(&i.id, i.parents.len()));
            w.ready = Some(list.into());
        } else {
            w.streaming = true;
        }
        Ok(w)
    }

    fn push(&mut self, id: ChangesetId, time: i64) {
        self.seq += 1;
        self.queue.push((time, Reverse(self.seq), id));
    }

    fn flag(&self, id: &ChangesetId) -> u8 {
        self.flags.get(id).copied().unwrap_or(0)
    }

    async fn info(&mut self, id: ChangesetId) -> Result<Info> {
        let cs = self.ws.engine.get_changeset(id).await?;
        let info = Info {
            time: cs.committer.time(),
            author_time: cs.author.time(),
            parents: cs.parents.clone(),
        };
        self.loaded.insert(id, info.parents.clone());
        Ok(info)
    }

    /// git's `mark_parents_uninteresting`: flags the ancestors already loaded.
    fn mark_parents_uninteresting(&mut self, id: ChangesetId) {
        let mut stack: Vec<ChangesetId> = self.loaded.get(&id).cloned().unwrap_or_default();
        while let Some(mut c) = stack.pop() {
            loop {
                let f = self.flags.entry(c).or_insert(0);
                if *f & UNINTERESTING != 0 {
                    break;
                }
                *f |= UNINTERESTING;
                let Some(ps) = self.loaded.get(&c) else { break };
                let Some((first, rest)) = ps.split_first() else {
                    break;
                };
                stack.extend(rest.iter().copied());
                c = *first;
            }
        }
    }

    /// git's `process_parents`: queues the parents of `id`.
    async fn process_parents(&mut self, id: ChangesetId, parents: &[ChangesetId]) -> Result<()> {
        if self.flag(&id) & ADDED != 0 {
            return Ok(());
        }
        *self.flags.entry(id).or_insert(0) |= ADDED;
        let own = self.flag(&id);
        if own & UNINTERESTING != 0 {
            for &p in parents {
                *self.flags.entry(p).or_insert(0) |= UNINTERESTING;
                let info = self.info(p).await?;
                self.mark_parents_uninteresting(p);
                if self.flag(&p) & SEEN != 0 {
                    continue;
                }
                *self.flags.entry(p).or_insert(0) |= SEEN;
                self.push(p, info.time);
            }
            return Ok(());
        }
        let simplified;
        let parents = if self.paths.is_empty() {
            parents
        } else {
            simplified = self.simplify(id, parents).await?;
            simplified.as_slice()
        };
        let take = if self.first_parent { 1 } else { parents.len() };
        for &p in &parents[..take.min(parents.len())] {
            if own & LEFT != 0 {
                *self.flags.entry(p).or_insert(0) |= LEFT;
            }
            if self.flag(&p) & SEEN != 0 {
                continue;
            }
            let info = self.info(p).await?;
            *self.flags.entry(p).or_insert(0) |= SEEN;
            self.push(p, info.time);
        }
        Ok(())
    }

    /// Whether git's `relevant_commit` holds: interesting, or an excluded end of the range.
    fn relevant(&self, id: &ChangesetId) -> bool {
        self.flag(id) & (UNINTERESTING | BOTTOM) != UNINTERESTING
    }

    /// Whether the commit is left out of the output: simplified away and not a merge, which a
    /// graph still draws.
    fn ignored(&self, id: &ChangesetId, parents: usize) -> bool {
        self.flag(id) & TREESAME != 0 && parents < 2
    }

    /// The entries under the pathspecs in `root`.
    async fn entries(&self, root: TreeId) -> Result<Vec<Option<(Kind, [u8; 32])>>> {
        let mut out = Vec::with_capacity(self.paths.len());
        for p in &self.paths {
            let e = self.ws.engine.lookup(root, p).await?;
            out.push(e.map(|e| (e.kind, e.id)));
        }
        Ok(out)
    }

    /// git's `try_to_simplify_commit`: the parents the walk keeps for `id`, flagging it
    /// `TREESAME` when nothing under the paths changed.
    async fn simplify(
        &mut self,
        id: ChangesetId,
        parents: &[ChangesetId],
    ) -> Result<Vec<ChangesetId>> {
        let cs = self.ws.engine.get_changeset(id).await?;
        let own = self.entries(cs.root).await?;
        if parents.is_empty() {
            if own.iter().all(Option::is_none) {
                *self.flags.entry(id).or_insert(0) |= TREESAME;
            }
            return Ok(Vec::new());
        }
        let (mut relevant_parents, mut relevant_change, mut irrelevant_change) = (0, false, false);
        for (nth, &p) in parents.iter().enumerate() {
            // Only the first parent counts when only its chain is followed.
            if self.first_parent && nth > 0 {
                break;
            }
            let relevant = self.relevant(&p);
            if relevant {
                relevant_parents += 1;
            }
            let pcs = self.ws.engine.get_changeset(p).await?;
            if self.entries(pcs.root).await? == own {
                if relevant {
                    *self.flags.entry(id).or_insert(0) |= TREESAME;
                    self.simplified.insert(id, vec![p]);
                    return Ok(vec![p]);
                }
                continue;
            }
            if relevant {
                relevant_change = true;
            } else {
                irrelevant_change = true;
            }
        }
        let changed = if relevant_parents > 0 {
            relevant_change
        } else {
            irrelevant_change
        };
        if !changed {
            *self.flags.entry(id).or_insert(0) |= TREESAME;
        }
        Ok(parents.to_vec())
    }

    /// git's `still_interesting`: how much slop is left once an uninteresting commit was popped.
    fn still_interesting(&self, date: i64, slop: u32) -> u32 {
        let Some(&(top, _, _)) = self.queue.peek() else {
            return 0;
        };
        if date <= top {
            return SLOP;
        }
        let all_uninteresting = self
            .queue
            .iter()
            .all(|(_, _, id)| self.flag(id) & UNINTERESTING != 0);
        if !all_uninteresting {
            return SLOP;
        }
        slop - 1
    }

    fn item(&self, id: ChangesetId, info: Info) -> Item {
        Item {
            id,
            time: info.time,
            author_time: info.author_time,
            parents: info.parents,
            left: self.flag(&id) & LEFT != 0,
        }
    }

    /// Walks everything to be shown, newest first. With `limited` this is git's `limit_list`.
    async fn limit_list(&mut self, limited: bool) -> Result<Vec<Item>> {
        let mut slop = SLOP;
        let mut date = i64::MAX;
        let mut out: Vec<Item> = Vec::new();
        while let Some((_, _, id)) = self.queue.pop() {
            let info = self.info(id).await?;
            if self.max_age.is_some_and(|m| info.time < m) {
                *self.flags.entry(id).or_insert(0) |= UNINTERESTING;
                if !limited {
                    continue;
                }
            }
            self.process_parents(id, &info.parents).await?;
            if self.flag(&id) & UNINTERESTING != 0 {
                self.mark_parents_uninteresting(id);
                slop = self.still_interesting(date, slop);
                if slop > 0 {
                    continue;
                }
                break;
            }
            date = info.time;
            let mut item = self.item(id, info);
            if let Some(ps) = self.simplified.get(&id) {
                item.parents.clone_from(ps);
            }
            out.push(item);
        }
        out.retain(|i| self.flag(&i.id) & UNINTERESTING == 0);
        Ok(out)
    }

    /// The parents of `item` that git's graph draws: those not hidden by the range, each
    /// rewritten to its nearest shown ancestor when history is simplified.
    pub fn shown_parents(&self, item: &Item) -> Vec<ChangesetId> {
        let take = if self.first_parent {
            1
        } else {
            item.parents.len()
        };
        let mut out: Vec<ChangesetId> = Vec::new();
        for &p in &item.parents[..take.min(item.parents.len())] {
            let Some(p) = self.rewrite(p) else { continue };
            if self.flag(&p) & UNINTERESTING == 0
                && !self.ignored(&p, self.parents_of(&p).len())
                && !out.contains(&p)
            {
                out.push(p);
            }
        }
        out
    }

    /// The parents the walk kept for `id`.
    fn parents_of(&self, id: &ChangesetId) -> &[ChangesetId] {
        self.simplified
            .get(id)
            .or_else(|| self.loaded.get(id))
            .map_or(&[], Vec::as_slice)
    }

    /// git's `rewrite_one`: follows `p` through simplified-away commits; `None` when that
    /// line of history ends without a shown commit.
    fn rewrite(&self, mut p: ChangesetId) -> Option<ChangesetId> {
        loop {
            if self.flag(&p) & (UNINTERESTING | TREESAME) != TREESAME {
                return Some(p);
            }
            let ps = self.parents_of(&p);
            if ps.is_empty() {
                return None;
            }
            let next = if self.first_parent {
                ps.first()
            } else {
                ps.iter().find(|q| self.relevant(q))
            };
            match next {
                Some(&q) => p = q,
                None => return Some(p),
            }
        }
    }

    /// The next commit in order.
    pub async fn next(&mut self) -> Result<Option<Item>> {
        if let Some(ready) = &mut self.ready {
            return Ok(ready.pop_front());
        }
        debug_assert!(self.streaming);
        while let Some((_, _, id)) = self.queue.pop() {
            let info = self.info(id).await?;
            if self.max_age.is_some_and(|m| info.time < m) {
                continue;
            }
            self.process_parents(id, &info.parents).await?;
            // The walk keeps only the frontier; the loaded map is for the limited modes.
            self.loaded.remove(&id);
            return Ok(Some(self.item(id, info)));
        }
        Ok(None)
    }
}

/// git's `sort_in_topological_order`.
fn sort_topologically(list: Vec<Item>, sort: Sort) -> Vec<Item> {
    let mut indegree: HashMap<ChangesetId, u32> = list.iter().map(|i| (i.id, 1)).collect();
    for item in &list {
        for p in &item.parents {
            if let Some(n) = indegree.get_mut(p).filter(|n| **n > 0) {
                *n += 1;
            }
        }
    }
    let key = |i: &Item| match sort {
        Sort::TopoAuthorDate => i.author_time,
        _ => i.time,
    };
    let by_id: HashMap<ChangesetId, &Item> = list.iter().map(|i| (i.id, i)).collect();
    let tips: Vec<&Item> = list.iter().filter(|i| indegree[&i.id] == 1).collect();
    let mut out: Vec<Item> = Vec::with_capacity(list.len());
    // `Topo` is a stack with the tips reversed; the date orders are priority queues.
    let mut stack: Vec<&Item> = tips.iter().rev().copied().collect();
    let mut heap: BinaryHeap<(i64, Reverse<u64>, ChangesetId)> = BinaryHeap::new();
    let mut seq = 0u64;
    if sort != Sort::Topo {
        stack.clear();
        for t in &tips {
            seq += 1;
            heap.push((key(t), Reverse(seq), t.id));
        }
    }
    loop {
        let next: &Item = if sort == Sort::Topo {
            match stack.pop() {
                Some(i) => i,
                None => break,
            }
        } else {
            match heap.pop() {
                Some((_, _, id)) => by_id[&id],
                None => break,
            }
        };
        for p in &next.parents {
            let Some(n) = indegree.get_mut(p) else {
                continue;
            };
            if *n == 0 {
                continue;
            }
            *n -= 1;
            if *n == 1 {
                let parent = by_id[p];
                if sort == Sort::Topo {
                    stack.push(parent);
                } else {
                    seq += 1;
                    heap.push((key(parent), Reverse(seq), parent.id));
                }
            }
        }
        indegree.insert(next.id, 0);
        out.push(next.clone());
    }
    out
}
