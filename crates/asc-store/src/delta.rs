//! Delta storage at rest (docs/design/DELTA-STORAGE.md): a file, tree or chunk stored as codec
//! `2`, a zstd frame compressed against an earlier object of the same kind (the version at the
//! same path, the same directory, or the same chunk position). Only the server writes
//! deltas, and only to loose objects; reads resolve them in [`BucketObjects`], so the wire never
//! carries one.

use std::collections::{HashMap, HashSet};

use asc_core::id::ChunkId;
use asc_core::object::ChunkList;
use bytes::Bytes;
use object_store::{ObjectStoreExt, PutPayload};

use crate::bucket::BucketObjects;
use crate::codec::{MAX_DELTA_DEPTH, delta_base, encode_delta};
use crate::key::ObjKey;
use crate::objects::{Objects, Result, decode_verified};

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

impl BucketObjects {
    /// Builds a delta of `key` against `base`, an object of the same kind, without writing
    /// anything.
    pub async fn plan_delta(&self, key: ObjKey, base: ObjKey) -> Result<Result<Planned, Skip>> {
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
        if delta_base(&raw).is_some() {
            return Ok(Err(Skip::AlreadyDelta));
        }
        // Walk the base's chain: it must not contain `key` (a cycle no read could resolve)
        // and must leave room for one more link under the read limit.
        let mut at = base;
        let mut depth = 1u8;
        loop {
            if at == key || depth >= MAX_DELTA_DEPTH {
                return Ok(Err(Skip::BadBase));
            }
            match self.get_loose_raw(&at).await?.and_then(|b| delta_base(&b)) {
                Some(next) => {
                    at = key
                        .delta_base_key(next)
                        .ok_or(crate::objects::Error::Codec(
                            at,
                            crate::codec::CodecError::DeltaKind,
                        ))?;
                    depth += 1;
                }
                // A full value, packed (packs never hold deltas) or missing: `get` tells which.
                None => break,
            }
        }
        let base_data = match self.get(&base).await {
            Ok(Some(d)) => d,
            Ok(None)
            | Err(crate::objects::Error::Codec(..) | crate::objects::Error::Corrupt(_)) => {
                return Ok(Err(Skip::BadBase));
            }
            Err(e) => return Err(e),
        };
        let data = decode_verified(&key, &raw)?;
        let Some(stored) = encode_delta(&base_id, &base_data, &data, raw.len()) else {
            return Ok(Err(Skip::NoGain));
        };
        Ok(Ok(Planned {
            key,
            base,
            stored,
            was: raw.len() as u64,
        }))
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
