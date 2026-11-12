//! Whole packs kept on local disk in front of a bucket (docs/design/PACKED-OBJECTS.md, "Tiered
//! store and cache").
//!
//! The first read of an object in a pack of at most `fetch_max` bytes fetches the whole pack
//! (one `GET`) and keeps it under `<dir>/<id>.pack`; every later read of an object of that pack
//! is a local `pread`, with no request. A larger pack is read by range as before. A pack is
//! immutable, so a cached copy whose trailer checked out stays right until it is evicted.
//! Eviction is the object cache's: [`CacheDir::evict_to`](crate::cachedir::CacheDir::evict_to)
//! counts the packs under its `pack/` directory as one file each, oldest first.

use std::fs::{self, File};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, SystemTime};

use bytes::Bytes;
use filetime::FileTime;

use super::format::{self, PackId};

/// The pack fetch limit when none is configured: the design's `pack.cache_fetch_max`.
pub const DEFAULT_FETCH_MAX: u64 = 8 << 20;
/// Reads touch the file's mtime when it is older than this, so eviction sees recent use.
const TOUCH_AFTER: Duration = Duration::from_secs(24 * 60 * 60);
/// Packs known to be larger than `fetch_max`, so they are not asked about again.
const TOO_BIG_MEMO: usize = 4096;

/// The name of the pack directory below a cache root.
pub const DIR: &str = "pack";

/// A directory of whole packs.
#[derive(Debug)]
pub struct PackCache {
    dir: PathBuf,
    fetch_max: u64,
    too_big: Mutex<Vec<PackId>>,
}

impl PackCache {
    /// Packs of the bucket named `source` (its URL) under `<cache_root>/pack/<scope>`, fetched
    /// whole up to `fetch_max` bytes. `fetch_max == 0` turns whole-pack fetching off (cached packs
    /// are still read). Pack ids are random, not content hashes, so each bucket gets its own
    /// directory: a pack of one bucket is never served for another that reuses its id.
    pub fn new(cache_root: &Path, source: &str, fetch_max: u64) -> Self {
        let scope = blake3::hash(source.as_bytes()).to_hex();
        Self {
            dir: cache_root.join(DIR).join(&scope.as_str()[..32]),
            fetch_max,
            too_big: Mutex::new(Vec::new()),
        }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn fetch_max(&self) -> u64 {
        self.fetch_max
    }

    fn path(&self, id: PackId) -> PathBuf {
        self.dir.join(format!("{}.pack", id.hex()))
    }

    /// Whether a pack of `size` bytes is fetched whole.
    pub fn wants(&self, size: u64) -> bool {
        size > 0 && size <= self.fetch_max
    }

    /// True when `id` was found too large for this cache before.
    pub fn known_too_big(&self, id: PackId) -> bool {
        self.too_big
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .contains(&id)
    }

    /// Remembers that `id` is read by range.
    pub fn note_too_big(&self, id: PackId) {
        let mut m = self.too_big.lock().unwrap_or_else(PoisonError::into_inner);
        if m.len() >= TOO_BIG_MEMO {
            m.clear();
        }
        if !m.contains(&id) {
            m.push(id);
        }
    }

    /// The bytes `[offset, offset + len)` of cached pack `id`; `None` when it is not cached or
    /// is shorter than that (a damaged file, which is removed so the next read fetches again).
    pub fn read(&self, id: PackId, offset: u64, len: u64) -> io::Result<Option<Bytes>> {
        let path = self.path(id);
        let mut f = match File::open(&path) {
            Ok(f) => f,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e),
        };
        let meta = f.metadata()?;
        let end = offset.saturating_add(len);
        if end > meta.len().saturating_sub(format::TRAILER_LEN as u64) {
            drop(f);
            remove_if_present(&path)?;
            return Ok(None);
        }
        let n = usize::try_from(len).map_err(|_| io::ErrorKind::InvalidInput)?;
        let mut buf = vec![0u8; n];
        f.seek(SeekFrom::Start(offset))?;
        f.read_exact(&mut buf)?;
        drop(f);
        touch_if_old(&path, &meta);
        Ok(Some(Bytes::from(buf)))
    }

    /// The first `n` bytes of cached pack `id` (its header and dictionary), if cached.
    pub fn read_head(&self, id: PackId, n: u64) -> io::Result<Option<Bytes>> {
        self.read(id, 0, n)
    }

    /// Keeps `bytes` as pack `id` when its trailer and entries check out; a pack that does not
    /// is refused and not written. The write stages beside the final name and renames, so a
    /// reader sees a whole pack or none.
    pub fn store(&self, id: PackId, bytes: &[u8]) -> io::Result<bool> {
        if format::parse(bytes).is_err() {
            return Ok(false);
        }
        fs::create_dir_all(&self.dir)?;
        let tmp = self
            .dir
            .join(format!(".{}.{}.tmp", id.hex(), PackId::random().hex()));
        let written = (|| {
            let mut f = File::create(&tmp)?;
            f.write_all(bytes)?;
            f.sync_all()
        })();
        if let Err(e) = written {
            let _ = fs::remove_file(&tmp);
            return Err(e);
        }
        match fs::rename(&tmp, self.path(id)) {
            Ok(()) => Ok(true),
            Err(e) => {
                let _ = fs::remove_file(&tmp);
                // Another reader cached the same pack first: its copy is the same bytes.
                if self.path(id).exists() {
                    Ok(true)
                } else {
                    Err(e)
                }
            }
        }
    }
}

/// The packs under `<cache_root>/pack/<scope>/`: `(mtime, len, path)` of each, for eviction.
pub fn list(cache_root: &Path) -> io::Result<Vec<(SystemTime, u64, PathBuf)>> {
    let mut out = Vec::new();
    for scope in read_dir_or_empty(&cache_root.join(DIR))? {
        let scope = scope?;
        if scope.file_type().is_ok_and(|t| t.is_dir()) {
            list_scope(&scope.path(), &mut out)?;
        }
    }
    Ok(out)
}

fn read_dir_or_empty(dir: &Path) -> io::Result<Box<dyn Iterator<Item = io::Result<fs::DirEntry>>>> {
    match fs::read_dir(dir) {
        Ok(rd) => Ok(Box::new(rd)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Box::new(std::iter::empty())),
        Err(e) => Err(e),
    }
}

fn list_scope(dir: &Path, out: &mut Vec<(SystemTime, u64, PathBuf)>) -> io::Result<()> {
    for entry in read_dir_or_empty(dir)? {
        let entry = entry?;
        let path = entry.path();
        let is_pack = path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.ends_with(".pack") && !n.starts_with('.'));
        if !is_pack {
            continue;
        }
        match entry.metadata() {
            Ok(m) if m.is_file() => out.push((m.modified()?, m.len(), path)),
            Ok(_) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

fn touch_if_old(path: &Path, meta: &fs::Metadata) {
    let old = meta
        .modified()
        .ok()
        .and_then(|m| SystemTime::now().duration_since(m).ok())
        .is_some_and(|age| age > TOUCH_AFTER);
    if old {
        // Recency only: a failure costs an earlier eviction.
        let _ = filetime::set_file_mtime(path, FileTime::now());
    }
}

fn remove_if_present(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use asc_core::id::FileId;

    use super::*;
    use crate::codec::encode_stored;
    use crate::key::ObjKey;

    fn pack() -> format::BuiltPack {
        let items: Vec<(ObjKey, Bytes)> = (0..5u8)
            .map(|i| {
                let data = vec![i; 100];
                (ObjKey::File(FileId::of(&data)), encode_stored(&data))
            })
            .collect();
        format::build(PackId::random(), items).unwrap()
    }

    #[test]
    fn a_stored_pack_reads_back_by_range_and_a_bad_one_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let c = PackCache::new(tmp.path(), "memory:///a", DEFAULT_FETCH_MAX);
        let p = pack();
        assert_eq!(c.read(p.id, 0, 4).unwrap(), None);
        assert!(c.store(p.id, &p.bytes).unwrap());
        let e = &p.entries[2];
        let got = c.read(p.id, e.offset, u64::from(e.len)).unwrap().unwrap();
        assert_eq!(got, format::slice(&p.bytes, e).unwrap());
        // A range into the trailer or past the end is no copy, and the file goes.
        let past = p.bytes.len() as u64;
        assert_eq!(c.read(p.id, past - 8, 4).unwrap(), None);
        assert_eq!(c.read(p.id, 0, 4).unwrap(), None);

        let mut bad = p.bytes.to_vec();
        let last = bad.len() - 1;
        bad[last] ^= 1;
        let other = PackId::random();
        assert!(!c.store(other, &bad).unwrap());
        assert_eq!(c.read(other, 0, 4).unwrap(), None);
    }

    #[test]
    fn the_listing_holds_packs_only_and_the_limit_decides_whole_fetches() {
        let tmp = tempfile::tempdir().unwrap();
        let c = PackCache::new(tmp.path(), "memory:///a", 1000);
        let p = pack();
        c.store(p.id, &p.bytes).unwrap();
        fs::write(c.dir().join(".x.tmp"), b"t").unwrap();
        fs::write(c.dir().join("note"), b"n").unwrap();
        let l = list(tmp.path()).unwrap();
        assert_eq!(l.len(), 1);
        assert_eq!(l[0].1, p.bytes.len() as u64);
        assert!(c.wants(1000));
        assert!(!c.wants(1001));
        assert!(!c.wants(0));
        assert!(!PackCache::new(tmp.path(), "memory:///a", 0).wants(1));
        assert!(list(&tmp.path().join("none")).unwrap().is_empty());

        // Another bucket does not see this bucket's pack under the same id.
        let other = PackCache::new(tmp.path(), "memory:///b", 1000);
        assert_ne!(other.dir(), c.dir());
        assert_eq!(other.read(p.id, 0, 4).unwrap(), None);

        let id = PackId::random();
        assert!(!c.known_too_big(id));
        c.note_too_big(id);
        assert!(c.known_too_big(id));
    }
}
