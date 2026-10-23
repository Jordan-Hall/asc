//! git's revision walk (`revision.c`): newest-first by committer date, `A..B` limiting with
//! `limit_list`, and the topological sorts behind `--topo-order`, `--date-order`,
//! `--author-date-order` and `--graph`. The output order is git's, tie for tie.
//!
//! The default order streams: only the frontier of the walk is held. A range with excluded
//! commits is limited first, as git does, and the topological orders need the whole limited
//! list before the first commit can be shown.

use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap, VecDeque};

use asc_core::id::ChangesetId;

use super::revs::Selection;
use crate::cli::common::{Result, WsCtx};

/// Commits the limited walk keeps looking at after everything left is uninteresting.
const SLOP: u32 = 5;

const SEEN: u8 = 1;
const UNINTERESTING: u8 = 2;
const ADDED: u8 = 4;
const LEFT: u8 = 8;

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
}

impl<'a> Ordered<'a> {
    /// Starts a walk over `sel`. `max_age` is `--since`: a commit older than it ends its line of
    /// history.
    pub async fn new(
        ws: &'a WsCtx,
        sel: &Selection,
        first_parent: bool,
        sort: Sort,
        max_age: Option<i64>,
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
        };
        let left = sel.symmetric.map(|(a, _)| a);
        for &id in sel.exclude.iter() {
            w.flags
                .entry(id)
                .and_modify(|f| *f |= UNINTERESTING)
                .or_insert(UNINTERESTING);
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
        let topo = sort != Sort::Date;
        if limited || topo {
            let mut list = w.limit_list(limited).await?;
            if topo {
                list = sort_topologically(list, sort);
            }
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
            out.push(self.item(id, info));
        }
        out.retain(|i| self.flag(&i.id) & UNINTERESTING == 0);
        Ok(out)
    }

    /// The parents of `item` that git's graph draws: those not hidden by the range.
    pub fn shown_parents(&self, item: &Item) -> Vec<ChangesetId> {
        let take = if self.first_parent {
            1
        } else {
            item.parents.len()
        };
        item.parents[..take.min(item.parents.len())]
            .iter()
            .copied()
            .filter(|p| self.flag(p) & UNINTERESTING == 0)
            .collect()
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
