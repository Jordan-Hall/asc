//! Skip-list acceleration for the commit graph (SPEC §4.3, §5).
//!
//! [`SkipSource`] is a [`GraphSource`] over derived `gen.v1` and `skip.v1`; it keeps the
//! pointers it loads in bounded in-memory caches. [`SkipIndex`] answers the synchronous
//! [`JumpIndex::jump`] from those caches only, so [`SkipSource::prime`] must first load the
//! pointers along the descent (O(log n) small memo reads, no changesets).

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use asc_core::id::ChangesetId;
use asc_core::object::Changeset;
use asc_store::objects::Objects;
use asc_store::refs::Refs;
use async_trait::async_trait;
use quick_cache::sync::Cache;

use asc_store::key::ObjKey;

use crate::derive::{self, Derived, Gen, Skip};
use crate::graph::{Error, GraphSource, JumpIndex, Node, Result};
use crate::{Engine, commit};

/// Skip pointer lists kept in memory.
const SKIP_CACHE_ITEMS: usize = 16 * 1024;
/// Generations of pointer targets kept in memory.
const GEN_CACHE_ITEMS: usize = 64 * 1024;

/// Maps a derivation failure for `D` at `id` into the graph error type.
fn lift<D: Derived>(id: ChangesetId, e: derive::Error) -> Error {
    Error::Engine(match e {
        derive::Error::Store(e) => commit::Error::Store(e),
        derive::Error::Commit(e) => e,
        _ => commit::Error::Store(asc_store::objects::Error::Corrupt(ObjKey::Derived {
            name: D::NAME,
            version: D::VERSION,
            id: *id.as_bytes(),
        })),
    })
}

/// Pointers loaded so far, shared by a [`SkipSource`] and its [`SkipIndex`].
struct Memory {
    skips: Cache<ChangesetId, Arc<[ChangesetId]>>,
    gens: Cache<ChangesetId, u32>,
}

impl Memory {
    fn new() -> Self {
        Self {
            skips: Cache::new(SKIP_CACHE_ITEMS),
            gens: Cache::new(GEN_CACHE_ITEMS),
        }
    }

    /// The deepest first-parent ancestor of `from` with generation `>= min_gen`, following
    /// only pointers that are loaded.
    fn descend(&self, from: ChangesetId, min_gen: u32) -> Option<ChangesetId> {
        let mut at = from;
        loop {
            let next = self.skips.get(&at).and_then(|ptrs| {
                ptrs.iter()
                    .rev()
                    .find(|p| self.gens.get(*p).is_some_and(|g| g >= min_gen))
                    .copied()
            });
            match next {
                Some(p) => at = p,
                None => return (at != from).then_some(at),
            }
        }
    }
}

/// The in-memory [`JumpIndex`] of a [`SkipSource`].
#[derive(Clone)]
pub struct SkipIndex(Arc<Memory>);

impl JumpIndex for SkipIndex {
    fn jump(&self, from: ChangesetId, min_gen: u32) -> Option<ChangesetId> {
        self.0.descend(from, min_gen)
    }
}

/// A [`GraphSource`] over an [`Engine`]'s derived generations and skip pointers.
///
/// Clones share their caches and read counter, so a [`Graph`](crate::graph::Graph) can own
/// one while the caller keeps another for [`SkipSource::prime`].
pub struct SkipSource<'a, O: Objects, R: Refs> {
    engine: &'a Engine<O, R>,
    memory: Arc<Memory>,
    reads: Arc<AtomicU64>,
}

impl<O: Objects, R: Refs> Clone for SkipSource<'_, O, R> {
    fn clone(&self) -> Self {
        Self {
            engine: self.engine,
            memory: self.memory.clone(),
            reads: self.reads.clone(),
        }
    }
}

impl<'a, O: Objects, R: Refs> SkipSource<'a, O, R> {
    pub fn new(engine: &'a Engine<O, R>) -> Self {
        Self {
            engine,
            memory: Arc::new(Memory::new()),
            reads: Arc::new(AtomicU64::new(0)),
        }
    }

    /// The jump index over what this source has loaded.
    pub fn index(&self) -> SkipIndex {
        SkipIndex(self.memory.clone())
    }

    async fn skips_of(&self, id: ChangesetId) -> Result<Arc<[ChangesetId]>> {
        if let Some(s) = self.memory.skips.get(&id) {
            return Ok(s);
        }
        let skip = self
            .engine
            .get_derived::<Skip>(id)
            .await
            .map_err(|e| lift::<Skip>(id, e))?;
        let ptrs: Arc<[ChangesetId]> = skip.ptrs.into();
        self.memory.skips.insert(id, ptrs.clone());
        Ok(ptrs)
    }

    async fn gen_of(&self, id: ChangesetId) -> Result<u32> {
        if let Some(g) = self.memory.gens.get(&id) {
            return Ok(g);
        }
        let g = self
            .engine
            .get_derived::<Gen>(id)
            .await
            .map_err(|e| lift::<Gen>(id, e))?
            .generation;
        self.memory.gens.insert(id, g);
        Ok(g)
    }

    /// Loads the pointers [`SkipIndex::jump`]`(from, min_gen)` will follow. Call it before
    /// the query; it reads O(log n) derived values and no changesets.
    pub async fn prime(&self, from: ChangesetId, min_gen: u32) -> Result<()> {
        let mut at = from;
        loop {
            let ptrs = self.skips_of(at).await?;
            let mut next = None;
            for &p in ptrs.iter().rev() {
                if self.gen_of(p).await? >= min_gen {
                    next = Some(p);
                    break;
                }
            }
            match next {
                Some(p) => at = p,
                None => return Ok(()),
            }
        }
    }
}

#[async_trait]
impl<O: Objects, R: Refs> GraphSource for SkipSource<'_, O, R> {
    async fn get(&self, id: ChangesetId) -> Result<Node> {
        let cs = self.changeset(id).await?;
        let generation = self.gen_of(id).await?;
        self.skips_of(id).await?;
        Ok(Node {
            generation,
            parents: cs.parents.clone(),
        })
    }

    async fn changeset(&self, id: ChangesetId) -> Result<Arc<Changeset>> {
        self.reads.fetch_add(1, Ordering::Relaxed);
        Ok(self.engine.get_changeset(id).await?)
    }

    fn reads(&self) -> u64 {
        self.reads.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commit::{NewChangeset, PathPolicy};
    use crate::derive::testutil::*;
    use crate::graph::{EngineSource, Graph};
    use asc_core::id::TreeId;
    use asc_core::sig::Signature;

    /// Cheap changesets that all share one tree; `salt` keeps the ids distinct.
    struct Builder {
        root: TreeId,
    }

    impl Builder {
        async fn new(e: &TestEngine) -> (Self, ChangesetId) {
            let first = commit_on(e, &[], b"first").await;
            let root = e.get_changeset(first).await.unwrap().root;
            (Self { root }, first)
        }

        async fn add(&self, e: &TestEngine, parents: &[ChangesetId], salt: u32) -> ChangesetId {
            let sig = Signature::new(b"A".to_vec(), b"a@x".to_vec(), 1_700_000_000, 0).unwrap();
            e.create_changeset(
                NewChangeset {
                    parents: parents.to_vec(),
                    author: sig.clone(),
                    committer: sig,
                    message: format!("m{salt}\n").into_bytes(),
                    extra: Vec::new(),
                    root: self.root,
                    copies: Vec::new(),
                },
                PathPolicy::Native { windows_safe: true },
            )
            .await
            .unwrap()
        }
    }

    #[tokio::test]
    async fn linear_chain_reads_log_n_changesets() {
        const N: usize = 100_000;
        let e = engine();
        let (b, first) = Builder::new(&e).await;
        let mut ids = vec![first];
        for i in 1..N {
            let p = ids[i - 1];
            ids.push(b.add(&e, &[p], i as u32).await);
        }
        let tip = ids[N - 1];
        e.get_derived::<Skip>(tip).await.unwrap();

        let src = SkipSource::new(&e);
        let g = Graph::with_jump(src.clone(), src.index());
        for &i in &[0usize, 1, 5_000, 77_777, 99_990, N - 2] {
            let a = ids[i];
            let before = g.reads();
            let ga = g.node(a).await.unwrap().generation;
            src.prime(tip, ga).await.unwrap();
            assert!(g.is_ancestor(a, tip).await.unwrap(), "ancestor {i}");
            assert!(
                g.reads() - before <= 4,
                "reads {} at {i}",
                g.reads() - before
            );
        }
        // A negative answer is cheap too: the generation check rejects it outright.
        let before = g.reads();
        assert!(!g.is_ancestor(tip, ids[50_000]).await.unwrap());
        assert!(g.reads() - before <= 4);
    }

    /// A pseudo-random DAG: each node has a random earlier first parent and sometimes a second.
    async fn random_dag(e: &TestEngine, n: usize) -> Vec<ChangesetId> {
        let (b, first) = Builder::new(e).await;
        let mut ids = vec![first];
        let mut x = 0x9e37_79b9_7f4a_7c15u64;
        let mut next = move || {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x
        };
        for i in 1..n {
            // Mostly recent parents so first-parent chains get long.
            let p1 = i - 1 - (next() % 3.min(i as u64)) as usize;
            let mut parents = vec![ids[p1]];
            if next() % 4 == 0 {
                let p2 = (next() % i as u64) as usize;
                if p2 != p1 {
                    parents.push(ids[p2]);
                }
            }
            ids.push(b.add(e, &parents, i as u32).await);
        }
        ids
    }

    #[tokio::test]
    async fn agrees_with_the_plain_walk_on_random_merges() {
        let e = engine();
        let ids = random_dag(&e, 400).await;
        let plain = Graph::new(EngineSource::new(&e));
        let src = SkipSource::new(&e);
        let fast = Graph::with_jump(src.clone(), src.index());
        let mut x = 12345u64;
        for _ in 0..300 {
            x = x
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            let a = ids[(x >> 33) as usize % ids.len()];
            x = x
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            let b = ids[(x >> 33) as usize % ids.len()];
            let ga = fast.node(a).await.unwrap().generation;
            src.prime(b, ga).await.unwrap();
            assert_eq!(
                fast.is_ancestor(a, b).await.unwrap(),
                plain.is_ancestor(a, b).await.unwrap()
            );
            assert_eq!(
                fast.merge_base(a, &[b], true).await.unwrap(),
                plain.merge_base(a, &[b], true).await.unwrap()
            );
        }
    }

    #[tokio::test]
    async fn jump_lands_on_the_deepest_first_parent_ancestor() {
        let e = engine();
        let (b, first) = Builder::new(&e).await;
        let mut ids = vec![first];
        for i in 1..200 {
            let p = ids[i - 1];
            ids.push(b.add(&e, &[p], i as u32).await);
        }
        let src = SkipSource::new(&e);
        let idx = src.index();
        let tip = ids[199];
        assert_eq!(idx.jump(tip, 1), None, "nothing is loaded yet");
        src.prime(tip, 37).await.unwrap();
        assert_eq!(idx.jump(tip, 37), Some(ids[36]));
        assert_eq!(idx.jump(tip, 200), None);
        src.prime(tip, 1).await.unwrap();
        assert_eq!(idx.jump(tip, 1), Some(ids[0]));
    }
}
