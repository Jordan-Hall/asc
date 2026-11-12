//! Delta storage at rest (docs/design/DELTA-STORAGE.md): a file, tree or chunk stored as codec
//! `2`, a zstd frame compressed against an earlier object of the same kind (the version at the
//! same path, the same directory, or the same chunk position). Only the server writes
//! deltas, and only to loose objects; reads resolve them in [`BucketObjects`], so the wire never
//! carries one.

use std::collections::{HashMap, HashSet, VecDeque};

use asc_core::id::ChunkId;
use asc_core::object::ChunkList;
use bytes::Bytes;
use object_store::{ObjectStoreExt, PutPayload};

use crate::bucket::BucketObjects;
use crate::codec::{MAX_DELTA_DEPTH, decode_delta, delta_base, encode_delta};
use crate::key::ObjKey;
use crate::objects::{Objects, Result, decode_verified, max_decoded, verify};

/// Why [`BucketObjects::plan_delta`] left an object alone.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Skip {
    /// The object is not loose (missing, or in a pack), or is of a kind never stored as a delta.
    NotLoose,
    /// The object is a delta already.
    AlreadyDelta,
    /// The base is missing, is the object itself or of another kind, the object is in the
    /// base's chain, or the chain is as deep as reads allow.
    BadBase,
    /// The delta would not be under half of the stored size.
    NoGain,
}

/// A delta ready to store: the object, its base and the codec `2` bytes.
#[derive(Clone, Debug)]
pub struct Planned {
    pub key: ObjKey,
    pub base: ObjKey,
    pub stored: Bytes,
    /// The stored size the delta replaces.
    pub was: u64,
}

/// Decoded values and delta links remembered across the plans of one pass, so a base that an
/// earlier plan read or wrote costs no reads. Values are verified against their keys and never
/// change. Each key keeps only its own link (its base, or none for a full value) and a chain is
/// rebuilt from the links on every plan, so a value that became a delta later in the pass
/// lengthens every chain through it. A plan that succeeds records its link even if the caller
/// never stores it: a remembered chain is never shorter than the stored one.
pub struct PlanCache {
    values: HashMap<ObjKey, Bytes>,
    order: VecDeque<ObjKey>,
    bytes: usize,
    cap: usize,
    links: HashMap<ObjKey, Option<ObjKey>>,
}

/// Links held before the cache forgets them all (about 80 bytes each).
const MAX_LINKS: usize = 1 << 20;

impl PlanCache {
    /// A cache holding at most `cap` bytes of decoded values.
    pub fn new(cap: usize) -> Self {
        Self {
            values: HashMap::new(),
            order: VecDeque::new(),
            bytes: 0,
            cap,
            links: HashMap::new(),
        }
    }

    fn value(&self, key: &ObjKey) -> Option<Bytes> {
        self.values.get(key).cloned()
    }

    fn put(&mut self, key: ObjKey, data: Bytes) {
        if data.len() > self.cap {
            return;
        }
        self.bytes += data.len();
        if let Some(old) = self.values.insert(key, data) {
            self.bytes -= old.len();
        } else {
            self.order.push_back(key);
        }
        while self.bytes > self.cap {
            let Some(k) = self.order.pop_front() else {
                break;
            };
            if let Some(d) = self.values.remove(&k) {
                self.bytes -= d.len();
            }
        }
    }

    fn link(&mut self, key: ObjKey, base: Option<ObjKey>) {
        if self.links.len() >= MAX_LINKS && !self.links.contains_key(&key) {
            self.links.clear();
        }
        self.links.insert(key, base);
    }
}

impl Default for PlanCache {
    /// 64 MiB of values.
    fn default() -> Self {
        Self::new(64 << 20)
    }
}

impl BucketObjects {
    /// Builds a delta of `key` against `base`, an object of the same kind, without writing
    /// anything.
    pub async fn plan_delta(&self, key: ObjKey, base: ObjKey) -> Result<Result<Planned, Skip>> {
        self.plan_delta_cached(key, base, &mut PlanCache::new(0))
            .await
    }

    /// [`Self::plan_delta`] with values and links remembered in `cache` across calls.
    pub async fn plan_delta_cached(
        &self,
        key: ObjKey,
        base: ObjKey,
        cache: &mut PlanCache,
    ) -> Result<Result<Planned, Skip>> {
        let Some(base_id) = base.delta_id() else {
            return Ok(Err(Skip::BadBase));
        };
        if key.delta_id().is_none() {
            return Ok(Err(Skip::NotLoose));
        }
        if key == base || key.delta_base_key(base_id) != Some(base) {
            return Ok(Err(Skip::BadBase));
        }
        let Some(raw) = self.get_loose_raw(&key).await? else {
            return Ok(Err(Skip::NotLoose));
        };
        if let Some(b) = delta_base(&raw) {
            cache.link(key, key.delta_base_key(b));
            return Ok(Err(Skip::AlreadyDelta));
        }
        let Some((base_data, chain)) = self.base_state(base, cache).await? else {
            return Ok(Err(Skip::BadBase));
        };
        // The new chain (`base` and its chain) must not contain `key`, a cycle no read could
        // resolve, and must stay under the read limit.
        if chain.contains(&key) || chain.len() + 1 >= MAX_DELTA_DEPTH as usize {
            return Ok(Err(Skip::BadBase));
        }
        let data = decode_verified(&key, &raw)?;
        let Some(stored) = encode_delta(&base_id, &base_data, &data, raw.len()) else {
            cache.link(key, None);
            cache.put(key, data);
            return Ok(Err(Skip::NoGain));
        };
        cache.link(key, Some(base));
        cache.put(key, data);
        Ok(Ok(Planned {
            key,
            base,
            stored,
            was: raw.len() as u64,
        }))
    }

    /// The verified bytes of `base` and the keys below it in its delta chain (empty for a full
    /// value), or `None` when it is missing, damaged or deeper than reads allow. Links are
    /// followed from the cache where known and read otherwise; bytes are decoded upward from the
    /// nearest remembered value.
    async fn base_state(
        &self,
        base: ObjKey,
        cache: &mut PlanCache,
    ) -> Result<Option<(Bytes, Vec<ObjKey>)>> {
        // `path[0]` is `base`, each next entry the one before's base.
        let mut path = vec![base];
        let mut raws: HashMap<ObjKey, Bytes> = HashMap::new();
        loop {
            let at = *path.last().expect("path starts with base");
            let next = match cache.links.get(&at) {
                Some(l) => *l,
                None => {
                    let raw = self.get_loose_raw(&at).await?;
                    let next = match raw.as_deref().and_then(delta_base) {
                        Some(b) => match at.delta_base_key(b) {
                            Some(k) => Some(k),
                            None => return Ok(None),
                        },
                        None => None,
                    };
                    if let (Some(raw), Some(_)) = (raw, next) {
                        raws.insert(at, raw);
                    }
                    cache.link(at, next);
                    next
                }
            };
            let Some(next) = next else { break };
            if path.len() >= MAX_DELTA_DEPTH as usize || path.contains(&next) {
                return Ok(None);
            }
            path.push(next);
        }
        // The nearest remembered value, or the bottom of the chain read in full (packed or loose).
        let (mut i, mut data) = match path.iter().position(|k| cache.values.contains_key(k)) {
            Some(i) => (i, cache.value(&path[i]).expect("value present")),
            None => {
                let i = path.len() - 1;
                match self.get(&path[i]).await {
                    Ok(Some(d)) => {
                        cache.put(path[i], d.clone());
                        (i, d)
                    }
                    Ok(None)
                    | Err(crate::objects::Error::Codec(..) | crate::objects::Error::Corrupt(_)) => {
                        return Ok(None);
                    }
                    Err(e) => return Err(e),
                }
            }
        };
        while i > 0 {
            let (k, below) = (path[i - 1], path[i]);
            let raw = match raws.remove(&k) {
                Some(r) => r,
                None => match self.get_loose_raw(&k).await? {
                    Some(r) => r,
                    None => return Ok(None),
                },
            };
            // A link remembered from a plan that was never stored, or changed by another writer:
            // the stored value is not a delta against `below`, so this chain cannot be decoded.
            if delta_base(&raw).and_then(|b| k.delta_base_key(b)) != Some(below) {
                return Ok(None);
            }
            let Ok(d) = decode_delta(&raw, &data, max_decoded(k.class())) else {
                return Ok(None);
            };
            if verify(&k, &d).is_err() {
                return Ok(None);
            }
            cache.put(k, d.clone());
            data = d;
            i -= 1;
        }
        Ok(Some((data, path[1..].to_vec())))
    }

    /// Replaces the loose copy of `p.key` with the planned delta. The caller records the base
    /// first, so a collector never sees a delta whose base it may delete.
    pub async fn store_delta(&self, p: &Planned) -> Result<()> {
        self.store
            .put(
                &self.key_path(&p.key),
                PutPayload::from_bytes(p.stored.clone()),
            )
            .await?;
        Ok(())
    }

    /// Turns a loose delta back into a full value; `true` when it was a delta. Run before a
    /// base may go (a purge), then drop the base record.
    pub async fn expand_delta(&self, key: ObjKey) -> Result<bool> {
        let Some(raw) = self.get_loose_raw(&key).await? else {
            return Ok(false);
        };
        if delta_base(&raw).is_none() {
            return Ok(false);
        }
        let Some(full) = self.get_loose_stored(&key).await? else {
            return Ok(false);
        };
        self.store
            .put(&self.key_path(&key), PutPayload::from_bytes(full))
            .await?;
        Ok(true)
    }

    /// Overwrites the loose copy of `key` with `stored`, a wire value checked against the key
    /// first. A fork uses it to put full values where the copy brought deltas.
    pub async fn replace_loose(&self, key: &ObjKey, stored: Bytes) -> Result<()> {
        crate::codec::check_codec(&stored).map_err(|e| crate::objects::Error::Codec(*key, e))?;
        decode_verified(key, &stored)?;
        self.store
            .put(&self.key_path(key), PutPayload::from_bytes(stored))
            .await?;
        Ok(())
    }

    /// The base of `key` when its loose copy is a delta.
    pub async fn delta_base_of(&self, key: ObjKey) -> Result<Option<ObjKey>> {
        Ok(self
            .get_loose_raw(&key)
            .await?
            .and_then(|r| delta_base(&r))
            .and_then(|b| key.delta_base_key(b)))
    }
}

/// `(new chunk, old chunk)` pairs for two versions of a large file. A chunk in both is left out;
/// each other new chunk takes the next old chunk that the new version dropped, counting from the
/// last chunk both share.
pub fn pair_chunks(new: &ChunkList, old: &ChunkList) -> Vec<(ChunkId, ChunkId)> {
    let in_new: HashSet<ChunkId> = new.chunks.iter().map(|c| c.0).collect();
    let at_old: HashMap<ChunkId, usize> = old
        .chunks
        .iter()
        .enumerate()
        .map(|(i, c)| (c.0, i))
        .collect();
    let mut out = Vec::new();
    let mut j = 0;
    for &(c, _) in &new.chunks {
        if let Some(&i) = at_old.get(&c) {
            j = j.max(i + 1);
            continue;
        }
        while j < old.chunks.len() && in_new.contains(&old.chunks[j].0) {
            j += 1;
        }
        let Some(&(o, _)) = old.chunks.get(j) else {
            break;
        };
        j += 1;
        out.push((c, o));
    }
    out
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, SystemTime};

    use asc_core::id::FileId;

    use super::*;
    use crate::codec::encode_stored;
    use crate::objects::Error;

    fn version(edit: usize) -> Vec<u8> {
        (0..3000)
            .map(|i| {
                if i == edit {
                    format!("fn edited_{i}() -> u32 {{ {i} }}\n")
                } else {
                    format!("fn item_{i}() -> u32 {{ {} }}\n", i * 3)
                }
            })
            .collect::<String>()
            .into_bytes()
    }

    async fn put(b: &BucketObjects, data: &[u8]) -> ObjKey {
        let key = ObjKey::File(FileId::of(data));
        b.put(&key, encode_stored(data)).await.unwrap();
        key
    }

    #[tokio::test]
    async fn a_delta_reads_back_as_a_full_value() {
        let b = BucketObjects::memory();
        let (v1, v2) = (version(usize::MAX), version(10));
        let base = put(&b, &v1).await;
        let file = put(&b, &v2).await;
        let p = b.plan_delta(file, base).await.unwrap().unwrap();
        assert!(
            p.stored.len() as u64 * 10 < p.was,
            "{} vs {}",
            p.stored.len(),
            p.was
        );
        b.store_delta(&p).await.unwrap();
        assert_eq!(b.delta_base_of(file).await.unwrap(), Some(base));
        let key = file;
        assert_eq!(b.get(&key).await.unwrap().unwrap(), v2);
        // What leaves the bucket is a wire value, never the delta.
        let stored = b.get_stored(&key).await.unwrap().unwrap();
        crate::codec::check_codec(&stored).unwrap();
        assert_eq!(b.get_range(&key, 0..5).await.unwrap().unwrap(), &v2[..5]);
        // A put of the same file keeps the delta.
        b.put(&key, encode_stored(&v2)).await.unwrap();
        assert_eq!(b.delta_base_of(file).await.unwrap(), Some(base));
        assert_eq!(
            b.plan_delta(file, base).await.unwrap().unwrap_err(),
            Skip::AlreadyDelta
        );
        assert!(b.expand_delta(file).await.unwrap());
        assert!(!b.expand_delta(file).await.unwrap());
        assert_eq!(b.delta_base_of(file).await.unwrap(), None);
        assert_eq!(b.get(&key).await.unwrap().unwrap(), v2);
    }

    #[tokio::test]
    async fn a_missing_base_reads_as_damage_and_a_put_repairs_it() {
        let b = BucketObjects::memory();
        let (v1, v2) = (version(usize::MAX), version(20));
        let base = put(&b, &v1).await;
        let file = put(&b, &v2).await;
        let p = b.plan_delta(file, base).await.unwrap().unwrap();
        b.store_delta(&p).await.unwrap();
        b.delete(&base).await.unwrap();
        let key = file;
        assert!(matches!(
            b.get(&key).await,
            Err(Error::Codec(_, crate::codec::CodecError::BadBase(id))) if *id == base
        ));
        b.put(&key, encode_stored(&v2)).await.unwrap();
        assert_eq!(b.get(&key).await.unwrap().unwrap(), v2);
        assert_eq!(b.delta_base_of(file).await.unwrap(), None);
    }

    #[tokio::test]
    async fn chains_stop_short_of_cycles_and_the_depth_limit() {
        let b = BucketObjects::memory();
        let mut ids = Vec::new();
        for i in 0..(MAX_DELTA_DEPTH as usize + 2) {
            ids.push(put(&b, &version(i)).await);
        }
        // ids[i] against ids[i - 1], as deep as plan allows.
        let mut stored = 0;
        for w in ids.windows(2) {
            match b.plan_delta(w[1], w[0]).await.unwrap() {
                Ok(p) => {
                    b.store_delta(&p).await.unwrap();
                    stored += 1;
                }
                Err(s) => assert_eq!(s, Skip::BadBase),
            }
        }
        assert_eq!(stored, MAX_DELTA_DEPTH as usize);
        for (i, id) in ids.iter().enumerate() {
            assert_eq!(b.get(id).await.unwrap().unwrap(), version(i), "version {i}");
        }
        // The head of a chain may not take a delta against its own tail, nor against itself.
        assert_eq!(
            b.plan_delta(ids[0], ids[3]).await.unwrap().unwrap_err(),
            Skip::BadBase
        );
        assert_eq!(
            b.plan_delta(ids[0], ids[0]).await.unwrap().unwrap_err(),
            Skip::BadBase
        );
    }

    #[tokio::test]
    async fn a_shared_cache_plans_chains_like_fresh_reads() {
        let b = BucketObjects::memory();
        let mut ids = Vec::new();
        for i in 0..(MAX_DELTA_DEPTH as usize + 2) {
            ids.push(put(&b, &version(i)).await);
        }
        let mut cache = PlanCache::default();
        let mut stored = 0;
        for w in ids.windows(2) {
            match b.plan_delta_cached(w[1], w[0], &mut cache).await.unwrap() {
                Ok(p) => {
                    b.store_delta(&p).await.unwrap();
                    stored += 1;
                }
                Err(s) => assert_eq!(s, Skip::BadBase),
            }
        }
        assert_eq!(stored, MAX_DELTA_DEPTH as usize);
        for (i, id) in ids.iter().enumerate() {
            assert_eq!(b.get(id).await.unwrap().unwrap(), version(i), "version {i}");
        }
        // Remembered chains still refuse a cycle, and a fresh cache reads the same chains.
        for mut c in [cache, PlanCache::default()] {
            assert_eq!(
                b.plan_delta_cached(ids[0], ids[3], &mut c)
                    .await
                    .unwrap()
                    .unwrap_err(),
                Skip::BadBase
            );
        }
    }

    #[tokio::test]
    async fn a_remembered_base_is_not_read_again() {
        let b = BucketObjects::memory();
        let base = put(&b, &version(usize::MAX)).await;
        let (x, y) = (put(&b, &version(1)).await, put(&b, &version(2)).await);
        let mut cache = PlanCache::default();
        assert!(
            b.plan_delta_cached(x, base, &mut cache)
                .await
                .unwrap()
                .is_ok()
        );
        b.delete(&base).await.unwrap();
        // Only the cache still knows `base`; a fresh plan reads the bucket and finds nothing.
        assert!(
            b.plan_delta_cached(y, base, &mut cache)
                .await
                .unwrap()
                .is_ok()
        );
        assert_eq!(
            b.plan_delta(y, base).await.unwrap().unwrap_err(),
            Skip::BadBase
        );
    }

    #[test]
    fn the_cache_drops_its_oldest_values_past_its_bound() {
        let k = |i: u8| ObjKey::File(FileId::from_bytes([i; 32]));
        let mut c = PlanCache::new(10);
        c.put(k(1), Bytes::from_static(b"aaaa"));
        c.put(k(2), Bytes::from_static(b"bbbb"));
        c.put(k(3), Bytes::from_static(b"cccc"));
        assert!(c.value(&k(1)).is_none());
        assert!(c.value(&k(2)).is_some() && c.value(&k(3)).is_some());
        assert_eq!(c.bytes, 8);
        // A value larger than the bound is never held; replacing a value keeps the count right.
        c.put(k(4), Bytes::from(vec![0u8; 11]));
        assert!(c.value(&k(4)).is_none());
        c.put(k(3), Bytes::from_static(b"cc"));
        assert_eq!(c.bytes, 6);
        let mut z = PlanCache::new(0);
        z.put(k(1), Bytes::from_static(b"a"));
        assert!(z.values.is_empty());
    }

    #[tokio::test]
    async fn a_value_that_becomes_a_delta_lengthens_the_chains_through_it() {
        let b = BucketObjects::memory();
        let mut ids = Vec::new();
        for i in 0..MAX_DELTA_DEPTH as usize {
            ids.push(put(&b, &version(i)).await);
        }
        let mut cache = PlanCache::default();
        // ids[1..] form a chain on ids[1], one short of the limit, remembered while ids[1] is full ...
        for w in ids[1..].windows(2) {
            let p = b
                .plan_delta_cached(w[1], w[0], &mut cache)
                .await
                .unwrap()
                .unwrap();
            b.store_delta(&p).await.unwrap();
        }
        // ... then ids[1] becomes a delta of ids[0]: the chain under the head is now full, though
        // a copy of the head's old chain would still allow one more link.
        let p = b
            .plan_delta_cached(ids[1], ids[0], &mut cache)
            .await
            .unwrap()
            .unwrap();
        b.store_delta(&p).await.unwrap();
        let head = *ids.last().unwrap();
        let extra = put(&b, &version(99)).await;
        assert_eq!(
            b.plan_delta_cached(extra, head, &mut cache)
                .await
                .unwrap()
                .unwrap_err(),
            Skip::BadBase
        );
        // ids[0] may not take a delta against anything above it.
        assert_eq!(
            b.plan_delta_cached(ids[0], ids[2], &mut cache)
                .await
                .unwrap()
                .unwrap_err(),
            Skip::BadBase
        );
        for (i, id) in ids.iter().enumerate() {
            assert_eq!(b.get(id).await.unwrap().unwrap(), version(i), "version {i}");
        }
    }

    #[tokio::test]
    async fn unrelated_or_absent_files_are_skipped() {
        let b = BucketObjects::memory();
        let base = put(&b, &version(usize::MAX)).await;
        let mut noise = vec![0u8; 20000];
        blake3::Hasher::new()
            .update(b"n")
            .finalize_xof()
            .fill(&mut noise);
        let other = put(&b, &noise).await;
        assert_eq!(
            b.plan_delta(other, base).await.unwrap().unwrap_err(),
            Skip::NoGain
        );
        let absent = ObjKey::File(FileId::of(b"absent"));
        assert_eq!(
            b.plan_delta(absent, base).await.unwrap().unwrap_err(),
            Skip::NotLoose
        );
        assert_eq!(
            b.plan_delta(other, absent).await.unwrap().unwrap_err(),
            Skip::BadBase
        );
    }

    #[tokio::test]
    async fn a_chunk_takes_a_chunk_as_its_base_and_never_another_kind() {
        let b = BucketObjects::memory();
        let (v1, v2) = (version(usize::MAX), version(30));
        let put_chunk = async |data: &[u8]| {
            let key = ObjKey::Chunk(asc_core::id::ChunkId::of(data));
            b.put(&key, encode_stored(data)).await.unwrap();
            key
        };
        let base = put_chunk(&v1).await;
        let chunk = put_chunk(&v2).await;
        // A file with the base chunk's bytes still is not a base for a chunk, nor the reverse.
        let file = put(&b, &v1).await;
        assert_eq!(
            b.plan_delta(chunk, file).await.unwrap().unwrap_err(),
            Skip::BadBase
        );
        assert_eq!(
            b.plan_delta(file, base).await.unwrap().unwrap_err(),
            Skip::BadBase
        );
        // Kinds that are never deltas are refused whatever the base.
        let list = ObjKey::ChunkList(FileId::of(b"l"));
        assert_eq!(
            b.plan_delta(list, base).await.unwrap().unwrap_err(),
            Skip::NotLoose
        );
        let p = b.plan_delta(chunk, base).await.unwrap().unwrap();
        b.store_delta(&p).await.unwrap();
        assert_eq!(b.delta_base_of(chunk).await.unwrap(), Some(base));
        assert_eq!(b.get(&chunk).await.unwrap().unwrap(), v2);
        // A missing chunk base names the chunk key in the error.
        b.delete(&base).await.unwrap();
        assert!(matches!(
            b.get(&chunk).await,
            Err(Error::Codec(_, crate::codec::CodecError::BadBase(k))) if *k == base
        ));
    }

    #[test]
    fn delta_keys_keep_their_kind() {
        let id = [7u8; 32];
        for key in [
            ObjKey::File(FileId::from_bytes(id)),
            ObjKey::Tree(asc_core::id::TreeId::from_bytes(id)),
            ObjKey::Chunk(asc_core::id::ChunkId::from_bytes(id)),
        ] {
            assert_eq!(key.delta_id(), Some(id));
            let base = key.delta_base_key([9; 32]).unwrap();
            assert_eq!(base.class(), key.class());
            assert_eq!(base.delta_id(), Some([9; 32]));
        }
        let list = ObjKey::ChunkList(FileId::from_bytes(id));
        assert_eq!(list.delta_id(), None);
        assert_eq!(list.delta_base_key(id), None);
    }

    #[tokio::test]
    async fn the_pack_job_leaves_deltas_loose_and_their_bases_may_be_packed() {
        let cfg = crate::pack::PackConfig {
            enabled: true,
            ..Default::default()
        };
        let b = BucketObjects::memory().with_bucket_packs(cfg);
        let small = |edit| version(edit)[..20000].to_vec();
        let base = put(&b, &small(usize::MAX)).await;
        let file = put(&b, &small(5)).await;
        let p = b.plan_delta(file, base).await.unwrap().unwrap();
        b.store_delta(&p).await.unwrap();
        let later = SystemTime::now() + Duration::from_secs(30 * 86_400);
        let rep = b.pack_run(&crate::gc::NoFence, later, true).await.unwrap();
        assert_eq!(rep.packed, 1, "{rep:?}");
        assert_eq!(b.delta_base_of(file).await.unwrap(), Some(base));
        assert_eq!(b.get_loose_raw(&base).await.unwrap(), None);
        assert_eq!(b.get(&file).await.unwrap().unwrap(), small(5));
    }

    fn list(ids: &[&[u8]]) -> ChunkList {
        ChunkList {
            size: ids.len() as u64,
            chunks: ids.iter().map(|b| (ChunkId::of(b), 1)).collect(),
        }
    }

    #[test]
    fn new_chunks_pair_with_the_dropped_chunks_between_shared_ones() {
        let id = |b: &[u8]| ChunkId::of(b);
        // `b` and `d` were edited into `B` and `D`; `x` was inserted after `e`.
        let old = list(&[b"a", b"b", b"c", b"d", b"e"]);
        let new = list(&[b"a", b"B", b"c", b"D", b"e", b"x"]);
        assert_eq!(
            pair_chunks(&new, &old),
            vec![(id(b"B"), id(b"b")), (id(b"D"), id(b"d"))]
        );
        // Two edits in one gap take the dropped chunks in order; a shared chunk that moved back
        // does not send the cursor back.
        let old = list(&[b"a", b"b", b"c", b"d"]);
        let new = list(&[b"d", b"P", b"Q", b"a"]);
        assert_eq!(pair_chunks(&new, &old), Vec::new());
        let new = list(&[b"a", b"P", b"Q", b"d"]);
        assert_eq!(
            pair_chunks(&new, &old),
            vec![(id(b"P"), id(b"b")), (id(b"Q"), id(b"c"))]
        );
        assert_eq!(pair_chunks(&old, &old), Vec::new());
    }
}
