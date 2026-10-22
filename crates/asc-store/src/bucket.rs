//! `Objects` over an `object_store` bucket (SPEC §3.3).

use std::ops::Range;
use std::sync::Arc;
use std::time::SystemTime;

use async_trait::async_trait;
use bytes::Bytes;
use futures::stream::{self, BoxStream};
use futures::{StreamExt, TryStreamExt};
use object_store::local::LocalFileSystem;
use object_store::memory::InMemory;
use object_store::path::Path;
use object_store::{ObjectStore, ObjectStoreExt, PutMode, PutOptions, PutPayload};

use crate::codec::check_codec;
use crate::key::{KeyClass, KeyPrefix, ObjKey};
use crate::objects::{Error, Objects, Result, decode_verified, slice_range};

/// Concurrent `HEAD` requests per `has_many`.
const HAS_CONCURRENCY: usize = 32;

/// Objects stored as codec-prefixed values under one namespace root.
#[derive(Clone)]
pub struct BucketObjects {
    store: Arc<dyn ObjectStore>,
    root: Path,
}

impl BucketObjects {
    /// Wraps a store; keys live under `root` (use `Path::default()` for none).
    pub fn new(store: Arc<dyn ObjectStore>, root: Path) -> Self {
        Self { store, root }
    }

    /// A private in-memory bucket.
    pub fn memory() -> Self {
        Self::new(Arc::new(InMemory::new()), Path::default())
    }

    /// A bucket in a local directory (created if missing).
    pub fn local(dir: &std::path::Path) -> Result<Self> {
        std::fs::create_dir_all(dir).map_err(|e| Error::Url(e.to_string()))?;
        let fs = LocalFileSystem::new_with_prefix(dir)?;
        Ok(Self::new(Arc::new(fs), Path::default()))
    }

    /// Opens `file:///…`, `memory:///` or any scheme `object_store` can build; the URL path is the root.
    pub fn from_url(url: &str) -> Result<Self> {
        Self::from_url_with(url, None)
    }

    /// Like [`Bucket::from_url`], with optional static S3 credentials injected by the caller.
    pub fn from_url_with(url: &str, credentials: Option<crate::s3::S3Credentials>) -> Result<Self> {
        let url = url::Url::parse(url).map_err(|e| Error::Url(e.to_string()))?;
        if url.scheme() == "s3" {
            let (store, root) = crate::s3::open_with(&url, credentials)?;
            return Ok(Self::new(store, root));
        }
        let (store, root) = object_store::parse_url(&url)?;
        Ok(Self::new(Arc::from(store), root))
    }

    fn loc(&self, rel: &str) -> Path {
        if self.root.as_ref().is_empty() {
            Path::from(rel)
        } else {
            Path::from(format!("{}/{rel}", self.root))
        }
    }

    fn key_path(&self, key: &ObjKey) -> Path {
        self.loc(&key.path())
    }
}

#[async_trait]
impl Objects for BucketObjects {
    async fn get(&self, key: &ObjKey) -> Result<Option<Bytes>> {
        let Some(stored) = self.get_stored(key).await? else {
            return Ok(None);
        };
        decode_verified(key, &stored).map(Some)
    }

    async fn get_stored(&self, key: &ObjKey) -> Result<Option<Bytes>> {
        match self.store.get(&self.key_path(key)).await {
            Ok(r) => Ok(Some(r.bytes().await?)),
            Err(object_store::Error::NotFound { .. }) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    async fn get_range(&self, key: &ObjKey, r: Range<u64>) -> Result<Option<Bytes>> {
        if !matches!(key.class(), KeyClass::File | KeyClass::Chunk) {
            return Err(Error::RangeUnsupported(*key));
        }
        // Values are at most a few MiB (inline file or chunk) and may be compressed.
        Ok(self.get(key).await?.map(|d| slice_range(&d, r)))
    }

    async fn put(&self, key: &ObjKey, stored: Bytes) -> Result<()> {
        check_codec(&stored).map_err(|e| Error::Codec(*key, e))?;
        // Never write bytes that do not match their key.
        decode_verified(key, &stored)?;
        let path = self.key_path(key);
        let opts = PutOptions {
            mode: PutMode::Create,
            ..Default::default()
        };
        match self
            .store
            .put_opts(&path, PutPayload::from_bytes(stored.clone()), opts)
            .await
        {
            Ok(_) => Ok(()),
            Err(object_store::Error::AlreadyExists { .. }) => {
                // Readers verify, so a valid existing value stays. A corrupt or undecodable one is
                // repaired: keys are content addresses, so a verified value is always correct.
                let existing = self.get_stored(key).await?;
                let damaged = existing.is_some_and(|e| {
                    matches!(
                        decode_verified(key, &e),
                        Err(Error::Corrupt(_) | Error::Codec(..))
                    )
                });
                if damaged {
                    self.store
                        .put(&path, PutPayload::from_bytes(stored))
                        .await?;
                }
                Ok(())
            }
            Err(e) => Err(e.into()),
        }
    }

    async fn has_many(&self, keys: &[ObjKey]) -> Result<Vec<bool>> {
        stream::iter(keys.iter().copied())
            .map(|k| async move {
                match self.store.head(&self.key_path(&k)).await {
                    Ok(_) => Ok(true),
                    Err(object_store::Error::NotFound { .. }) => Ok(false),
                    Err(e) => Err(Error::from(e)),
                }
            })
            .buffered(HAS_CONCURRENCY)
            .try_collect()
            .await
    }

    async fn delete(&self, key: &ObjKey) -> Result<()> {
        match self.store.delete(&self.key_path(key)).await {
            Ok(()) | Err(object_store::Error::NotFound { .. }) => Ok(()),
            Err(e) => Err(e.into()),
        }
    }

    fn list(&self, prefix: KeyPrefix) -> BoxStream<'_, Result<(ObjKey, SystemTime)>> {
        let dir = prefix
            .dir()
            .map_or_else(|| self.root.clone(), |d| self.loc(d));
        let strip = if self.root.as_ref().is_empty() {
            String::new()
        } else {
            format!("{}/", self.root)
        };
        self.store
            .list(Some(&dir))
            .filter_map(move |r| {
                let item = match r {
                    Err(e) => Some(Err(e.into())),
                    Ok(meta) => meta
                        .location
                        .as_ref()
                        .strip_prefix(&strip)
                        .and_then(ObjKey::parse)
                        .map(|k| Ok((k, SystemTime::from(meta.last_modified)))),
                };
                std::future::ready(item)
            })
            .boxed()
    }
}

#[cfg(test)]
mod tests {
    use bytes::Bytes;
    use asc_core::id::FileId;
    use futures::TryStreamExt;

    use super::*;
    use crate::codec::encode_stored;
    use crate::conformance::run_objects_suite;

    /// Replaces the stored value behind `key`, bypassing the `Objects` API.
    async fn tamper(store: Arc<dyn ObjectStore>, root: Path, key: ObjKey, stored: Bytes) {
        let b = BucketObjects::new(store.clone(), root);
        store
            .put(&b.key_path(&key), PutPayload::from_bytes(stored))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn memory_conformance() {
        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let root = Path::from("ns/0123");
        let b = BucketObjects::new(store.clone(), root.clone());
        run_objects_suite(&b, |k, v| tamper(store.clone(), root.clone(), k, v)).await;
    }

    #[tokio::test]
    async fn local_conformance() {
        let dir = tempfile::tempdir().unwrap();
        let b = BucketObjects::local(dir.path()).unwrap();
        let (store, root) = (b.store.clone(), b.root.clone());
        run_objects_suite(&b, |k, v| tamper(store.clone(), root.clone(), k, v)).await;
    }

    #[tokio::test]
    async fn from_url_file_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let url = url::Url::from_directory_path(dir.path()).unwrap();
        let b = BucketObjects::from_url(url.as_str()).unwrap();
        let key = ObjKey::File(FileId::of(b"via url"));
        b.put(&key, encode_stored(b"via url")).await.unwrap();
        assert_eq!(&b.get(&key).await.unwrap().unwrap()[..], b"via url");
        let on_disk = dir.path().join("f").join(&key.path()[2..]);
        assert!(on_disk.exists(), "{on_disk:?}");
    }

    #[tokio::test]
    async fn list_filters_to_object_keys_under_root() {
        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let b = BucketObjects::new(store.clone(), Path::from("ns/ab"));
        let other = BucketObjects::new(store.clone(), Path::from("ns/cd"));
        let key = ObjKey::File(FileId::of(b"x"));
        b.put(&key, encode_stored(b"x")).await.unwrap();
        other
            .put(&ObjKey::File(FileId::of(b"y")), encode_stored(b"y"))
            .await
            .unwrap();
        store
            .put(
                &Path::from("ns/ab/r/refs/heads/master"),
                PutPayload::from_static(b"h"),
            )
            .await
            .unwrap();
        let all: Vec<_> = b.list(KeyPrefix::all()).try_collect().await.unwrap();
        assert_eq!(all.iter().map(|(k, _)| *k).collect::<Vec<_>>(), vec![key]);
        let trees: Vec<_> = b
            .list(KeyPrefix::class(KeyClass::Tree))
            .try_collect()
            .await
            .unwrap();
        assert!(trees.is_empty());
    }

    #[tokio::test]
    async fn put_rejects_bad_codec_and_range_kinds() {
        let b = BucketObjects::memory();
        let key = ObjKey::File(FileId::of(b"x"));
        assert!(matches!(
            b.put(&key, Bytes::from_static(&[7, 1])).await,
            Err(Error::Codec(..))
        ));
        let tree = ObjKey::Tree(asc_core::id::TreeId::of(b"x"));
        assert!(matches!(
            b.get_range(&tree, 0..1).await,
            Err(Error::RangeUnsupported(_))
        ));
    }
}
