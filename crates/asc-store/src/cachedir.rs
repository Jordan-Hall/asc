//! `CacheDir`: decoded objects as plain files (SPEC §3.3, §3.5).
//!
//! Layout is `<root>/<class>/<h2>/<hex>`. Files hold decoded data so callers can copy,
//! reflink or pass them through. Writes stage in `tmp` and rename into place.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use async_trait::async_trait;
use bytes::Bytes;
use asc_core::id::FileId;
use filetime::FileTime;
use futures::StreamExt;
use futures::stream::{self, BoxStream};

use crate::codec::CodecError;
use crate::key::{KeyClass, KeyPrefix, ObjKey};
use crate::objects::{Error, Objects, Result, decode_verified, max_decoded, slice_range, verify};

/// Reads touch the mtime when it is older than this (LRU recency).
const TOUCH_AFTER: Duration = Duration::from_secs(24 * 60 * 60);
/// Eviction stops once the cache is at or below this share of the limit.
const EVICT_TARGET_PERCENT: u128 = 80;
/// Codec byte of `get_stored` output: the file holds raw decoded data.
const RAW: u8 = 0;

/// A directory of decoded objects.
#[derive(Clone, Debug)]
pub struct CacheDir {
    inner: Arc<Inner>,
}

#[derive(Debug)]
struct Inner {
    root: PathBuf,
    tmp: PathBuf,
    /// Delete files that fail verification on `get` (always on outside tests).
    evict_corrupt: bool,
    /// A tier that is only read: it never writes and never deletes (SPEC §3.5).
    read_only: bool,
}

fn io_err(e: io::Error) -> Error {
    Error::Store(object_store::Error::Generic {
        store: "CacheDir",
        source: Box::new(e),
    })
}

impl CacheDir {
    /// Opens (creating if missing) a cache at `root`, staging writes in `tmp`.
    ///
    /// `tmp` should be on the same volume as `root` so the final rename is atomic.
    pub fn open(root: impl Into<PathBuf>, tmp: impl Into<PathBuf>) -> Result<Self> {
        let (root, tmp) = (root.into(), tmp.into());
        fs::create_dir_all(&root).map_err(io_err)?;
        fs::create_dir_all(&tmp).map_err(io_err)?;
        Ok(Self {
            inner: Arc::new(Inner {
                root,
                tmp,
                evict_corrupt: true,
                read_only: false,
            }),
        })
    }

    /// Opens `root` as a read-only tier (`asc.cache.readOnly`, SPEC §3.5). It creates nothing:
    /// a missing directory is an empty cache. `put`, `delete` and eviction do nothing, and a
    /// corrupt file is reported as `Error::Corrupt` and left in place.
    pub fn open_read_only(root: impl Into<PathBuf>) -> Result<Self> {
        let root = root.into();
        Ok(Self {
            inner: Arc::new(Inner {
                tmp: root.clone(),
                root,
                evict_corrupt: false,
                read_only: true,
            }),
        })
    }

    /// Test hook: leave corrupt files in place so the conformance suite can re-read them.
    #[cfg(test)]
    pub(crate) fn keep_corrupt(self) -> Self {
        Self {
            inner: Arc::new(Inner {
                root: self.inner.root.clone(),
                tmp: self.inner.tmp.clone(),
                evict_corrupt: false,
                read_only: self.inner.read_only,
            }),
        }
    }

    /// Path of the content file of `id` if it is present, for reflink, ProjFS and passthrough.
    pub fn content_path(&self, id: FileId) -> Option<PathBuf> {
        let p = self.inner.path(&ObjKey::File(id));
        p.is_file().then_some(p)
    }

    /// Evicts the oldest-mtime files until the total is at most 80% of `max_bytes`.
    ///
    /// Does nothing while the cache is within `max_bytes`. Returns the bytes freed.
    pub async fn evict_to(&self, max_bytes: u64) -> Result<u64> {
        if self.inner.read_only {
            return Ok(0);
        }
        self.blocking(move |i| i.evict_to(max_bytes).map_err(io_err))
            .await
    }

    /// Moves `key` into `dest` (same volume); `false` if it is not here.
    pub(crate) async fn demote_to(&self, dest: &Self, key: ObjKey) -> Result<bool> {
        let dest = dest.inner.clone();
        self.blocking(move |i| i.demote(&dest, &key).map_err(io_err))
            .await
    }

    async fn blocking<T: Send + 'static>(
        &self,
        f: impl FnOnce(&Inner) -> Result<T> + Send + 'static,
    ) -> Result<T> {
        let inner = self.inner.clone();
        match tokio::task::spawn_blocking(move || f(&inner)).await {
            Ok(r) => r,
            Err(e) => Err(io_err(io::Error::other(e))),
        }
    }
}

impl Inner {
    fn path(&self, key: &ObjKey) -> PathBuf {
        self.root.join(key.path())
    }

    /// `0x00 ‖ file bytes`, touching the mtime when stale.
    ///
    /// A file over the class size cap is refused unread (bounded memory).
    fn read_stored(&self, key: &ObjKey) -> Result<Option<Bytes>> {
        let path = self.path(key);
        let mut file = match open_shared(&path) {
            Ok(f) => f,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(io_err(e)),
        };
        let meta = file.metadata().map_err(io_err)?;
        let max = max_decoded(key.class());
        if meta.len() > max {
            self.discard_if_corrupt(key);
            return Err(Error::Codec(*key, CodecError::TooLarge(max)));
        }
        let mut buf = Vec::with_capacity(meta.len() as usize + 1);
        buf.push(RAW);
        file.read_to_end(&mut buf).map_err(io_err)?;
        drop(file);
        let stale = meta
            .modified()
            .ok()
            .and_then(|m| m.elapsed().ok())
            .is_some_and(|age| age > TOUCH_AFTER);
        if stale && !self.read_only {
            // Best effort: a failed touch only makes the file look older.
            let _ = filetime::set_file_mtime(&path, FileTime::now());
        }
        Ok(Some(buf.into()))
    }

    fn discard_if_corrupt(&self, key: &ObjKey) {
        if self.evict_corrupt {
            let _ = fs::remove_file(self.path(key));
        }
    }

    fn get(&self, key: &ObjKey) -> Result<Option<Bytes>> {
        let Some(stored) = self.read_stored(key)? else {
            return Ok(None);
        };
        let data = stored.slice(1..);
        if let Err(e) = verify(key, &data) {
            self.discard_if_corrupt(key);
            return Err(e);
        }
        Ok(Some(data))
    }

    /// Atomic write: temp file, fsync, rename. An existing valid destination is success;
    /// an existing corrupt one is replaced.
    fn write(&self, key: &ObjKey, data: &[u8]) -> Result<()> {
        if matches!(self.get(key), Ok(Some(_))) {
            return Ok(());
        }
        self.stage_and_publish(key, data).map_err(io_err)
    }

    fn stage_and_publish(&self, key: &ObjKey, data: &[u8]) -> io::Result<()> {
        let dest = self.path(key);
        let tmp = self.tmp.join(uuid::Uuid::now_v7().to_string());
        let mut file = OpenOptions::new().write(true).create_new(true).open(&tmp)?;
        file.write_all(data)?;
        file.sync_all()?;
        drop(file);
        let renamed = dest
            .parent()
            .map_or(Ok(()), fs::create_dir_all)
            .and_then(|()| fs::rename(&tmp, &dest));
        match renamed {
            Ok(()) => Ok(()),
            Err(_) if dest.exists() => {
                let _ = fs::remove_file(&tmp);
                Ok(())
            }
            Err(e) => {
                let _ = fs::remove_file(&tmp);
                Err(e)
            }
        }
    }

    fn demote(&self, dest: &Self, key: &ObjKey) -> io::Result<bool> {
        let (from, to) = (self.path(key), dest.path(key));
        if !from.exists() {
            return Ok(false);
        }
        if to.exists() {
            remove_if_present(&from)?;
            return Ok(true);
        }
        if let Some(dir) = to.parent() {
            fs::create_dir_all(dir)?;
        }
        match fs::rename(&from, &to) {
            Ok(()) => Ok(true),
            Err(_) if !from.exists() => Ok(false),
            Err(e) => Err(e),
        }
    }

    fn evict_to(&self, max_bytes: u64) -> io::Result<u64> {
        let mut files = Vec::new();
        let mut total = 0u64;
        for item in Walk::new(&self.root, None)? {
            let (_, path, meta) = item?;
            total += meta.len();
            files.push((meta.modified()?, meta.len(), path));
        }
        if total <= max_bytes {
            return Ok(0);
        }
        let target = (u128::from(max_bytes) * EVICT_TARGET_PERCENT / 100) as u64;
        files.sort_by_key(|f| f.0);
        let mut freed = 0;
        for (_, len, path) in files {
            if total <= target {
                break;
            }
            // A file that cannot be removed is skipped and not counted as freed.
            if remove_if_present(&path).is_ok() {
                total -= len;
                freed += len;
            }
        }
        Ok(freed)
    }
}

/// Opens for reading; on Windows other handles may delete or replace the file meanwhile.
fn open_shared(path: &Path) -> io::Result<File> {
    let mut opts = OpenOptions::new();
    opts.read(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        // FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE
        opts.share_mode(0x1 | 0x2 | 0x4);
    }
    opts.open(path)
}

fn remove_if_present(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}

type WalkItem = (ObjKey, PathBuf, fs::Metadata);

/// Depth-first walk over object files below a directory, skipping non-object paths.
struct Walk {
    root: PathBuf,
    stack: Vec<fs::ReadDir>,
}

impl Walk {
    fn new(root: &Path, dir: Option<&str>) -> io::Result<Self> {
        let start = dir.map_or_else(|| root.to_owned(), |d| root.join(d));
        let stack = match fs::read_dir(start) {
            Ok(rd) => vec![rd],
            Err(e) if e.kind() == io::ErrorKind::NotFound => Vec::new(),
            Err(e) => return Err(e),
        };
        Ok(Self {
            root: root.to_owned(),
            stack,
        })
    }

    fn key_of(&self, path: &Path) -> Option<ObjKey> {
        let rel = path.strip_prefix(&self.root).ok()?;
        let parts: Option<Vec<_>> = rel.components().map(|c| c.as_os_str().to_str()).collect();
        ObjKey::parse(&parts?.join("/"))
    }
}

impl Iterator for Walk {
    type Item = io::Result<WalkItem>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            let entry = match self.stack.last_mut()?.next() {
                None => {
                    self.stack.pop();
                    continue;
                }
                Some(Err(e)) => return Some(Err(e)),
                Some(Ok(entry)) => entry,
            };
            let path = entry.path();
            let kind = match entry.file_type() {
                Ok(k) => k,
                Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
                Err(e) => return Some(Err(e)),
            };
            if kind.is_dir() {
                match fs::read_dir(&path) {
                    Ok(rd) => self.stack.push(rd),
                    Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                    Err(e) => return Some(Err(e)),
                }
            } else if let Some(key) = self.key_of(&path) {
                match entry.metadata() {
                    Ok(meta) => return Some(Ok((key, path, meta))),
                    Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                    Err(e) => return Some(Err(e)),
                }
            }
        }
    }
}

#[async_trait]
impl Objects for CacheDir {
    async fn get(&self, key: &ObjKey) -> Result<Option<Bytes>> {
        let key = *key;
        self.blocking(move |i| i.get(&key)).await
    }

    /// `0x00 ‖ file bytes`; the file is not verified.
    async fn get_stored(&self, key: &ObjKey) -> Result<Option<Bytes>> {
        let key = *key;
        self.blocking(move |i| i.read_stored(&key)).await
    }

    async fn get_range(&self, key: &ObjKey, r: Range<u64>) -> Result<Option<Bytes>> {
        if !matches!(key.class(), KeyClass::File | KeyClass::Chunk) {
            return Err(Error::RangeUnsupported(*key));
        }
        Ok(self.get(key).await?.map(|d| slice_range(&d, r)))
    }

    /// Verifies and decodes `stored`, then writes the decoded data.
    async fn put(&self, key: &ObjKey, stored: Bytes) -> Result<()> {
        let key = *key;
        let data = decode_verified(&key, &stored)?;
        drop(stored);
        if self.inner.read_only {
            return Ok(());
        }
        self.blocking(move |i| i.write(&key, &data)).await
    }

    async fn has_many(&self, keys: &[ObjKey]) -> Result<Vec<bool>> {
        let keys = keys.to_vec();
        self.blocking(move |i| Ok(keys.iter().map(|k| i.path(k).is_file()).collect()))
            .await
    }

    async fn delete(&self, key: &ObjKey) -> Result<()> {
        if self.inner.read_only {
            return Ok(());
        }
        let key = *key;
        self.blocking(move |i| remove_if_present(&i.path(&key)).map_err(io_err))
            .await
    }

    /// Entries are read lazily from the directory tree.
    fn list(&self, prefix: KeyPrefix) -> BoxStream<'_, Result<(ObjKey, SystemTime)>> {
        let items = Walk::new(&self.inner.root, prefix.dir())
            .map_err(io_err)
            .map(|walk| {
                walk.map(|r| {
                    let (key, _, meta) = r.map_err(io_err)?;
                    Ok((key, meta.modified().map_err(io_err)?))
                })
            });
        match items {
            Ok(it) => stream::iter(it).boxed(),
            Err(e) => stream::once(std::future::ready(Err(e))).boxed(),
        }
    }
}

/// Test adapter: re-encodes `get_stored` canonically so the shared conformance suite,
/// which compares against `encode_stored`, applies to stores that keep decoded data.
#[cfg(test)]
pub(crate) struct Canonical<O>(pub O);

#[cfg(test)]
#[async_trait]
impl<O: Objects> Objects for Canonical<O> {
    async fn get(&self, key: &ObjKey) -> Result<Option<Bytes>> {
        self.0.get(key).await
    }
    async fn get_stored(&self, key: &ObjKey) -> Result<Option<Bytes>> {
        let Some(stored) = self.0.get_stored(key).await? else {
            return Ok(None);
        };
        let data = crate::codec::decode_stored_capped(&stored, max_decoded(key.class()))
            .map_err(|e| Error::Codec(*key, e))?;
        Ok(Some(crate::codec::encode_stored(&data)))
    }
    async fn get_range(&self, key: &ObjKey, r: Range<u64>) -> Result<Option<Bytes>> {
        self.0.get_range(key, r).await
    }
    async fn put(&self, key: &ObjKey, stored: Bytes) -> Result<()> {
        self.0.put(key, stored).await
    }
    async fn has_many(&self, keys: &[ObjKey]) -> Result<Vec<bool>> {
        self.0.has_many(keys).await
    }
    async fn delete(&self, key: &ObjKey) -> Result<()> {
        self.0.delete(key).await
    }
    fn list(&self, prefix: KeyPrefix) -> BoxStream<'_, Result<(ObjKey, SystemTime)>> {
        self.0.list(prefix)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use asc_core::id::ChunkId;
    use futures::TryStreamExt;

    use super::*;
    use crate::codec::{decode_stored_capped, encode_stored};
    use crate::conformance::run_objects_suite;

    pub(crate) fn open_in(dir: &Path, name: &str) -> CacheDir {
        CacheDir::open(dir.join(name), dir.join("tmp").join(name)).unwrap()
    }

    /// Overwrites the backing file with decoded data, bypassing `put`.
    pub(crate) fn tamper(c: &CacheDir, key: ObjKey, stored: Bytes) {
        let data = decode_stored_capped(&stored, 256 * 1024 * 1024).unwrap_or(stored);
        let path = c.inner.path(&key);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, data).unwrap();
    }

    fn file_key(data: &[u8]) -> ObjKey {
        ObjKey::File(FileId::of(data))
    }

    #[tokio::test]
    async fn conformance() {
        let tmp = tempfile::tempdir().unwrap();
        let c = open_in(tmp.path(), "cache").keep_corrupt();
        let c2 = c.clone();
        run_objects_suite(&Canonical(c), |k, v| {
            tamper(&c2, k, v);
            std::future::ready(())
        })
        .await;
    }

    #[tokio::test]
    async fn layout_and_decoded_content() {
        let tmp = tempfile::tempdir().unwrap();
        let c = open_in(tmp.path(), "cache");
        let data = vec![9u8; 5000];
        let key = file_key(&data);
        c.put(&key, encode_stored(&data)).await.unwrap();
        let hex = FileId::of(&data).to_string();
        let path = tmp.path().join("cache/f").join(&hex[..2]).join(&hex);
        assert_eq!(fs::read(&path).unwrap(), data);
        assert_eq!(c.content_path(FileId::of(&data)), Some(path));
        assert!(c.content_path(FileId::of(b"nope")).is_none());
        let stored = c.get_stored(&key).await.unwrap().unwrap();
        assert_eq!(stored[0], 0);
        assert_eq!(&stored[1..], data.as_slice());
    }

    #[tokio::test]
    async fn corrupt_file_is_deleted_on_get() {
        let tmp = tempfile::tempdir().unwrap();
        let c = open_in(tmp.path(), "cache");
        let key = file_key(b"real");
        tamper(&c, key, encode_stored(b"fake"));
        assert!(matches!(c.get(&key).await, Err(Error::Corrupt(_))));
        assert_eq!(c.has_many(&[key]).await.unwrap(), [false]);
        assert!(c.get(&key).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn read_only_tier_neither_writes_nor_deletes() {
        let tmp = tempfile::tempdir().unwrap();
        let rw = open_in(tmp.path(), "cache");
        let good = b"good bytes".to_vec();
        let good_key = file_key(&good);
        rw.put(&good_key, encode_stored(&good)).await.unwrap();
        let ro = CacheDir::open_read_only(tmp.path().join("cache")).unwrap();
        assert_eq!(ro.get(&good_key).await.unwrap().unwrap(), good);

        // `put` succeeds without writing.
        let other = b"other bytes".to_vec();
        let other_key = file_key(&other);
        ro.put(&other_key, encode_stored(&other)).await.unwrap();
        assert_eq!(rw.has_many(&[other_key]).await.unwrap(), [false]);
        // A mismatched value is still refused.
        assert!(ro.put(&file_key(b"a"), encode_stored(b"b")).await.is_err());

        // A corrupt file reads as `Corrupt` and stays on disk.
        let bad_key = file_key(b"real");
        tamper(&rw, bad_key, encode_stored(b"fake"));
        let path = rw.inner.path(&bad_key);
        assert!(matches!(ro.get(&bad_key).await, Err(Error::Corrupt(_))));
        assert!(matches!(ro.get(&bad_key).await, Err(Error::Corrupt(_))));
        assert!(path.exists(), "the read-only tier must not delete");

        // `delete` and eviction leave the files alone; a missing root is an empty cache.
        ro.delete(&good_key).await.unwrap();
        assert_eq!(ro.evict_to(0).await.unwrap(), 0);
        assert_eq!(rw.has_many(&[good_key]).await.unwrap(), [true]);
        let absent = CacheDir::open_read_only(tmp.path().join("nowhere")).unwrap();
        assert!(absent.get(&good_key).await.unwrap().is_none());
        assert!(!tmp.path().join("nowhere").exists());
    }

    #[tokio::test]
    async fn put_rejects_mismatched_data() {
        let tmp = tempfile::tempdir().unwrap();
        let c = open_in(tmp.path(), "cache");
        let err = c.put(&file_key(b"a"), encode_stored(b"b")).await;
        assert!(matches!(err, Err(Error::Corrupt(_))));
        let listed: Vec<_> = c.list(KeyPrefix::all()).try_collect().await.unwrap();
        assert!(listed.is_empty());
    }

    #[tokio::test]
    async fn sixteen_concurrent_writers_leave_one_file() {
        let tmp = tempfile::tempdir().unwrap();
        let c = open_in(tmp.path(), "cache");
        let data = Bytes::from(b"same bytes".repeat(100));
        let key = file_key(&data);
        let tasks: Vec<_> = (0..16)
            .map(|_| {
                let (c, data) = (c.clone(), data.clone());
                tokio::spawn(async move { c.put(&key, encode_stored(&data)).await })
            })
            .collect();
        for t in tasks {
            t.await.unwrap().unwrap();
        }
        let listed: Vec<_> = c.list(KeyPrefix::all()).try_collect().await.unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(c.get(&key).await.unwrap().unwrap(), data);
        assert_eq!(fs::read_dir(&c.inner.tmp).unwrap().count(), 0);
    }

    #[tokio::test]
    async fn stale_mtime_is_touched_on_read() {
        let tmp = tempfile::tempdir().unwrap();
        let c = open_in(tmp.path(), "cache");
        let key = file_key(b"old");
        c.put(&key, encode_stored(b"old")).await.unwrap();
        let path = c.inner.path(&key);
        let old = FileTime::from_system_time(SystemTime::now() - Duration::from_secs(3 * 86_400));
        filetime::set_file_mtime(&path, old).unwrap();
        c.get(&key).await.unwrap().unwrap();
        let age = fs::metadata(&path)
            .unwrap()
            .modified()
            .unwrap()
            .elapsed()
            .unwrap();
        assert!(age < Duration::from_secs(3600), "{age:?}");
    }

    #[tokio::test]
    async fn evicts_oldest_down_to_eighty_percent() {
        let tmp = tempfile::tempdir().unwrap();
        let c = open_in(tmp.path(), "cache");
        let mut keys = Vec::new();
        for i in 0..10u8 {
            let data = vec![i; 100];
            let key = file_key(&data);
            c.put(&key, encode_stored(&data)).await.unwrap();
            let age = Duration::from_secs(1000 - u64::from(i) * 10);
            let t = FileTime::from_system_time(SystemTime::now() - age);
            filetime::set_file_mtime(c.inner.path(&key), t).unwrap();
            keys.push(key);
        }
        assert_eq!(c.evict_to(1000).await.unwrap(), 0);
        // 1000 bytes held, limit 500: evict oldest until at most 400 remain.
        assert_eq!(c.evict_to(500).await.unwrap(), 600);
        let present = c.has_many(&keys).await.unwrap();
        assert_eq!(present[..6], [false; 6]);
        assert_eq!(present[6..], [true; 4]);
    }

    #[tokio::test]
    async fn list_filters_by_class() {
        let tmp = tempfile::tempdir().unwrap();
        let c = open_in(tmp.path(), "cache");
        let chunk = ObjKey::Chunk(ChunkId::of(b"c"));
        c.put(&chunk, encode_stored(b"c")).await.unwrap();
        c.put(&file_key(b"f"), encode_stored(b"f")).await.unwrap();
        let only: Vec<_> = c
            .list(KeyPrefix::class(KeyClass::Chunk))
            .try_collect()
            .await
            .unwrap();
        assert_eq!(only.len(), 1);
        assert_eq!(only[0].0, chunk);
    }
}
