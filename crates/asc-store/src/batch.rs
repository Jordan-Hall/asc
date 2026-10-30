//! `BatchedObjects`: a write buffer in front of a store, for bulk writers such as the importer.
//!
//! `put` checks only the codec byte and keeps the value in memory, deduplicated by key. Reads
//! see the buffered values first, so a writer reads back what it wrote. [`flush`] hands the
//! buffer to the inner store's [`Objects::put_many`], which verifies each value as `put` does,
//! in fewer and larger operations than one request per object.
//!
//! [`flush`]: BatchedObjects::flush

use std::collections::HashMap;
use std::ops::Range;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::SystemTime;

use async_trait::async_trait;
use bytes::Bytes;
use futures::stream::BoxStream;

use crate::codec::check_codec;
use crate::key::{KeyClass, KeyPrefix, ObjKey};
use crate::objects::{Error, Objects, Result, decode_verified, slice_range};

#[derive(Default)]
struct Pending {
    map: HashMap<ObjKey, Bytes>,
    bytes: u64,
}

/// Buffers writes to `inner` until [`BatchedObjects::flush`].
pub struct BatchedObjects<O: Objects> {
    inner: Arc<O>,
    pending: Mutex<Pending>,
}

impl<O: Objects> BatchedObjects<O> {
    pub fn new(inner: Arc<O>) -> Self {
        Self {
            inner,
            pending: Mutex::new(Pending::default()),
        }
    }

    /// The store under the buffer.
    pub fn inner(&self) -> &Arc<O> {
        &self.inner
    }

    /// Stored bytes waiting for a flush.
    pub fn pending_bytes(&self) -> u64 {
        self.lock().bytes
    }

    /// Objects waiting for a flush.
    pub fn pending_len(&self) -> usize {
        self.lock().map.len()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Pending> {
        self.pending.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn peek(&self, key: &ObjKey) -> Option<Bytes> {
        self.lock().map.get(key).cloned()
    }

    /// Writes everything buffered to the inner store. On error the objects stay buffered, so a
    /// retry writes them again; nothing is lost and nothing is half-trusted.
    pub async fn flush(&self) -> Result<()> {
        let items: Vec<(ObjKey, Bytes)> = {
            let g = self.lock();
            g.map.iter().map(|(k, v)| (*k, v.clone())).collect()
        };
        if items.is_empty() {
            return Ok(());
        }
        self.inner.put_many(items.clone()).await?;
        let mut g = self.lock();
        for (k, v) in items {
            if g.map.remove(&k).is_some() {
                g.bytes = g.bytes.saturating_sub(v.len() as u64);
            }
        }
        Ok(())
    }
}

#[async_trait]
impl<O: Objects> Objects for BatchedObjects<O> {
    async fn get(&self, key: &ObjKey) -> Result<Option<Bytes>> {
        match self.peek(key) {
            Some(stored) => decode_verified(key, &stored).map(Some),
            None => self.inner.get(key).await,
        }
    }

    async fn get_stored(&self, key: &ObjKey) -> Result<Option<Bytes>> {
        match self.peek(key) {
            Some(stored) => Ok(Some(stored)),
            None => self.inner.get_stored(key).await,
        }
    }

    async fn get_range(&self, key: &ObjKey, r: Range<u64>) -> Result<Option<Bytes>> {
        if !matches!(key.class(), KeyClass::File | KeyClass::Chunk) {
            return Err(Error::RangeUnsupported(*key));
        }
        match self.peek(key) {
            Some(stored) => Ok(Some(slice_range(&decode_verified(key, &stored)?, r))),
            None => self.inner.get_range(key, r).await,
        }
    }

    async fn put(&self, key: &ObjKey, stored: Bytes) -> Result<()> {
        check_codec(&stored).map_err(|e| Error::Codec(*key, e))?;
        let mut g = self.lock();
        if !g.map.contains_key(key) {
            g.bytes += stored.len() as u64;
            g.map.insert(*key, stored);
        }
        Ok(())
    }

    async fn put_many(&self, items: Vec<(ObjKey, Bytes)>) -> Result<()> {
        for (key, stored) in items {
            self.put(&key, stored).await?;
        }
        Ok(())
    }

    async fn has_many(&self, keys: &[ObjKey]) -> Result<Vec<bool>> {
        let mut found: Vec<bool> = {
            let g = self.lock();
            keys.iter().map(|k| g.map.contains_key(k)).collect()
        };
        let rest: Vec<usize> = (0..keys.len()).filter(|&i| !found[i]).collect();
        if rest.is_empty() {
            return Ok(found);
        }
        let ask: Vec<ObjKey> = rest.iter().map(|&i| keys[i]).collect();
        for (i, present) in rest.into_iter().zip(self.inner.has_many(&ask).await?) {
            found[i] = present;
        }
        Ok(found)
    }

    async fn delete(&self, key: &ObjKey) -> Result<()> {
        let removed = self.lock().map.remove(key);
        if let Some(v) = removed {
            let mut g = self.lock();
            g.bytes = g.bytes.saturating_sub(v.len() as u64);
        }
        self.inner.delete(key).await
    }

    fn list(&self, prefix: KeyPrefix) -> BoxStream<'_, Result<(ObjKey, SystemTime)>> {
        self.inner.list(prefix)
    }
}

#[cfg(test)]
mod tests {
    use asc_core::id::FileId;

    use super::*;
    use crate::bucket::BucketObjects;
    use crate::codec::encode_stored;

    fn key(data: &[u8]) -> ObjKey {
        ObjKey::File(FileId::of(data))
    }

    #[tokio::test]
    async fn writes_wait_for_the_flush_and_reads_see_them() {
        let inner = Arc::new(BucketObjects::memory());
        let b = BatchedObjects::new(inner.clone());
        let (a, c) = (key(b"a"), key(b"c"));
        b.put(&a, encode_stored(b"a")).await.unwrap();
        b.put(&a, encode_stored(b"a")).await.unwrap();
        b.put(&c, encode_stored(b"c")).await.unwrap();
        assert_eq!(b.pending_len(), 2, "a key is buffered once");
        assert_eq!(&b.get(&a).await.unwrap().unwrap()[..], b"a");
        assert_eq!(&b.get_range(&c, 0..1).await.unwrap().unwrap()[..], b"c");
        assert_eq!(b.has_many(&[a, key(b"none")]).await.unwrap(), [true, false]);
        assert_eq!(inner.has_many(&[a, c]).await.unwrap(), [false, false]);
        b.flush().await.unwrap();
        assert_eq!(b.pending_len(), 0);
        assert_eq!(b.pending_bytes(), 0);
        assert_eq!(inner.has_many(&[a, c]).await.unwrap(), [true, true]);
        assert_eq!(&b.get(&c).await.unwrap().unwrap()[..], b"c");
    }

    #[tokio::test]
    async fn a_bad_value_fails_the_flush_and_stays_buffered() {
        let inner = Arc::new(BucketObjects::memory());
        let b = BatchedObjects::new(inner.clone());
        let good = key(b"good");
        b.put(&good, encode_stored(b"good")).await.unwrap();
        // A well-formed value that is not the object its key names.
        let bad = key(b"named");
        b.put(&bad, encode_stored(b"other")).await.unwrap();
        assert!(b.flush().await.is_err());
        assert_eq!(b.pending_len(), 2, "nothing is dropped on error");
        assert!(matches!(b.get(&bad).await, Err(Error::Corrupt(_))));
        b.delete(&bad).await.unwrap();
        b.flush().await.unwrap();
        assert_eq!(inner.has_many(&[good, bad]).await.unwrap(), [true, false]);
    }

    #[tokio::test]
    async fn a_bad_codec_byte_is_refused_at_put() {
        let b = BatchedObjects::new(Arc::new(BucketObjects::memory()));
        let r = b.put(&key(b"x"), Bytes::from_static(&[9, 1])).await;
        assert!(matches!(r, Err(Error::Codec(..))));
    }
}
