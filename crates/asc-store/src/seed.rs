//! Seed snapshots, client side and packing (SPEC §12.3; PLAN U72).
//!
//! A seed is a zstd tar of a [`CacheDir`](crate::cachedir::CacheDir) subtree (`c/ t/ f/ l/ k/
//! d/`, files under their final content-addressed names) whose first entry is `seed.json`.
//! [`unpack`] treats the archive as untrusted: only regular files with a canonical cache path
//! are accepted, sizes and counts are bounded, nothing is installed unless the whole archive
//! reads cleanly (the zstd frame carries a checksum), an existing file is never overwritten, and
//! no symlinked ancestor is followed. Cache files are verified on read, so a planted file with
//! the wrong bytes is evicted by the first `get`.

use std::collections::HashSet;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};

/// The only `seed.json` version this build reads and writes.
pub const VERSION: u32 = 1;
/// Name of the manifest entry, always first in the archive.
pub const MANIFEST: &str = "seed.json";
/// Largest compressed archive accepted.
pub const MAX_ARCHIVE_BYTES: u64 = 8 * 1024 * 1024 * 1024;
/// Largest total of unpacked bytes.
pub const MAX_TOTAL_BYTES: u64 = 32 * 1024 * 1024 * 1024;
/// Most entries accepted.
pub const MAX_ENTRIES: u64 = 4_000_000;
/// Largest `seed.json`.
const MAX_MANIFEST_BYTES: u64 = 64 * 1024;
/// Largest single `f/` or `k/` entry (SPEC §3.2 inline cap); other classes use 256 MiB.
const MAX_BLOB_ENTRY: u64 = 4 * 1024 * 1024;
const MAX_OTHER_ENTRY: u64 = 256 * 1024 * 1024;
const ZSTD_LEVEL: i32 = 3;

/// What can go wrong packing or unpacking a seed.
#[derive(Debug, thiserror::Error)]
pub enum SeedError {
    #[error("seed io: {0}")]
    Io(#[from] io::Error),
    #[error("seed is corrupt: {0}")]
    Corrupt(String),
    #[error("seed refused: {0}")]
    Refused(String),
    #[error("seed download: {0}")]
    Download(String),
}

pub type Result<T> = std::result::Result<T, SeedError>;

fn corrupt(m: impl Into<String>) -> SeedError {
    SeedError::Corrupt(m.into())
}

/// `seed.json`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SeedManifest {
    pub version: u32,
    /// `owner/repo`.
    pub repo: String,
    /// Lowercase hex changeset id of the master tip the seed was cut at.
    pub changeset: String,
    /// Unix seconds.
    pub created: u64,
}

impl SeedManifest {
    pub fn new(repo: &str, changeset: &str, created: u64) -> Self {
        Self {
            version: VERSION,
            repo: repo.to_owned(),
            changeset: changeset.to_owned(),
            created,
        }
    }

    fn validate(&self) -> Result<()> {
        if self.version != VERSION {
            return Err(SeedError::Refused(format!(
                "seed version {} (upgrade asc)",
                self.version
            )));
        }
        let hex = !self.changeset.is_empty()
            && self.changeset.len() <= 64
            && self
                .changeset
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
        if !hex || self.repo.is_empty() || self.repo.len() > 256 {
            return Err(corrupt("bad seed.json"));
        }
        Ok(())
    }
}

/// What [`unpack`] did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Unpacked {
    pub manifest: SeedManifest,
    /// Files newly installed.
    pub installed: u64,
    /// Files already present (left untouched).
    pub present: u64,
    pub bytes: u64,
}

/// What [`pack`] wrote.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct Packed {
    pub files: u64,
    /// Selected files that were missing (evicted meanwhile) and skipped.
    pub missing: u64,
    pub bytes: u64,
}

fn is_lower_hex(s: &str, len: usize) -> bool {
    s.len() == len
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn derived_dir_ok(s: &str) -> bool {
    let Some((name, ver)) = s.rsplit_once(".v") else {
        return false;
    };
    !name.is_empty()
        && name.len() <= 64
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'_' | b'-'))
        && !ver.is_empty()
        && ver.len() <= 9
        && ver.bytes().all(|b| b.is_ascii_digit())
}

/// Checks a cache-relative path (`/`-separated): `<class>/<h2>/<hex64>` or
/// `d/<name>.v<N>/<h2>/<hex64>`. Returns the largest size an entry there may have.
pub fn check_rel(rel: &str) -> Result<u64> {
    let bad = || SeedError::Refused(format!("bad entry path {rel:?}"));
    let parts: Vec<&str> = rel.split('/').collect();
    let (class, tail) = match parts.as_slice() {
        [c, h2, hex] => (*c, (*h2, *hex)),
        ["d", dir, h2, hex] if derived_dir_ok(dir) => ("d", (*h2, *hex)),
        _ => return Err(bad()),
    };
    if !matches!(class, "c" | "t" | "f" | "l" | "k" | "d") {
        return Err(bad());
    }
    let (h2, hex) = tail;
    if !is_lower_hex(hex, 64) || h2 != &hex[..2] {
        return Err(bad());
    }
    Ok(if matches!(class, "f" | "k") {
        MAX_BLOB_ENTRY
    } else {
        MAX_OTHER_ENTRY
    })
}

fn is_symlink_or_reparse(meta: &fs::Metadata) -> bool {
    if meta.file_type().is_symlink() {
        return true;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        // FILE_ATTRIBUTE_REPARSE_POINT
        if meta.file_attributes() & 0x400 != 0 {
            return true;
        }
    }
    false
}

/// Creates `dir` (below `root`, which the caller vouches for) one component at a time and
/// fails when a component exists as anything but a plain directory.
fn make_dirs(root: &Path, rel_dir: &Path) -> Result<PathBuf> {
    let mut cur = root.to_path_buf();
    for c in rel_dir.components() {
        cur.push(c);
        match fs::symlink_metadata(&cur) {
            Ok(m) if m.is_dir() && !is_symlink_or_reparse(&m) => {}
            Ok(_) => {
                return Err(SeedError::Refused(format!(
                    "{} is not a plain directory",
                    cur.display()
                )));
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => match fs::create_dir(&cur) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                    let m = fs::symlink_metadata(&cur)?;
                    if !m.is_dir() || is_symlink_or_reparse(&m) {
                        return Err(SeedError::Refused("raced directory".into()));
                    }
                }
                Err(e) => return Err(e.into()),
            },
            Err(e) => return Err(e.into()),
        }
    }
    Ok(cur)
}

// ------------------------------------------------------------------------------------ pack

/// Writes a seed of `rel_paths` (cache-relative, as [`check_rel`] accepts) from `cache` to
/// `out`. Missing files are skipped; symlinks and oversized files are refused.
pub fn pack(
    cache: &Path,
    manifest: &SeedManifest,
    rel_paths: &[String],
    out: impl Write,
) -> Result<Packed> {
    manifest.validate()?;
    let mut enc = zstd::Encoder::new(out, ZSTD_LEVEL)?;
    enc.include_checksum(true)?;
    let mut tar = tar::Builder::new(enc);
    tar.mode(tar::HeaderMode::Deterministic);

    let json = serde_json::to_vec(manifest).map_err(|e| corrupt(e.to_string()))?;
    append_bytes(&mut tar, MANIFEST, &json)?;

    let mut stats = Packed::default();
    let mut seen = HashSet::new();
    for rel in rel_paths {
        let cap = check_rel(rel)?;
        if !seen.insert(rel.as_str()) {
            continue;
        }
        let path = cache.join(rel);
        let meta = match fs::symlink_metadata(&path) {
            Ok(m) => m,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                stats.missing += 1;
                continue;
            }
            Err(e) => return Err(e.into()),
        };
        if !meta.is_file() || is_symlink_or_reparse(&meta) || meta.len() > cap {
            return Err(SeedError::Refused(format!("cannot pack {rel}")));
        }
        let mut f = File::open(&path)?;
        let mut h = tar::Header::new_gnu();
        h.set_entry_type(tar::EntryType::Regular);
        h.set_size(meta.len());
        h.set_mode(0o644);
        h.set_mtime(0);
        // The file may shrink between stat and read; `append_data` then errors, which fails the
        // pack rather than writing a short entry.
        tar.append_data(&mut h, rel, (&mut f).take(meta.len()))?;
        stats.files += 1;
        stats.bytes += meta.len();
    }
    tar.into_inner()?.finish()?.flush()?;
    Ok(stats)
}

fn append_bytes<W: Write>(tar: &mut tar::Builder<W>, name: &str, data: &[u8]) -> Result<()> {
    let mut h = tar::Header::new_gnu();
    h.set_entry_type(tar::EntryType::Regular);
    h.set_size(data.len() as u64);
    h.set_mode(0o644);
    h.set_mtime(0);
    tar.append_data(&mut h, name, data)?;
    Ok(())
}

// ----------------------------------------------------------------------------------- unpack

static STAGE_SEQ: AtomicU64 = AtomicU64::new(0);

/// Removes the staging directory on every exit path.
struct Stage(PathBuf);

impl Drop for Stage {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Reads a seed from `src` into `cache`. Installs nothing unless the whole archive is sound.
///
/// `expect_repo`, when given, must equal the manifest's `repo`.
pub fn unpack_reader(src: impl Read, cache: &Path, expect_repo: Option<&str>) -> Result<Unpacked> {
    fs::create_dir_all(cache)?;
    let root_meta = fs::symlink_metadata(cache)?;
    if !root_meta.is_dir() {
        return Err(SeedError::Refused("cache root is not a directory".into()));
    }
    let n = STAGE_SEQ.fetch_add(1, Ordering::Relaxed);
    let stage = Stage(cache.join(format!(".seed-stage-{}-{n}", std::process::id())));
    fs::create_dir(&stage.0)?;

    let dec = zstd::Decoder::new(src)?;
    let mut ar = tar::Archive::new(dec);
    let mut manifest: Option<SeedManifest> = None;
    let mut staged: Vec<String> = Vec::new();
    let mut seen = HashSet::new();
    let (mut total, mut count) = (0u64, 0u64);

    for entry in ar.entries()? {
        let mut entry = entry.map_err(|e| corrupt(e.to_string()))?;
        count += 1;
        if count > MAX_ENTRIES {
            return Err(SeedError::Refused("too many entries".into()));
        }
        if entry.header().entry_type() != tar::EntryType::Regular {
            return Err(SeedError::Refused("entry is not a regular file".into()));
        }
        let name = std::str::from_utf8(&entry.path_bytes())
            .map_err(|_| corrupt("entry path is not utf-8"))?
            .to_owned();
        let size = entry.header().size().map_err(|e| corrupt(e.to_string()))?;
        if manifest.is_none() {
            if name != MANIFEST || size > MAX_MANIFEST_BYTES {
                return Err(corrupt("seed.json must be the first entry"));
            }
            let mut buf = Vec::new();
            (&mut entry).take(size).read_to_end(&mut buf)?;
            let m: SeedManifest =
                serde_json::from_slice(&buf).map_err(|e| corrupt(format!("seed.json: {e}")))?;
            m.validate()?;
            if let Some(r) = expect_repo
                && m.repo != r
            {
                return Err(SeedError::Refused(format!("seed is for {}", m.repo)));
            }
            manifest = Some(m);
            continue;
        }
        let cap = check_rel(&name)?;
        if size > cap {
            return Err(SeedError::Refused(format!("entry {name} is too large")));
        }
        total = total.saturating_add(size);
        if total > MAX_TOTAL_BYTES {
            return Err(SeedError::Refused("seed unpacks too large".into()));
        }
        if !seen.insert(name.clone()) {
            return Err(corrupt(format!("duplicate entry {name}")));
        }
        let dst = stage.0.join(format!("{}", staged.len()));
        let mut f = OpenOptions::new().write(true).create_new(true).open(&dst)?;
        let copied = io::copy(&mut (&mut entry).take(size), &mut f)?;
        if copied != size {
            return Err(corrupt(format!("entry {name} is truncated")));
        }
        staged.push(name);
    }
    // The tar reader stops at its end marker; drain the rest (bounded) so the zstd frame's
    // checksum is verified and a truncated or flipped tail is rejected.
    let mut rest = ar.into_inner().take(MAX_TOTAL_BYTES);
    io::copy(&mut rest, &mut io::sink()).map_err(|e| corrupt(e.to_string()))?;
    let manifest = manifest.ok_or_else(|| corrupt("empty seed"))?;

    let (mut installed, mut present) = (0u64, 0u64);
    for (i, rel) in staged.iter().enumerate() {
        let src = stage.0.join(i.to_string());
        let rel_path = Path::new(rel);
        let parent = rel_path.parent().unwrap_or(Path::new(""));
        let dir = make_dirs(cache, parent)?;
        let dst = dir.join(rel_path.file_name().unwrap_or_default());
        if fs::symlink_metadata(&dst).is_ok() {
            present += 1;
            continue;
        }
        match fs::rename(&src, &dst) {
            Ok(()) => installed += 1,
            Err(_) if fs::symlink_metadata(&dst).is_ok() => present += 1,
            Err(e) => return Err(e.into()),
        }
    }
    Ok(Unpacked {
        manifest,
        installed,
        present,
        bytes: total,
    })
}

/// Unpacks the seed file at `path`.
pub fn unpack_file(path: &Path, cache: &Path, expect_repo: Option<&str>) -> Result<Unpacked> {
    let f = File::open(path)?;
    if f.metadata()?.len() > MAX_ARCHIVE_BYTES {
        return Err(SeedError::Refused("seed archive is too large".into()));
    }
    unpack_reader(io::BufReader::new(f), cache, expect_repo)
}

/// Fetches the seed at `url` (`file:`, `http:` or `https:`) and unpacks it into `cache`.
///
/// `http(s)` bodies stream to a temporary file under `cache` (bounded by
/// [`MAX_ARCHIVE_BYTES`]), so memory stays constant.
pub async fn unpack(url: &url::Url, cache: &Path, expect_repo: Option<&str>) -> Result<Unpacked> {
    let cache = cache.to_path_buf();
    let expect = expect_repo.map(str::to_owned);
    match url.scheme() {
        "file" => {
            let path = url
                .to_file_path()
                .map_err(|()| SeedError::Refused("bad file url".into()))?;
            blocking(move || unpack_file(&path, &cache, expect.as_deref())).await
        }
        "http" | "https" => {
            fs::create_dir_all(&cache)?;
            let n = STAGE_SEQ.fetch_add(1, Ordering::Relaxed);
            let tmp = cache.join(format!(".seed-dl-{}-{n}", std::process::id()));
            let _guard = RemoveOnDrop(tmp.clone());
            download(url, &tmp).await?;
            blocking(move || unpack_file(&tmp, &cache, expect.as_deref())).await
        }
        s => Err(SeedError::Refused(format!("unsupported seed scheme {s:?}"))),
    }
}

/// [`unpack`] for a URL given as text (a command-line flag or the discovery document).
pub async fn unpack_str(url: &str, cache: &Path, expect_repo: Option<&str>) -> Result<Unpacked> {
    let url = url::Url::parse(url).map_err(|e| SeedError::Refused(format!("bad seed url: {e}")))?;
    unpack(&url, cache, expect_repo).await
}

struct RemoveOnDrop(PathBuf);

impl Drop for RemoveOnDrop {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

async fn blocking<T: Send + 'static>(f: impl FnOnce() -> Result<T> + Send + 'static) -> Result<T> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| SeedError::Download(e.to_string()))?
}

async fn download(url: &url::Url, to: &Path) -> Result<()> {
    use tokio::io::AsyncWriteExt;
    let dl = |e: reqwest::Error| SeedError::Download(e.to_string());
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::limited(3))
        .connect_timeout(std::time::Duration::from_secs(15))
        .build()
        .map_err(dl)?;
    let mut resp = client
        .get(url.clone())
        .send()
        .await
        .map_err(dl)?
        .error_for_status()
        .map_err(dl)?;
    if resp.content_length().is_some_and(|n| n > MAX_ARCHIVE_BYTES) {
        return Err(SeedError::Refused("seed archive is too large".into()));
    }
    let mut f = tokio::fs::File::create(to).await?;
    let mut got = 0u64;
    while let Some(chunk) = resp.chunk().await.map_err(dl)? {
        got += chunk.len() as u64;
        if got > MAX_ARCHIVE_BYTES {
            return Err(SeedError::Refused("seed archive is too large".into()));
        }
        f.write_all(&chunk).await?;
    }
    f.flush().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use bytes::Bytes;
    use asc_core::id::FileId;

    use super::*;
    use crate::cachedir::CacheDir;
    use crate::codec::encode_stored;
    use crate::key::ObjKey;
    use crate::objects::Objects;

    fn cache(dir: &Path, name: &str) -> CacheDir {
        CacheDir::open(dir.join(name), dir.join("tmp").join(name)).unwrap()
    }

    fn rel(data: &[u8]) -> String {
        ObjKey::File(FileId::of(data)).path()
    }

    async fn put(c: &CacheDir, data: &[u8]) -> String {
        let key = ObjKey::File(FileId::of(data));
        c.put(&key, encode_stored(data)).await.unwrap();
        key.path()
    }

    fn manifest() -> SeedManifest {
        SeedManifest::new("acme/web", &"ab".repeat(32), 1_700_000_000)
    }

    fn pack_to_vec(root: &Path, rels: &[String]) -> Vec<u8> {
        let mut out = Vec::new();
        pack(root, &manifest(), rels, &mut out).unwrap();
        out
    }

    /// Builds an archive by hand for hostile-input tests.
    fn raw_archive(entries: &[(&str, &[u8], tar::EntryType)]) -> Vec<u8> {
        let mut enc = zstd::Encoder::new(Vec::new(), 1).unwrap();
        enc.include_checksum(true).unwrap();
        let mut tar = tar::Builder::new(enc);
        let json = serde_json::to_vec(&manifest()).unwrap();
        append_bytes(&mut tar, MANIFEST, &json).unwrap();
        for (name, data, ty) in entries {
            let mut h = tar::Header::new_gnu();
            h.set_entry_type(*ty);
            h.set_size(data.len() as u64);
            h.set_mode(0o644);
            // Bypass `set_path` validation so traversal names reach the reader.
            let raw = h.as_old_mut();
            raw.name[..name.len()].copy_from_slice(name.as_bytes());
            h.set_cksum();
            tar.append(&h, *data).unwrap();
        }
        tar.into_inner().unwrap().finish().unwrap()
    }

    #[tokio::test]
    async fn round_trip_installs_verifiable_files() {
        let tmp = tempfile::tempdir().unwrap();
        let src = cache(tmp.path(), "src");
        let a = put(&src, b"alpha").await;
        let b = put(&src, b"beta").await;
        let bytes = pack_to_vec(&tmp.path().join("src"), &[a, b.clone(), b]);

        let dst_root = tmp.path().join("dst");
        let got = unpack_reader(&bytes[..], &dst_root, Some("acme/web")).unwrap();
        assert_eq!((got.installed, got.present), (2, 0));
        assert_eq!(got.manifest, manifest());
        let dst = cache(tmp.path(), "dst");
        let key = ObjKey::File(FileId::of(b"alpha"));
        assert_eq!(
            dst.get(&key).await.unwrap().unwrap(),
            Bytes::from_static(b"alpha")
        );
        assert_eq!(
            fs::read_dir(&dst_root)
                .unwrap()
                .filter(|e| {
                    e.as_ref()
                        .unwrap()
                        .file_name()
                        .to_string_lossy()
                        .starts_with(".seed")
                })
                .count(),
            0
        );
    }

    #[tokio::test]
    async fn seeded_cache_leaves_only_the_delta_to_fetch() {
        let tmp = tempfile::tempdir().unwrap();
        let remote = cache(tmp.path(), "remote");
        let mut all = Vec::new();
        for i in 0..20u32 {
            all.push(put(&remote, format!("blob {i}").as_bytes()).await);
        }
        // The seed was cut when only the first 15 existed.
        let bytes = pack_to_vec(&tmp.path().join("remote"), &all[..15]);
        let local_root = tmp.path().join("local");
        unpack_reader(&bytes[..], &local_root, None).unwrap();
        let local = cache(tmp.path(), "local");
        let mut fetched = 0;
        for i in 0..20u32 {
            let data = format!("blob {i}");
            let key = ObjKey::File(FileId::of(data.as_bytes()));
            if local.get(&key).await.unwrap().is_none() {
                fetched += 1;
                let stored = remote.get_stored(&key).await.unwrap().unwrap();
                local.put(&key, stored).await.unwrap();
            }
        }
        assert_eq!(
            fetched, 5,
            "a stale seed is still correct; only the delta is fetched"
        );
    }

    #[tokio::test]
    async fn existing_files_are_never_overwritten() {
        let tmp = tempfile::tempdir().unwrap();
        let src = cache(tmp.path(), "src");
        let a = put(&src, b"alpha").await;
        let bytes = pack_to_vec(&tmp.path().join("src"), std::slice::from_ref(&a));
        let dst_root = tmp.path().join("dst");
        fs::create_dir_all(dst_root.join(&a).parent().unwrap()).unwrap();
        fs::write(dst_root.join(&a), b"local").unwrap();
        let got = unpack_reader(&bytes[..], &dst_root, None).unwrap();
        assert_eq!((got.installed, got.present), (0, 1));
        assert_eq!(fs::read(dst_root.join(&a)).unwrap(), b"local");
    }

    #[tokio::test]
    async fn corrupted_archive_is_rejected_and_installs_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let src = cache(tmp.path(), "src");
        let mut rels = Vec::new();
        for i in 0..8u32 {
            rels.push(put(&src, format!("data {i}").repeat(50).as_bytes()).await);
        }
        let good = pack_to_vec(&tmp.path().join("src"), &rels);
        let dst_root = tmp.path().join("dst");
        for case in 0..3 {
            let mut bad = good.clone();
            match case {
                0 => {
                    let mid = bad.len() / 2;
                    bad[mid] ^= 0xff;
                }
                1 => bad.truncate(good.len() - 6),
                _ => {
                    let last = bad.len() - 1;
                    bad[last] ^= 0x01;
                }
            }
            assert!(
                unpack_reader(&bad[..], &dst_root, None).is_err(),
                "case {case}"
            );
        }
        let leftover: Vec<_> = fs::read_dir(&dst_root).unwrap().collect();
        assert!(
            leftover.is_empty(),
            "nothing installed, stage cleaned: {leftover:?}"
        );
    }

    #[test]
    fn hostile_entries_are_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let h = "ab".repeat(32);
        let cases: Vec<(String, tar::EntryType)> = vec![
            (format!("../f/ab/{h}"), tar::EntryType::Regular),
            (format!("/f/ab/{h}"), tar::EntryType::Regular),
            (format!("f/cd/{h}"), tar::EntryType::Regular),
            (format!("x/ab/{h}"), tar::EntryType::Regular),
            (
                format!("f/ab/{}", h.to_uppercase()),
                tar::EntryType::Regular,
            ),
            ("f/ab/short".into(), tar::EntryType::Regular),
            (format!("f/ab/{h}"), tar::EntryType::Symlink),
            (format!("f/ab/{h}"), tar::EntryType::Link),
            (format!("f/ab/{h}"), tar::EntryType::Directory),
            (format!("d/../ab/{h}"), tar::EntryType::Regular),
        ];
        for (name, ty) in cases {
            let a = raw_archive(&[(&name, b"x", ty)]);
            let root = tmp.path().join("c");
            assert!(unpack_reader(&a[..], &root, None).is_err(), "{name} {ty:?}");
            let any = fs::read_dir(&root).unwrap().count();
            assert_eq!(any, 0, "{name}");
        }
    }

    #[test]
    fn manifest_must_come_first_and_match() {
        let tmp = tempfile::tempdir().unwrap();
        // No manifest: the first entry is a cache file.
        let mut enc = zstd::Encoder::new(Vec::new(), 1).unwrap();
        enc.include_checksum(true).unwrap();
        let mut tar = tar::Builder::new(enc);
        append_bytes(&mut tar, &format!("f/ab/{}", "ab".repeat(32)), b"x").unwrap();
        let a = tar.into_inner().unwrap().finish().unwrap();
        assert!(unpack_reader(&a[..], &tmp.path().join("c"), None).is_err());
        // Wrong repo.
        let ok = raw_archive(&[]);
        assert!(matches!(
            unpack_reader(&ok[..], &tmp.path().join("c"), Some("evil/repo")),
            Err(SeedError::Refused(_))
        ));
        assert!(unpack_reader(&ok[..], &tmp.path().join("c"), Some("acme/web")).is_ok());
    }

    #[test]
    fn unknown_version_is_refused() {
        let mut m = manifest();
        m.version = 2;
        assert!(pack(Path::new("."), &m, &[], Vec::new()).is_err());
    }

    #[test]
    fn oversized_blob_entry_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let big = vec![0u8; (MAX_BLOB_ENTRY + 1) as usize];
        let name = format!("f/ab/{}", "ab".repeat(32));
        let a = raw_archive(&[(&name, &big, tar::EntryType::Regular)]);
        assert!(matches!(
            unpack_reader(&a[..], &tmp.path().join("c"), None),
            Err(SeedError::Refused(_))
        ));
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_class_directory_is_not_followed() {
        let tmp = tempfile::tempdir().unwrap();
        let outside = tmp.path().join("outside");
        fs::create_dir_all(&outside).unwrap();
        let root = tmp.path().join("c");
        fs::create_dir_all(&root).unwrap();
        std::os::unix::fs::symlink(&outside, root.join("f")).unwrap();
        let name = format!("f/ab/{}", "ab".repeat(32));
        let a = raw_archive(&[(&name, b"x", tar::EntryType::Regular)]);
        assert!(unpack_reader(&a[..], &root, None).is_err());
        assert_eq!(fs::read_dir(&outside).unwrap().count(), 0);
    }

    #[test]
    fn rel_paths_cover_the_cache_layout() {
        let h = "0f".repeat(32);
        for ok in [
            format!("c/0f/{h}"),
            format!("t/0f/{h}"),
            format!("f/0f/{h}"),
            format!("l/0f/{h}"),
            format!("k/0f/{h}"),
            format!("d/skip.v1/0f/{h}"),
        ] {
            assert!(check_rel(&ok).is_ok(), "{ok}");
        }
        assert!(check_rel(&rel(b"x")).is_ok());
        assert!(check_rel(&format!("d/Skip.v1/0f/{h}")).is_err());
        assert!(check_rel(&format!("g/0f/{h}")).is_err());
    }

    #[tokio::test]
    async fn unpack_from_file_url() {
        let tmp = tempfile::tempdir().unwrap();
        let src = cache(tmp.path(), "src");
        let a = put(&src, b"alpha").await;
        let bytes = pack_to_vec(&tmp.path().join("src"), &[a]);
        let file = tmp.path().join("s.tar.zst");
        fs::write(&file, bytes).unwrap();
        let url = url::Url::from_file_path(&file).unwrap();
        let got = unpack(&url, &tmp.path().join("dst"), None).await.unwrap();
        assert_eq!(got.installed, 1);
        let bad = url::Url::parse("ftp://x/y").unwrap();
        assert!(unpack(&bad, &tmp.path().join("dst"), None).await.is_err());
    }
}
