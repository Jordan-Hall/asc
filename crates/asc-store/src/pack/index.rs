//! Where each packed key is: the [`PackIndex`] trait and an in-memory implementation.

use std::collections::BTreeMap;
use std::sync::{Mutex, PoisonError};

use async_trait::async_trait;

use super::format::{ENTRY_HEADER_LEN, PackEntry, PackId};
use crate::key::{KeyClass, ObjKey};
use crate::objects::Result;

/// Lifecycle of a pack.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PackState {
    /// Readers use it.
    Live,
    /// Replaced by another pack; its file stays for `retire_age` so a reader that already holds
    /// a range finishes.
    Retired,
}

impl PackState {
    pub const fn code(self) -> i64 {
        match self {
            Self::Live => 1,
            Self::Retired => 2,
        }
    }

    pub const fn from_code(c: i64) -> Option<Self> {
        match c {
            1 => Some(Self::Live),
            2 => Some(Self::Retired),
            _ => None,
        }
    }
}

/// One pack as the index knows it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PackInfo {
    pub id: PackId,
    /// Entries in the file.
    pub objects: u64,
    /// Size of the file.
    pub bytes: u64,
    /// Seconds since the epoch.
    pub created: i64,
    pub state: PackState,
    /// When it was retired; 0 while live.
    pub retired: i64,
}

/// A pack and how much of it is still referenced.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PackDetail {
    pub info: PackInfo,
    /// Index rows that point into the pack.
    pub live_objects: u64,
    /// Bytes those rows cover, entry headers included.
    pub live_bytes: u64,
}

/// A packed value's place.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PackLoc {
    pub pack: PackId,
    /// Of the stored bytes in the pack file.
    pub offset: u64,
    pub len: u64,
}

/// The index of one repository namespace's packs.
///
/// A key has at most one row. `commit` is the only way rows are added; `forget` removes them
/// (a GC delete); nothing else changes them, so a reader never sees a row whose pack is not
/// durable (the caller writes the pack file before `commit`).
#[async_trait]
pub trait PackIndex: Send + Sync {
    /// True when a live pack exists. Callers use it to skip lookups in a repository without
    /// packs, and ask again, uncached, before reporting an object missing.
    async fn any_live(&self) -> Result<bool>;

    /// True when the index serves answers from a cache that other writers can leave behind.
    fn is_cached(&self) -> bool {
        false
    }

    /// Forgets anything cached, so the next call sees other writers' changes. A caller about to
    /// report an object missing calls it first.
    async fn refresh(&self) -> Result<()> {
        Ok(())
    }

    /// Rows for `keys`, in order.
    async fn locate(&self, keys: &[ObjKey]) -> Result<Vec<Option<PackLoc>>>;

    /// Makes `new` live with one row per entry, atomically. With no `replaces`, an entry whose
    /// key has a row keeps it (the new pack's copy is unused until a merge drops it). With
    /// `replaces`, rows that point into a replaced pack move to `new` and the replaced packs
    /// retire at `new.created`; entries with no row (forgotten meanwhile) or a row elsewhere
    /// are skipped. Returns how many rows were added or moved.
    async fn commit(
        &self,
        new: &PackInfo,
        entries: &[PackEntry],
        replaces: &[PackId],
    ) -> Result<u64>;

    /// Retires packs that no row points into any more, or that an operator retires.
    async fn retire(&self, ids: &[PackId], now: i64) -> Result<()>;

    /// Removes the rows of `keys`; returns how many existed.
    async fn forget(&self, keys: &[ObjKey]) -> Result<u64>;

    /// Every pack, live and retired.
    async fn packs(&self) -> Result<Vec<PackDetail>>;

    /// The rows that point into `id`.
    async fn rows_of(&self, id: PackId) -> Result<Vec<PackEntry>>;

    /// Drops a pack and any rows still pointing into it (its file is already gone or condemned).
    async fn remove(&self, id: PackId) -> Result<()>;

    /// Keys in key order after `after` (class code, id), at most `limit`, with the creation
    /// time of the pack that holds them. `class` restricts the listing.
    async fn list(
        &self,
        class: Option<KeyClass>,
        after: Option<(u8, [u8; 32])>,
        limit: usize,
    ) -> Result<Vec<(ObjKey, i64)>>;

    /// Replaces every pack and row with those read from the pack files (disaster recovery).
    async fn rebuild(&self, packs: &[(PackInfo, Vec<PackEntry>)]) -> Result<()>;
}

// ------------------------------------------------------------------------------ memory

#[derive(Default)]
struct State {
    packs: BTreeMap<PackId, PackInfo>,
    rows: BTreeMap<(u8, [u8; 32]), (PackId, u64, u32)>,
}

/// An index held in memory.
#[derive(Default)]
pub struct MemPackIndex {
    state: Mutex<State>,
}

impl MemPackIndex {
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

fn row_key(k: &ObjKey) -> (u8, [u8; 32]) {
    (k.class().code(), k.id32())
}

#[async_trait]
impl PackIndex for MemPackIndex {
    async fn any_live(&self) -> Result<bool> {
        Ok(self
            .lock()
            .packs
            .values()
            .any(|p| p.state == PackState::Live))
    }

    async fn locate(&self, keys: &[ObjKey]) -> Result<Vec<Option<PackLoc>>> {
        let g = self.lock();
        Ok(keys
            .iter()
            .map(|k| {
                g.rows.get(&row_key(k)).map(|&(pack, offset, len)| PackLoc {
                    pack,
                    offset,
                    len: u64::from(len),
                })
            })
            .collect())
    }

    async fn commit(
        &self,
        new: &PackInfo,
        entries: &[PackEntry],
        replaces: &[PackId],
    ) -> Result<u64> {
        let mut g = self.lock();
        let mut n = 0;
        for e in entries {
            let k = row_key(&e.key);
            let put = match g.rows.get(&k) {
                Some(&(p, ..)) => replaces.contains(&p),
                None => replaces.is_empty(),
            };
            if put {
                g.rows.insert(k, (new.id, e.offset, e.len));
                n += 1;
            }
        }
        g.packs.insert(new.id, new.clone());
        for r in replaces {
            if let Some(p) = g.packs.get_mut(r) {
                p.state = PackState::Retired;
                p.retired = new.created;
            }
        }
        Ok(n)
    }

    async fn retire(&self, ids: &[PackId], now: i64) -> Result<()> {
        let mut g = self.lock();
        for id in ids {
            if let Some(p) = g.packs.get_mut(id) {
                p.state = PackState::Retired;
                p.retired = now;
            }
        }
        Ok(())
    }

    async fn forget(&self, keys: &[ObjKey]) -> Result<u64> {
        let mut g = self.lock();
        Ok(keys
            .iter()
            .filter(|k| g.rows.remove(&row_key(k)).is_some())
            .count() as u64)
    }

    async fn packs(&self) -> Result<Vec<PackDetail>> {
        let g = self.lock();
        Ok(g.packs
            .values()
            .map(|info| {
                let (mut live_objects, mut live_bytes) = (0, 0);
                for &(p, _, len) in g.rows.values() {
                    if p == info.id {
                        live_objects += 1;
                        live_bytes += u64::from(len) + ENTRY_HEADER_LEN as u64;
                    }
                }
                PackDetail {
                    info: info.clone(),
                    live_objects,
                    live_bytes,
                }
            })
            .collect())
    }

    async fn rows_of(&self, id: PackId) -> Result<Vec<PackEntry>> {
        let g = self.lock();
        Ok(g.rows
            .iter()
            .filter(|(_, v)| v.0 == id)
            .filter_map(|(&(c, i), &(_, offset, len))| {
                Some(PackEntry {
                    key: ObjKey::from_pack(KeyClass::from_code(c)?, i)?,
                    offset,
                    len,
                })
            })
            .collect())
    }

    async fn remove(&self, id: PackId) -> Result<()> {
        let mut g = self.lock();
        g.packs.remove(&id);
        g.rows.retain(|_, v| v.0 != id);
        Ok(())
    }

    async fn list(
        &self,
        class: Option<KeyClass>,
        after: Option<(u8, [u8; 32])>,
        limit: usize,
    ) -> Result<Vec<(ObjKey, i64)>> {
        let g = self.lock();
        let mut out = Vec::new();
        for (&(c, i), &(pack, ..)) in &g.rows {
            if after.is_some_and(|a| (c, i) <= a) || out.len() >= limit {
                continue;
            }
            let Some(cl) = KeyClass::from_code(c) else {
                continue;
            };
            if class.is_some_and(|want| want != cl) {
                continue;
            }
            if let Some(key) = ObjKey::from_pack(cl, i) {
                out.push((key, g.packs.get(&pack).map_or(0, |p| p.created)));
            }
        }
        Ok(out)
    }

    async fn rebuild(&self, packs: &[(PackInfo, Vec<PackEntry>)]) -> Result<()> {
        let mut g = self.lock();
        *g = State::default();
        for (info, entries) in packs {
            g.packs.insert(info.id, info.clone());
            if info.state != PackState::Live {
                continue;
            }
            for e in entries {
                g.rows
                    .entry(row_key(&e.key))
                    .or_insert((info.id, e.offset, e.len));
            }
        }
        Ok(())
    }
}

// ------------------------------------------------------------------------ conformance

/// The behaviour every [`PackIndex`] must have. Each implementation's tests run it on an empty
/// index.
#[cfg(any(test, feature = "testkit"))]
pub async fn run_index_suite(index: &dyn PackIndex) {
    use asc_core::id::{FileId, TreeId};

    let key = |n: u8| ObjKey::File(FileId::from_bytes([n; 32]));
    let tree = |n: u8| ObjKey::Tree(TreeId::from_bytes([n; 32]));
    let info = |id: PackId, objects: u64, created: i64| PackInfo {
        id,
        objects,
        bytes: 1000 * objects,
        created,
        state: PackState::Live,
        retired: 0,
    };
    let entry = |k: ObjKey, offset: u64| PackEntry {
        key: k,
        offset,
        len: 10,
    };

    assert!(!index.any_live().await.unwrap());
    assert_eq!(index.locate(&[key(1)]).await.unwrap(), [None]);
    assert!(index.packs().await.unwrap().is_empty());

    // A fresh pack adds its rows.
    let (a, b, c) = (PackId::random(), PackId::random(), PackId::random());
    let n = index
        .commit(
            &info(a, 3, 100),
            &[entry(key(1), 16), entry(key(2), 80), entry(tree(1), 140)],
            &[],
        )
        .await
        .unwrap();
    assert_eq!(n, 3);
    assert!(index.any_live().await.unwrap());
    let at = index.locate(&[key(2), key(9), tree(1)]).await.unwrap();
    assert_eq!(
        at,
        [
            Some(PackLoc {
                pack: a,
                offset: 80,
                len: 10
            }),
            None,
            Some(PackLoc {
                pack: a,
                offset: 140,
                len: 10
            })
        ]
    );
    let d = index.packs().await.unwrap();
    assert_eq!(d.len(), 1);
    assert_eq!((d[0].live_objects, d[0].live_bytes), (3, 3 * (10 + 37)));

    // A second pack holding a duplicate keeps the first row.
    let n = index
        .commit(
            &info(b, 2, 200),
            &[entry(key(2), 16), entry(key(3), 80)],
            &[],
        )
        .await
        .unwrap();
    assert_eq!(n, 1);
    assert_eq!(index.locate(&[key(2)]).await.unwrap()[0].unwrap().pack, a);

    // Forgetting removes rows and reports how many existed.
    assert_eq!(index.forget(&[key(1), key(77)]).await.unwrap(), 1);
    assert_eq!(index.locate(&[key(1)]).await.unwrap(), [None]);

    // Listing is ordered by (class, id), pages, filters and reports the pack's time.
    let all = index.list(None, None, 100).await.unwrap();
    assert_eq!(
        all.iter().map(|(k, _)| *k).collect::<Vec<_>>(),
        [key(2), key(3), tree(1)]
    );
    assert_eq!(all[0].1, 100);
    assert_eq!(all[1].1, 200);
    let page = index.list(None, None, 2).await.unwrap();
    assert_eq!(page.len(), 2);
    let after = (page[1].0.class().code(), page[1].0.id32());
    let rest = index.list(None, Some(after), 2).await.unwrap();
    assert_eq!(rest.iter().map(|(k, _)| *k).collect::<Vec<_>>(), [tree(1)]);
    let only = index.list(Some(KeyClass::Tree), None, 10).await.unwrap();
    assert_eq!(only.len(), 1);

    // Rewriting `a` and `b` into `c`: rows that still point there move; a forgotten key stays
    // forgotten; a key with a row elsewhere is left.
    let moved = index
        .commit(
            &info(c, 4, 300),
            &[
                entry(key(1), 16),
                entry(key(2), 80),
                entry(key(3), 144),
                entry(tree(1), 208),
            ],
            &[a, b],
        )
        .await
        .unwrap();
    assert_eq!(moved, 3, "key 1 was forgotten and is not resurrected");
    assert_eq!(index.locate(&[key(1)]).await.unwrap(), [None]);
    assert_eq!(
        index.locate(&[key(3)]).await.unwrap()[0].unwrap().offset,
        144
    );
    let d = index.packs().await.unwrap();
    let state = |id| d.iter().find(|p| p.info.id == id).unwrap().clone();
    assert_eq!(state(a).info.state, PackState::Retired);
    assert_eq!(state(a).info.retired, 300);
    assert_eq!(state(a).live_objects, 0);
    assert_eq!(state(c).live_objects, 3);
    assert_eq!(index.rows_of(c).await.unwrap().len(), 3);
    assert!(index.rows_of(a).await.unwrap().is_empty());

    // Removing a pack drops it and its rows.
    index.remove(a).await.unwrap();
    index.remove(b).await.unwrap();
    assert_eq!(index.packs().await.unwrap().len(), 1);
    index.retire(&[c], 400).await.unwrap();
    assert_eq!(
        index.packs().await.unwrap()[0].info.state,
        PackState::Retired
    );

    // A rebuild replaces everything with what the pack files say; retired packs add no rows.
    let (x, y) = (PackId::random(), PackId::random());
    let mut retired = info(y, 1, 5);
    retired.state = PackState::Retired;
    retired.retired = 9;
    index
        .rebuild(&[
            (info(x, 2, 50), vec![entry(key(5), 16), entry(key(6), 80)]),
            (retired, vec![entry(key(7), 16)]),
        ])
        .await
        .unwrap();
    assert_eq!(index.packs().await.unwrap().len(), 2);
    assert!(index.locate(&[key(5)]).await.unwrap()[0].is_some());
    assert_eq!(index.locate(&[key(7), key(2)]).await.unwrap(), [None, None]);
    assert!(index.any_live().await.unwrap());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn the_memory_index_conforms() {
        run_index_suite(&MemPackIndex::new()).await;
    }
}
