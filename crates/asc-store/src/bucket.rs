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

use crate::codec::CodecError;
use crate::codec::check_codec;
use crate::key::{KeyClass, KeyPrefix, ObjKey};
use crate::objects::{Error, Objects, Result, decode_verified, max_decoded, slice_range};
use crate::pack::PackLayer;

/// Concurrent `HEAD` requests per `has_many`.
const HAS_CONCURRENCY: usize = 32;

/// Objects stored as codec-prefixed values under one namespace root.
#[derive(Clone)]
pub struct BucketObjects {
    pub(crate) store: Arc<dyn ObjectStore>,
    pub(crate) root: Path,
    /// Packs of small objects, when this namespace uses them.
    pub(crate) packs: Option<Arc<PackLayer>>,
}

impl BucketObjects {
    /// Wraps a store; keys live under `root` (use `Path::default()` for none).
    pub fn new(store: Arc<dyn ObjectStore>, root: Path) -> Self {
        Self {
            store,
            root,
            packs: None,
        }
    }

    /// A private in-memory bucket.
    pub fn memory() -> Self {
        Self::new(Arc::new(InMemory::new()), Path::default())
    }

    /// A bucket in a local directory (created if missing).
    pub fn local(dir: &std::path::Path) -> Result<Self> {
        std::fs::create_dir_all(dir).map_err(|e| Error::Url(e.to_string()))?;
        let fs = LocalFileSystem::new_with_prefix(dir)?;
        Ok(Self::new(Arc::new(fs), Path::default()).with_bucket_packs(Default::default()))
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
            return Ok(Self::new(store, root).with_bucket_packs(Default::default()));
        }
        let (store, root) = object_store::parse_url(&url)?;
        Ok(Self::new(Arc::from(store), root).with_bucket_packs(Default::default()))
    }

    pub(crate) fn loc(&self, rel: &str) -> Path {
        if self.root.as_ref().is_empty() {
            Path::from(rel)
        } else {
            Path::from(format!("{}/{rel}", self.root))
        }
    }

    pub(crate) fn key_path(&self, key: &ObjKey) -> Path {
        self.loc(&key.path())
    }
}

/// Largest stored size for a value decoding to `max`: codec byte plus zstd framing slack.
fn stored_limit(max: u64) -> u64 {
    max.saturating_add(max >> 8).saturating_add(4096)
}

/// Buffers a GET body, or `None` once it exceeds `cap` bytes. The declared size is only a fast
/// first rejection: an untrusted store may send more than it declares.
pub(crate) async fn read_capped(
    r: object_store::GetResult,
    cap: u64,
) -> object_store::Result<Option<Bytes>> {
    if r.meta.size > cap {
        return Ok(None);
    }
    collect_capped(r.into_stream(), cap).await
}

async fn collect_capped(
    mut body: BoxStream<'static, object_store::Result<Bytes>>,
    cap: u64,
) -> object_store::Result<Option<Bytes>> {
    let mut buf = bytes::BytesMut::new();
    while let Some(chunk) = body.try_next().await? {
        if (buf.len() as u64).saturating_add(chunk.len() as u64) > cap {
            return Ok(None);
        }
        buf.extend_from_slice(&chunk);
    }
    Ok(Some(buf.freeze()))
}

impl BucketObjects {
    /// The loose object's stored bytes, size-capped.
    pub(crate) async fn get_loose_stored(&self, key: &ObjKey) -> Result<Option<Bytes>> {
        match self.store.get(&self.key_path(key)).await {
            Ok(r) => {
                // The bucket is untrusted: refuse an oversized body without buffering it.
                let max = max_decoded(key.class());
                match read_capped(r, stored_limit(max)).await? {
                    Some(b) => Ok(Some(b)),
                    None => Err(Error::Codec(*key, CodecError::TooLarge(max))),
                }
            }
            Err(object_store::Error::NotFound { .. }) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Presence of loose objects, one `HEAD` each.
    pub(crate) async fn has_many_loose(&self, keys: &[ObjKey]) -> Result<Vec<bool>> {
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

    /// Writes a loose object whose bytes the caller has verified against `key`. An existing
    /// valid one stays; a damaged one is repaired.
    pub(crate) async fn put_loose(&self, key: &ObjKey, stored: Bytes) -> Result<()> {
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
                // An oversized existing value is damaged too.
                let damaged = match self.get_loose_stored(key).await {
                    Ok(Some(e)) => matches!(
                        decode_verified(key, &e),
                        Err(Error::Corrupt(_) | Error::Codec(..))
                    ),
                    Ok(None) => false,
                    Err(Error::Codec(..)) => true,
                    Err(e) => return Err(e),
                };
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
}

#[async_trait]
impl Objects for BucketObjects {
    async fn get(&self, key: &ObjKey) -> Result<Option<Bytes>> {
        if self.packs.is_some() {
            return self.get_with_packs(key).await;
        }
        let Some(stored) = self.get_loose_stored(key).await? else {
            return Ok(None);
        };
        decode_verified(key, &stored).map(Some)
    }

    async fn get_stored(&self, key: &ObjKey) -> Result<Option<Bytes>> {
        if self.packs.is_some() {
            return self.get_stored_with_packs(key).await;
        }
        self.get_loose_stored(key).await
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
        if self.packs.is_some() && key.packable() {
            // Already packed and whole: nothing to write. A damaged packed copy is dropped from
            // the index so the loose copy written below is the one readers find.
            if self.packed_and_valid(key).await? {
                return Ok(());
            }
            self.forget_packed(&[*key]).await?;
        }
        self.put_loose(key, stored).await
    }

    async fn put_many(&self, items: Vec<(ObjKey, Bytes)>) -> Result<()> {
        self.put_many_with_packs(items).await
    }

    async fn has_many(&self, keys: &[ObjKey]) -> Result<Vec<bool>> {
        if self.packs.is_some() {
            return self.has_many_with_packs(keys).await;
        }
        self.has_many_loose(keys).await
    }

    async fn delete(&self, key: &ObjKey) -> Result<()> {
        self.forget_packed(&[*key]).await?;
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
        let loose = self
            .store
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
            .boxed();
        if self.packs.is_none() {
            return loose;
        }
        // Packed keys follow the loose ones; a key held both ways is listed twice, which every
        // consumer already tolerates (`StoreSet` does the same).
        loose.chain(self.list_packed(&prefix)).boxed()
    }
}

#[cfg(test)]
mod tests {
    use asc_core::id::FileId;
    use bytes::Bytes;
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
    async fn oversized_remote_object_is_refused_unread() {
        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let b = BucketObjects::new(store.clone(), Path::default());
        let key = ObjKey::File(FileId::of(b"big"));
        let mut raw = vec![0u8];
        raw.resize(1 + 4 * 1024 * 1024 + 1024 * 1024, 7);
        tamper(store, Path::default(), key, Bytes::from(raw)).await;
        for r in [b.get_stored(&key).await, b.get(&key).await.map(|_| None)] {
            assert!(matches!(r, Err(Error::Codec(_, CodecError::TooLarge(_)))));
        }
    }

    #[tokio::test]
    async fn put_repairs_an_oversized_existing_value() {
        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let b = BucketObjects::new(store.clone(), Path::default());
        let text = b"repair me".to_vec();
        let key = ObjKey::File(FileId::of(&text));
        let mut raw = vec![0u8];
        raw.resize(1 + 4 * 1024 * 1024 + 1024 * 1024, 7);
        tamper(store, Path::default(), key, Bytes::from(raw)).await;
        b.put(&key, encode_stored(&text)).await.unwrap();
        assert_eq!(b.get(&key).await.unwrap().unwrap(), text.as_slice());
    }

    #[tokio::test]
    async fn body_longer_than_declared_is_cut_off() {
        let chunks = (0..8).map(|_| Ok(Bytes::from(vec![1u8; 1024])));
        let body = futures::stream::iter(chunks).boxed();
        assert!(collect_capped(body, 4096).await.unwrap().is_none());
        let body = futures::stream::iter([Ok(Bytes::from(vec![1u8; 4096]))]).boxed();
        assert_eq!(
            collect_capped(body, 4096).await.unwrap().unwrap().len(),
            4096
        );
    }

    #[tokio::test]
    async fn memory_conformance() {
        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let root = Path::from("ns/0123");
        let b = BucketObjects::new(store.clone(), root.clone());
        run_objects_suite(&b, |k, v| tamper(store.clone(), root.clone(), k, v)).await;
    }

    #[tokio::test]
    async fn conformance_holds_with_a_pack_layer_attached() {
        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let root = Path::from("ns/0123");
        let b = BucketObjects::new(store.clone(), root.clone())
            .with_bucket_packs(crate::pack::PackConfig::default());
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
