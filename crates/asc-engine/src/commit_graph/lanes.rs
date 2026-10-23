//! Deterministic lane assignment for the commit graph (SPEC §20.6).
//!
//! The walk visits changesets by generation descending, then id. A [`Frontier`] is the whole
//! state between two rows: every changeset that a visited row points at and that is not yet
//! visited (a *pending* changeset), each with the lane its line runs in. Rows are produced by
//! [`Frontier::visit`] and depend on nothing but the frontier and the row, so the rows of a
//! walk are the same whether it runs in one page or in many, with the frontier carried between
//! pages by [`Frontier::encode`].
//!
//! Lanes are numbered from 0 and reused lowest-first. At most [`MAX_LANES`] lines are drawn
//! apart; a line that finds every lane taken shares the single [`OVERFLOW_COLUMN`], and a row
//! whose changeset sits in it has `overflow` set. The walk still follows every pending
//! changeset, so a wide history is shown completely, only drawn more coarsely.

use std::cmp::Reverse;
use std::collections::{BTreeMap, HashSet};

use asc_core::id::ChangesetId;

/// Lanes drawn apart.
pub const MAX_LANES: usize = 256;
/// The column shared by every line past [`MAX_LANES`].
pub const OVERFLOW_COLUMN: u32 = MAX_LANES as u32;
/// Pending changesets one walk keeps; a wider history stops the walk with [`TooWide`].
pub const MAX_FRONTIER: usize = 16_384;

/// The history has more simultaneous lines than the server follows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("the history has more than {MAX_FRONTIER} open lines")]
pub struct TooWide;

/// Where a pending changeset's line runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lane {
    /// No lane yet (a start point not reached) or none needed (hidden).
    None,
    Col(u16),
    Overflow,
}

impl Lane {
    fn column(self) -> Option<u32> {
        match self {
            Lane::None => None,
            Lane::Col(c) => Some(u32::from(c)),
            Lane::Overflow => Some(OVERFLOW_COLUMN),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Pending {
    /// Reachable from an excluded start point: never shown, never given a lane.
    excluded: bool,
    lane: Lane,
}

/// A line from a row to one of its parents.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Edge {
    /// The column the line continues in below the row.
    pub to_column: u32,
    pub parent: ChangesetId,
}

/// One row of the graph.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Row {
    pub id: ChangesetId,
    pub generation: u32,
    pub column: u32,
    /// The row sits in the shared overflow column.
    pub overflow: bool,
    /// One per shown parent, in parent order.
    pub edges: Vec<Edge>,
}

/// A changeset taken from the frontier, to be [`visit`](Frontier::visit)ed or
/// [`skip`](Frontier::skip)ped.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Popped {
    pub id: ChangesetId,
    pub generation: u32,
    pub excluded: bool,
    lane: Lane,
}

/// The walk's state between two rows.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Frontier {
    pending: BTreeMap<(Reverse<u32>, ChangesetId), Pending>,
    /// Lanes held by a pending changeset; trailing free lanes are trimmed.
    used: Vec<bool>,
    /// Pending changesets that are not excluded.
    live: usize,
}

impl Frontier {
    /// Adds a start point. Exclusion wins when a changeset is added both ways.
    pub fn push_tip(
        &mut self,
        id: ChangesetId,
        generation: u32,
        excluded: bool,
    ) -> Result<(), TooWide> {
        let key = (Reverse(generation), id);
        if self.pending.contains_key(&key) {
            if excluded {
                self.mark_excluded(&key);
            }
            return Ok(());
        }
        self.insert(key, excluded, Lane::None)
    }

    /// True when nothing left to visit would be shown.
    pub fn is_done(&self) -> bool {
        self.live == 0
    }

    pub fn len(&self) -> usize {
        self.pending.len()
    }

    pub fn is_empty(&self) -> bool {
        self.pending.is_empty()
    }

    /// The next changeset in walk order, or `None` once only hidden ones remain.
    pub fn pop(&mut self) -> Option<Popped> {
        if self.live == 0 {
            self.pending.clear();
            self.used.clear();
            return None;
        }
        let ((Reverse(generation), id), p) = self.pending.pop_first()?;
        if !p.excluded {
            self.live -= 1;
        }
        Some(Popped {
            id,
            generation,
            excluded: p.excluded,
            lane: p.lane,
        })
    }

    /// Places a shown changeset and opens or joins a line for each parent. `parents` pairs each
    /// parent with its generation, in parent order.
    pub fn visit(&mut self, at: &Popped, parents: &[(ChangesetId, u32)]) -> Result<Row, TooWide> {
        debug_assert!(!at.excluded);
        let lane = match at.lane {
            Lane::None => self.alloc(),
            l => l,
        };
        let mut own = match lane {
            Lane::Col(c) => Some(c),
            _ => None,
        };
        let mut edges = Vec::with_capacity(parents.len());
        for &(parent, generation) in parents {
            let key = (Reverse(generation), parent);
            let to = match self.pending.get(&key) {
                Some(p) if p.excluded => continue,
                Some(p) if p.lane != Lane::None => p.lane,
                Some(_) => {
                    // A start point that is also an ancestor: its line starts here.
                    let l = self.take_or_alloc(&mut own);
                    if let Some(p) = self.pending.get_mut(&key) {
                        p.lane = l;
                    }
                    l
                }
                None => {
                    let l = self.take_or_alloc(&mut own);
                    if let Err(e) = self.insert(key, false, l) {
                        self.release(l);
                        return Err(e);
                    }
                    l
                }
            };
            edges.push(Edge {
                to_column: to.column().unwrap_or(OVERFLOW_COLUMN),
                parent,
            });
        }
        if let Some(c) = own {
            self.used[usize::from(c)] = false;
            self.trim();
        }
        let column = lane.column().unwrap_or(OVERFLOW_COLUMN);
        Ok(Row {
            id: at.id,
            generation: at.generation,
            column,
            overflow: column == OVERFLOW_COLUMN,
            edges,
        })
    }

    /// Passes over a hidden changeset: its parents are hidden too.
    pub fn skip(&mut self, at: &Popped, parents: &[(ChangesetId, u32)]) -> Result<(), TooWide> {
        debug_assert!(at.excluded);
        for &(parent, generation) in parents {
            let key = (Reverse(generation), parent);
            if self.pending.contains_key(&key) {
                self.mark_excluded(&key);
            } else {
                self.insert(key, true, Lane::None)?;
            }
        }
        Ok(())
    }

    fn insert(
        &mut self,
        key: (Reverse<u32>, ChangesetId),
        excluded: bool,
        lane: Lane,
    ) -> Result<(), TooWide> {
        if self.pending.len() >= MAX_FRONTIER {
            return Err(TooWide);
        }
        self.pending.insert(key, Pending { excluded, lane });
        if !excluded {
            self.live += 1;
        }
        Ok(())
    }

    fn mark_excluded(&mut self, key: &(Reverse<u32>, ChangesetId)) {
        let Some(p) = self.pending.get_mut(key) else {
            return;
        };
        if p.excluded {
            return;
        }
        p.excluded = true;
        let lane = std::mem::replace(&mut p.lane, Lane::None);
        self.live -= 1;
        self.release(lane);
    }

    fn release(&mut self, lane: Lane) {
        if let Lane::Col(c) = lane {
            self.used[usize::from(c)] = false;
            self.trim();
        }
    }

    fn trim(&mut self) {
        while self.used.last() == Some(&false) {
            self.used.pop();
        }
    }

    /// The lowest free lane, else the overflow column.
    fn alloc(&mut self) -> Lane {
        if let Some(i) = self.used.iter().position(|u| !u) {
            self.used[i] = true;
            return Lane::Col(i as u16);
        }
        if self.used.len() < MAX_LANES {
            self.used.push(true);
            return Lane::Col((self.used.len() - 1) as u16);
        }
        Lane::Overflow
    }

    fn take_or_alloc(&mut self, own: &mut Option<u16>) -> Lane {
        match own.take() {
            Some(c) => Lane::Col(c),
            None => self.alloc(),
        }
    }

    // ------------------------------------------------------------------------ token form

    /// The state as bytes: a version, a count, then per pending changeset its id, generation,
    /// flags and lane.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(5 + self.pending.len() * ENTRY);
        out.push(STATE_VERSION);
        out.extend((self.pending.len() as u32).to_le_bytes());
        for ((Reverse(generation), id), p) in &self.pending {
            out.extend(id.as_bytes());
            out.extend(generation.to_le_bytes());
            out.push(u8::from(p.excluded));
            out.extend(
                match p.lane {
                    Lane::None => LANE_NONE,
                    Lane::Overflow => LANE_OVERFLOW,
                    Lane::Col(c) => c,
                }
                .to_le_bytes(),
            );
        }
        out
    }

    /// Parses [`encode`](Self::encode) output. `None` for anything it could not have written:
    /// a token is client input.
    pub fn decode(b: &[u8]) -> Option<Self> {
        let (&version, rest) = b.split_first()?;
        if version != STATE_VERSION {
            return None;
        }
        let (n, rest) = rest.split_first_chunk::<4>()?;
        let n = u32::from_le_bytes(*n) as usize;
        if n > MAX_FRONTIER || rest.len() != n * ENTRY {
            return None;
        }
        let mut f = Frontier::default();
        let mut ids = HashSet::with_capacity(n);
        for e in rest.chunks_exact(ENTRY) {
            let id = ChangesetId::from_bytes(e[..32].try_into().ok()?);
            let generation = u32::from_le_bytes(e[32..36].try_into().ok()?);
            let excluded = match e[36] {
                0 => false,
                1 => true,
                _ => return None,
            };
            let lane = match u16::from_le_bytes(e[37..39].try_into().ok()?) {
                LANE_NONE => Lane::None,
                LANE_OVERFLOW => Lane::Overflow,
                c if usize::from(c) < MAX_LANES => Lane::Col(c),
                _ => return None,
            };
            if !ids.insert(id) || (excluded && lane != Lane::None) {
                return None;
            }
            if let Lane::Col(c) = lane {
                let c = usize::from(c);
                if f.used.len() <= c {
                    f.used.resize(c + 1, false);
                }
                if f.used[c] {
                    return None;
                }
                f.used[c] = true;
            }
            if !excluded {
                f.live += 1;
            }
            f.pending
                .insert((Reverse(generation), id), Pending { excluded, lane });
        }
        Some(f)
    }
}

const STATE_VERSION: u8 = 1;
const ENTRY: usize = 32 + 4 + 1 + 2;
const LANE_NONE: u16 = 0xFFFE;
const LANE_OVERFLOW: u16 = 0xFFFF;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commit_graph::{Error, GraphPage, Start};
    use crate::derive::testutil::*;

    fn cid(n: u8) -> ChangesetId {
        ChangesetId::from_bytes([n; 32])
    }

    /// Runs a hand-made DAG (`id -> parents`, generation = longest path) through the frontier.
    fn walk(dag: &[(u8, &[u8])], tips: &[u8]) -> Vec<Row> {
        let parents_of = |n: u8| dag.iter().find(|(i, _)| *i == n).unwrap().1;
        fn gen_of(dag: &[(u8, &[u8])], n: u8) -> u32 {
            let ps = dag.iter().find(|(i, _)| *i == n).unwrap().1;
            1 + ps.iter().map(|&p| gen_of(dag, p)).max().unwrap_or(0)
        }
        let mut f = Frontier::default();
        for &t in tips {
            f.push_tip(cid(t), gen_of(dag, t), false).unwrap();
        }
        let mut rows = Vec::new();
        while let Some(p) = f.pop() {
            let ps: Vec<_> = parents_of(p.id.as_bytes()[0])
                .iter()
                .map(|&q| (cid(q), gen_of(dag, q)))
                .collect();
            rows.push(f.visit(&p, &ps).unwrap());
        }
        rows
    }

    fn cols(rows: &[Row]) -> Vec<u32> {
        rows.iter().map(|r| r.column).collect()
    }

    #[test]
    fn a_line_stays_in_lane_zero() {
        let rows = walk(&[(3, &[2]), (2, &[1]), (1, &[])], &[3]);
        assert_eq!(cols(&rows), [0, 0, 0]);
        assert_eq!(
            rows[0].edges,
            [Edge {
                to_column: 0,
                parent: cid(2)
            }]
        );
        assert!(rows[2].edges.is_empty());
    }

    #[test]
    fn a_fork_and_a_merge_use_two_lanes_and_free_one() {
        // 1 <- 2 <- 4 (merge of 2 and 3), 1 <- 3
        let rows = walk(&[(4, &[2, 3]), (2, &[1]), (3, &[1]), (1, &[])], &[4]);
        let ids: Vec<u8> = rows.iter().map(|r| r.id.as_bytes()[0]).collect();
        assert_eq!(ids, [4, 2, 3, 1]);
        // The merge: first parent continues in lane 0, the second opens lane 1.
        assert_eq!(
            rows[0].edges,
            [
                Edge {
                    to_column: 0,
                    parent: cid(2)
                },
                Edge {
                    to_column: 1,
                    parent: cid(3)
                }
            ]
        );
        assert_eq!(cols(&rows), [0, 0, 1, 0]);
        // 2 and 3 both point at 1: 2 opens its line in lane 0, and 3 joins it.
        assert_eq!(
            rows[1].edges,
            [Edge {
                to_column: 0,
                parent: cid(1)
            }]
        );
        assert_eq!(
            rows[2].edges,
            [Edge {
                to_column: 0,
                parent: cid(1)
            }]
        );
    }

    #[test]
    fn a_start_point_that_is_also_an_ancestor_is_one_row() {
        let rows = walk(&[(2, &[1]), (1, &[])], &[2, 1]);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].edges[0].to_column, rows[1].column);
    }

    #[test]
    fn excluded_history_is_hidden() {
        let mut f = Frontier::default();
        // 1 <- 2 <- 3, hide 2.
        f.push_tip(cid(3), 3, false).unwrap();
        f.push_tip(cid(2), 2, true).unwrap();
        let p = f.pop().unwrap();
        let row = f.visit(&p, &[(cid(2), 2)]).unwrap();
        assert!(row.edges.is_empty(), "no line to a hidden parent");
        // Only hidden changesets remain.
        assert!(f.is_done());
        assert!(f.pop().is_none());
        assert!(f.is_empty());
    }

    #[test]
    fn a_pending_changeset_hidden_later_gives_its_lane_back() {
        let mut f = Frontier::default();
        f.push_tip(cid(9), 5, false).unwrap();
        f.push_tip(cid(8), 4, true).unwrap();
        // 9 points at 7; 8 (hidden) also points at 7, so 7 is hidden too.
        let p = f.pop().unwrap();
        f.visit(&p, &[(cid(7), 3)]).unwrap();
        assert_eq!(f.used, [true]);
        let p = f.pop().unwrap();
        assert!(p.excluded);
        f.skip(&p, &[(cid(7), 3)]).unwrap();
        assert!(f.used.is_empty(), "the lane is free again");
        assert!(f.is_done());
    }

    #[test]
    fn lanes_past_the_cap_share_the_overflow_column() {
        let n = MAX_LANES + 10;
        let mut f = Frontier::default();
        // `n` start points of equal generation, each with its own parent.
        for i in 0..n {
            let mut id = [0u8; 32];
            id[..4].copy_from_slice(&(i as u32).to_be_bytes());
            f.push_tip(ChangesetId::from_bytes(id), 2, false).unwrap();
        }
        let mut rows = Vec::new();
        for _ in 0..n {
            let p = f.pop().unwrap();
            let mut parent = p.id.as_bytes().to_owned();
            parent[31] = 1;
            rows.push(
                f.visit(&p, &[(ChangesetId::from_bytes(parent), 1)])
                    .unwrap(),
            );
        }
        assert_eq!(rows.len(), n);
        let over = rows
            .iter()
            .filter(|r| r.edges[0].to_column == OVERFLOW_COLUMN)
            .count();
        assert_eq!(over, 10);
        assert_eq!(rows.iter().filter(|r| r.overflow).count(), 10);
        assert!(
            rows.iter()
                .filter(|r| !r.overflow)
                .all(|r| r.column < OVERFLOW_COLUMN)
        );
        assert_eq!(f.len(), n);
    }

    #[test]
    fn state_round_trips_and_refuses_damage() {
        let mut f = Frontier::default();
        f.push_tip(cid(5), 3, false).unwrap();
        f.push_tip(cid(6), 3, false).unwrap();
        f.push_tip(cid(1), 1, true).unwrap();
        let p = f.pop().unwrap();
        f.visit(&p, &[(cid(2), 2), (cid(3), 2)]).unwrap();
        let b = f.encode();
        assert_eq!(Frontier::decode(&b), Some(f.clone()));
        for cut in 0..b.len() {
            assert!(Frontier::decode(&b[..cut]).is_none(), "cut at {cut}");
        }
        let mut more = b.clone();
        more.push(0);
        assert!(Frontier::decode(&more).is_none());
        let mut bad_version = b.clone();
        bad_version[0] = 9;
        assert!(Frontier::decode(&bad_version).is_none());
        let mut huge = b.clone();
        huge[1..5].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(Frontier::decode(&huge).is_none());
        // Two entries in one lane.
        let mut dup = b.clone();
        let (first, second) = (5 + ENTRY + 37, 5 + 2 * ENTRY + 37);
        let lane = [dup[first], dup[first + 1]];
        dup[second..second + 2].copy_from_slice(&lane);
        assert!(Frontier::decode(&dup).is_none());
    }

    // ---------------------------------------------------------------------- engine paging

    type Rows = Vec<Row>;

    async fn pages(
        e: &TestEngine,
        tips: &[ChangesetId],
        exclude: &[ChangesetId],
        size: usize,
    ) -> (Rows, Vec<usize>) {
        let mut rows = Vec::new();
        let mut token_sizes = Vec::new();
        let mut token: Option<Vec<u8>> = None;
        loop {
            let start = match &token {
                Some(t) => Start::Token(t),
                None => Start::New {
                    include: tips,
                    exclude,
                },
            };
            let GraphPage { rows: page, next } = e.graph_page(start, size).await.unwrap();
            assert!(page.len() <= size);
            rows.extend(page);
            match next {
                Some(t) => {
                    token_sizes.push(t.len());
                    token = Some(t);
                }
                None => return (rows, token_sizes),
            }
        }
    }

    /// A history of `n` changesets: runs of side branches that fork from and merge back into
    /// the mainline, so lanes open, join and close.
    async fn busy_history(e: &TestEngine, n: usize) -> ChangesetId {
        let mut main = commit_on(e, &[], b"root").await;
        let mut side: Option<ChangesetId> = None;
        for i in 1..n {
            let body = format!("c{i}");
            main = match (i % 7, side) {
                (3, None) => {
                    side = Some(commit_on(e, &[main], format!("s{i}").as_bytes()).await);
                    commit_on(e, &[main], body.as_bytes()).await
                }
                (5, Some(s)) => {
                    side = None;
                    commit_on(e, &[main, s], body.as_bytes()).await
                }
                (4, Some(s)) => {
                    side = Some(commit_on(e, &[s], format!("t{i}").as_bytes()).await);
                    commit_on(e, &[main], body.as_bytes()).await
                }
                _ => commit_on(e, &[main], body.as_bytes()).await,
            };
        }
        main
    }

    #[tokio::test]
    async fn lanes_are_identical_for_one_page_of_1000_and_ten_pages_of_100() {
        let e = engine();
        let tip = busy_history(&e, 800).await;
        let (whole, _) = pages(&e, &[tip], &[], 2000).await;
        assert!(
            whole.len() >= 1000,
            "side branches add rows: {}",
            whole.len()
        );
        let (thousands, _) = pages(&e, &[tip], &[], 1000).await;
        let (tenth, sizes) = pages(&e, &[tip], &[], 100).await;
        assert_eq!(whole, thousands);
        assert_eq!(whole, tenth);
        assert!(
            whole.iter().any(|r| r.column > 0),
            "the history has more than one lane"
        );
        assert!(sizes.iter().all(|&s| s < 4096));
        // Walk order is generation descending, then id.
        assert!(whole.windows(2).all(|w| {
            (std::cmp::Reverse(w[0].generation), w[0].id)
                < (std::cmp::Reverse(w[1].generation), w[1].id)
        }));
    }

    #[tokio::test]
    async fn a_thousand_live_lanes_give_a_token_under_4_kib() {
        let e = engine();
        let mut tips = Vec::new();
        for i in 0..1000 {
            let root = commit_on(&e, &[], format!("r{i}").as_bytes()).await;
            tips.push(commit_on(&e, &[root], format!("t{i}").as_bytes()).await);
        }
        let page = e
            .graph_page(
                Start::New {
                    include: &tips,
                    exclude: &[],
                },
                1000,
            )
            .await
            .unwrap();
        assert_eq!(page.rows.len(), 1000);
        let token = page.next.expect("the roots are still pending");
        assert!(token.len() < 4096, "token is {} bytes", token.len());
        assert_eq!(
            page.rows.iter().filter(|r| r.overflow).count(),
            1000 - MAX_LANES
        );
        let over = page
            .rows
            .iter()
            .filter(|r| r.edges[0].to_column == OVERFLOW_COLUMN)
            .count();
        assert_eq!(over, 1000 - MAX_LANES);
        // The rest of the walk resumes from the token and sees every root.
        let rest = e.graph_page(Start::Token(&token), 2000).await.unwrap();
        assert_eq!(rest.rows.len(), 1000);
        assert!(rest.next.is_none());
        assert_eq!(
            rest.rows.iter().filter(|r| r.overflow).count(),
            1000 - MAX_LANES
        );
    }

    #[tokio::test]
    async fn exclude_hides_the_shared_history() {
        let e = engine();
        let a = commit_on(&e, &[], b"a").await;
        let b = commit_on(&e, &[a], b"b").await;
        let c = commit_on(&e, &[b], b"c").await;
        let d = commit_on(&e, &[b], b"d").await;
        let (rows, _) = pages(&e, &[c, d], &[b], 10).await;
        let ids: Vec<_> = rows.iter().map(|r| r.id).collect();
        assert_eq!(ids.len(), 2);
        assert!(ids.contains(&c) && ids.contains(&d));
        assert!(rows.iter().all(|r| r.edges.is_empty()));
    }

    #[tokio::test]
    async fn a_forged_token_is_refused_or_expired() {
        let e = engine();
        let a = commit_on(&e, &[], b"a").await;
        let b = commit_on(&e, &[a], b"b").await;
        let page = e
            .graph_page(
                Start::New {
                    include: &[b],
                    exclude: &[],
                },
                1,
            )
            .await
            .unwrap();
        let token = page.next.unwrap();
        assert!(matches!(
            e.graph_page(Start::Token(&[]), 10).await,
            Err(Error::Query(_))
        ));
        let mut flipped = token.clone();
        *flipped.last_mut().unwrap() ^= 1;
        assert!(e.graph_page(Start::Token(&flipped), 10).await.is_err());
        // A real changeset under the wrong generation is refused when reached.
        let mut f = Frontier::default();
        f.push_tip(a, 99, false).unwrap();
        let mut forged = vec![0u8];
        forged.extend(f.encode());
        assert!(matches!(
            e.graph_page(Start::Token(&forged), 10).await,
            Err(Error::Query(_))
        ));
        // A spill reference to nothing stored.
        let mut gone = vec![1u8];
        gone.extend([7u8; 32]);
        assert!(matches!(
            e.graph_page(Start::Token(&gone), 10).await,
            Err(Error::Expired)
        ));
        // Too long to be anything written.
        assert!(matches!(
            e.graph_page(Start::Token(&vec![0u8; 5000]), 10).await,
            Err(Error::Query(_))
        ));
    }

    #[tokio::test]
    async fn an_unknown_start_point_is_a_missing_changeset() {
        let e = engine();
        let ghost = ChangesetId::from_bytes([42; 32]);
        assert!(
            e.graph_page(
                Start::New {
                    include: &[ghost],
                    exclude: &[]
                },
                10
            )
            .await
            .is_err()
        );
    }
}
