//! Delta storage at rest (docs/design/DELTA-STORAGE.md): a file version stored as codec `2`, a
//! zstd frame compressed against an earlier version of the same path. Only the server writes
//! deltas, and only to loose objects; reads resolve them in [`BucketObjects`], so the wire never
//! carries one.

use asc_core::id::FileId;
use bytes::Bytes;
use object_store::{ObjectStoreExt, PutPayload};

use crate::bucket::BucketObjects;
use crate::codec::{MAX_DELTA_DEPTH, delta_base, encode_delta};
use crate::key::ObjKey;
use crate::objects::{Objects, Result, decode_verified};

/// Why [`BucketObjects::plan_delta`] left a file alone.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Skip {
    /// The file is not a loose object (missing, or in a pack).
    NotLoose,
    /// The file is a delta already.
    AlreadyDelta,
    /// The base is missing or is the file itself, the file is in the base's chain, or the
    /// chain is as deep as reads allow.
    BadBase,
    /// The delta would not be under half of the stored size.
    NoGain,
}

/// A delta ready to store: the file, its base and the codec `2` bytes.
#[derive(Clone, Debug)]
pub struct Planned {
    pub file: FileId,
    pub base: FileId,
    pub stored: Bytes,
    /// The stored size the delta replaces.
    pub was: u64,
}

impl BucketObjects {
    /// Builds a delta of `file` against `base` without writing anything.
    pub async fn plan_delta(&self, file: FileId, base: FileId) -> Result<Result<Planned, Skip>> {
        if file == base {
            return Ok(Err(Skip::BadBase));
        }
        let key = ObjKey::File(file);
        let Some(raw) = self.get_loose_raw(&key).await? else {
            return Ok(Err(Skip::NotLoose));
        };
        if delta_base(&raw).is_some() {
            return Ok(Err(Skip::AlreadyDelta));
        }
        // Walk the base's chain: it must not contain `file` (a cycle no read could resolve)
        // and must leave room for one more link under the read limit.
        let mut at = base;
        let mut depth = 1u8;
        loop {
            if at == file || depth >= MAX_DELTA_DEPTH {
                return Ok(Err(Skip::BadBase));
            }
            match self
                .get_loose_raw(&ObjKey::File(at))
                .await?
                .and_then(|b| delta_base(&b))
            {
                Some(next) => {
                    at = FileId::from_bytes(next);
                    depth += 1;
                }
                // A full value, packed (packs never hold deltas) or missing: `get` tells which.
                None => break,
            }
        }
        let base_data = match self.get(&ObjKey::File(base)).await {
            Ok(Some(d)) => d,
            Ok(None)
            | Err(crate::objects::Error::Codec(..) | crate::objects::Error::Corrupt(_)) => {
                return Ok(Err(Skip::BadBase));
            }
            Err(e) => return Err(e),
        };
        let data = decode_verified(&key, &raw)?;
        let Some(stored) = encode_delta(base.as_bytes(), &base_data, &data, raw.len()) else {
            return Ok(Err(Skip::NoGain));
        };
        Ok(Ok(Planned {
            file,
            base,
            stored,
            was: raw.len() as u64,
        }))
    }

    /// Replaces the loose copy of `p.file` with the planned delta. The caller records the base
    /// first, so a collector never sees a delta whose base it may delete.
    pub async fn store_delta(&self, p: &Planned) -> Result<()> {
        self.store
            .put(
                &self.key_path(&ObjKey::File(p.file)),
                PutPayload::from_bytes(p.stored.clone()),
            )
            .await?;
        Ok(())
    }

    /// Turns a loose delta back into a full value; `true` when it was a delta. Run before a
    /// base may go (a purge), then drop the base record.
    pub async fn expand_delta(&self, file: FileId) -> Result<bool> {
        let key = ObjKey::File(file);
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

    /// The base of `file` when its loose copy is a delta.
    pub async fn delta_base_of(&self, file: FileId) -> Result<Option<FileId>> {
        Ok(self
            .get_loose_raw(&ObjKey::File(file))
            .await?
            .and_then(|r| delta_base(&r))
            .map(FileId::from_bytes))
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, SystemTime};

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

    async fn put(b: &BucketObjects, data: &[u8]) -> FileId {
        let id = FileId::of(data);
        b.put(&ObjKey::File(id), encode_stored(data)).await.unwrap();
        id
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
        let key = ObjKey::File(file);
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
        b.delete(&ObjKey::File(base)).await.unwrap();
        let key = ObjKey::File(file);
        assert!(matches!(
            b.get(&key).await,
            Err(Error::Codec(_, crate::codec::CodecError::BadBase(id))) if id == base
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
            assert_eq!(
                b.get(&ObjKey::File(*id)).await.unwrap().unwrap(),
                version(i),
                "version {i}"
            );
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
        let absent = FileId::of(b"absent");
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
        assert_eq!(b.get_loose_raw(&ObjKey::File(base)).await.unwrap(), None);
        assert_eq!(b.get(&ObjKey::File(file)).await.unwrap().unwrap(), small(5));
    }
}
