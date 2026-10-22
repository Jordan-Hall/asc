//! `Tiered<A, B>`: read-through with fill, writes to the near tier (SPEC §3.3).

use std::ops::Range;
use std::time::SystemTime;

use async_trait::async_trait;
use bytes::Bytes;
use futures::stream::BoxStream;

use crate::key::{KeyClass, KeyPrefix, ObjKey};
use crate::objects::{Error, Objects, Result, decode_verified, slice_range};

/// Reads `a`, falls back to `b` and fills `a` with `b`'s stored bytes. Writes go to `a`.
pub struct Tiered<A, B> {
    a: A,
    b: B,
}

impl<A: Objects, B: Objects> Tiered<A, B> {
    pub fn new(a: A, b: B) -> Self {
        Self { a, b }
    }

    /// Copies stored bytes into `a`. A bad value is an error; other failures only cost a re-fetch.
    async fn fill(&self, key: &ObjKey, stored: &Bytes) -> Result<()> {
        match self.a.put(key, stored.clone()).await {
            Err(e @ (Error::Corrupt(_) | Error::Codec(..))) => Err(e),
            _ => Ok(()),
        }
    }
}

#[async_trait]
impl<A: Objects, B: Objects> Objects for Tiered<A, B> {
    async fn get(&self, key: &ObjKey) -> Result<Option<Bytes>> {
        // A corrupt near copy counts as a miss unless the far tier cannot supply it.
        let near_corrupt = match self.a.get(key).await {
            Ok(Some(data)) => return Ok(Some(data)),
            Ok(None) => None,
            Err(e @ (Error::Corrupt(_) | Error::Codec(..))) => Some(e),
            Err(e) => return Err(e),
        };
        let Some(stored) = self.b.get_stored(key).await? else {
            return near_corrupt.map_or(Ok(None), Err);
        };
        let data = decode_verified(key, &stored)?;
        self.fill(key, &stored).await?;
        Ok(Some(data))
    }

    async fn get_stored(&self, key: &ObjKey) -> Result<Option<Bytes>> {
        if let Some(stored) = self.a.get_stored(key).await? {
            return Ok(Some(stored));
        }
        let Some(stored) = self.b.get_stored(key).await? else {
            return Ok(None);
        };
        self.fill(key, &stored).await?;
        Ok(Some(stored))
    }

    async fn get_range(&self, key: &ObjKey, r: Range<u64>) -> Result<Option<Bytes>> {
        if !matches!(key.class(), KeyClass::File | KeyClass::Chunk) {
            return Err(Error::RangeUnsupported(*key));
        }
        Ok(self.get(key).await?.map(|d| slice_range(&d, r)))
    }

    async fn put(&self, key: &ObjKey, stored: Bytes) -> Result<()> {
        self.a.put(key, stored).await
    }

    async fn has_many(&self, keys: &[ObjKey]) -> Result<Vec<bool>> {
        let mut found = self.a.has_many(keys).await?;
        let missing: Vec<usize> = (0..keys.len()).filter(|&i| !found[i]).collect();
        if missing.is_empty() {
            return Ok(found);
        }
        let far_keys: Vec<ObjKey> = missing.iter().map(|&i| keys[i]).collect();
        for (i, present) in missing.into_iter().zip(self.b.has_many(&far_keys).await?) {
            found[i] = present;
        }
        Ok(found)
    }

    /// Deletes from the near tier only.
    async fn delete(&self, key: &ObjKey) -> Result<()> {
        self.a.delete(key).await
    }

    /// Lists the near tier only.
    fn list(&self, prefix: KeyPrefix) -> BoxStream<'_, Result<(ObjKey, SystemTime)>> {
        self.a.list(prefix)
    }
}

#[cfg(test)]
mod tests {
    use asc_core::id::FileId;

    use super::*;
    use crate::bucket::BucketObjects;
    use crate::cachedir::tests::{open_in, tamper};
    use crate::cachedir::{CacheDir, Canonical};
    use crate::codec::encode_stored;
    use crate::conformance::run_objects_suite;

    fn pair(dir: &std::path::Path) -> (CacheDir, BucketObjects) {
        (open_in(dir, "near"), BucketObjects::memory())
    }

    #[tokio::test]
    async fn conformance() {
        let tmp = tempfile::tempdir().unwrap();
        let (near, far) = pair(tmp.path());
        let near = near.keep_corrupt();
        let near2 = near.clone();
        let t = Canonical(Tiered::new(near, far));
        run_objects_suite(&t, |k, v| {
            tamper(&near2, k, v);
            std::future::ready(())
        })
        .await;
    }

    #[tokio::test]
    async fn read_through_fills_near_tier() {
        let tmp = tempfile::tempdir().unwrap();
        let (near, far) = pair(tmp.path());
        let data = b"compressible ".repeat(200);
        let key = ObjKey::File(FileId::of(&data));
        let stored = encode_stored(&data);
        assert_eq!(stored[0], 1, "test needs a zstd value");
        far.put(&key, stored.clone()).await.unwrap();
        let t = Tiered::new(near.clone(), far);

        assert_eq!(near.has_many(&[key]).await.unwrap(), [false]);
        assert_eq!(t.has_many(&[key]).await.unwrap(), [true]);
        assert_eq!(t.get_stored(&key).await.unwrap().unwrap(), stored);
        // Filled with decoded data.
        let near_stored = near.get_stored(&key).await.unwrap().unwrap();
        assert_eq!((near_stored[0], &near_stored[1..]), (0, data.as_slice()));
        assert_eq!(t.get(&key).await.unwrap().unwrap(), data.as_slice());
    }

    #[tokio::test]
    async fn get_fills_and_writes_stay_near() {
        let tmp = tempfile::tempdir().unwrap();
        let (near, far) = pair(tmp.path());
        let old = ObjKey::File(FileId::of(b"far"));
        far.put(&old, encode_stored(b"far")).await.unwrap();
        let t = Tiered::new(near.clone(), far.clone());
        assert_eq!(&t.get(&old).await.unwrap().unwrap()[..], b"far");
        assert_eq!(near.has_many(&[old]).await.unwrap(), [true]);

        let new = ObjKey::File(FileId::of(b"new"));
        t.put(&new, encode_stored(b"new")).await.unwrap();
        assert_eq!(near.has_many(&[new]).await.unwrap(), [true]);
        assert_eq!(far.has_many(&[new]).await.unwrap(), [false]);
    }

    #[tokio::test]
    async fn corrupt_near_copy_is_refetched() {
        let tmp = tempfile::tempdir().unwrap();
        let (near, far) = pair(tmp.path());
        let key = ObjKey::File(FileId::of(b"good"));
        far.put(&key, encode_stored(b"good")).await.unwrap();
        tamper(&near, key, encode_stored(b"bad"));
        let t = Tiered::new(near, far);
        assert_eq!(&t.get(&key).await.unwrap().unwrap()[..], b"good");
    }

    #[tokio::test]
    async fn corrupt_far_value_is_not_filled() {
        let tmp = tempfile::tempdir().unwrap();
        let near = open_in(tmp.path(), "near");
        let far = open_in(tmp.path(), "far");
        let key = ObjKey::File(FileId::of(b"good"));
        tamper(&far, key, encode_stored(b"evil"));
        let t = Tiered::new(near.clone(), far);
        assert!(matches!(t.get(&key).await, Err(Error::Corrupt(_))));
        assert!(t.get_stored(&key).await.is_err());
        assert_eq!(near.has_many(&[key]).await.unwrap(), [false]);
    }
}
