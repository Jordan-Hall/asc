//! `StoreSet`: the machine `store/` and `cache/` directories as one view (SPEC §3.5).

use std::ops::Range;
use std::time::SystemTime;

use async_trait::async_trait;
use bytes::Bytes;
use futures::StreamExt;
use futures::stream::BoxStream;

use crate::cachedir::CacheDir;
use crate::key::{KeyPrefix, ObjKey};
use crate::objects::{Objects, Result};

/// Reads `store` then `cache`; new objects go to `store`, which is never evicted.
#[derive(Clone, Debug)]
pub struct StoreSet {
    /// Objects that exist only on this machine.
    pub store: CacheDir,
    /// Evictable copies of remote objects.
    pub cache: CacheDir,
}

impl StoreSet {
    /// Moves pushed objects from `store` to `cache`; returns how many moved.
    pub async fn demote(&self, keys: &[ObjKey]) -> Result<usize> {
        let mut moved = 0;
        for key in keys {
            moved += usize::from(self.store.demote_to(&self.cache, *key).await?);
        }
        Ok(moved)
    }
}

#[async_trait]
impl Objects for StoreSet {
    async fn get(&self, key: &ObjKey) -> Result<Option<Bytes>> {
        match self.store.get(key).await? {
            Some(data) => Ok(Some(data)),
            None => self.cache.get(key).await,
        }
    }

    async fn get_stored(&self, key: &ObjKey) -> Result<Option<Bytes>> {
        match self.store.get_stored(key).await? {
            Some(stored) => Ok(Some(stored)),
            None => self.cache.get_stored(key).await,
        }
    }

    async fn get_range(&self, key: &ObjKey, r: Range<u64>) -> Result<Option<Bytes>> {
        match self.store.get_range(key, r.clone()).await? {
            Some(data) => Ok(Some(data)),
            None => self.cache.get_range(key, r).await,
        }
    }

    async fn put(&self, key: &ObjKey, stored: Bytes) -> Result<()> {
        self.store.put(key, stored).await
    }

    async fn has_many(&self, keys: &[ObjKey]) -> Result<Vec<bool>> {
        let mut found = self.store.has_many(keys).await?;
        let missing: Vec<usize> = (0..keys.len()).filter(|&i| !found[i]).collect();
        if missing.is_empty() {
            return Ok(found);
        }
        let rest: Vec<ObjKey> = missing.iter().map(|&i| keys[i]).collect();
        for (i, present) in missing.into_iter().zip(self.cache.has_many(&rest).await?) {
            found[i] = present;
        }
        Ok(found)
    }

    async fn delete(&self, key: &ObjKey) -> Result<()> {
        self.store.delete(key).await?;
        self.cache.delete(key).await
    }

    /// Keys present in both directories are listed twice.
    fn list(&self, prefix: KeyPrefix) -> BoxStream<'_, Result<(ObjKey, SystemTime)>> {
        self.store
            .list(prefix.clone())
            .chain(self.cache.list(prefix))
            .boxed()
    }
}

#[cfg(test)]
mod tests {
    use asc_core::id::FileId;
    use futures::TryStreamExt;

    use super::*;
    use crate::cachedir::tests::open_in;
    use crate::codec::encode_stored;

    fn set(dir: &std::path::Path) -> StoreSet {
        StoreSet {
            store: open_in(dir, "store"),
            cache: open_in(dir, "cache"),
        }
    }

    fn key(data: &[u8]) -> ObjKey {
        ObjKey::File(FileId::of(data))
    }

    #[tokio::test]
    async fn reads_both_writes_store() {
        let tmp = tempfile::tempdir().unwrap();
        let s = set(tmp.path());
        let (local, remote) = (key(b"local"), key(b"remote"));
        s.cache
            .put(&remote, encode_stored(b"remote"))
            .await
            .unwrap();
        s.put(&local, encode_stored(b"local")).await.unwrap();

        assert_eq!(
            s.store.has_many(&[local, remote]).await.unwrap(),
            [true, false]
        );
        assert_eq!(s.has_many(&[local, remote]).await.unwrap(), [true, true]);
        assert_eq!(&s.get(&remote).await.unwrap().unwrap()[..], b"remote");
        assert_eq!(
            &s.get_range(&local, 1..3).await.unwrap().unwrap()[..],
            b"oc"
        );
        assert_eq!(&s.get_stored(&local).await.unwrap().unwrap()[1..], b"local");
    }

    #[tokio::test]
    async fn demote_moves_store_to_cache() {
        let tmp = tempfile::tempdir().unwrap();
        let s = set(tmp.path());
        let (a, b, absent) = (key(b"a"), key(b"b"), key(b"absent"));
        s.put(&a, encode_stored(b"a")).await.unwrap();
        s.put(&b, encode_stored(b"b")).await.unwrap();
        // `b` is already cached; demoting just drops the store copy.
        s.cache.put(&b, encode_stored(b"b")).await.unwrap();

        assert_eq!(s.demote(&[a, b, absent]).await.unwrap(), 2);
        assert_eq!(s.store.has_many(&[a, b]).await.unwrap(), [false, false]);
        assert_eq!(s.cache.has_many(&[a, b]).await.unwrap(), [true, true]);
        assert_eq!(&s.get(&a).await.unwrap().unwrap()[..], b"a");
    }

    #[tokio::test]
    async fn eviction_leaves_store_intact() {
        let tmp = tempfile::tempdir().unwrap();
        let s = set(tmp.path());
        let (local, remote) = (key(&[1; 200]), key(&[2; 200]));
        s.put(&local, encode_stored(&[1; 200])).await.unwrap();
        s.cache
            .put(&remote, encode_stored(&[2; 200]))
            .await
            .unwrap();

        assert_eq!(s.cache.evict_to(0).await.unwrap(), 200);
        assert_eq!(s.has_many(&[local, remote]).await.unwrap(), [true, false]);
        let listed: Vec<_> = s.list(KeyPrefix::all()).try_collect().await.unwrap();
        assert_eq!(listed.len(), 1);
    }
}
